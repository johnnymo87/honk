use honk_testgoose::{StubGoose, Turn};
use serde_json::{Value, json};

mod common;
use common::{Honkd, Viewer, new_session, prompt_params};

#[tokio::test(flavor = "multi_thread")]
async fn cancel_during_gated_text_turn_owner_gets_stop_reason_cancelled() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "gate_cancel_turn".into(),
        Box::new(Turn::Text(vec![
            "text chunk 1".into(),
            "text chunk 2".into(),
        ])),
    )]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    let p_id = v
        .request(
            "session/prompt",
            prompt_params(&sid, "test cancel gated", None),
        )
        .await;

    // Wait until run is active
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // Send cancel while turn is active
    v.call("session/cancel", json!({ "sessionId": &sid })).await;

    // Open gate so model stream unblocks if waiting
    g.model().open("gate_cancel_turn");

    let p_resp = v.response(&p_id).await;
    assert_eq!(
        p_resp["result"]["stopReason"], "cancelled",
        "owner gets stopReason: cancelled on cancel: {p_resp:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_while_parked_on_permission_answers_cancelled_and_run_ends() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([
        Turn::ToolCall {
            tool: "shell".into(),
            arguments: json!({"command": "echo test_cancel_perm"}),
        },
        Turn::Text(vec!["should never reach here".into()]),
    ]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    // Attach a second honk-aware viewer
    let mut v2 = Viewer::connect(&h).await;
    v2.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Put session in approve mode so tool call requires permission
    let mode_res = v
        .call(
            "session/set_mode",
            json!({"sessionId": &sid, "modeId": "approve"}),
        )
        .await;
    assert!(mode_res.get("error").is_none(), "set_mode succeeded");

    let p_id = v
        .request(
            "session/prompt",
            prompt_params(&sid, "run shell in approve mode", None),
        )
        .await;

    // Wait for permission request on both viewers
    let perm_req = v
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;
    let _perm_req2 = v2
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("request_permission")
                || f.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
        .await;
    let perm_id = perm_req.get("id").expect("permission id").clone();

    // Cancel while parked on permission
    v.call("session/cancel", json!({ "sessionId": &sid })).await;

    // Honkd broadcasts _honk/request_resolved with reason "cancelled" to all honk-aware viewers
    let resolved = v
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/request_resolved"))
        .await;
    assert_eq!(
        resolved["params"]["reason"], "cancelled",
        "request_resolved broadcast has reason cancelled"
    );
    assert_eq!(
        resolved["params"]["requestId"], perm_id,
        "resolved requestId matches permission request"
    );

    let resolved2 = v2
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/request_resolved"))
        .await;
    assert_eq!(
        resolved2["params"]["reason"], "cancelled",
        "viewer 2 also receives request_resolved broadcast with reason cancelled"
    );
    assert_eq!(
        resolved2["params"]["requestId"], perm_id,
        "viewer 2 resolved requestId matches permission request"
    );

    // Goose receives cancelled answer and ends the run
    let p_resp = v.response(&p_id).await;
    assert_eq!(
        p_resp["result"]["stopReason"], "cancelled",
        "prompt completed with stopReason: cancelled: {p_resp:?}"
    );

    // The second turn in the model was never reached
    assert_eq!(
        g.model().turn_requests().len(),
        1,
        "model only received the first turn request (tool call)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn early_cancel_before_announcement_reply_cancelled_and_turn_never_completes() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([
        Turn::Gate(
            "gate_early_cancel".into(),
            Box::new(Turn::ToolCall {
                tool: "shell".into(),
                arguments: json!({"command": "echo early_cancel_cmd"}),
            }),
        ),
        Turn::Text(vec!["second turn should not execute".into()]),
    ]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    // Send prompt
    let p_id = v
        .request(
            "session/prompt",
            prompt_params(&sid, "early cancel test", None),
        )
        .await;

    // IMMEDIATELY send cancel before announcement
    v.call("session/cancel", json!({ "sessionId": &sid })).await;

    // Open gate so tool call can proceed if goose started it
    g.model().open("gate_early_cancel");

    // The turn is cancelled and prompt reply is cancelled
    let p_resp = v.response(&p_id).await;
    assert_eq!(
        p_resp["result"]["stopReason"], "cancelled",
        "early cancel results in stopReason: cancelled: {p_resp:?}"
    );

    // The turn never completed: the second model turn was never called
    let turn_requests = g.model().turn_requests();
    assert!(
        turn_requests.len() <= 1,
        "the turn never completes: got {} turn requests, expected at most 1",
        turn_requests.len()
    );
}
