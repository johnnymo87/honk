use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Honkd, Viewer, new_session, prompt_params};
use honk_testgoose::{StubGoose, Turn};
use honk_upstream::{Recorder, Upstream, UpstreamOpts};
use serde_json::{Value, json};

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
    let fake = honk_testgoose::FakeGoose::start().await;
    fake.advertise_patched(true);
    let h =
        Arc::new(Honkd::spawn(&fake.url(), fake.secret_file(), &["--idle-ttl-secs", "1"]).await);

    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    // Detach and wait for eviction
    v.call("session/close", json!({ "sessionId": &sid })).await;
    tokio::time::sleep(Duration::from_millis(2200)).await;

    let state = read_state(&h);
    assert!(!state.sessions.contains_key(&sid));
    assert_eq!(state.retired_epochs.get(&sid), Some(&1));

    let initial_loads = fake
        .inbound()
        .iter()
        .filter(|f| f.get("method").and_then(serde_json::Value::as_str) == Some("session/load"))
        .count();

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

    // Assert that upstream received EXACTLY ONE session/load call across all concurrent activations
    let final_loads = fake
        .inbound()
        .iter()
        .filter(|f| f.get("method").and_then(serde_json::Value::as_str) == Some("session/load"))
        .count();
    assert_eq!(
        final_loads - initial_loads,
        1,
        "coalescing must result in exactly one upstream session/load call"
    );

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

    // Spawn honkd with HONKD_TEST_PAUSE_EVICT_MS=1500 to pause eviction between handle removal and DELETE
    let h = Honkd::spawn_with_env(
        &g.url(),
        g.secret_file(),
        &["--idle-ttl-secs", "1"],
        &[("HONKD_TEST_PAUSE_EVICT_MS", "1500")],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, &stored_cwd).await;

    // Prompt initial turn so goose commits session to DB
    g.model().push([Turn::Text(vec!["initial ok".into()])]);
    let req0 = v
        .request("session/prompt", prompt_params(&sid, "init", None))
        .await;
    let _ = v.response(&req0).await;

    v.call("session/close", json!({ "sessionId": &sid })).await;

    // Wait 1.1s: idle TTL (1.0s) has expired and eviction has started,
    // paused inside the 1500ms HONKD_TEST_PAUSE_EVICT_MS barrier between handle removal and DELETE.
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // Prompt while eviction is paused mid-flight!
    g.model()
        .push([Turn::Text(vec!["race turn answered".into()])]);
    let req_id = v
        .request("session/prompt", prompt_params(&sid, "racing prompt", None))
        .await;

    let resp = v.response_timeout(&req_id, Duration::from_secs(15)).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn max_sessions_concurrent_activations_limited() {
    let fake = honk_testgoose::FakeGoose::start().await;
    fake.advertise_patched(true);
    let h = Arc::new(
        Honkd::spawn(
            &fake.url(),
            fake.secret_file(),
            &["--max-sessions", "2", "--idle-ttl-secs", "600"],
        )
        .await,
    );

    // Create 3 sessions upstream directly
    let mut direct_up =
        Upstream::initialize(&fake.url(), fake.secret().to_string(), Recorder::disabled())
            .await
            .expect("init direct");
    direct_up.subscribe(None).await.expect("sub direct");

    let mut sids = Vec::new();
    for _ in 1..=3 {
        let res = direct_up
            .call(
                "session/new",
                json!({"cwd": "/tmp", "mcpServers": []}),
                None,
                Duration::from_secs(10),
            )
            .await
            .expect("session/new");
        sids.push(res["sessionId"].as_str().expect("sessionId").to_string());
    }
    let _ = direct_up.delete().await;

    // Attempt to activate all 3 concurrently with max_sessions = 2
    let mut tasks = Vec::new();
    for sid in sids {
        let h_clone = h.clone();
        tasks.push(tokio::spawn(async move {
            let mut viewer = Viewer::connect(&h_clone).await;
            let id = viewer
                .request(
                    "session/load",
                    json!({
                        "sessionId": sid,
                        "cwd": ".",
                        "mcpServers": []
                    }),
                )
                .await;
            viewer.response(&id).await
        }));
    }

    let mut successes = 0;
    let mut limit_errors = 0;
    for task in tasks {
        let resp = task.await.expect("join task");
        if resp.get("result").is_some() {
            successes += 1;
        } else if resp.pointer("/error/code") == Some(&json!(-32000)) {
            limit_errors += 1;
        }
    }

    assert_eq!(
        successes, 2,
        "exactly 2 activations must succeed under max-sessions=2"
    );
    assert_eq!(
        limit_errors, 1,
        "the 3rd activation must be refused with -32000"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn answer_routes_to_pending_request_across_sessions() {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return;
    };
    goose.model().push([
        Turn::ToolCall {
            tool: "shell".to_string(),
            arguments: json!({"command": "echo answer_test"}),
        },
        Turn::Text(vec!["done".to_string()]),
    ]);

    let h = Honkd::spawn(
        &goose.url(),
        goose.secret_file(),
        &["--idle-ttl-secs", "600"],
    )
    .await;

    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, goose.cwd().to_str().unwrap()).await;

    let mode_res = v1
        .call(
            "session/set_mode",
            json!({"sessionId": &sid, "modeId": "approve"}),
        )
        .await;
    assert!(mode_res.get("error").is_none());

    let _pid = v1
        .request(
            "session/prompt",
            prompt_params(&sid, "run echo in approve mode", None),
        )
        .await;

    let req = v1
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;
    let req_id = req["id"].clone();

    // Detach v1 (close session)
    v1.call("session/close", json!({ "sessionId": &sid })).await;

    // A fresh viewer v2 (with no attached_sessions) answers the request!
    let mut v2 = Viewer::connect(&h).await;
    v2.respond(
        req_id.clone(),
        json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
    )
    .await;

    // Verify pending request is answered and removed from state
    let start = std::time::Instant::now();
    let mut resolved = false;
    while start.elapsed() < Duration::from_secs(5) {
        let state = read_state(&h);
        if let Some(held) = state.sessions.get(&sid)
            && held.pending.is_empty()
        {
            resolved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        resolved,
        "pending request must be answered and removed from state by unattached viewer answer"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_queued_msgs_during_paused_eviction_dispatched_in_order() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let stored_cwd = g.cwd().to_str().unwrap().to_string();

    let h = Honkd::spawn_with_env(
        &g.url(),
        g.secret_file(),
        &["--idle-ttl-secs", "1"],
        &[("HONKD_TEST_PAUSE_EVICT_MS", "2500")],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, &stored_cwd).await;

    // Prompt initial turn so goose commits session to DB
    g.model().push([Turn::Text(vec!["initial ok".into()])]);
    let req0 = v
        .request("session/prompt", prompt_params(&sid, "init", None))
        .await;
    let _ = v.response(&req0).await;

    v.call("session/close", json!({ "sessionId": &sid })).await;

    // Wait 1.8s: eviction starts (~1.2s) and is paused inside the 2500ms HONKD_TEST_PAUSE_EVICT_MS barrier
    tokio::time::sleep(Duration::from_millis(1800)).await;

    g.model().push([Turn::Text(vec!["prompt response".into()])]);

    // Send session/load first, then session/prompt second while eviction is paused
    let mut v2 = Viewer::connect(&h).await;
    let load_id = v2
        .request(
            "session/load",
            json!({
                "sessionId": &sid,
                "cwd": ".",
                "mcpServers": []
            }),
        )
        .await;
    let prompt_id = v2
        .request(
            "session/prompt",
            prompt_params(&sid, "second prompt after load", None),
        )
        .await;

    // Load response must arrive for load_id with epoch 2
    let load_resp = v2.response_timeout(&load_id, Duration::from_secs(15)).await;
    assert_eq!(
        load_resp
            .pointer("/result/_meta/honk/epoch")
            .and_then(Value::as_u64),
        Some(2),
        "load must be answered with epoch 2"
    );

    // Prompt response must arrive for prompt_id with end_turn
    let prompt_resp = v2
        .response_timeout(&prompt_id, Duration::from_secs(15))
        .await;
    assert_eq!(
        prompt_resp
            .pointer("/result/stopReason")
            .and_then(Value::as_str),
        Some("end_turn"),
        "prompt must complete with end_turn"
    );
    assert_eq!(
        prompt_resp
            .pointer("/result/_meta/honk/text")
            .and_then(Value::as_str),
        Some("prompt response"),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn evict_reactivate_kill9_restart_uses_real_reply_not_stale_sidecar() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let stored_cwd = g.cwd().to_str().unwrap().to_string();

    g.model().push([Turn::Text(vec!["reply 1".into()])]);

    let mut h = Honkd::spawn(&g.url(), g.secret_file(), &["--idle-ttl-secs", "1"]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, &stored_cwd).await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Turn 1 completes
    let r1 = v
        .call(
            "session/prompt",
            prompt_params(&sid, "prompt 1", Some("r1")),
        )
        .await;
    assert_eq!(r1["result"]["stopReason"], "end_turn");
    assert_eq!(r1["result"]["_meta"]["honk"]["text"], "reply 1");

    // Detach and wait for eviction (> 1 sec)
    v.call("session/close", json!({ "sessionId": &sid })).await;
    drop(v);

    let wait_evict = Instant::now();
    loop {
        let state = read_state(&h);
        if !state.sessions.contains_key(&sid) {
            break;
        }
        if wait_evict.elapsed() > Duration::from_secs(5) {
            panic!("session failed to evict");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Queue reply 2 for the model
    g.model().push([Turn::Text(vec!["reply 2".into()])]);

    // Reconnect and reactivate
    let mut v2 = Viewer::connect(&h).await;
    v2.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Send prompt 2
    v2.request(
        "session/prompt",
        prompt_params(&sid, "prompt 2", Some("r2")),
    )
    .await;

    // Wait until model receives prompt 2
    let wait_start = Instant::now();
    while g.model().turn_requests().len() < 2 && wait_start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Kill honkd mid-turn while Turn 2 is running
    h.kill9();

    // Give stub goose enough time to complete Turn 2 in SQLite
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Restart honkd
    h.restart().await;

    // Connect new viewer and retry prompt 2
    let mut w = Viewer::connect(&h).await;
    let resp = w
        .call(
            "session/prompt",
            prompt_params(&sid, "prompt 2", Some("r2")),
        )
        .await;

    assert_eq!(resp["result"]["stopReason"], "end_turn");
    assert_eq!(
        resp["result"]["_meta"]["honk"]["text"], "reply 2",
        "must use real reply 2, not stale sidecar reply 1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn activation_finds_stored_cwd_via_session_info() {
    let fake = honk_testgoose::FakeGoose::start().await;
    fake.advertise_patched(true);

    // Old session stored upstream
    fake.insert_session("ses_old", "/tmp/cwd_old");

    let h = Arc::new(Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await);
    let mut v = Viewer::connect(&h).await;

    // Loading ses_old must activate successfully
    let load_res = v
        .call(
            "session/load",
            json!({
                "sessionId": "ses_old",
                "cwd": "/ignored",
                "mcpServers": []
            }),
        )
        .await;
    assert!(
        load_res.get("error").is_none(),
        "expected ses_old to activate successfully, got: {load_res}"
    );

    // Verify session/info was called in one call for ses_old
    let info_calls = fake
        .inbound()
        .iter()
        .filter(|f| {
            f.get("method").and_then(Value::as_str) == Some("_goose/unstable/session/info")
                && f.pointer("/params/sessionId").and_then(Value::as_str) == Some("ses_old")
        })
        .count();
    assert_eq!(
        info_calls, 1,
        "session/info must be called in exactly one call"
    );

    // Side connection must be deleted and orphan_conns must be empty
    assert!(
        !fake.deletes().is_empty(),
        "side connection id must appear in FakeGoose deletes"
    );
    let state_after_old = read_state(&h);
    assert!(
        state_after_old.orphan_conns.is_empty(),
        "orphan_conns must be empty after successful activation"
    );

    // Loading a truly missing session must fail with -32602 "Session not found"
    let deletes_before_missing = fake.deletes().len();
    let missing_res = v
        .call(
            "session/load",
            json!({
                "sessionId": "ses_missing",
                "cwd": "/ignored",
                "mcpServers": []
            }),
        )
        .await;
    let err = missing_res
        .get("error")
        .expect("expected error for missing session");
    assert_eq!(
        err.get("code").and_then(Value::as_i64),
        Some(-32602),
        "expected -32602 for missing session, got: {missing_res}"
    );
    assert!(
        err.get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("Session not found: ses_missing"),
        "expected 'Session not found: ses_missing', got: {missing_res}"
    );
    assert_eq!(
        fake.deletes().len(),
        deletes_before_missing + 1,
        "missing session side connection must be deleted"
    );
    let state_after_missing = read_state(&h);
    assert!(
        state_after_missing.orphan_conns.is_empty(),
        "orphan_conns must be empty after missing-session activation"
    );

    // Transient error from goose session/info (-32603) -> honkd answers -32603
    fake.error_next_session_info(-32603, "transient error from goose");
    let deletes_before_transient = fake.deletes().len();
    let transient_res = v
        .call(
            "session/load",
            json!({
                "sessionId": "ses_transient",
                "cwd": "/ignored",
                "mcpServers": []
            }),
        )
        .await;
    let transient_err = transient_res
        .get("error")
        .expect("expected error for transient session/info failure");
    assert_eq!(
        transient_err.get("code").and_then(Value::as_i64),
        Some(-32603),
        "expected -32603 for transient session/info failure, got: {transient_res}"
    );
    assert_eq!(
        fake.deletes().len(),
        deletes_before_transient + 1,
        "transient error side connection must be deleted"
    );
    let state_after_transient = read_state(&h);
    assert!(
        state_after_transient.orphan_conns.is_empty(),
        "orphan_conns must be empty after transient session/info failure"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn activation_carries_post_load_usage_update() {
    let fake = honk_testgoose::FakeGoose::start().await;
    fake.advertise_patched(true);

    let sid = "ses_usage_act";
    fake.insert_session(sid, "/tmp");
    fake.set_export_data(
        sid,
        json!({
            "messages": []
        }),
    );

    let usage_frame = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": sid,
            "update": {
                "sessionUpdate": "usage_update",
                "used": 42000,
                "size": 128000,
                "cost": {
                    "amount": 0.05,
                    "currency": "USD"
                }
            }
        }
    });
    fake.set_load_frames_before(sid, vec![usage_frame]);

    let h = Arc::new(Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await);
    let mut v = Viewer::connect(&h).await;

    let load_id = v
        .request(
            "session/load",
            json!({
                "sessionId": sid,
                "cwd": "/ignored",
                "mcpServers": []
            }),
        )
        .await;

    let mut frames_before_done = Vec::new();
    loop {
        let f = tokio::time::timeout(Duration::from_secs(5), v.recv_frame())
            .await
            .expect("timeout waiting for frame")
            .expect("stream open");
        if f.get("method").and_then(Value::as_str) == Some("_honk/replay_done") {
            break;
        }
        frames_before_done.push(f);
    }

    let load_resp = frames_before_done
        .iter()
        .find(|f| honk_upstream::rpc_id_matches(&load_id, f.get("id").unwrap_or(&Value::Null)))
        .expect("must receive session/load response");
    assert!(load_resp.get("error").is_none());

    let usage_f = frames_before_done
        .iter()
        .find(|f| {
            f.get("method").and_then(Value::as_str) == Some("session/update")
                && f.pointer("/params/update/sessionUpdate")
                    .and_then(Value::as_str)
                    == Some("usage_update")
        })
        .expect("must receive usage_update in replay before _honk/replay_done");

    let usage = honk_wire::usage(usage_f.pointer("/params/update").unwrap()).unwrap();
    assert_eq!(usage.used, 42000);
    assert_eq!(usage.size, 128000);
    let cost = usage.cost.expect("cost present");
    assert_eq!(cost.amount, 0.05);
    assert_eq!(cost.currency, "USD");
}

#[tokio::test(flavor = "multi_thread")]
async fn activation_carries_post_load_usage_update_after_reply() {
    let fake = honk_testgoose::FakeGoose::start().await;
    fake.advertise_patched(true);

    let sid = "ses_usage_act_after";
    fake.insert_session(sid, "/tmp");
    fake.set_export_data(
        sid,
        json!({
            "messages": []
        }),
    );

    let usage_frame = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": sid,
            "update": {
                "sessionUpdate": "usage_update",
                "used": 45000,
                "size": 130000,
                "cost": {
                    "amount": 0.06,
                    "currency": "USD"
                }
            }
        }
    });
    fake.set_load_frames(sid, vec![usage_frame]);

    let h = Arc::new(Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await);
    let mut v = Viewer::connect(&h).await;

    let load_id = v
        .request(
            "session/load",
            json!({
                "sessionId": sid,
                "cwd": "/ignored",
                "mcpServers": []
            }),
        )
        .await;

    let mut frames_before_done = Vec::new();
    loop {
        let f = tokio::time::timeout(Duration::from_secs(5), v.recv_frame())
            .await
            .expect("timeout waiting for frame")
            .expect("stream open");
        if f.get("method").and_then(Value::as_str) == Some("_honk/replay_done") {
            break;
        }
        frames_before_done.push(f);
    }

    let load_resp = frames_before_done
        .iter()
        .find(|f| honk_upstream::rpc_id_matches(&load_id, f.get("id").unwrap_or(&Value::Null)))
        .expect("must receive session/load response");
    assert!(load_resp.get("error").is_none());

    let usage_f = frames_before_done
        .iter()
        .find(|f| {
            f.get("method").and_then(Value::as_str) == Some("session/update")
                && f.pointer("/params/update/sessionUpdate")
                    .and_then(Value::as_str)
                    == Some("usage_update")
        })
        .expect("must receive usage_update in replay before _honk/replay_done");

    let usage = honk_wire::usage(usage_f.pointer("/params/update").unwrap()).unwrap();
    assert_eq!(usage.used, 45000);
    assert_eq!(usage.size, 130000);
    let cost = usage.cost.expect("cost present");
    assert_eq!(cost.amount, 0.06);
    assert_eq!(cost.currency, "USD");
}
