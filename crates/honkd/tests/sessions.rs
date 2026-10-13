use common::{Honkd, Viewer};
use honk_testgoose::{FakeGoose, StubGoose};
use std::time::Duration;

mod common;

#[tokio::test(flavor = "multi_thread")]
async fn session_new_passes_extension_results_verbatim_against_stub_goose() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect_pigeon(&h).await;

    let res = v
        .call(
            "session/new",
            serde_json::json!({
                "cwd": g.cwd().to_str().unwrap(),
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": []
                }
            }),
        )
        .await;

    // 1. Result is verbatim (extensionResults present)
    let extension_results = res["result"]
        .pointer("/_meta/extensionResults")
        .or_else(|| res["result"].get("extensionResults"))
        .unwrap_or_else(|| panic!("expected extensionResults in session/new result: {res}"));
    assert_eq!(extension_results, &serde_json::json!([]));

    let sid = res["result"]["sessionId"]
        .as_str()
        .expect("sessionId string");

    // 2. State file holds session with conn_id and cwd, and no orphans
    let state_bytes = std::fs::read(h.state_dir.path().join("state.json")).expect("read state");
    let state: honkd::state::State = serde_json::from_slice(&state_bytes).expect("parse state");

    assert!(state.orphan_conns.is_empty(), "expected zero orphans");
    let held = state
        .sessions
        .get(sid)
        .expect("session present in held sessions");
    assert_eq!(held.cwd, g.cwd().to_str().unwrap());
    assert!(!held.conn_id.is_empty());
    assert_eq!(held.epoch, 1);

    // Session log file was created
    let log_file = h
        .state_dir
        .path()
        .join("sessions")
        .join(format!("{sid}.jsonl"));
    assert!(log_file.exists(), "session log must exist");
}

