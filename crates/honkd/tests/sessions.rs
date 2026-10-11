use common::{Honkd, Viewer};
use honk_testgoose::{FakeGoose, StubGoose};

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
