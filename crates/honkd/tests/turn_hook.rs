mod common;

use common::{Honkd, HookStub, Viewer, new_session, prompt_params};
use honk_testgoose::FakeGoose;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread")]
async fn normal_prompt_reports_started_and_ended() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let hook = HookStub::start().await;
    let token_dir = tempfile::tempdir().expect("token dir");
    let token_path = token_dir.path().join("hook_token");
    std::fs::write(&token_path, "secret-token-123\n").expect("write token");

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--turn-hook-url",
            &hook.url(),
            "--turn-hook-token-file",
            token_path.to_str().unwrap(),
            "--turn-hook-retry-secs",
            "5",
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    fake.script_prompt(vec!["p1".into()]);
    fake.set_export_data(
        &sid,
        json!({
            "sessionId": sid,
            "conversation": [
                {
                    "role": "user",
                    "metadata": { "userVisible": true },
                    "content": [{ "type": "text", "text": "test prompt 1" }]
                },
                {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "reply 1 text" }]
                }
            ]
        }),
    );

    let r = v
        .call(
            "session/prompt",
            prompt_params(&sid, "test prompt 1", Some("c1")),
        )
        .await;
    assert_eq!(r["result"]["stopReason"], "end_turn");
    let receipt_text = r["result"]["_meta"]["honk"]["text"]
        .as_str()
        .expect("receipt text");
    assert_eq!(receipt_text, "reply 1 text");

    let events = hook.wait_for_events(2, Duration::from_secs(5)).await;
    assert_eq!(
        events.len(),
        2,
        "expected exactly 2 hook events: {events:?}"
    );

    let (auth0, ev0) = &events[0];
    assert_eq!(auth0.as_deref(), Some("Bearer secret-token-123"));
    assert_eq!(ev0["v"], 1);
    assert_eq!(ev0["type"], "turn.started");
    let up_id = ev0["upstream_id"].as_str().expect("upstream_id");
    assert_eq!(ev0["event_id"], format!("{sid}:{up_id}:started"));
    assert_eq!(ev0["session_id"], sid);
    assert_eq!(ev0["client_req_ids"], json!(["c1"]));
    assert_eq!(ev0["prompt_text"], "test prompt 1");
    assert!(ev0["run_id"].is_string());
    assert!(ev0["at_ms"].as_u64().unwrap_or(0) > 0);

    let (auth1, ev1) = &events[1];
    assert_eq!(auth1.as_deref(), Some("Bearer secret-token-123"));
    assert_eq!(ev1["v"], 1);
    assert_eq!(ev1["type"], "turn.ended");
    assert_eq!(ev1["event_id"], format!("{sid}:{up_id}:ended"));
    assert_eq!(ev1["session_id"], sid);
    assert_eq!(ev1["upstream_id"], up_id);
    assert_eq!(ev1["client_req_ids"], json!(["c1"]));
    assert_eq!(ev1["prompt_text"], "test prompt 1");
    assert_eq!(ev1["stop_reason"], "end_turn");
    assert_eq!(ev1["final_text"], receipt_text);
    assert_eq!(ev1["final_text_truncated"], false);
    assert!(ev1.get("honk_reason").is_none());
    assert!(ev1.get("error").is_none());
    assert!(ev1["at_ms"].as_u64().unwrap_or(0) >= ev0["at_ms"].as_u64().unwrap_or(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_viewers_one_prompt_reports_once() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let hook = HookStub::start().await;

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--turn-hook-url",
            &hook.url(),
            "--turn-hook-retry-secs",
            "5",
        ],
    )
    .await;

    let mut v1 = Viewer::connect(&h).await;
    let sid = new_session(&mut v1, ".").await;

    let mut v2 = Viewer::connect(&h).await;
    v2.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    fake.script_prompt(vec!["p2".into()]);
    let r = v1
        .call(
            "session/prompt",
            prompt_params(&sid, "test multi viewer", Some("c2")),
        )
        .await;
    assert_eq!(r["result"]["stopReason"], "end_turn");

    let events = hook.wait_for_events(2, Duration::from_secs(5)).await;
    assert_eq!(events.len(), 2, "expected 2 events: {events:?}");
    assert_eq!(events[0].1["type"], "turn.started");
    assert_eq!(events[1].1["type"], "turn.ended");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        hook.received().await.len(),
        2,
        "two viewers must not cause duplicate hook events"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_announced_turn_reports_stop_reason_cancelled() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let hook = HookStub::start().await;

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--turn-hook-url",
            &hook.url(),
            "--turn-hook-retry-secs",
            "5",
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    fake.script_prompt(vec!["chunk".to_string(); 50]);

    let p_id = v
        .request(
            "session/prompt",
            prompt_params(&sid, "test cancel turn", Some("c3")),
        )
        .await;

    v.wait_for(|f| {
        f.pointer("/params/update/_meta/goose/activeRunId")
            .and_then(Value::as_str)
            .is_some()
    })
    .await;

    v.call("session/cancel", json!({ "sessionId": &sid })).await;

    let resp = v.response(&p_id).await;
    assert_eq!(resp["result"]["stopReason"], "cancelled");

    let events = hook.wait_for_events(2, Duration::from_secs(5)).await;
    assert_eq!(events.len(), 2, "expected 2 events: {events:?}");
    assert_eq!(events[0].1["type"], "turn.started");
    assert_eq!(events[1].1["type"], "turn.ended");
    assert_eq!(events[1].1["stop_reason"], "cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn goose_restart_mid_turn_reports_upstream_restart() {
    let mut fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let hook = HookStub::start().await;

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--turn-hook-url",
            &hook.url(),
            "--turn-hook-retry-secs",
            "5",
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    fake.script_prompt(vec!["chunk".to_string(); 50]);

    let p_id = v
        .request(
            "session/prompt",
            prompt_params(&sid, "test restart turn", Some("c4")),
        )
        .await;

    v.wait_for(|f| {
        f.pointer("/params/update/_meta/goose/activeRunId")
            .and_then(Value::as_str)
            .is_some()
    })
    .await;

    fake.restart().await;

    let resp = v.response_timeout(&p_id, Duration::from_secs(10)).await;
    assert_eq!(resp["result"]["stopReason"], "cancelled");
    assert_eq!(
        resp["result"]["_meta"]["honk"]["reason"],
        "upstream_restart"
    );

    let events = hook.wait_for_events(2, Duration::from_secs(5)).await;
    assert_eq!(events.len(), 2, "expected 2 events: {events:?}");
    assert_eq!(events[0].1["type"], "turn.started");
    assert_eq!(events[1].1["type"], "turn.ended");
    assert_eq!(events[1].1["stop_reason"], "cancelled");
    assert_eq!(events[1].1["honk_reason"], "upstream_restart");
}

