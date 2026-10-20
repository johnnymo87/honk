//! honkd reports why each session's last turn failed in `session/list`
//! `_meta.honk.error` (honk-9qf.15.11.3): from goose's
//! `_meta.goose.turnError`, from a JSON-RPC error reply, or from an
//! interrupted turn. A later turn clears it when it starts or ends clean.

use std::time::Duration;

use honk_testgoose::FakeGoose;
use serde_json::{Value, json};

mod common;
use common::{Honkd, Viewer, new_session, prompt_params};

const UNPATCHED: &[&str] = &["--allow-unpatched-upstream"];

fn turn_error_meta(kind: &str, message: &str) -> Value {
    json!({ "goose": { "turnError": { "kind": kind, "message": message } } })
}

async fn listed_error(v: &mut Viewer, sid: &str) -> Value {
    let list = v.call("session/list", json!({})).await;
    list.pointer("/result/sessions")
        .and_then(Value::as_array)
        .and_then(|sessions| {
            sessions
                .iter()
                .find(|e| e.get("sessionId").and_then(Value::as_str) == Some(sid))
        })
        .and_then(|e| e.pointer("/_meta/honk/error"))
        .cloned()
        .unwrap_or_else(|| panic!("session {sid} has no _meta.honk.error in {list}"))
}

async fn prompt(v: &mut Viewer, sid: &str, text: &str) -> Value {
    let id = v
        .request("session/prompt", prompt_params(sid, text, None))
        .await;
    v.response(&id).await
}

#[tokio::test(flavor = "multi_thread")]
async fn goose_turn_error_is_listed_until_a_clean_turn() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(&fake.url(), fake.secret_file(), UNPATCHED).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    assert_eq!(listed_error(&mut v, &sid).await, Value::Null);

    fake.set_prompt_result_meta(Some(turn_error_meta("network", "connection reset")));
    let reply = prompt(&mut v, &sid, "fails").await;
    assert_eq!(reply["result"]["stopReason"], "end_turn");

    let error = listed_error(&mut v, &sid).await;
    assert_eq!(error["kind"], "network");
    assert_eq!(error["message"], "connection reset");
    assert!(
        error["at"].as_u64().is_some(),
        "at is unix seconds: {error}"
    );

    fake.set_prompt_result_meta(None);
    prompt(&mut v, &sid, "works").await;
    assert_eq!(listed_error(&mut v, &sid).await, Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_error_survives_a_honkd_restart() {
    let fake = FakeGoose::start().await;
    let mut h = Honkd::spawn(&fake.url(), fake.secret_file(), UNPATCHED).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    fake.set_prompt_result_meta(Some(turn_error_meta("provider", "HTTP 500")));
    prompt(&mut v, &sid, "fails").await;
    drop(v);

    h.restart().await;
    let mut v = Viewer::connect_pigeon(&h).await;
    assert_eq!(listed_error(&mut v, &sid).await["kind"], "provider");
}

#[tokio::test(flavor = "multi_thread")]
async fn json_rpc_error_reply_is_listed_as_rpc() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(&fake.url(), fake.secret_file(), UNPATCHED).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    fake.set_prompt_error_reply(Some(json!({
        "code": -32603,
        "message": "Internal error",
        "data": "Error in agent response stream: boom"
    })));
    let reply = prompt(&mut v, &sid, "fails").await;
    assert!(reply.get("error").is_some(), "{reply}");

    let error = listed_error(&mut v, &sid).await;
    assert_eq!(error["kind"], "rpc");
    assert_eq!(
        error["message"],
        "Internal error: Error in agent response stream: boom"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_run_clears_the_error_before_it_ends() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(&fake.url(), fake.secret_file(), UNPATCHED).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    fake.set_prompt_result_meta(Some(turn_error_meta("network", "connection reset")));
    prompt(&mut v, &sid, "fails").await;
    assert_eq!(listed_error(&mut v, &sid).await["kind"], "network");

    // A long second turn that fails again: while it runs the session is not
    // errored; once it ends it is again.
    fake.script_prompt((0..100).map(|i| format!("chunk {i} ")).collect());
    let id = v
        .request("session/prompt", prompt_params(&sid, "again", None))
        .await;
    v.wait_for(|f| {
        f.pointer("/params/update/_meta/goose/activeRunId")
            .and_then(Value::as_str)
            .is_some()
    })
    .await;
    assert_eq!(listed_error(&mut v, &sid).await, Value::Null);
    v.response(&id).await;
    assert_eq!(listed_error(&mut v, &sid).await["kind"], "network");
}

/// The clean reply of turn N waits for an export before honkd answers it; a
/// parked turn N+1 that fails inline is answered first. The recorded state
/// must follow the turns' order, not the answers'.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_parked_turn_is_not_cleared_by_the_earlier_turns_late_reply() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn_with_env(
        &fake.url(),
        fake.secret_file(),
        UNPATCHED,
        &[("HONKD_TEST_EXPORT_DELAY_MS", "500")],
    )
    .await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    // A slow run announcement keeps the second prompt parked behind the first.
    fake.set_run_announcement_delay(Some(Duration::from_millis(500)));
    fake.script_prompt(vec!["first ".into(), "turn".into()]);
    let first = v
        .request(
            "session/prompt",
            prompt_params(&sid, "first", Some("crid_1")),
        )
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while fake.received_prompts(&sid).is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "first prompt never reached goose"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let second = v
        .request(
            "session/prompt",
            prompt_params(&sid, "second", Some("crid_2")),
        )
        .await;
    // Only the parked second prompt, sent after the first ends, sees this.
    fake.set_prompt_error_reply(Some(json!({
        "code": -32603,
        "message": "Internal error",
        "data": "second failed"
    })));

    let second_reply = v.response(&second).await;
    assert!(second_reply.get("error").is_some(), "{second_reply}");
    let first_reply = v.response(&first).await;
    assert_eq!(first_reply["result"]["stopReason"], "end_turn");

    assert_eq!(listed_error(&mut v, &sid).await["kind"], "rpc");
}
