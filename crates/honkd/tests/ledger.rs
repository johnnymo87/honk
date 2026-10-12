use honk_testgoose::{FakeGoose, StubGoose, TcpProxy, Turn};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::time::Duration;

mod common;
use common::{Honkd, Viewer, new_session, prompt_params};

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_duplicate_client_req_id_one_upstream_prompt_two_replies() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "gate_dup".into(),
        Box::new(Turn::Text(vec!["hello duplicate".into()])),
    )]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, g.cwd().to_str().unwrap()).await;

    // Send first prompt with crid-dup-1
    let req1 = v1
        .request(
            "session/prompt",
            prompt_params(&sid, "test dup", Some("crid-dup-1")),
        )
        .await;

    // Wait until run is active on upstream
    v1.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // Send second prompt with the exact same client_req_id on another viewer
    let mut v2 = Viewer::connect(&h).await;
    let req2 = v2
        .request(
            "session/prompt",
            prompt_params(&sid, "test dup retry", Some("crid-dup-1")),
        )
        .await;

    // Release gate
    g.model().open("gate_dup");

    let res1 = v1.response(&req1).await;
    let res2 = v2.response(&req2).await;

    assert_eq!(res1["id"], req1);
    assert_eq!(res1["result"]["stopReason"], "end_turn");
    assert_eq!(res1["result"]["_meta"]["honk"]["text"], "hello duplicate");

    assert_eq!(res2["id"], req2);
    assert_eq!(res2["result"]["stopReason"], "end_turn");
    assert_eq!(res2["result"]["_meta"]["honk"]["text"], "hello duplicate");

    // "One upstream prompt" must be asserted via the stub model's request count
    assert_eq!(
        g.model().turn_requests().len(),
        1,
        "exactly one prompt reached the upstream model"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_after_done_returns_cached_reply_including_text() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model()
        .push([Turn::Text(vec!["cached response text".into()])]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, g.cwd().to_str().unwrap()).await;

    let res1 = v1
        .call(
            "session/prompt",
            prompt_params(&sid, "first call", Some("crid-done-1")),
        )
        .await;

    assert_eq!(res1["result"]["stopReason"], "end_turn");
    assert_eq!(
        res1["result"]["_meta"]["honk"]["text"],
        "cached response text"
    );
    assert_eq!(
        g.model().turn_requests().len(),
        1,
        "first call reached the model"
    );

    // Second call with same client_req_id after completion returns cached response
    let mut v2 = Viewer::connect(&h).await;
    let res2 = v2
        .call(
            "session/prompt",
            prompt_params(&sid, "retry call", Some("crid-done-1")),
        )
        .await;

    assert_eq!(res2["result"]["stopReason"], "end_turn");
    assert_eq!(
        res2["result"]["_meta"]["honk"]["text"],
        "cached response text"
    );

    // Model was not contacted a second time
    assert_eq!(
        g.model().turn_requests().len(),
        1,
        "retry after done did not contact upstream model"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn client_req_id_reused_on_another_session_errors() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Text(vec!["session 1 done".into()])]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid1 = new_session(&mut v, g.cwd().to_str().unwrap()).await;
    let sid2 = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    // Run prompt on session 1 to completion with crid-cross
    let res1 = v
        .call(
            "session/prompt",
            prompt_params(&sid1, "prompt s1", Some("crid-cross")),
        )
        .await;
    assert_eq!(res1["result"]["stopReason"], "end_turn");

    // Reuse crid-cross on session 2
    let res2 = v
        .call(
            "session/prompt",
            prompt_params(&sid2, "prompt s2 reuse crid", Some("crid-cross")),
        )
        .await;

    assert_eq!(
        res2["error"]["code"], -32602,
        "reusing client_req_id on another session returns -32602: {res2:?}"
    );
    assert_eq!(
        res2["error"]["message"], "client_req_id belongs to another session",
        "error message must match spec: {res2:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn client_req_id_inflight_on_another_session_errors() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "gate_inflight".into(),
        Box::new(Turn::Text(vec!["done".into()])),
    )]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid1 = new_session(&mut v, g.cwd().to_str().unwrap()).await;
    let sid2 = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    // Start prompt on session 1 (gated, so stays in-flight)
    let p1_id = v
        .request(
            "session/prompt",
            prompt_params(&sid1, "prompt s1", Some("crid-inflight-cross")),
        )
        .await;

    // Wait until run is active on session 1
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // While s1 prompt is in-flight, try using the same client_req_id on s2
    let res2 = v
        .call(
            "session/prompt",
            prompt_params(&sid2, "prompt s2 reuse crid", Some("crid-inflight-cross")),
        )
        .await;

    assert_eq!(
        res2["error"]["code"], -32602,
        "reusing inflight client_req_id on another session returns -32602: {res2:?}"
    );
    assert_eq!(
        res2["error"]["message"], "client_req_id belongs to another session",
        "error message must match spec: {res2:?}"
    );

    // Release gate and verify s1 finishes cleanly
    g.model().open("gate_inflight");
    let res1 = v.response(&p1_id).await;
    assert_eq!(res1["result"]["stopReason"], "end_turn");
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_viewer_unconsumed_steer_logs_warning_no_notification() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    // Model returns text directly; steer will arrive after run ends
    g.model().push([Turn::Gate(
        "gate_steer_unconsumed".into(),
        Box::new(Turn::Text(vec!["text turn".into()])),
    )]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    let p_id = v
        .request(
            "session/prompt",
            prompt_params(&sid, "test unconsumed", None),
        )
        .await;

    // Wait until run is active
    let active_frame = v
        .wait_for(|f| {
            f.pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("session_info_update")
                && f.pointer("/params/update/_meta/goose/activeRunId")
                    .and_then(Value::as_str)
                    .is_some()
        })
        .await;

    let active_run = active_frame
        .pointer("/params/update/_meta/goose/activeRunId")
        .and_then(Value::as_str)
        .expect("active run id")
        .to_string();

    // Release gate so turn finishes
    g.model().open("gate_steer_unconsumed");
    let resp = v.response(&p_id).await;
    assert_eq!(resp["result"]["stopReason"], "end_turn");

    // After run end, send steer with the old run id
    // Goose will return "no active run to steer"
    let steer_resp = v
        .call(
            "_goose/unstable/session/steer",
            json!({
                "sessionId": &sid,
                "expectedRunId": active_run,
                "prompt": [{
                    "type": "text",
                    "text": "late steer message",
                }],
            }),
        )
        .await;

    assert!(
        steer_resp["error"]["data"]
            .as_str()
            .unwrap_or("")
            .contains("no active run to steer"),
        "late steer returned no active run error: {steer_resp:?}"
    );

    // No _honk/steer_unconsumed sent to legacy viewer
    assert!(
        !v.raw_methods.iter().any(|m| m == "_honk/steer_unconsumed"),
        "legacy viewer must never receive _honk/steer_unconsumed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn steer_during_gated_turn_consumed_by_stub_goose_no_notification() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };

    // Turn 1: gated tool call
    // Turn 2: text response
    g.model().push([
        Turn::Gate(
            "gate_steer".into(),
            Box::new(Turn::ToolCall {
                tool: "shell".into(),
                arguments: json!({"command": "echo steer_tool"}),
            }),
        ),
        Turn::Text(vec!["steer handled".into()]),
    ]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    let prompt_id = v
        .request("session/prompt", prompt_params(&sid, "start gated", None))
        .await;

    // Wait until run is active
    let active_frame = v
        .wait_for(|f| {
            f.pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("session_info_update")
                && f.pointer("/params/update/_meta/goose/activeRunId")
                    .and_then(Value::as_str)
                    .is_some()
        })
        .await;

    let active_run = active_frame
        .pointer("/params/update/_meta/goose/activeRunId")
        .and_then(Value::as_str)
        .expect("active run id")
        .to_string();

    // Send steer while turn is active
    let steer_resp = v
        .call(
            "_goose/unstable/session/steer",
            json!({
                "sessionId": &sid,
                "expectedRunId": active_run,
                "prompt": [{
                    "type": "text",
                    "text": "my steer message",
                }],
            }),
        )
        .await;

    let steer_msg_id = steer_resp["result"]["messageId"]
        .as_str()
        .expect("steer result messageId");
    assert!(
        steer_msg_id.starts_with("steer_") || !steer_msg_id.is_empty(),
        "valid steer message id: {steer_resp:?}"
    );

    // Open gate so tool call proceeds, goose executes it and requests next turn
    g.model().open("gate_steer");

    // The prompt response arrives
    let p_resp = v.response(&prompt_id).await;
    assert_eq!(p_resp["result"]["stopReason"], "end_turn");

    // Steer was consumed: the stub model's next request contains the steer message
    let reqs = g.model().turn_requests();
    assert!(
        reqs.len() >= 2,
        "expected at least 2 turn requests (tool call + final text), got {}",
        reqs.len()
    );
    let second_req = &reqs[1];
    let msgs = second_req
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages array in second turn request");

    let steer_present = msgs.iter().any(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .map(|c| c.contains("my steer message"))
            .unwrap_or(false)
            || m.pointer("/content/0/text")
                .and_then(Value::as_str)
                .map(|c| c.contains("my steer message"))
                .unwrap_or(false)
    });
    assert!(
        steer_present,
        "steer text must be present in the next model request: {second_req:?}"
    );

    // No _honk/steer_unconsumed notification was sent to the viewer
    assert!(
        !v.raw_methods.iter().any(|m| m == "_honk/steer_unconsumed"),
        "consumed steer must not emit _honk/steer_unconsumed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn steer_error_passes_through_verbatim() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    // When idle (no active run), steer with expectedRunId fails with "no active run to steer"
    let steer_resp = v
        .call(
            "_goose/unstable/session/steer",
            json!({
                "sessionId": &sid,
                "expectedRunId": "run_bogus",
                "prompt": [{
                    "type": "text",
                    "text": "steer when idle",
                }],
            }),
        )
        .await;

    let err_data = steer_resp["error"]["data"].as_str().unwrap_or("");
    let err_msg = steer_resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        err_data.contains("no active run to steer") || err_msg.contains("no active run to steer"),
        "steer error must pass through verbatim, got: {steer_resp:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_prunes_old_done_ledger_entries() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let mut h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;

    // Stop honkd to prepare pre-boot state
    h.kill9();

    let state_path = h.state_dir.path().join("state.json");
    let state_content = std::fs::read_to_string(&state_path).unwrap();
    let mut state: Value = serde_json::from_str(&state_content).unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Populate ledger with:
    // - old_done: 48h ago (should be pruned)
    // - fresh_done: 10s ago (should be kept)
    // - live_inflight: should be kept
    state["sessions"]["s1"] = json!({
        "conn_id": "conn_1",
        "cwd": "/tmp",
        "epoch": 1,
        "last_conn_event_id": null,
        "pending": [],
        "inflight": {
            "upstream_id": "up_1",
            "client_req_ids": ["live_inflight"],
            "params": {},
            "announced": true
        },
        "last_active_unix": 1000
    });

    state["ledger"] = json!({
        "old_done": {
            "state": "done",
            "session": "s1",
            "response": { "jsonrpc": "2.0", "result": { "stopReason": "end_turn" } },
            "done_unix": now.saturating_sub(48 * 3600)
        },
        "fresh_done": {
            "state": "done",
            "session": "s1",
            "response": { "jsonrpc": "2.0", "result": { "stopReason": "end_turn" } },
            "done_unix": now.saturating_sub(10)
        },
        "live_inflight": {
            "state": "in_flight",
            "session": "s1",
            "upstream_id": "up_1"
        }
    });

    std::fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

    // Restart honkd -> boot sequence runs
    h.restart().await;

    let updated_content = std::fs::read_to_string(&state_path).unwrap();
    let updated_state: Value = serde_json::from_str(&updated_content).unwrap();
    let ledger = updated_state["ledger"].as_object().unwrap();

    assert!(
        !ledger.contains_key("old_done"),
        "old_done must be pruned at boot"
    );
    assert!(ledger.contains_key("fresh_done"), "fresh_done must be kept");
    assert!(
        ledger.contains_key("live_inflight"),
        "live_inflight must be kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_busy_rejection_clears_ledger_and_retry_after_idle_runs() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    // Configure fake goose to answer next session/prompt with an upstream busy rejection
    fake.set_prompt_error_reply(Some(json!({
        "code": -32602,
        "message": "Invalid params",
        "data": "session already has active run `run_fake_busy`; use _goose/unstable/session/steer"
    })));

    let r1 = v
        .call(
            "session/prompt",
            prompt_params(&sid, "prompt 1", Some("crid-busy-retry")),
        )
        .await;

    assert_eq!(r1["error"]["code"], -32602);
    assert!(
        r1["error"]["data"]
            .as_str()
            .unwrap_or("")
            .contains("session already has active run")
    );

    // Verify ledger did NOT retain crid-busy-retry as Done (it was removed!)
    let state_content = std::fs::read_to_string(h.state_dir.path().join("state.json")).unwrap();
    let state: Value = serde_json::from_str(&state_content).unwrap();
    assert!(
        !state["ledger"]
            .as_object()
            .unwrap()
            .contains_key("crid-busy-retry"),
        "busy rejection must remove ledger entry so it can be retried"
    );
    assert!(
        state["sessions"][&sid]["inflight"].is_null(),
        "busy rejection must clear held.inflight"
    );

    // Clear busy error on fake goose -> session goes idle: activeRunId: null
    fake.set_prompt_error_reply(None);
    fake.emit_frame(
        &sid,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": &sid,
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "goose": {
                            "activeRunId": Value::Null
                        }
                    }
                }
            }
        }),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Retry with the exact same client_req_id
    let r2 = v
        .call(
            "session/prompt",
            prompt_params(&sid, "prompt 1 retry", Some("crid-busy-retry")),
        )
        .await;

    assert_eq!(
        r2["result"]["stopReason"], "end_turn",
        "retry after busy rejection must run and succeed: {r2:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_prunes_orphan_inflight_entries() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let mut h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    h.kill9();

    let state_path = h.state_dir.path().join("state.json");
    let state_content = std::fs::read_to_string(&state_path).unwrap();
    let mut state: Value = serde_json::from_str(&state_content).unwrap();

    // Populate sessions with:
    // - s_held: has held.inflight referencing only ["crid_held"]
    // s_unheld is NOT in sessions
    state["sessions"]["s_held"] = json!({
        "conn_id": "conn_1",
        "cwd": "/tmp",
        "epoch": 1,
        "last_conn_event_id": null,
        "pending": [],
        "inflight": {
            "upstream_id": "u1",
            "client_req_ids": ["crid_held"],
            "params": {},
            "announced": true
        },
        "last_active_unix": 1000
    });

    state["ledger"] = json!({
        "crid_unheld": {
            "state": "in_flight",
            "session": "s_unheld",
            "upstream_id": "u_unheld"
        },
        "crid_no_inflight": {
            "state": "in_flight",
            "session": "s_held",
            "upstream_id": "u_other"
        },
        "crid_held": {
            "state": "in_flight",
            "session": "s_held",
            "upstream_id": "u1"
        }
    });

    std::fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

    h.restart().await;

    let updated_content = std::fs::read_to_string(&state_path).unwrap();
    let updated_state: Value = serde_json::from_str(&updated_content).unwrap();
    let ledger = updated_state["ledger"].as_object().unwrap();

    assert!(
        !ledger.contains_key("crid_unheld"),
        "crid_unheld must be pruned at boot because its session is not held"
    );
    assert_eq!(
        ledger["crid_no_inflight"]["state"], "done",
        "crid_no_inflight must be resolved to Done at boot because held.inflight doesn't reference it"
    );
    assert!(
        ledger.contains_key("crid_held"),
        "crid_held must be kept because held.inflight references it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gone_session_resolves_inflight_to_done_upstream_gone() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    v.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Script an active run with a tool call in FakeGoose
    fake.script_frames(vec![
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": &sid,
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "goose": {
                            "activeRunId": "run_probe_test_gone"
                        }
                    }
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": &sid,
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call_1",
                    "title": "shell",
                    "status": "in_progress"
                }
            }
        }),
    ]);

    // Induce gap on resume, and 404 on steer probe POST
    fake.gap_on_next_resume(&sid);
    fake.set_steer_status(Some(StatusCode::NOT_FOUND));

    let pid = v
        .request(
            "session/prompt",
            prompt_params(&sid, "test gone prompt", Some("crid-gone")),
        )
        .await;

    // Wait until tool_call frame appears
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("tool_call")
    })
    .await;

    // Kick stream while turn is in flight
    fake.kick_stream(&sid);

    // Watchdog or stream ended causes reattach -> gap -> steer probe POST returns 404 -> ReattachFailed { gone: true } -> Gone!
    // The in-flight prompt should get cancelled with reason upstream_restart
    let resp = v.response_timeout(&pid, Duration::from_secs(10)).await;
    assert_eq!(resp["result"]["stopReason"], "cancelled");
    assert_eq!(
        resp.pointer("/result/_meta/honk/reason")
            .and_then(Value::as_str),
        Some("upstream_restart")
    );

    // Check state.json: crid-gone is resolved to Done with cancelled upstream_restart reply
    let state_content = std::fs::read_to_string(h.state_dir.path().join("state.json")).unwrap();
    let state: Value = serde_json::from_str(&state_content).unwrap();
    let entry = &state["ledger"]["crid-gone"];
    assert_eq!(
        entry["state"], "done",
        "InFlight must be resolved to Done on Gone"
    );
    assert_eq!(
        entry
            .pointer("/response/result/_meta/honk/reason")
            .and_then(Value::as_str),
        Some("upstream_restart"),
        "cached response must indicate upstream_restart"
    );

    // When client retries with the same client_req_id, it gets the cached cancelled reply
    let mut v2 = Viewer::connect(&h).await;
    let r2 = v2
        .call(
            "session/prompt",
            prompt_params(&sid, "test gone prompt retry", Some("crid-gone")),
        )
        .await;

    assert_eq!(
        r2["result"]["stopReason"], "cancelled",
        "retry gets cached cancelled: {r2:?}"
    );
    assert_eq!(
        r2.pointer("/result/_meta/honk/reason")
            .and_then(Value::as_str),
        Some("upstream_restart"),
        "retry result has reason upstream_restart"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_post_500_failure_clears_ledger_and_allows_retry() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    // Set 500 status on prompt POST (certainly not started)
    fake.set_prompt_status(Some(StatusCode::INTERNAL_SERVER_ERROR));

    let r1 = v
        .call(
            "session/prompt",
            prompt_params(&sid, "prompt 500", Some("crid-500-test")),
        )
        .await;

    assert_eq!(
        r1["error"]["code"], -32603,
        "POST 500 fails prompt send: {r1:?}"
    );

    // Verify ledger entry was removed and held.inflight cleared
    let state_content = std::fs::read_to_string(h.state_dir.path().join("state.json")).unwrap();
    let state: Value = serde_json::from_str(&state_content).unwrap();
    assert!(
        !state["ledger"]
            .as_object()
            .unwrap()
            .contains_key("crid-500-test"),
        "500 POST rejection must remove ledger entry"
    );
    assert!(
        state["sessions"][&sid]["inflight"].is_null(),
        "500 POST rejection must clear held.inflight"
    );

    // Clear 500 status
    fake.set_prompt_status(None);

    // Retry with SAME client_req_id
    let r2 = v
        .call(
            "session/prompt",
            prompt_params(&sid, "prompt 500 retry", Some("crid-500-test")),
        )
        .await;

    assert_eq!(
        r2["result"]["stopReason"], "end_turn",
        "retry after 500 failure must run and succeed: {r2:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_post_ambiguous_failure_keeps_inflight_without_false_negative_not_delivered() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    let proxy = TcpProxy::start(fake.addr()).await.unwrap();

    let proxy_url = format!("http://{}/acp", proxy.addr());
    let h = Honkd::spawn(
        &proxy_url,
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    // Simulate async commit delay in goose: export does NOT have the user message yet!
    // Honkd must NOT declare false-negative not_delivered while the connection is live.
    fake.set_export_data(&sid, json!({ "conversation": [] }));

    // Drop automatic prompt reply so we can observe the in-flight state
    fake.set_drop_prompt_reply(true);

    // Enable connection drop on prompt in proxy
    proxy.set_drop_on_prompt(true);

    let pid = v
        .request(
            "session/prompt",
            prompt_params(&sid, "ambiguous delivery text", Some("crid-ambig")),
        )
        .await;

    // Wait for transport drop to occur
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Check state.json: crid-ambig MUST STILL BE InFlight (not marked not_delivered!)
    let state_content = std::fs::read_to_string(h.state_dir.path().join("state.json")).unwrap();
    let state: Value = serde_json::from_str(&state_content).unwrap();
    assert!(
        state["ledger"]
            .as_object()
            .unwrap()
            .contains_key("crid-ambig"),
        "ambiguous failure must keep ledger InFlight entry"
    );
    assert_eq!(
        state["ledger"]["crid-ambig"]["state"], "in_flight",
        "ledger entry must remain in_flight without false-negative not_delivered"
    );

    // A concurrent retry with the same client_req_id attaches to the in-flight prompt
    let mut v2 = Viewer::connect(&h).await;
    let pid2 = v2
        .request(
            "session/prompt",
            prompt_params(&sid, "ambiguous delivery text retry", Some("crid-ambig")),
        )
        .await;

    // Disable drop and emit prompt reply from fake goose
    proxy.set_drop_on_prompt(false);
    fake.emit_frame(
        &sid,
        json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "result": {
                "stopReason": "end_turn"
            }
        }),
    );

    let r1 = v.response_timeout(&pid, Duration::from_secs(10)).await;
    let r2 = v2.response_timeout(&pid2, Duration::from_secs(10)).await;

    assert_eq!(r1["result"]["stopReason"], "end_turn");
    assert_eq!(r2["result"]["stopReason"], "end_turn");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_on_unannounced_inflight_ends_with_reply_and_done_ledger() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    // Fake goose does not announce a run (simulating a prompt that never started/arrived at goose)
    fake.set_drop_prompt_announcement(true);

    let pid = v
        .request(
            "session/prompt",
            prompt_params(
                &sid,
                "prompt never arrives",
                Some("crid-unannounced-cancel"),
            ),
        )
        .await;

    // Wait for prompt to be in-flight in honkd
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify it is in-flight in ledger
    let state_content = std::fs::read_to_string(h.state_dir.path().join("state.json")).unwrap();
    let state: Value = serde_json::from_str(&state_content).unwrap();
    assert_eq!(
        state["ledger"]["crid-unannounced-cancel"]["state"],
        "in_flight"
    );

    // Client cancels the unannounced inflight prompt
    v.call("session/cancel", json!({ "sessionId": &sid })).await;

    // Prompt owner receives cancelled reply
    let resp = v.response_timeout(&pid, Duration::from_secs(10)).await;
    assert_eq!(
        resp["result"]["stopReason"], "cancelled",
        "cancel on unannounced inflight must reply cancelled: {resp:?}"
    );

    // State file shows inflight is resolved to Done and held.inflight is None
    let state_content = std::fs::read_to_string(h.state_dir.path().join("state.json")).unwrap();
    let state: Value = serde_json::from_str(&state_content).unwrap();
    assert_eq!(
        state["ledger"]["crid-unannounced-cancel"]["state"], "done",
        "ledger entry must be resolved to Done"
    );
    assert!(
        state["sessions"][&sid]["inflight"].is_null(),
        "held.inflight must be cleared"
    );
}
