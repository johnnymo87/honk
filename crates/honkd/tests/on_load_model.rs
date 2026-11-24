mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Honkd, Viewer};
use honk_testgoose::FakeGoose;
use serde_json::json;

fn create_hook_script(dir: &Path, script_body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("hook.sh");
    std::fs::write(&path, script_body).expect("write hook script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("set permissions");
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn test_on_load_model_switch_succeeds() {
    let fake = FakeGoose::start().await;
    let sid = "session-test-switch-succeeds";
    fake.insert_session(sid, "/tmp");

    let td = tempfile::tempdir().unwrap();
    let log_path = td.path().join("hook.log");
    let hook_body = format!(
        r#"#!/bin/sh
echo "$@" >> "{}"
if [ "$1" = "decide" ]; then
    echo "gemini-3.8-flash"
fi
"#,
        log_path.display()
    );
    let hook_path = create_hook_script(td.path(), &hook_body);

    let ts_before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--on-load-model-cmd",
            hook_path.to_str().unwrap(),
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let load_reply = v.call("session/load", json!({ "sessionId": sid })).await;

    let ts_after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let opts = load_reply["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    let model_opt = opts
        .iter()
        .find(|o| o["id"] == "model")
        .expect("model option");
    assert_eq!(model_opt["currentValue"], "gemini-3.8-flash");

    // Check hook log
    let log_content = std::fs::read_to_string(&log_path).expect("read log");
    let lines: Vec<&str> = log_content.lines().collect();
    assert_eq!(lines.len(), 2, "expected decide and applied in log: {log_content}");
    assert_eq!(
        lines[0],
        format!("decide --session {sid} --current claude-opus-5-5")
    );
    assert!(
        lines[1].starts_with(&format!(
            "applied --session {sid} --from claude-opus-5-5 --to gemini-3.8-flash --ts-ms "
        )),
        "unexpected applied call: {}",
        lines[1]
    );
    let ts_str = lines[1].split("--ts-ms ").nth(1).unwrap().trim();
    let ts: u64 = ts_str.parse().expect("parsed ts_ms");
    assert!(
        ts >= ts_before.saturating_sub(1000) && ts <= ts_after + 1000,
        "ts {ts} must be between {ts_before} and {ts_after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_on_load_model_noop_on_empty_or_same_model() {
    let fake = FakeGoose::start().await;
    let sid_empty = "session-noop-empty";
    let sid_same = "session-noop-same";
    fake.insert_session(sid_empty, "/tmp");
    fake.insert_session(sid_same, "/tmp");

    let td = tempfile::tempdir().unwrap();
    let log_path = td.path().join("hook.log");
    let hook_body = format!(
        r#"#!/bin/sh
echo "$@" >> "{}"
if [ "$1" = "decide" ]; then
    if [ "$3" = "{sid_empty}" ]; then
        echo ""
    elif [ "$3" = "{sid_same}" ]; then
        echo "claude-opus-5-5"
    fi
fi
"#,
        log_path.display()
    );
    let hook_path = create_hook_script(td.path(), &hook_body);

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--on-load-model-cmd",
            hook_path.to_str().unwrap(),
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;

    // 1. Empty stdout
    let load_empty = v.call("session/load", json!({ "sessionId": sid_empty })).await;
    let opts_empty = load_empty["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    assert_eq!(
        opts_empty.iter().find(|o| o["id"] == "model").unwrap()["currentValue"],
        "claude-opus-5-5"
    );

    // 2. Same model
    let load_same = v.call("session/load", json!({ "sessionId": sid_same })).await;
    let opts_same = load_same["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    assert_eq!(
        opts_same.iter().find(|o| o["id"] == "model").unwrap()["currentValue"],
        "claude-opus-5-5"
    );

    let log_content = std::fs::read_to_string(&log_path).expect("read log");
    let lines: Vec<&str> = log_content.lines().collect();
    assert_eq!(lines.len(), 2, "only 2 decide calls expected: {log_content}");
    assert!(
        lines.iter().all(|l| l.starts_with("decide")),
        "no applied calls should occur: {log_content}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_on_load_model_fail_open() {
    let fake = FakeGoose::start().await;
    let sid_fail = "session-fail-exit";
    let sid_timeout = "session-fail-timeout";
    fake.insert_session(sid_fail, "/tmp");
    fake.insert_session(sid_timeout, "/tmp");

    let td = tempfile::tempdir().unwrap();
    let log_path = td.path().join("hook.log");
    let hook_body = format!(
        r#"#!/bin/sh
echo "$@" >> "{}"
if [ "$1" = "decide" ]; then
    if [ "$3" = "{sid_fail}" ]; then
        echo "some error message on stderr" >&2
        exit 1
    elif [ "$3" = "{sid_timeout}" ]; then
        sleep 5
        echo "gemini-3.8-flash"
    fi
fi
"#,
        log_path.display()
    );
    let hook_path = create_hook_script(td.path(), &hook_body);

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--on-load-model-cmd",
            hook_path.to_str().unwrap(),
            "--on-load-model-timeout-secs",
            "1",
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;

    // Subcase A: Non-zero exit
    let load_fail = v.call("session/load", json!({ "sessionId": sid_fail })).await;
    let opts_fail = load_fail["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    assert_eq!(
        opts_fail.iter().find(|o| o["id"] == "model").unwrap()["currentValue"],
        "claude-opus-5-5"
    );

    // Subcase B: Timeout
    let t0 = Instant::now();
    let load_timeout = v.call("session/load", json!({ "sessionId": sid_timeout })).await;
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(4),
        "timeout should fail open within timeout secs"
    );
    let opts_timeout = load_timeout["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    assert_eq!(
        opts_timeout.iter().find(|o| o["id"] == "model").unwrap()["currentValue"],
        "claude-opus-5-5"
    );

    // Subcase C: Non-existent binary
    let sid_nonexistent = "session-fail-nonexistent";
    fake.insert_session(sid_nonexistent, "/tmp");
    let h_nonexistent = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--on-load-model-cmd",
            "/tmp/non_existent_hook_binary_12345",
        ],
    )
    .await;
    let mut v2 = Viewer::connect(&h_nonexistent).await;
    let load_nonexistent = v2
        .call("session/load", json!({ "sessionId": sid_nonexistent }))
        .await;
    let opts_nonexistent = load_nonexistent["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    assert_eq!(
        opts_nonexistent
            .iter()
            .find(|o| o["id"] == "model")
            .unwrap()["currentValue"],
        "claude-opus-5-5"
    );

    // Verify no applied calls occurred in log
    if let Ok(log_content) = std::fs::read_to_string(&log_path) {
        assert!(
            !log_content.contains("applied"),
            "no applied calls expected: {log_content}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_on_load_model_concurrent_activations() {
    let fake = FakeGoose::start().await;
    let sid = "session-concurrent-activation";
    fake.insert_session(sid, "/tmp");

    let td = tempfile::tempdir().unwrap();
    let log_path = td.path().join("hook.log");
    let hook_body = format!(
        r#"#!/bin/sh
echo "$@" >> "{}"
if [ "$1" = "decide" ]; then
    sleep 0.1
    echo "gemini-3.8-flash"
fi
"#,
        log_path.display()
    );
    let hook_path = create_hook_script(td.path(), &hook_body);

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--on-load-model-cmd",
            hook_path.to_str().unwrap(),
        ],
    )
    .await;

    let mut v1 = Viewer::connect(&h).await;
    let mut v2 = Viewer::connect(&h).await;

    let f1 = v1.call("session/load", json!({ "sessionId": sid }));
    let f2 = v2.call("session/load", json!({ "sessionId": sid }));
    let (r1, r2) = tokio::join!(f1, f2);

    let opts1 = r1["result"]["configOptions"].as_array().expect("opts1");
    let opts2 = r2["result"]["configOptions"].as_array().expect("opts2");
    assert_eq!(
        opts1.iter().find(|o| o["id"] == "model").unwrap()["currentValue"],
        "gemini-3.8-flash"
    );
    assert_eq!(
        opts2.iter().find(|o| o["id"] == "model").unwrap()["currentValue"],
        "gemini-3.8-flash"
    );

    let log_content = std::fs::read_to_string(&log_path).expect("read log");
    let lines: Vec<&str> = log_content.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "expected exactly 1 decide and 1 applied: {log_content}"
    );
    assert!(lines[0].starts_with("decide "));
    assert!(lines[1].starts_with("applied "));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_on_load_model_upstream_rpc_error() {
    let fake = FakeGoose::start().await;
    let sid = "session-upstream-rpc-error";
    fake.insert_session(sid, "/tmp");
    fake.set_config_option_error(Some(json!({
        "code": -32000,
        "message": "upstream refuses model switch"
    })));

    let td = tempfile::tempdir().unwrap();
    let log_path = td.path().join("hook.log");
    let hook_body = format!(
        r#"#!/bin/sh
echo "$@" >> "{}"
if [ "$1" = "decide" ]; then
    echo "gemini-3.8-flash"
fi
"#,
        log_path.display()
    );
    let hook_path = create_hook_script(td.path(), &hook_body);

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--on-load-model-cmd",
            hook_path.to_str().unwrap(),
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let load_reply = v.call("session/load", json!({ "sessionId": sid })).await;

    let opts = load_reply["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    assert_eq!(
        opts.iter().find(|o| o["id"] == "model").unwrap()["currentValue"],
        "claude-opus-5-5",
        "load_reply must retain original model on upstream RPC error"
    );

    let log_content = std::fs::read_to_string(&log_path).expect("read log");
    let lines: Vec<&str> = log_content.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "applied must NOT be called on explicit RPC error: {log_content}"
    );
    assert!(lines[0].starts_with("decide"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_on_load_model_upstream_reply_mismatch() {
    let fake = FakeGoose::start().await;
    let sid = "session-upstream-mismatch";
    fake.insert_session(sid, "/tmp");
    // Return a result where model is still claude-opus-5-5 instead of the requested gemini-3.8-flash
    fake.set_config_option_result(Some(json!({
        "configOptions": [
            {
                "id": "model",
                "currentValue": "claude-opus-5-5"
            }
        ]
    })));

    let td = tempfile::tempdir().unwrap();
    let log_path = td.path().join("hook.log");
    let hook_body = format!(
        r#"#!/bin/sh
echo "$@" >> "{}"
if [ "$1" = "decide" ]; then
    echo "gemini-3.8-flash"
fi
"#,
        log_path.display()
    );
    let hook_path = create_hook_script(td.path(), &hook_body);

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--on-load-model-cmd",
            hook_path.to_str().unwrap(),
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let load_reply = v.call("session/load", json!({ "sessionId": sid })).await;

    let opts = load_reply["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    assert_eq!(
        opts.iter().find(|o| o["id"] == "model").unwrap()["currentValue"],
        "claude-opus-5-5",
        "load_reply must retain original model on mismatch"
    );

    let log_content = std::fs::read_to_string(&log_path).expect("read log");
    let lines: Vec<&str> = log_content.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "applied must NOT be called on mismatch: {log_content}"
    );
    assert!(lines[0].starts_with("decide"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_on_load_model_upstream_timeout() {
    let fake = FakeGoose::start().await;
    let sid = "session-upstream-timeout";
    fake.insert_session(sid, "/tmp");
    // Delay set_config_option for 5s, while honkd RPC timeout is 1s
    fake.set_config_option_delay(Some(Duration::from_secs(5)));

    let td = tempfile::tempdir().unwrap();
    let log_path = td.path().join("hook.log");
    let hook_body = format!(
        r#"#!/bin/sh
echo "$@" >> "{}"
if [ "$1" = "decide" ]; then
    echo "gemini-3.8-flash"
fi
"#,
        log_path.display()
    );
    let hook_path = create_hook_script(td.path(), &hook_body);

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--on-load-model-cmd",
            hook_path.to_str().unwrap(),
            "--on-load-model-rpc-timeout-secs",
            "1",
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let load_reply = v.call("session/load", json!({ "sessionId": sid })).await;

    // Activation succeeds fail-open: load_reply keeps original model
    let opts = load_reply["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    assert_eq!(
        opts.iter().find(|o| o["id"] == "model").unwrap()["currentValue"],
        "claude-opus-5-5",
        "load_reply must retain original model when upstream times out"
    );

    // BUT applied IS called due to ambiguous timeout!
    let log_content = std::fs::read_to_string(&log_path).expect("read log");
    let lines: Vec<&str> = log_content.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "applied MUST be called on timeout: {log_content}"
    );
    assert!(lines[0].starts_with("decide "));
    assert!(lines[1].starts_with(&format!(
        "applied --session {sid} --from claude-opus-5-5 --to gemini-3.8-flash --ts-ms "
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_on_load_model_upstream_timeout_late_update_captured_in_drain() {
    let fake = FakeGoose::start().await;
    let sid = "session-upstream-timeout-late-update";
    fake.insert_session(sid, "/tmp");
    // Delay set_config_option for 1200ms, while honkd RPC timeout is 1s
    fake.set_config_option_delay(Some(Duration::from_millis(1200)));

    let td = tempfile::tempdir().unwrap();
    let log_path = td.path().join("hook.log");
    let hook_body = format!(
        r#"#!/bin/sh
echo "$@" >> "{}"
if [ "$1" = "decide" ]; then
    echo "gemini-3.8-flash"
elif [ "$1" = "applied" ]; then
    sleep 0.6
fi
"#,
        log_path.display()
    );
    let hook_path = create_hook_script(td.path(), &hook_body);

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--on-load-model-cmd",
            hook_path.to_str().unwrap(),
            "--on-load-model-rpc-timeout-secs",
            "1",
            "--on-load-model-applied-timeout-secs",
            "2",
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let load_reply = v.call("session/load", json!({ "sessionId": sid })).await;

    // Although set_config_option RPC call timed out, goose's late config_option_update
    // arrived during the applied hook's 600ms sleep and was captured in the drain loop:
    let opts = load_reply["result"]["configOptions"]
        .as_array()
        .expect("configOptions array");
    assert_eq!(
        opts.iter().find(|o| o["id"] == "model").unwrap()["currentValue"],
        "gemini-3.8-flash",
        "load_reply must capture target model from drained config_option_update"
    );

    let log_content = std::fs::read_to_string(&log_path).expect("read log");
    let lines: Vec<&str> = log_content.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "both decide and applied must be called: {log_content}"
    );
    assert!(lines[0].starts_with("decide "));
    assert!(lines[1].starts_with(&format!(
        "applied --session {sid} --from claude-opus-5-5 --to gemini-3.8-flash --ts-ms "
    )));
}
