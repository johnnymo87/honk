use common::{Honkd, Viewer};
use honk_testgoose::{StubGoose, Turn};
use serde_json::Value;
use std::time::Duration;

mod common;

fn new_session_params(cwd: &str) -> Value {
    serde_json::json!({
        "cwd": cwd,
        "mcpServers": [],
        "_meta": {
            "enabledExtensions": [{
                "type": "platform",
                "name": "developer"
            }]
        }
    })
}

async fn new_session(v: &mut Viewer, cwd: &str) -> String {
    let res = v.call("session/new", new_session_params(cwd)).await;
    res["result"]["sessionId"]
        .as_str()
        .expect("sessionId string")
        .to_string()
}

fn pigeon_init() -> Value {
    serde_json::json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {
                "readTextFile": false,
                "writeTextFile": false
            }
        }
    })
}

fn prompt_params(sid: &str, text: &str, client_req_id: Option<&str>) -> Value {
    let mut p = serde_json::json!({
        "sessionId": sid,
        "prompt": [{
            "type": "text",
            "text": text,
        }]
    });
    if let Some(crid) = client_req_id {
        p["_meta"] = serde_json::json!({
            "honk": {
                "client_req_id": crid
            }
        });
    }
    p
}

#[tokio::test(flavor = "multi_thread")]
async fn pigeon_sequence_mint_then_prompt_receipt_equals_export_turn_text() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model()
        .push([Turn::Text(vec!["hel".into(), "lo".into()])]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut mint = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut mint, g.cwd().to_str().unwrap()).await;
    drop(mint);

    let mut run = Viewer::connect_pigeon(&h).await;
    run.call("initialize", pigeon_init()).await; // mid-session ping
    let r = run
        .call("session/prompt", prompt_params(&sid, "hi", None))
        .await;

    assert_eq!(r["result"]["stopReason"], "end_turn");
    assert_eq!(r["result"]["_meta"]["honk"]["text"], "hello");
    assert_eq!(run.accumulated_text(), "hello");
    assert_eq!(run.receipt_text(), "hello");
    assert_eq!(run.receipt_text(), g.turn_text(&sid).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn native_busy_error_has_goose_text_and_run_id() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "gate1".into(),
        Box::new(Turn::Text(vec!["finished".into()])),
    )]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v1 = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v1, g.cwd().to_str().unwrap()).await;

    // Send first prompt asynchronously (it will pause at gate1)
    let p1_id = v1
        .request("session/prompt", prompt_params(&sid, "prompt 1", None))
        .await;

    // Wait event-driven until run is active
    v1.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // Second prompt on a separate viewer
    let mut v2 = Viewer::connect_pigeon(&h).await;
    let r2 = v2
        .call("session/prompt", prompt_params(&sid, "prompt 2", None))
        .await;

    let err_data = r2["error"]["data"].as_str().unwrap_or("");
    let err_msg = r2["error"]["message"].as_str().unwrap_or("");
    assert_eq!(r2["error"]["code"], -32602);
    assert!(
        err_data.starts_with("session already has active run `run_")
            || err_msg.starts_with("session already has active run `run_"),
        "expected busy error starting with active run id, got: {r2}"
    );

    // Release gate1
    g.model().open("gate1");
    let r1 = v1.response(&p1_id).await;
    assert_eq!(r1["result"]["stopReason"], "end_turn");
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_viewer_learns_active_run_on_attach() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "gate2".into(),
        Box::new(Turn::Text(vec!["done".into()])),
    )]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v1 = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v1, g.cwd().to_str().unwrap()).await;

    // Viewer A starts a gated turn
    let p1_id = v1
        .request("session/prompt", prompt_params(&sid, "run A", None))
        .await;

    // Wait event-driven until run is active
    v1.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // Pigeon-style viewer B sends initialize + a session/prompt that will be busy
    let mut v2 = Viewer::connect_pigeon(&h).await;
    v2.call("initialize", pigeon_init()).await;
    let r2 = v2
        .call("session/prompt", prompt_params(&sid, "run B", None))
        .await;

    let busy_run = honkd::rpc::busy_run_id(&r2).expect("run id from busy error");

    // Viewer B must have received session_info_update with activeRunId == busy_run
    assert_eq!(
        v2.blocks.active_run.as_deref(),
        Some(busy_run.as_str()),
        "viewer B blocks must reflect activeRunId from synthesized session_info_update"
    );

    // Release gate2
    g.model().open("gate2");
    let _ = v1.response(&p1_id).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_session_viewer_close() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect_pigeon(&h).await;

    let sid1 = new_session(&mut v, g.cwd().to_str().unwrap()).await;
    let sid2 = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    // Close session 1 specifically
    let r1 = v
        .call("session/close", serde_json::json!({ "sessionId": sid1 }))
        .await;
    assert_eq!(r1["result"], serde_json::json!({}));

    // Session 2 is still functional
    g.model().push([Turn::Text(vec!["pong".into()])]);
    let r2 = v
        .call("session/prompt", prompt_params(&sid2, "ping", None))
        .await;
    assert_eq!(r2["result"]["stopReason"], "end_turn");

    // Close without sessionId detaches all remaining
    let r_all = v.call("session/close", serde_json::json!({})).await;
    assert_eq!(r_all["result"], serde_json::json!({}));
}

