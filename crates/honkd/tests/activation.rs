use std::sync::Arc;
use std::time::Duration;

use common::{Honkd, Viewer, new_session, prompt_params};
use honk_testgoose::{StubGoose, Turn};
use honk_upstream::{Recorder, Upstream, UpstreamOpts};
use serde_json::json;

mod common;

fn read_state(h: &Honkd) -> honkd::state::State {
    let state_bytes = std::fs::read(h.state_dir.path().join("state.json")).expect("read state");
    serde_json::from_slice(&state_bytes).expect("parse state")
}

#[tokio::test(flavor = "multi_thread")]
async fn activation_passes_stored_cwd_not_viewers() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let stored_cwd = g.cwd().to_str().unwrap().to_string();

    // Spawn honkd with --idle-ttl-secs 1
    let h = Honkd::spawn(&g.url(), g.secret_file(), &["--idle-ttl-secs", "1"]).await;
    let mut v = Viewer::connect(&h).await;

    let sid = new_session(&mut v, &stored_cwd).await;

    // Prompt initial turn so goose commits session to DB
    g.model().push([Turn::Text(vec!["initial ok".into()])]);
    let req0 = v
        .request("session/prompt", prompt_params(&sid, "init", None))
        .await;
    let _ = v.response(&req0).await;

    // Detach viewer so session has no viewers, no run, no inflight, no pending
    v.call("session/close", json!({ "sessionId": &sid })).await;

    // Wait for idle TTL eviction (> 1 sec)
    tokio::time::sleep(Duration::from_millis(2200)).await;

    // Verify session is evicted from state
    let state = read_state(&h);
    assert!(
        !state.sessions.contains_key(&sid),
        "session must be evicted after idle TTL"
    );
    assert_eq!(
        state.retired_epochs.get(&sid),
        Some(&1),
        "retired_epochs must record evicted session epoch 1"
    );

    // Queue a model turn response for prompt
    g.model().push([Turn::Text(vec!["cwd response ok".into()])]);

    // Viewer connects and triggers activation with a bogus cwd
    let mut v2 = Viewer::connect(&h).await;
    let bogus_cwd = "/tmp/bogus_viewer_cwd_must_not_be_used";
    let req_id = v2
        .request(
            "session/prompt",
            json!({
                "sessionId": sid,
                "cwd": bogus_cwd,
                "prompt": [{
                    "type": "text",
                    "text": "test stored cwd",
                }]
            }),
        )
        .await;

    let resp = v2.response(&req_id).await;
    assert_eq!(
        resp.pointer("/result/stopReason").and_then(|v| v.as_str()),
        Some("end_turn"),
        "prompt must succeed on re-activated session"
    );

    // 1. Check session/list on goose directly: goose must have stored_cwd, NOT bogus_cwd
    let mut direct_up =
        Upstream::initialize(&g.url(), g.secret().to_string(), Recorder::disabled())
            .await
            .expect("init upstream");
    direct_up.subscribe(None).await.expect("sub conn");
    let list_res = direct_up
        .call("session/list", json!({}), None, Duration::from_secs(10))
        .await
        .expect("call session/list");
    let sessions = list_res["sessions"].as_array().expect("sessions array");
    let entry = sessions
        .iter()
        .find(|s| s["sessionId"] == sid)
        .expect("session found in session/list");
    let actual_cwd = entry["cwd"].as_str().expect("cwd string");
    assert_eq!(
        actual_cwd, stored_cwd,
        "goose session/list must reflect stored cwd, not viewer cwd"
    );
    assert_ne!(
        actual_cwd, bogus_cwd,
        "goose session/list must NEVER be the viewer bogus cwd"
    );

    // 2. Check stub model turn requests: the system message / messages must contain stored_cwd and not bogus_cwd
    let turn_reqs = g.model().turn_requests();
    let last_req = turn_reqs.last().expect("must have recorded a turn request");
    let req_str = last_req.to_string();
    assert!(
        req_str.contains(&stored_cwd),
        "model turn request must contain stored cwd; was:\n{req_str}"
    );
    assert!(
        !req_str.contains(bogus_cwd),
        "model turn request must NOT contain bogus cwd; was:\n{req_str}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn after_reactivation_honk_aware_viewers_get_reset_with_epoch_increment_and_export_transcript()
 {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let stored_cwd = g.cwd().to_str().unwrap().to_string();

    let h = Honkd::spawn(&g.url(), g.secret_file(), &["--idle-ttl-secs", "1"]).await;
    let mut v = Viewer::connect(&h).await;

    let sid = new_session(&mut v, &stored_cwd).await;

    // Prompt first turn
    g.model()
        .push([Turn::Text(vec!["hello first turn".into()])]);
    let req_id = v
        .request(
            "session/prompt",
            prompt_params(&sid, "first user message", None),
        )
        .await;
    let _ = v.response(&req_id).await;

    // Detach viewer and wait for eviction
    v.call("session/close", json!({ "sessionId": &sid })).await;
    tokio::time::sleep(Duration::from_millis(2200)).await;

    // Reconnect as a new honk-aware viewer and call session/load
    let mut v2 = Viewer::connect(&h).await;
    let load_id = v2
        .request(
            "session/load",
            json!({
                "sessionId": sid,
                "cwd": ".",
                "mcpServers": []
            }),
        )
        .await;

    // Viewer must receive _honk/reset with epoch 2
    let reset_frame = v2
        .wait_for(|f| {
            f.get("method").and_then(|v| v.as_str()) == Some("_honk/reset")
                && f.pointer("/params/sessionId").and_then(|v| v.as_str()) == Some(&sid)
        })
        .await;
    assert_eq!(
        reset_frame
            .pointer("/params/epoch")
            .and_then(|v| v.as_u64()),
        Some(2),
        "_honk/reset must carry epoch 2"
    );

    // Wait for the session/load response
    let load_resp = v2.response(&load_id).await;
    assert_eq!(
        load_resp
            .pointer("/result/_meta/honk/epoch")
            .and_then(|v| v.as_u64()),
        Some(2),
        "session/load result must carry epoch 2 in _meta.honk"
    );

    // Wait for _honk/replay_done with epoch 2
    let replay_done = v2
        .wait_for(|f| {
            f.get("method").and_then(|v| v.as_str()) == Some("_honk/replay_done")
                && f.pointer("/params/sessionId").and_then(|v| v.as_str()) == Some(&sid)
        })
        .await;
    assert_eq!(
        replay_done
            .pointer("/params/epoch")
            .and_then(|v| v.as_u64()),
        Some(2)
    );

    // Export transcript was received
    assert_eq!(
        v2.blocks.user_texts(),
        vec!["first user message".to_string()],
        "re-activated session must replay export transcript"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn busy_session_with_gated_turn_is_not_evicted() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let stored_cwd = g.cwd().to_str().unwrap().to_string();

    let h = Honkd::spawn(&g.url(), g.secret_file(), &["--idle-ttl-secs", "1"]).await;
    let mut v = Viewer::connect(&h).await;

    let sid = new_session(&mut v, &stored_cwd).await;

    // Queue a gated turn
    g.model().push([Turn::Gate(
        "gate_busy".into(),
        Box::new(Turn::Text(vec!["unparked answer".into()])),
    )]);

    let _prompt_id = v
        .request("session/prompt", prompt_params(&sid, "parked turn", None))
        .await;

    // Wait for run to become active
    v.wait_for(|f| {
        f.get("method").and_then(|v| v.as_str()) == Some("session/update")
            && f.pointer("/params/update/sessionUpdate")
                .and_then(|v| v.as_str())
                == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(|v| v.as_str())
                .is_some()
    })
    .await;

    // Detach viewer while turn is still running
    v.call("session/close", json!({ "sessionId": &sid })).await;

    // Wait 2.2 seconds (longer than --idle-ttl-secs 1)
    tokio::time::sleep(Duration::from_millis(2200)).await;

    // Verify session was NOT evicted because a run is active
    let state = read_state(&h);
    assert!(
        state.sessions.contains_key(&sid),
        "busy session with active run must NOT be evicted"
    );
    assert!(
        !state.retired_epochs.contains_key(&sid),
        "busy session must not be retired"
    );

    // Now unblock the gate
    g.model().open("gate_busy");

    // Connect a new viewer to attach and observe turn end
    let mut v2 = Viewer::connect(&h).await;
    v2.call(
        "session/load",
        json!({
            "sessionId": &sid,
            "cwd": ".",
            "mcpServers": []
        }),
    )
    .await;

    // Wait for activeRunId: null indicating turn finished
    v2.wait_for(|f| {
        f.get("method").and_then(|v| v.as_str()) == Some("session/update")
            && f.pointer("/params/update/sessionUpdate")
                .and_then(|v| v.as_str())
                == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId") == Some(&serde_json::Value::Null)
    })
    .await;

    // Detach viewer now that turn has ended
    v2.call("session/close", json!({ "sessionId": &sid })).await;

    // Wait 2.2s: now that it is idle, it should be evicted!
    tokio::time::sleep(Duration::from_millis(2200)).await;

    let state_after = read_state(&h);
    assert!(
        !state_after.sessions.contains_key(&sid),
        "session must be evicted once idle"
    );
    assert_eq!(state_after.retired_epochs.get(&sid), Some(&1));
}

#[tokio::test(flavor = "multi_thread")]
async fn eviction_deletes_exactly_the_idle_sessions_connection() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let stored_cwd = g.cwd().to_str().unwrap().to_string();

    let h = Honkd::spawn(&g.url(), g.secret_file(), &["--idle-ttl-secs", "1"]).await;
    let mut v1 = Viewer::connect(&h).await;
    let mut v2 = Viewer::connect(&h).await;

    let s1 = new_session(&mut v1, &stored_cwd).await;
    let s2 = new_session(&mut v2, &stored_cwd).await;

    let state_initial = read_state(&h);
    let c1 = state_initial.sessions[&s1].conn_id.clone();
    let c2 = state_initial.sessions[&s2].conn_id.clone();
    assert_ne!(c1, c2);

    // Detach v1 from s1 only
    v1.call("session/close", json!({ "sessionId": &s1 })).await;

    // Keep v2 attached to s2 (v2 does not close)
    tokio::time::sleep(Duration::from_millis(2200)).await;

    let state_after = read_state(&h);
    assert!(
        !state_after.sessions.contains_key(&s1),
        "s1 must be evicted"
    );
    assert!(
        state_after.sessions.contains_key(&s2),
        "s2 must remain held"
    );

    // Verify c1 was DELETEd upstream by attempting to resume with c1: it should 404 (gone)
    let opts = UpstreamOpts::default();
    let mut direct_c1 = Upstream::resume_with(
        &g.url(),
        g.secret().to_string(),
        c1,
        Recorder::disabled(),
        &opts,
    )
    .expect("resume c1");
    let del_err = direct_c1
        .subscribe_with(
            None,
            honk_upstream::SubscribeOpts {
                takeover: false,
                resume_after: None,
            },
        )
        .await;
    assert!(
        del_err.is_err(),
        "c1 must be deleted upstream and fail to subscribe"
    );
    let err = del_err.unwrap_err();
    assert!(
        honk_upstream::is_gone(&err),
        "deleted upstream connection c1 must return 404 (gone): {err:#}"
    );

    // Verify c2 is still alive and can execute a turn
    g.model().push([Turn::Text(vec!["s2 response ok".into()])]);
    let req_id = v2
        .request("session/prompt", prompt_params(&s2, "hello s2", None))
        .await;
    let resp = v2.response(&req_id).await;
    assert_eq!(
        resp.pointer("/result/stopReason").and_then(|v| v.as_str()),
        Some("end_turn")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn coalesced_concurrent_activation() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let stored_cwd = g.cwd().to_str().unwrap().to_string();

    let h = Arc::new(Honkd::spawn(&g.url(), g.secret_file(), &["--idle-ttl-secs", "1"]).await);
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, &stored_cwd).await;

    // Prompt initial turn so goose commits session to DB
    g.model().push([Turn::Text(vec!["initial ok".into()])]);
    let req0 = v
        .request("session/prompt", prompt_params(&sid, "init", None))
        .await;
    let _ = v.response(&req0).await;

    // Detach and wait for eviction
    v.call("session/close", json!({ "sessionId": &sid })).await;
    tokio::time::sleep(Duration::from_millis(2200)).await;

    let state = read_state(&h);
    assert!(!state.sessions.contains_key(&sid));
    assert_eq!(state.retired_epochs.get(&sid), Some(&1));

    // Spawn 4 concurrent tasks, all calling session/load on sid simultaneously
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let h_clone = h.clone();
        let sid_clone = sid.clone();
        tasks.push(tokio::spawn(async move {
            let mut viewer = Viewer::connect(&h_clone).await;
            let load_id = viewer
                .request(
                    "session/load",
                    json!({
                        "sessionId": sid_clone,
                        "cwd": ".",
                        "mcpServers": []
                    }),
                )
                .await;
            let resp = viewer.response(&load_id).await;
            (resp, viewer)
        }));
    }

    for task in tasks {
        let (resp, _viewer) = task.await.expect("join task");
        assert_eq!(
            resp.pointer("/result/_meta/honk/epoch")
                .and_then(|v| v.as_u64()),
            Some(2),
            "all coalesced activations must see epoch 2"
        );
    }

    // Exactly one activation should have occurred: epoch must be 2, not 5
    let state_final = read_state(&h);
    assert_eq!(
        state_final.sessions[&sid].epoch, 2,
        "coalesced activations must result in exactly epoch 2"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn eviction_vs_prompt_race() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let stored_cwd = g.cwd().to_str().unwrap().to_string();

    let h = Honkd::spawn(&g.url(), g.secret_file(), &["--idle-ttl-secs", "1"]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, &stored_cwd).await;

    // Prompt initial turn so goose commits session to DB
    g.model().push([Turn::Text(vec!["initial ok".into()])]);
    let req0 = v
        .request("session/prompt", prompt_params(&sid, "init", None))
        .await;
    let _ = v.response(&req0).await;

    v.call("session/close", json!({ "sessionId": &sid })).await;

    // Sleep right around the 1 second eviction boundary (1000ms)
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Prompt right as eviction fires/races
    g.model()
        .push([Turn::Text(vec!["race turn answered".into()])]);
    let req_id = v
        .request("session/prompt", prompt_params(&sid, "racing prompt", None))
        .await;

    let resp = v.response(&req_id).await;
    assert_eq!(
        resp.pointer("/result/stopReason").and_then(|v| v.as_str()),
        Some("end_turn"),
        "racing prompt must not lose: must be answered successfully"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn max_sessions_refuses_activation_with_32000() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let stored_cwd = g.cwd().to_str().unwrap().to_string();

    // Spawn honkd with --max-sessions 1 and long idle TTL so s1 stays held
    let h = Honkd::spawn(
        &g.url(),
        g.secret_file(),
        &["--max-sessions", "1", "--idle-ttl-secs", "600"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;

    let s1 = new_session(&mut v, &stored_cwd).await;

    // Create session s2 upstream directly
    let mut direct_up =
        Upstream::initialize(&g.url(), g.secret().to_string(), Recorder::disabled())
            .await
            .expect("init direct upstream");
    direct_up.subscribe(None).await.expect("sub direct");
    let s2_res = direct_up
        .call(
            "session/new",
            json!({
                "cwd": &stored_cwd,
                "mcpServers": []
            }),
            None,
            Duration::from_secs(10),
        )
        .await
        .expect("session/new s2");
    let s2 = s2_res["sessionId"].as_str().expect("s2 id").to_string();
    let _ = direct_up.delete().await;

    // Now honkd has s1 held (1/1 held sessions).
    // Attempting to activate s2 via session/load must be refused with -32000 "honkd: held-session limit reached"
    let req_id = v
        .request(
            "session/load",
            json!({
                "sessionId": s2,
                "cwd": ".",
                "mcpServers": []
            }),
        )
        .await;

    let resp = v.response(&req_id).await;
    assert_eq!(
        resp.pointer("/error/code").and_then(|v| v.as_i64()),
        Some(-32000),
        "must return code -32000 on max sessions reached"
    );
    assert_eq!(
        resp.pointer("/error/message").and_then(|v| v.as_str()),
        Some("honkd: held-session limit reached"),
        "must return exact message 'honkd: held-session limit reached'"
    );

    // Verify s1 is still held and unaffected
    let state = read_state(&h);
    assert!(state.sessions.contains_key(&s1));
}
