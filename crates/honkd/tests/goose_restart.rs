mod common;

use common::{Honkd, Viewer, new_session, prompt_params};
use honk_testgoose::{FakeGoose, StubGoose, Turn};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const SECS: Duration = Duration::from_secs(5);

fn is_method(f: &Value, method: &str) -> bool {
    f.get("method").and_then(Value::as_str) == Some(method)
}

#[tokio::test(flavor = "multi_thread")]
async fn goose_restart_mid_turn_cancelled_reloaded_and_retry_single() {
    let Some(mut g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "g".into(),
        Box::new(Turn::Text(vec!["never".into()])),
    )]);
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;
    let id = v
        .request("session/prompt", prompt_params(&sid, "first", Some("a")))
        .await;
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
    })
    .await;
    g.restart().await;
    let r = v.response_timeout(&id, SECS * 3).await;
    assert_eq!(r["result"]["stopReason"], "cancelled");
    assert_eq!(r["result"]["_meta"]["honk"]["reason"], "upstream_restart");
    v.wait_for(|f| is_method(f, honk_wire::method::REPLAY_DONE))
        .await;
    assert_eq!(
        v.blocks.user_texts(),
        vec!["first".to_string()],
        "export shows the unanswered prompt once (P5e)"
    );
    g.model().push([Turn::Text(vec!["ok".into()])]);
    let id2 = v
        .request("session/prompt", prompt_params(&sid, "first", Some("b")))
        .await;
    let id3 = v
        .request("session/prompt", prompt_params(&sid, "first", Some("b")))
        .await; // double press
    v.response(&id2).await;
    v.response(&id3).await;
    let users: Vec<_> = g.export(&sid).await["conversation"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user" && m["content"][0]["text"] == "first")
        .cloned()
        .collect();
    assert_eq!(users.len(), 2, "original + exactly one retry");

    // Reconcile with A14: retry with the SAME client_req_id returns the cached cancelled reply
    let id_retry_a = v
        .request("session/prompt", prompt_params(&sid, "first", Some("a")))
        .await;
    let r_retry_a = v.response_timeout(&id_retry_a, SECS).await;
    assert_eq!(r_retry_a["result"]["stopReason"], "cancelled");
    assert_eq!(
        r_retry_a["result"]["_meta"]["honk"]["reason"],
        "upstream_restart"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn degraded_after_restart_refusal_test() {
    let mut fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    v.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Upstream goose restarts and is no longer patched
    fake.advertise_patched(false);
    fake.restart().await;

    // Wait briefly for control connection to reconnect and detect unpatched upstream
    tokio::time::sleep(Duration::from_millis(500)).await;

    let pid = v
        .request(
            "session/prompt",
            prompt_params(&sid, "test unpatched prompt", Some("crid-unpatched")),
        )
        .await;

    let resp = v.response_timeout(&pid, SECS).await;
    assert_eq!(
        resp.pointer("/error/code").and_then(Value::as_i64),
        Some(-32000),
        "refused prompt gets -32000 code: {resp:?}"
    );
    assert_eq!(
        resp.pointer("/error/message").and_then(Value::as_str),
        Some("upstream goose is no longer patched"),
        "refused prompt gets expected error message: {resp:?}"
    );

    // Stderr must contain the error log
    let stderr = h.stderr_text();
    assert!(
        stderr.contains("upstream goose is no longer patched"),
        "stderr must log error that upstream goose is no longer patched: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn staggered_reactivation_of_two_viewed_sessions() {
    let mut fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;

    let mut v1 = Viewer::connect(&h).await;
    let sid1 = new_session(&mut v1, ".").await;
    v1.call(
        "session/load",
        json!({"sessionId": &sid1, "cwd": ".", "mcpServers": []}),
    )
    .await;

    let mut v2 = Viewer::connect(&h).await;
    let sid2 = new_session(&mut v2, ".").await;
    v2.call(
        "session/load",
        json!({"sessionId": &sid2, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Both sessions have attached viewers. Goose restarts!
    let restart_time = Instant::now();
    fake.restart().await;

    let res = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            async {
                v1.wait_for(|f| {
                    is_method(f, honk_wire::method::REPLAY_DONE)
                        && f.pointer("/params/epoch") == Some(&json!(2))
                })
                .await;
                Instant::now()
            },
            async {
                v2.wait_for(|f| {
                    is_method(f, honk_wire::method::REPLAY_DONE)
                        && f.pointer("/params/epoch") == Some(&json!(2))
                })
                .await;
                Instant::now()
            }
        )
    })
    .await;

    if res.is_err() {
        eprintln!("honkd stderr:\n{}", h.stderr_text());
        panic!("timed out waiting for both sessions to re-activate");
    }
    let (t1, t2) = res.unwrap();

    let diff = if t1 > t2 {
        t1.duration_since(t2)
    } else {
        t2.duration_since(t1)
    };

    assert!(
        diff >= Duration::from_millis(400),
        "staggered re-activation must be at least ~500ms apart; diff was {diff:?} (t1: {:?}, t2: {:?}, restart: {:?})",
        t1.duration_since(restart_time),
        t2.duration_since(restart_time),
        restart_time
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unviewed_session_lazily_reactivated_on_first_prompt() {
    let mut fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;

    // Session 1: viewed
    let mut v1 = Viewer::connect(&h).await;
    let sid1 = new_session(&mut v1, ".").await;
    v1.call(
        "session/load",
        json!({"sessionId": &sid1, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Session 2: created, then detached (unviewed)
    let mut v2 = Viewer::connect(&h).await;
    let sid2 = new_session(&mut v2, ".").await;
    v2.call(
        "session/load",
        json!({"sessionId": &sid2, "cwd": ".", "mcpServers": []}),
    )
    .await;
    v2.call("session/close", json!({"sessionId": &sid2})).await;

    // Goose restarts!
    fake.restart().await;

    // Session 1 with attached viewer re-activates eagerly
    v1.wait_for(|f| {
        is_method(f, honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch") == Some(&json!(2))
    })
    .await;

    // Session 2 had no attached viewers: verify it is NOT currently held in state
    let state_bytes = std::fs::read(h.state_dir.path().join("state.json")).expect("read state");
    let state: Value = serde_json::from_slice(&state_bytes).expect("parse state");
    assert!(
        state["sessions"].get(&sid2).is_none(),
        "unviewed session must not be held in state before lazy activation"
    );

    // Prompting session 2 lazily re-activates it on first prompt
    let mut v2_new = Viewer::connect(&h).await;
    fake.script_prompt(vec!["lazy prompt ok".into()]);
    let req_id = v2_new
        .request(
            "session/prompt",
            prompt_params(&sid2, "hello unviewed", Some("crid-lazy")),
        )
        .await;

    let resp = v2_new.response_timeout(&req_id, SECS * 2).await;
    assert_eq!(
        resp.pointer("/result/stopReason").and_then(Value::as_str),
        Some("end_turn"),
        "unviewed session lazily re-activated and prompt completed successfully: {resp:?}"
    );

    // State now holds session 2
    let state_bytes2 = std::fs::read(h.state_dir.path().join("state.json")).expect("read state");
    let state2: Value = serde_json::from_slice(&state_bytes2).expect("parse state");
    assert!(
        state2["sessions"].get(&sid2).is_some(),
        "session 2 is held in state after lazy activation"
    );
}
