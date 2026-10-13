use common::{Honkd, Viewer, new_session, prompt_params};
use honk_testgoose::{FakeGoose, StubGoose, Turn};
use serde_json::{Value, json};

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
    fake.insert_session("ses_unheld", "/tmp");
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

    let unheld = sessions
        .iter()
        .find(|s| s["sessionId"] == "ses_unheld")
        .expect("unheld session found in session/list");
    assert_eq!(unheld["_meta"]["honk"]["held"], false);
    assert_eq!(unheld["_meta"]["honk"]["running"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_list_reports_whether_session_is_running() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    // Prompt initial turn so goose commits session with messages
    g.model().push([Turn::Text(vec!["init ok".into()])]);
    let req0 = v
        .request("session/prompt", prompt_params(&sid, "init", None))
        .await;
    let _ = v.response(&req0).await;

    // Connect fresh viewer so its incoming frame buffer is empty
    let mut v2 = Viewer::connect_pigeon(&h).await;

    // 1. Before prompt 2: held is true, running is false
    let list_res = v2.call("session/list", json!({})).await;
    let sessions = list_res["result"]["sessions"].as_array().expect("sessions");
    let entry = sessions
        .iter()
        .find(|s| s["sessionId"] == sid)
        .expect("session found");
    assert_eq!(entry["_meta"]["honk"]["held"], true);
    assert_eq!(entry["_meta"]["honk"]["running"], false);

    // 2. Start prompt 2 (which triggers an active run parked at gate_running_test)
    g.model().push([Turn::Gate(
        "gate_running_test".into(),
        Box::new(Turn::Text(vec!["done".into()])),
    )]);
    let p_req = v2
        .request(
            "session/prompt",
            prompt_params(&sid, "test run", Some("crid_1")),
        )
        .await;

    // Wait until run is active
    v2.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // 3. While run is active: session/list must report running: true
    let list_res_running = v2.call("session/list", json!({})).await;
    let sessions_running = list_res_running["result"]["sessions"]
        .as_array()
        .expect("sessions");
    let entry_running = sessions_running
        .iter()
        .find(|s| s["sessionId"] == sid)
        .expect("session found");
    assert_eq!(entry_running["_meta"]["honk"]["held"], true);
    assert_eq!(entry_running["_meta"]["honk"]["running"], true);

    // 4. Release gate and wait for prompt completion
    g.model().open("gate_running_test");
    let _ = v2.response(&p_req).await;

    // Wait until run ends (activeRunId is null)
    v2.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .is_some_and(|v| v.is_null())
    })
    .await;

    // 5. After run finishes: session/list must report running: false
    let list_res_after = v2.call("session/list", json!({})).await;
    let sessions_after = list_res_after["result"]["sessions"]
        .as_array()
        .expect("sessions");
    let entry_after = sessions_after
        .iter()
        .find(|s| s["sessionId"] == sid)
        .expect("session found");
    assert_eq!(entry_after["_meta"]["honk"]["held"], true);
    assert_eq!(entry_after["_meta"]["honk"]["running"], false);
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
