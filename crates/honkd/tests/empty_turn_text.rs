use common::{Honkd, Viewer, new_session};
use honk_testgoose::{StubGoose, Turn};
use serde_json::{Value, json};
use std::time::Duration;

mod common;

fn params(sid: &str, text: &str, crid: &str, resume_only: bool) -> Value {
    let mut honk = json!({ "client_req_id": crid });
    if resume_only {
        honk["resume_only"] = json!(true);
    }
    json!({
        "sessionId": sid,
        "prompt": [{ "type": "text", "text": text }],
        "_meta": { "honk": honk },
    })
}

async fn wait_turn_request(g: &StubGoose, n: usize) {
    let start = std::time::Instant::now();
    while g.model().turn_requests().len() < n {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "turn request never reached model"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// When goose's turn produces no assistant text (the model returned an empty completion),
/// honkd's reply sends `_meta.honk.text: ""` (distinguishable from missing text / export failure).
#[tokio::test(flavor = "multi_thread")]
async fn empty_model_turn_reply_has_empty_honk_text_without_restart() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    // A tool call, then an empty completion. goose does NOT retry an empty completion
    // that follows a successful tool response (agent.rs ~3521
    // `!ends_with_successful_tool_response`), so the turn ends end_turn with no assistant
    // text -- the shape of live sandbox session 20261013_68.
    g.model().push([
        Turn::ToolCall {
            tool: "shell".into(),
            arguments: json!({ "command": "echo ZNJ13" }),
        },
        Turn::Text(vec![]),
    ]);
    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;
    let r = v
        .call(
            "session/prompt",
            params(&sid, "empty turn", "crid-empty", false),
        )
        .await;
    assert_eq!(r["result"]["stopReason"], "end_turn");
    assert_eq!(
        r.pointer("/result/_meta/honk/text").and_then(Value::as_str),
        Some(""),
        "empty turn -> _meta.honk.text is empty string: {r}"
    );
}

/// The pigeon "recovery case 4" shape: client prompts with crid, disconnects; honkd is
/// killed mid-turn; the turn finishes while honkd is down; honkd restarts; a fresh client
/// re-asks with resume_only. When the model DID produce text, the reply carries it.
#[tokio::test(flavor = "multi_thread")]
async fn restart_mid_turn_turn_ends_while_down_resume_only_has_text() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "g".into(),
        Box::new(Turn::Text(vec!["after ".into(), "restart".into()])),
    )]);
    let mut h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, g.cwd().to_str().unwrap()).await;
    let _req = v1
        .request(
            "session/prompt",
            params(&sid, "restart turn", "crid-r", false),
        )
        .await;
    wait_turn_request(&g, 1).await;
    drop(v1);
    h.kill9();
    g.model().open("g");
    tokio::time::sleep(Duration::from_millis(500)).await; // turn ends while honkd is down
    h.restart().await;
    let mut v2 = Viewer::connect(&h).await;
    let r = v2
        .call(
            "session/prompt",
            params(&sid, "restart turn", "crid-r", true),
        )
        .await;
    assert_eq!(r["result"]["stopReason"], "end_turn");
    assert_eq!(r["result"]["_meta"]["honk"]["text"], "after restart");
}

/// Same, but honkd is back BEFORE the turn ends.
#[tokio::test(flavor = "multi_thread")]
async fn restart_mid_turn_back_before_turn_ends_resume_only_has_text() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "g".into(),
        Box::new(Turn::Text(vec!["back ".into(), "early".into()])),
    )]);
    let mut h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, g.cwd().to_str().unwrap()).await;
    let _req = v1
        .request(
            "session/prompt",
            params(&sid, "early turn", "crid-e", false),
        )
        .await;
    wait_turn_request(&g, 1).await;
    drop(v1);
    h.kill9();
    h.restart().await;
    let mut v2 = Viewer::connect(&h).await;
    let req = v2
        .request("session/prompt", params(&sid, "early turn", "crid-e", true))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    g.model().open("g");
    let r = v2.response(&req).await;
    assert_eq!(r["result"]["stopReason"], "end_turn");
    assert_eq!(r["result"]["_meta"]["honk"]["text"], "back early");
}