#[tokio::test(flavor = "multi_thread")]
async fn session_list_marks_held_true() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect_pigeon(&h).await;

    let res = v
        .call(
            "session/new",
            serde_json::json!({
                "cwd": "/tmp",
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": []
                }
            }),
        )
        .await;

    let sid = res["result"]["sessionId"]
        .as_str()
        .expect("sessionId string");

    let list_res = v.call("session/list", serde_json::json!({})).await;
    let sessions = list_res["result"]["sessions"]
        .as_array()
        .expect("sessions array in session/list");
    let entry = sessions
        .iter()
        .find(|s| s["sessionId"] == sid)
        .expect("created session found in session/list");
    assert_eq!(entry["_meta"]["honk"]["held"], true);
    assert_eq!(entry["_meta"]["honk"]["running"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn orphan_in_state_deleted_on_restart() {
    let fake = FakeGoose::start().await;
    let mut h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;

    // Create a connection on FakeGoose to simulate a connection created in step 1 of create_session
    let up = honk_upstream::Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await
    .unwrap();
    let orphan_id = up.conn_id().to_string();

    // Stop honkd
    h.kill9();

    // Inject the orphan connection into state.json while honkd is stopped
    let state_path = h.state_dir.path().join("state.json");
    let state_bytes = std::fs::read(&state_path).expect("read state");
    let mut state: honkd::state::State = serde_json::from_slice(&state_bytes).expect("parse state");
    state.orphan_conns.push(orphan_id.clone());
    let new_state_bytes = serde_json::to_vec_pretty(&state).expect("serialize state");
    std::fs::write(&state_path, new_state_bytes).expect("write modified state");

    // Restart honkd
    h.restart().await;

    // FakeGoose must have received DELETE for the orphan id
    assert!(
        fake.deletes().contains(&orphan_id),
        "expected FakeGoose to record DELETE for orphan id, got deletes: {:?}",
        fake.deletes()
    );

    // State file after restart must have cleared orphan_conns
    let state_bytes_after = std::fs::read(&state_path).expect("read state after restart");
    let state_after: honkd::state::State =
        serde_json::from_slice(&state_bytes_after).expect("parse state after restart");
    assert!(
        state_after.orphan_conns.is_empty(),
        "orphans must be cleared after restart"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn session_new_developer_extensions_verbatim_matches_direct_goose() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };

    // 1. Direct call to goose for baseline
    let mut direct_up = honk_upstream::Upstream::initialize(
        &g.url(),
        g.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await
    .expect("initialize direct upstream");
    direct_up
        .subscribe(None)
        .await
        .expect("subscribe direct conn");

    let direct_res = direct_up
        .call(
            "session/new",
            serde_json::json!({
                "cwd": g.cwd().to_str().unwrap(),
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": [{
                        "type": "platform",
                        "name": "developer"
                    }]
                }
            }),
            None,
            std::time::Duration::from_secs(30),
        )
        .await
        .expect("call direct session/new");

    let direct_exts = direct_res
        .pointer("/_meta/extensionResults")
        .or_else(|| direct_res.get("extensionResults"))
        .expect("direct extensionResults");
    assert!(
        !direct_exts.as_array().expect("array").is_empty(),
        "expected non-empty developer extension results directly from goose"
    );

    // 2. Call through honkd
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect_pigeon(&h).await;

    let honkd_res = v
        .call(
            "session/new",
            serde_json::json!({
                "cwd": g.cwd().to_str().unwrap(),
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": [{
                        "type": "platform",
                        "name": "developer"
                    }]
                }
            }),
        )
        .await;

    let honkd_exts = honkd_res["result"]
        .pointer("/_meta/extensionResults")
        .or_else(|| honkd_res["result"].get("extensionResults"))
        .expect("honkd extensionResults");

    assert_eq!(
        honkd_exts, direct_exts,
        "honkd must return extensionResults verbatim matching direct goose"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn session_close_and_cancel_request_never_forwarded_upstream() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect_pigeon(&h).await;

    // 1. Create session
    let new_res = v
        .call(
            "session/new",
            serde_json::json!({
                "cwd": "/tmp",
                "mcpServers": [],
                "_meta": { "enabledExtensions": [] }
            }),
        )
        .await;
    let sid = new_res["result"]["sessionId"].as_str().unwrap();

    let inbound_before = fake.inbound().len();

    // 2. session/close -> answered locally with {}, not forwarded
    let close_res = v
        .call("session/close", serde_json::json!({ "sessionId": sid }))
        .await;
    assert_eq!(close_res["result"], serde_json::json!({}));

    // 3. $/cancel_request -> dropped, not forwarded
    v.request(
        "$/cancel_request",
        serde_json::json!({ "sessionId": sid, "requestId": 123 }),
    )
    .await;

    // Small tick to ensure no errant forward in flight
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let inbound_after = fake.inbound();
    let new_inbound = &inbound_after[inbound_before..];
    for frame in new_inbound {
        let m = frame.get("method").and_then(serde_json::Value::as_str);
        assert_ne!(
            m,
            Some("session/close"),
            "session/close must never be forwarded upstream"
        );
        assert_ne!(
            m,
            Some("$/cancel_request"),
            "$/cancel_request must never be forwarded upstream"
        );
    }

    // 4. Verify socket is still open: initialize works
    let init_res = v
        .call(
            "initialize",
            serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {} }),
        )
        .await;
    assert_eq!(init_res["result"]["_meta"]["honk"]["protocol"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_frame_returns_32600() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect_pigeon(&h).await;

    // Missing method, not a response -> invalid
    let res = v.call("", serde_json::json!({})).await;
    assert_eq!(res["error"]["code"], -32600);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_new_failure_deletes_upstream_connection() {
    let fake = FakeGoose::start().await;
    fake.set_session_new_status(Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect_pigeon(&h).await;

    let res = v
        .call(
            "session/new",
            serde_json::json!({
                "cwd": "/tmp",
                "mcpServers": [],
            }),
        )
        .await;

    assert!(
        res.get("error").is_some(),
        "expected session/new to fail, got: {res}"
    );

    // After a failed session/new, FakeGoose must have seen a DELETE for that connection
    assert!(
        !fake.deletes().is_empty(),
        "expected FakeGoose to record DELETE for the failed connection, got deletes: {:?}",
        fake.deletes()
    );

    // State file must have no orphan connections and no held sessions
    let state_bytes = std::fs::read(h.state_dir.path().join("state.json")).expect("read state");
    let state: honkd::state::State = serde_json::from_slice(&state_bytes).expect("parse state");
    assert!(
        state.orphan_conns.is_empty(),
        "orphans must be cleared after failed session/new, got: {:?}",
        state.orphan_conns
    );
    assert!(
        state.sessions.is_empty(),
        "sessions must be empty after failed session/new"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn session_new_timeout_deletes_upstream_connection() {
    let fake = FakeGoose::start().await;
    fake.set_session_new_delay(Some(Duration::from_millis(1500)));
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--allow-unpatched-upstream",
            "--session-new-timeout-ms",
            "100",
        ],
    )
    .await;
    let mut v = Viewer::connect_pigeon(&h).await;

    let res = v
        .call(
            "session/new",
            serde_json::json!({
                "cwd": "/tmp",
                "mcpServers": [],
            }),
        )
        .await;

    assert!(
        res.get("error").is_some(),
        "expected session/new to time out, got: {res}"
    );

    // After a timed-out session/new, FakeGoose must have seen a DELETE for that connection
    assert!(
        !fake.deletes().is_empty(),
        "expected FakeGoose to record DELETE for the timed out connection, got deletes: {:?}",
        fake.deletes()
    );

    // State file must have no orphan connections and no held sessions
    let state_bytes = std::fs::read(h.state_dir.path().join("state.json")).expect("read state");
    let state: honkd::state::State = serde_json::from_slice(&state_bytes).expect("parse state");
    assert!(
        state.orphan_conns.is_empty(),
        "orphans must be cleared after timed out session/new, got: {:?}",
        state.orphan_conns
    );
    assert!(
        state.sessions.is_empty(),
        "sessions must be empty after timed out session/new"
    );
}
