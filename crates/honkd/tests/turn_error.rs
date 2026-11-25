//! honkd reports why each session's last turn failed in `session/list`
//! `_meta.honk.error` (honk-9qf.15.11.3): from goose's
//! `_meta.goose.turnError`, from a JSON-RPC error reply, or from an
//! interrupted turn. A later turn clears it when it starts or ends clean.

use std::time::Duration;

use honk_testgoose::{FakeGoose, StubGoose, Turn};
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
async fn a_clean_reply_clears_even_without_a_run_start() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(&fake.url(), fake.secret_file(), UNPATCHED).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    fake.set_prompt_result_meta(Some(turn_error_meta("network", "connection reset")));
    prompt(&mut v, &sid, "fails").await;
    assert_eq!(listed_error(&mut v, &sid).await["kind"], "network");

    // Scripted frames carry no activeRunId, so only the reply can clear.
    fake.set_prompt_result_meta(None);
    fake.script_frames(vec![json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": &sid,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "ok" }
            }
        }
    })]);
    let reply = prompt(&mut v, &sid, "works").await;
    assert_eq!(reply["result"]["stopReason"], "end_turn");
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
async fn turn_failed_error_reply_is_listed_with_its_kind_and_message() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(&fake.url(), fake.secret_file(), UNPATCHED).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    fake.set_prompt_error_reply(Some(json!({
        "code": -32603,
        "message": "Internal error",
        "data": {
            "reason": "turn_failed",
            "kind": "provider",
            "message": "overloaded"
        }
    })));
    let reply = prompt(&mut v, &sid, "fails").await;
    assert!(reply.get("error").is_some(), "{reply}");

    let error = listed_error(&mut v, &sid).await;
    assert_eq!(error["kind"], "provider");
    assert_eq!(error["message"], "overloaded");
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_failed_error_reply_stock_goose_is_listed_as_rpc() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(&fake.url(), fake.secret_file(), UNPATCHED).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    fake.set_prompt_error_reply(Some(json!({
        "code": -32603,
        "message": "Turn failed",
        "data": {
            "reason": "turn_failed"
        }
    })));
    let reply = prompt(&mut v, &sid, "fails").await;
    assert!(reply.get("error").is_some(), "{reply}");

    let error = listed_error(&mut v, &sid).await;
    assert_eq!(error["kind"], "rpc");
    assert_eq!(error["message"], "Turn failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_failed_real_sequence_streams_chunks_and_is_listed_as_error() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(&fake.url(), fake.secret_file(), UNPATCHED).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    // Real goose 1.60 failure sequence: activeRunId Some, chunks, activeRunId null, then error reply
    fake.script_prompt(vec!["partial stream ".into(), "before failure".into()]);
    fake.set_prompt_turn_failed(Some("provider"), Some("rate limit exceeded"));

    let id = v
        .request("session/prompt", prompt_params(&sid, "fails", None))
        .await;

    // During the turn, activeRunId was announced
    v.wait_for(|f| {
        f.pointer("/params/update/_meta/goose/activeRunId")
            .and_then(Value::as_str)
            .is_some()
    })
    .await;

    let reply = v.response(&id).await;
    assert!(reply.get("error").is_some(), "{reply}");

    let error = listed_error(&mut v, &sid).await;
    assert_eq!(error["kind"], "provider");
    assert_eq!(error["message"], "rate limit exceeded");

    // Clean turn clears it
    fake.set_prompt_end_error(None);
    fake.script_prompt(vec!["all good".into()]);
    let clean_reply = prompt(&mut v, &sid, "works").await;
    assert_eq!(clean_reply["result"]["stopReason"], "end_turn");
    assert_eq!(listed_error(&mut v, &sid).await, Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_failed_stock_goose_real_sequence_is_listed_as_rpc() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(&fake.url(), fake.secret_file(), UNPATCHED).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    fake.script_prompt(vec!["streaming...".into()]);
    fake.set_prompt_end_error(Some(json!({
        "code": -32603,
        "message": "Stock turn failed",
        "data": { "reason": "turn_failed" }
    })));

    let id = v
        .request("session/prompt", prompt_params(&sid, "fails", None))
        .await;
    v.wait_for(|f| {
        f.pointer("/params/update/_meta/goose/activeRunId")
            .and_then(Value::as_str)
            .is_some()
    })
    .await;

    let reply = v.response(&id).await;
    assert!(reply.get("error").is_some(), "{reply}");

    let error = listed_error(&mut v, &sid).await;
    assert_eq!(error["kind"], "rpc");
    assert_eq!(error["message"], "Stock turn failed");
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

fn turn_failed_warn_logs(h: &Honkd, sid: &str) -> Vec<Value> {
    h.stderr_text()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| {
            entry.get("level").and_then(Value::as_str) == Some("WARN")
                && entry.pointer("/fields/sid").and_then(Value::as_str) == Some(sid)
                && entry.pointer("/fields/raw_error").is_some()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_failed_logs_warn_with_structured_fields_for_each_failure_kind() {
    let fake = FakeGoose::start().await;
    let h = Honkd::spawn(&fake.url(), fake.secret_file(), UNPATCHED).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, "/tmp").await;

    // 1. JSON-RPC turn_failed error reply
    fake.set_prompt_error_reply(Some(json!({
        "code": -32603,
        "message": "Internal error",
        "data": {
            "reason": "turn_failed",
            "kind": "provider",
            "message": "overloaded"
        }
    })));
    let reply = prompt(&mut v, &sid, "turn_failed_case").await;
    assert!(reply.get("error").is_some(), "{reply}");

    // 2. _meta.goose.turnError on a Result reply
    fake.set_prompt_error_reply(None);
    fake.set_prompt_result_meta(Some(turn_error_meta("network", "connection reset")));
    let reply = prompt(&mut v, &sid, "turn_error_meta_case").await;
    assert_eq!(reply["result"]["stopReason"], "end_turn");

    // 3. HTTP 500 on prompt POST (on_prompt_send_failed -> turn is None, upstream_id still logged)
    fake.set_prompt_result_meta(None);
    fake.set_prompt_status(Some(axum::http::StatusCode::INTERNAL_SERVER_ERROR));
    let reply = prompt(&mut v, &sid, "http_500_case").await;
    assert!(reply.get("error").is_some(), "{reply}");

    // 4. Huge raw error object is capped on a char boundary
    fake.set_prompt_status(None);
    fake.set_prompt_error_reply(Some(json!({
        "code": -32603,
        "message": "Internal error",
        "data": "x".repeat(5000)
    })));
    let reply = prompt(&mut v, &sid, "huge_err_case").await;
    assert!(reply.get("error").is_some(), "{reply}");

    // Give the stderr reader thread a brief moment to flush all lines.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let logs = loop {
        let found = turn_failed_warn_logs(&h, &sid);
        if found.len() >= 4 || tokio::time::Instant::now() >= deadline {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        logs.len(),
        4,
        "expected 4 turn-failed WARN logs, got {}:\n{}",
        logs.len(),
        h.stderr_text()
    );

    // Check 1: turn_failed
    let f1 = &logs[0]["fields"];
    assert_eq!(f1["sid"], sid);
    assert_eq!(f1["kind"], "provider");
    assert_eq!(f1["error_message"], "overloaded");
    assert_eq!(f1["method"], "session/prompt");
    assert_ne!(f1["upstream_id"].as_str().unwrap_or("-"), "-");
    let raw1: Value = serde_json::from_str(f1["raw_error"].as_str().unwrap()).unwrap();
    assert_eq!(raw1["data"]["reason"], "turn_failed");
    assert_eq!(raw1["data"]["kind"], "provider");

    // Check 2: _meta.goose.turnError
    let f2 = &logs[1]["fields"];
    assert_eq!(f2["kind"], "network");
    assert_eq!(f2["error_message"], "connection reset");
    assert_eq!(f2["method"], "session/prompt");
    assert_ne!(f2["upstream_id"].as_str().unwrap_or("-"), "-");
    let raw2: Value = serde_json::from_str(f2["raw_error"].as_str().unwrap()).unwrap();
    assert_eq!(
        raw2,
        json!({ "kind": "network", "message": "connection reset" })
    );

    // Check 3: on_prompt_send_failed (HTTP 500)
    let f3 = &logs[2]["fields"];
    assert_eq!(f3["kind"], "rpc");
    assert!(
        f3["error_message"]
            .as_str()
            .unwrap()
            .contains("Prompt POST failed"),
        "expected Prompt POST failed: {f3}"
    );
    assert_eq!(f3["method"], "session/prompt");
    assert_ne!(
        f3["upstream_id"].as_str().unwrap_or("-"),
        "-",
        "on_prompt_send_failed must still log upstream_id: {f3}"
    );
    assert!(
        f3["raw_error"]
            .as_str()
            .unwrap()
            .contains("Prompt POST failed"),
        "{f3}"
    );

    // Check 4: capped huge raw error
    let f4 = &logs[3]["fields"];
    assert_eq!(f4["kind"], "rpc");
    let raw4 = f4["raw_error"].as_str().unwrap();
    assert!(raw4.len() <= honkd::turn::TURN_ERROR_MAX_BYTES);
    assert!(raw4.ends_with('…'), "expected capped raw_error: {raw4}");
}

#[tokio::test(flavor = "multi_thread")]
async fn back_to_back_queued_prompts_against_stub_goose_do_not_record_error() {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&goose.url(), goose.secret_file(), &[]).await;
    let mut v = Viewer::connect_pigeon(&h).await;
    let sid = new_session(&mut v, goose.cwd().to_str().unwrap()).await;

    // Part 1: 3 prompts fired back-to-back during the announce window (parked & drained).
    goose.model().push([
        Turn::Text(vec!["reply-1".into()]),
        Turn::Text(vec!["reply-2".into()]),
        Turn::Text(vec!["reply-3".into()]),
    ]);
    let id1 = v
        .request(
            "session/prompt",
            prompt_params(&sid, "queued-1", Some("q1")),
        )
        .await;
    let id2 = v
        .request(
            "session/prompt",
            prompt_params(&sid, "queued-2", Some("q2")),
        )
        .await;
    let id3 = v
        .request(
            "session/prompt",
            prompt_params(&sid, "queued-3", Some("q3")),
        )
        .await;

    let r1 = v.response(&id1).await;
    assert_eq!(r1["result"]["stopReason"], "end_turn", "r1={r1}");
    let r2 = v.response(&id2).await;
    // Depending on how fast goose announces activeRunId, r2/r3 may either be parked
    // and run cleanly, or receive a busy rejection. Neither may set _meta.honk.error.
    if r2.get("error").is_some() {
        assert!(
            r2.pointer("/error/data")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("already has active run")),
            "unexpected non-busy error on r2: {r2}"
        );
    } else {
        assert_eq!(r2["result"]["stopReason"], "end_turn", "r2={r2}");
    }
    let r3 = v.response(&id3).await;
    if r3.get("error").is_some() {
        assert!(
            r3.pointer("/error/data")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("already has active run")),
            "unexpected non-busy error on r3: {r3}"
        );
    } else {
        assert_eq!(r3["result"]["stopReason"], "end_turn", "r3={r3}");
    }

    assert_eq!(listed_error(&mut v, &sid).await, Value::Null);
    assert!(
        turn_failed_warn_logs(&h, &sid).is_empty(),
        "unexpected turn-failed WARN logs:\n{}",
        h.stderr_text()
    );

    // Part 2: 3 sequential back-to-back prompts, each sent immediately on the previous reply.
    goose.model().push([
        Turn::Text(vec!["seq-1".into()]),
        Turn::Text(vec!["seq-2".into()]),
        Turn::Text(vec!["seq-3".into()]),
    ]);
    for i in 1..=3 {
        let reply = prompt(&mut v, &sid, &format!("seq-prompt-{i}")).await;
        assert_eq!(
            reply["result"]["stopReason"], "end_turn",
            "seq {i} reply={reply}"
        );
        assert_eq!(listed_error(&mut v, &sid).await, Value::Null);
    }
    assert!(
        turn_failed_warn_logs(&h, &sid).is_empty(),
        "unexpected turn-failed WARN logs:\n{}",
        h.stderr_text()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_busy_prompt_and_steer_race_against_stub_goose_do_not_record_error() {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return;
    };
    let h = Honkd::spawn(&goose.url(), goose.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, goose.cwd().to_str().unwrap()).await;

    goose.model().push([
        Turn::Gate("g1".into(), Box::new(Turn::Text(vec!["gated-1".into()]))),
        Turn::Text(vec!["steer-or-after-2".into()]),
        Turn::Text(vec!["after-race-3".into()]),
        Turn::Text(vec!["after-race-4".into()]),
    ]);

    let id1 = v
        .request("session/prompt", prompt_params(&sid, "p1", Some("r1")))
        .await;
    let run_frame = v
        .wait_for(|f| {
            f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
        })
        .await;
    let run_id = run_frame
        .pointer("/params/update/_meta/goose/activeRunId")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    let run_seq = run_frame
        .pointer("/params/_meta/honk/seq")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // Overlapping prompt while run is active -> busy rejection, must NOT record Failed.
    let busy_id = v
        .request(
            "session/prompt",
            prompt_params(&sid, "overlap", Some("r_busy")),
        )
        .await;
    let busy_reply = v.response(&busy_id).await;
    assert_eq!(busy_reply["error"]["code"], -32602, "{busy_reply}");
    assert_eq!(listed_error(&mut v, &sid).await, Value::Null);

    // Open gate and immediately race a steer against turn teardown.
    goose.model().open("g1");
    let steer_id = v
        .request(
            "_goose/unstable/session/steer",
            json!({
                "sessionId": sid,
                "expectedRunId": run_id,
                "prompt": [{ "type": "text", "text": "racing steer" }]
            }),
        )
        .await;

    // Wait for activeRunId: null AFTER this run started (teardown), then immediately fire
    // Prompt 2 & Prompt 3 before awaiting Prompt 1's reply (so they park behind Prompt 1's reply).
    v.wait_for(|f| {
        f.pointer("/params/update/_meta/goose/activeRunId")
            .is_some_and(Value::is_null)
            && f.pointer("/params/_meta/honk/seq")
                .and_then(Value::as_u64)
                .is_some_and(|s| s > run_seq)
    })
    .await;
    let id2 = v
        .request("session/prompt", prompt_params(&sid, "p2", Some("r2")))
        .await;
    let id3 = v
        .request("session/prompt", prompt_params(&sid, "p3", Some("r3")))
        .await;

    let _steer_reply = v.response(&steer_id).await;
    let r1 = v.response(&id1).await;
    assert_eq!(r1["result"]["stopReason"], "end_turn", "r1={r1}");
    let r2 = v.response(&id2).await;
    assert_eq!(r2["result"]["stopReason"], "end_turn", "r2={r2}");
    let r3 = v.response(&id3).await;
    if r3.get("error").is_some() {
        assert!(
            r3.pointer("/error/data")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("already has active run")),
            "unexpected non-busy error on r3: {r3}"
        );
    } else {
        assert_eq!(r3["result"]["stopReason"], "end_turn", "r3={r3}");
    }

    assert_eq!(listed_error(&mut v, &sid).await, Value::Null);
    assert!(
        turn_failed_warn_logs(&h, &sid).is_empty(),
        "unexpected turn-failed WARN logs:\n{}",
        h.stderr_text()
    );
}
