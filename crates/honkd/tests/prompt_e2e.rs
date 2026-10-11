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
    assert_eq!(run.receipt_text(), g.turn_text(&sid).await);
    assert_eq!(run.receipt_text(), "hello");
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

    // Wait until run is active
    tokio::time::sleep(Duration::from_millis(200)).await;

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

    tokio::time::sleep(Duration::from_millis(200)).await;

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