#[tokio::test(flavor = "multi_thread")]
async fn goose_restart_unannounced_prompt_emits_started_then_ended() {
    let mut fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let hook = HookStub::start().await;

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--turn-hook-url",
            &hook.url(),
            "--turn-hook-retry-secs",
            "5",
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    // Drop run announcement so prompt is never announced while in-flight
    fake.set_drop_prompt_announcement(true);

    let p_id = v
        .request(
            "session/prompt",
            prompt_params(&sid, "test unannounced restart", Some("c-unannounced")),
        )
        .await;

    // Give honkd time to send prompt upstream to fake goose
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Restart goose while prompt is unannounced
    fake.restart().await;

    let resp = v.response_timeout(&p_id, Duration::from_secs(10)).await;
    assert_eq!(resp["result"]["stopReason"], "cancelled");
    assert_eq!(
        resp["result"]["_meta"]["honk"]["reason"],
        "upstream_restart"
    );

    let events = hook.wait_for_events(2, Duration::from_secs(5)).await;
    assert_eq!(events.len(), 2, "expected started then ended: {events:?}");
    assert_eq!(events[0].1["type"], "turn.started");
    assert_eq!(events[1].1["type"], "turn.ended");
    assert_eq!(events[1].1["honk_reason"], "upstream_restart");
    assert_eq!(events[0].1["upstream_id"], events[1].1["upstream_id"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn hook_receiver_down_prompts_answered_normally_and_promptly() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = listener.local_addr().unwrap();
    drop(listener);

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--turn-hook-url",
            &format!("http://{dead_addr}/hook"),
            "--turn-hook-retry-secs",
            "1",
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    fake.script_prompt(vec!["chunk".into()]);

    let start = Instant::now();
    let r = v
        .call(
            "session/prompt",
            prompt_params(&sid, "test dead hook", Some("c5")),
        )
        .await;
    let elapsed = start.elapsed();

    assert_eq!(r["result"]["stopReason"], "end_turn");
    assert!(
        elapsed < Duration::from_secs(3),
        "prompt must not be blocked by unreachable turn hook (took {elapsed:?})"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_turn_hook_url_sees_nothing() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let hook = HookStub::start().await;

    let h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;

    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;

    fake.script_prompt(vec!["chunk".into()]);

    let r = v
        .call(
            "session/prompt",
            prompt_params(&sid, "test no hook flag", Some("c6")),
        )
        .await;
    assert_eq!(r["result"]["stopReason"], "end_turn");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        hook.received().await.is_empty(),
        "receiver must see no events when --turn-hook-url is absent"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stub_goose_normal_prompt_reports_to_hook() {
    let Some(g) = honk_testgoose::StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([honk_testgoose::Turn::Text(vec![
        "stub reply part 1".into(),
        "stub reply part 2".into(),
    ])]);

    let hook = HookStub::start().await;
    let h = Honkd::spawn(
        &g.url(),
        g.secret_file(),
        &[
            "--turn-hook-url",
            &hook.url(),
            "--turn-hook-retry-secs",
            "5",
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    let r = v
        .call(
            "session/prompt",
            prompt_params(&sid, "hello stub goose", Some("c-stub")),
        )
        .await;
    assert_eq!(r["result"]["stopReason"], "end_turn");

    let events = hook.wait_for_events(2, Duration::from_secs(5)).await;
    assert_eq!(
        events.len(),
        2,
        "expected 2 events from StubGoose: {events:?}"
    );
    assert_eq!(events[0].1["type"], "turn.started");
    assert_eq!(events[0].1["client_req_ids"], json!(["c-stub"]));
    assert_eq!(events[1].1["type"], "turn.ended");
    assert_eq!(events[1].1["stop_reason"], "end_turn");
    assert_eq!(
        events[1].1["final_text"],
        r["result"]["_meta"]["honk"]["text"]
    );
}
