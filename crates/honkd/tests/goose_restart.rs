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
    // Ensure the turn has reached the model and is waiting on the gate
    let wait_start = Instant::now();
    while g.model().turn_requests().is_empty() && wait_start.elapsed() < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    g.restart().await;
    let r = v.response_timeout(&id, SECS * 3).await;
    assert_eq!(r["result"]["stopReason"], "cancelled");
    assert_eq!(r["result"]["_meta"]["honk"]["reason"], "upstream_restart");
    v.wait_for(|f| {
        is_method(f, honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch") == Some(&json!(2))
    })
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

    // Poll until control connection reconnects and detects unpatched upstream
    let start = Instant::now();
    let resp = loop {
        let pid = v
            .request(
                "session/prompt",
                prompt_params(&sid, "test unpatched prompt", Some("crid-unpatched")),
            )
            .await;

        let r = v.response_timeout(&pid, SECS).await;
        if r.pointer("/error/code").and_then(Value::as_i64) == Some(-32000)
            && r.pointer("/error/message").and_then(Value::as_str)
                == Some("upstream goose is no longer patched")
        {
            break r;
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!("timed out waiting for unpatched refusal: {r:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

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

#[tokio::test(flavor = "multi_thread")]
async fn synthesized_active_run_id_null_reaches_legacy_viewer() {
    let mut fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, ".").await;

    // Send a prompt on legacy viewer to establish a run
    fake.script_frames(vec![json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": &sid,
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": {
                    "goose": {
                        "activeRunId": "run_legacy_test"
                    }
                }
            }
        }
    })]);

    let req_id = v
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{ "type": "text", "text": "hello legacy" }]
            }),
        )
        .await;

    // Wait for activeRunId to be announced
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                == Some("run_legacy_test")
    })
    .await;

    // Goose restarts!
    fake.restart().await;

    // Legacy viewer must receive session_info_update with activeRunId: null
    let null_frame = v
        .wait_for(|f| {
            f.pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("session_info_update")
                && f.pointer("/params/update/_meta/goose/activeRunId") == Some(&Value::Null)
        })
        .await;

    // Confirm that honk metadata was stripped for legacy viewer
    assert!(
        null_frame.pointer("/params/_meta/honk").is_none(),
        "legacy viewer frame must not contain honk metadata: {null_frame:?}"
    );

    // Prompt response is cancelled
    let resp = v.response_timeout(&req_id, SECS * 2).await;
    assert_eq!(resp["result"]["stopReason"], "cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn reactivation_permanent_failure_missing_session_terminates_actor() {
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

    // Remove session from FakeGoose so session/list will not contain it after restart
    fake.remove_session(&sid);
    fake.restart().await;

    // Honk-aware viewer receives _honk/evicted
    let evicted = v
        .wait_for(|f| is_method(f, honk_wire::method::EVICTED))
        .await;
    assert_eq!(evicted["params"]["sessionId"], sid);

    // Verify actor is removed from state.sessions and retired_epochs is recorded
    let state_bytes = std::fs::read(h.state_dir.path().join("state.json")).expect("read state");
    let state: Value = serde_json::from_slice(&state_bytes).expect("parse state");
    assert!(
        state["sessions"].get(&sid).is_none(),
        "session must be removed from state.sessions on permanent reactivation failure"
    );
    assert!(
        state["retired_epochs"].get(&sid).is_some(),
        "session retired_epoch must be recorded"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn control_reconnect_drains_pending_control_oneshots() {
    let mut fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;

    // Induce control connection failure
    fake.restart().await;

    // Immediately send session/list request on viewer: should return error quickly (< 2s), NOT wait 30s
    let start = Instant::now();
    let resp = v.call("session/list", json!({})).await;
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "control_request must not wait for 30s timeout; took {elapsed:?}"
    );
    assert!(
        resp["error"].is_object(),
        "must return an error response: {resp:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn control_connection_half_open_byte_watchdog() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    // Disable keepalives so stream goes silent
    fake.set_keepalive_interval(Duration::from_secs(60));

    // Spawn honkd with stream-dead-secs 2
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--stream-dead-secs", "2"],
    )
    .await;

    // Wait > 2.5s for the watchdog to detect dead control connection and trigger reconnect
    tokio::time::sleep(Duration::from_millis(2700)).await;

    let stderr = h.stderr_text();
    assert!(
        stderr.contains("Control connection watchdog detected dead/half-open stream"),
        "control watchdog must trigger reconnect; stderr:\n{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn steer_post_404_triggers_handle_gone() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    v.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Set steer status to 404
    fake.set_steer_status(Some(reqwest::StatusCode::NOT_FOUND));

    let steer_resp = v
        .call(
            "_goose/unstable/session/steer",
            json!({
                "sessionId": &sid,
                "prompt": [{ "type": "text", "text": "test steer" }]
            }),
        )
        .await;

    assert!(
        steer_resp["error"].is_object(),
        "steer 404 returns error to caller: {steer_resp:?}"
    );

    // Session handles gone and re-activates with epoch 2
    v.wait_for(|f| {
        is_method(f, honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch") == Some(&json!(2))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rebuilding_session_queues_requests_fifo_and_serves_attaches() {
    let mut fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, ".").await;

    v1.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Goose restarts!
    fake.restart().await;

    // While session is rebuilding, a new viewer attaches: attach must be served immediately from log!
    let mut v2 = Viewer::connect(&h).await;
    let load_res = v2
        .call(
            "session/load",
            json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
        )
        .await;
    assert!(
        load_res["result"].is_object(),
        "attach during rebuilding must succeed: {load_res:?}"
    );

    // Queue a prompt during rebuilding on v1
    fake.script_prompt(vec!["prompt drained ok".into()]);
    let pid = v1
        .request(
            "session/prompt",
            prompt_params(&sid, "queued during rebuild", Some("crid-queued")),
        )
        .await;

    // Both viewers must receive replay_done for epoch 2 once reactivation completes
    v1.wait_for(|f| {
        is_method(f, honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch") == Some(&json!(2))
    })
    .await;

    // The queued prompt must be drained and succeed!
    let presp = v1.response_timeout(&pid, SECS * 3).await;
    assert_eq!(
        presp.pointer("/result/stopReason").and_then(Value::as_str),
        Some("end_turn"),
        "queued prompt must be drained and succeed: {presp:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn double_gone_triggers_exactly_one_reactivation() {
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

    v.blocks.resets = 0;

    // Induce a double gone by restarting FakeGoose and kicking stream
    fake.restart().await;
    fake.kick_stream(&sid);

    // Wait for reactivation to complete
    v.wait_for(|f| {
        is_method(f, honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch") == Some(&json!(2))
    })
    .await;

    // Exactly one reset delivered to viewer
    assert_eq!(
        v.blocks.resets, 1,
        "exactly one reset should be delivered on double gone"
    );

    // Inbound session/load calls on FakeGoose should be exactly 1 from the single reactivation
    let loads: Vec<_> = fake
        .inbound()
        .into_iter()
        .filter(|f| f.get("method").and_then(Value::as_str) == Some("session/load"))
        .collect();
    assert_eq!(
        loads.len(),
        1,
        "must have exactly 1 session/load call from single reactivation"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gone_during_gap_rebuild_reactivation_wins_late_rebuilt_ignored() {
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

    // Delay export so gap rebuild is in progress when restart occurs
    fake.set_export_delay(Some(Duration::from_millis(300)));

    // Induce a gap rebuild: kick stream with gap_on_next_resume while idle
    fake.gap_on_next_resume(&sid);
    fake.kick_stream(&sid);

    // Give the actor a moment to observe the gap and enter ActorState::Rebuilding
    tokio::time::sleep(Duration::from_millis(50)).await;

    // While in gap rebuild, clear delay for reactivation and restart goose -> 404 / Gone occurs!
    fake.set_export_delay(None);
    fake.restart().await;
    fake.kick_stream(&sid);

    v.wait_for(|f| {
        is_method(f, honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                >= 2
    })
    .await;

    // A prompt on the reactivated session must succeed
    fake.script_prompt(vec!["prompt after gap rebuild gone ok".into()]);
    let req_id = v
        .request(
            "session/prompt",
            prompt_params(&sid, "test prompt", Some("crid-rebuild-gone")),
        )
        .await;

    let resp = v.response_timeout(&req_id, SECS * 2).await;
    assert_eq!(
        resp.pointer("/result/stopReason").and_then(Value::as_str),
        Some("end_turn"),
        "prompt after reactivation over interrupted rebuild must succeed: {resp:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_requests_fail_with_32000_on_connection_replacement() {
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

    // Send a forward request that FakeGoose doesn't answer
    let forward_id = v
        .request(
            "session/set_config_option",
            json!({
                "sessionId": &sid,
                "configId": "model",
                "value": "custom-model"
            }),
        )
        .await;

    // Goose restarts!
    fake.restart().await;

    // The in-flight forward request must fail with -32000, not hang!
    let resp = v
        .response_timeout(&forward_id, Duration::from_secs(5))
        .await;
    assert_eq!(
        resp.pointer("/error/code").and_then(Value::as_i64),
        Some(-32000),
        "pending request must fail with -32000: {resp:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn on_rebuilt_sets_live_before_draining_events_so_notifications_are_not_lost() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    fake.set_keepalive_interval(Duration::from_secs(60));

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--stream-dead-secs", "2"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    v.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Trigger gap rebuild
    fake.gap_on_next_resume(&sid);
    fake.kick_stream(&sid);

    // Wait for replay_done for epoch 2
    v.wait_for(|f| {
        is_method(f, honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch") == Some(&json!(2))
    })
    .await;

    // Send a prompt to verify session actor is in Live state and can process events
    fake.script_prompt(vec!["live after rebuild".into()]);
    let pid = v
        .request("session/prompt", prompt_params(&sid, "test", None))
        .await;
    let resp = v.response_timeout(&pid, Duration::from_secs(5)).await;
    assert_eq!(resp["result"]["stopReason"], "end_turn");
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_steer_probe_response_clears_active_probe_on_timeout_and_retries() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    fake.set_keepalive_interval(Duration::from_secs(60));

    // Spawn honkd with probe_timeout_secs = 1
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--probe-timeout-secs", "1", "--stream-dead-secs", "2"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    v.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Drop the first steer probe response in fake goose so it is lost
    fake.lose_next(&sid, |f| {
        f.get("error")
            .and_then(|e| e.get("data"))
            .and_then(Value::as_str)
            == Some("no active run to steer")
    });

    // Trigger gap reattach so steer probe is issued
    fake.gap_on_next_resume(&sid);
    fake.kick_stream(&sid);

    // Because first probe was dropped, probe timeout clears active_probe_id and retries.
    // The retried probe succeeds, rebuild completes, and replay_done arrives!
    v.wait_for(|f| {
        is_method(f, honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch").and_then(Value::as_u64) == Some(2)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn goose_restart_reactivation_carries_post_load_usage_update() {
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

    let usage_frame = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": &sid,
            "update": {
                "sessionUpdate": "usage_update",
                "used": 55000,
                "size": 160000,
                "cost": {
                    "amount": 0.09,
                    "currency": "USD"
                }
            }
        }
    });
    fake.set_load_frames_before(&sid, vec![usage_frame]);

    // Restart goose -> triggers reactivation
    fake.restart().await;

    // Wait for replay_done with epoch 2
    v.wait_for(|f| {
        is_method(f, honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch").and_then(Value::as_u64) == Some(2)
    })
    .await;

    // Attach fresh viewer to verify the new generation log carries usage_update
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

    let mut frames_before_done = Vec::new();
    loop {
        let f = tokio::time::timeout(Duration::from_secs(5), v2.recv_frame())
            .await
            .expect("timeout waiting for frame")
            .expect("stream open");
        if is_method(&f, honk_wire::method::REPLAY_DONE) {
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
        .expect("fresh viewer must receive usage_update in epoch 2 replay");

    assert_eq!(
        usage_f
            .pointer("/params/_meta/honk/epoch")
            .and_then(Value::as_u64),
        Some(2)
    );

    let usage = honk_wire::usage(usage_f.pointer("/params/update").unwrap()).unwrap();
    assert_eq!(usage.used, 55000);
    assert_eq!(usage.size, 160000);
    let cost = usage.cost.expect("cost present");
    assert_eq!(cost.amount, 0.09);
    assert_eq!(cost.currency, "USD");
}