/// Gap test: honkd is killed after receiving goose's prompt reply and writing to the
/// sidecar, but BEFORE the text export completes. On restart, honkd's boot prune
/// resolves the InFlight ledger entry from the raw sidecar (which has no text), but
/// honkd computes the text from the transcript so a collecting client gets the text.
#[tokio::test(flavor = "multi_thread")]
async fn restart_between_reply_and_export_recovers_text() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model()
        .push([Turn::Text(vec!["text before ".into(), "export".into()])]);

    // Set delay hook so export task in honkd pauses, passed via spawn_with_env per-process env
    let mut h = Honkd::spawn_with_env(
        &g.url(),
        g.secret_file(),
        &[],
        &[("HONKD_TEST_EXPORT_DELAY_MS", "5000")],
    )
    .await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, g.cwd().to_str().unwrap()).await;
    let _req = v1
        .request(
            "session/prompt",
            params(&sid, "gap prompt", "crid-gap", false),
        )
        .await;

    // Wait until sidecar has the reply recorded
    let sidecar_path = h
        .state_dir
        .path()
        .join("sessions")
        .join(format!("{sid}.replies.jsonl"));
    let start = std::time::Instant::now();
    loop {
        if sidecar_path.exists()
            && let Ok(content) = std::fs::read_to_string(&sidecar_path)
            && !content.trim().is_empty()
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for sidecar reply to be written"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    drop(v1);
    h.kill9();

    // Restart without delay
    h.restart().await;

    let mut v2 = Viewer::connect(&h).await;
    let r = v2
        .call(
            "session/prompt",
            params(&sid, "gap prompt", "crid-gap", true),
        )
        .await;

    assert_eq!(r["result"]["stopReason"], "end_turn");
    assert_eq!(
        r["result"]["_meta"]["honk"]["text"], "text before export",
        "recovered reply must have text: {r}"
    );
}

/// Two prompts: second prompt arrives and is parked/dispatched while first prompt's reply
/// arrives. Honkd is killed before first prompt's export completes. On restart, crid1 must
/// get crid1's text (or absent), NEVER crid2's text!
#[tokio::test(flavor = "multi_thread")]
async fn two_prompts_kill_between_reply_and_export_never_gets_second_prompt_text() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model()
        .push([Turn::Text(vec!["first ".into(), "reply".into()])]);
    g.model()
        .push([Turn::Text(vec!["second ".into(), "reply".into()])]);

    let mut h = Honkd::spawn_with_env(
        &g.url(),
        g.secret_file(),
        &[],
        &[("HONKD_TEST_EXPORT_DELAY_MS", "5000")],
    )
    .await;
    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, g.cwd().to_str().unwrap()).await;

    // Send Prompt 1
    let _req1 = v1
        .request(
            "session/prompt",
            params(&sid, "turn 1 prompt", "crid-1", false),
        )
        .await;
    wait_turn_request(&g, 1).await;

    // Send Prompt 2 while Prompt 1 is finalizing/in-flight (gets parked)
    let _req2 = v1
        .request(
            "session/prompt",
            params(&sid, "turn 2 prompt", "crid-2", false),
        )
        .await;

    // Wait until sidecar has the reply for Prompt 1 recorded
    let sidecar_path = h
        .state_dir
        .path()
        .join("sessions")
        .join(format!("{sid}.replies.jsonl"));
    let start = std::time::Instant::now();
    loop {
        if sidecar_path.exists()
            && let Ok(content) = std::fs::read_to_string(&sidecar_path)
            && !content.trim().is_empty()
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for sidecar reply to be written"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    drop(v1);
    h.kill9();

    // Restart without the delay
    h.restart().await;

    let mut v2 = Viewer::connect(&h).await;
    let r1 = v2
        .call(
            "session/prompt",
            params(&sid, "turn 1 prompt", "crid-1", true),
        )
        .await;

    assert_eq!(r1["result"]["stopReason"], "end_turn");
    let text1 = r1
        .pointer("/result/_meta/honk/text")
        .and_then(Value::as_str);
    assert_eq!(
        text1,
        Some("first reply"),
        "crid-1 must have crid-1's text: {r1}"
    );
    assert_ne!(
        text1,
        Some("second reply"),
        "crid-1 must NEVER receive crid-2's text"
    );
}