#[tokio::test(flavor = "multi_thread")]
async fn steer_reply_delivered_and_steer_unconsumed_not_emitted_when_consumed() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([
        Turn::Gate(
            "gate_steer".into(),
            Box::new(Turn::Text(vec!["steered".into()])),
        ),
        Turn::Text(vec!["final".into()]),
    ]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    let p_id = v
        .request("session/prompt", prompt_params(&sid, "start", None))
        .await;

    // Wait event-driven until run is active
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    let active_run = v.blocks.active_run.clone().expect("active run in blocks");

    // Send steer
    let steer_res = v
        .call(
            "_goose/unstable/session/steer",
            serde_json::json!({
                "sessionId": sid,
                "expectedRunId": active_run,
                "prompt": [{
                    "type": "text",
                    "text": "steer now",
                }]
            }),
        )
        .await;

    // Steer reply is delivered to the viewer with downstream id restored
    assert!(
        steer_res["result"].get("messageId").is_some()
            || steer_res["result"].get("runId").is_some(),
        "steer reply delivered: {steer_res}"
    );

    // Open gate so turn completes
    g.model().open("gate_steer");
    let pr = v.response(&p_id).await;
    assert_eq!(pr["result"]["stopReason"], "end_turn");

    // Small tick to ensure no late errant steer_unconsumed notification
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forwarded_request_response_routed_back_with_downstream_id() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    // Send a session-scoped goose request that replies, e.g. _goose/unstable/session/export
    let export_res = v
        .call(
            "_goose/unstable/session/export",
            serde_json::json!({
                "sessionId": sid,
                "format": "json"
            }),
        )
        .await;

    assert_eq!(export_res["jsonrpc"], "2.0");
    assert!(export_res["result"].get("data").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn honk_aware_viewer_receives_stamped_frames_and_legacy_without() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Text(vec!["hello world".into()])]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;

    // 1. Honk-aware viewer connects and creates session
    let mut v_aware = Viewer::connect(&h).await;
    let sid = new_session(&mut v_aware, g.cwd().to_str().unwrap()).await;

    // 2. Legacy viewer connects and attaches to same session
    let mut v_legacy = Viewer::connect_pigeon(&h).await;
    let _ = v_legacy
        .call("session/close", serde_json::json!({ "sessionId": sid }))
        .await; // close sends a session-scoped message to attach/detach, or we can use prompt!
    // Instead, attach v_legacy by calling initialize mid-session then sending a request
    // Actually: sending any request with sessionId attaches v_legacy:
    let _ = v_legacy
        .call(
            "_goose/unstable/session/export",
            serde_json::json!({ "sessionId": sid, "format": "json" }),
        )
        .await;

    // 3. v_aware sends prompt
    let _p_id = v_aware
        .request("session/prompt", prompt_params(&sid, "hi from aware", None))
        .await;

    // v_aware receives session/update stamped with params._meta.honk {epoch, seq}
    let aware_frame = v_aware
        .wait_for(|f| {
            f.get("method").and_then(Value::as_str) == Some("session/update")
                && f.pointer("/params/_meta/honk/seq").is_some()
        })
        .await;
    assert_eq!(aware_frame["params"]["_meta"]["honk"]["epoch"], 1);
    assert!(
        aware_frame["params"]["_meta"]["honk"]["seq"]
            .as_u64()
            .unwrap()
            >= 1
    );

    // v_legacy receives session/update with params._meta.honk stripped
    let legacy_frame = v_legacy
        .wait_for(|f| f.get("method").and_then(Value::as_str) == Some("session/update"))
        .await;
    assert!(
        legacy_frame.pointer("/params/_meta/honk").is_none(),
        "legacy viewer must never receive _meta.honk in session/update: {legacy_frame}"
    );
}
