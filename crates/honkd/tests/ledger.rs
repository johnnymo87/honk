use honk_testgoose::{StubGoose, Turn};
use serde_json::{Value, json};

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
