mod common;

use common::{Honkd, Viewer, new_session, prompt_params};
use honk_testgoose::{FakeGoose, StubGoose, TcpProxy, Turn};
use serde_json::{Value, json};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn kill9_honkd_mid_turn_resumes_with_delta_and_no_duplicate_text() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Gate(
        "g".into(),
        Box::new(Turn::Text(vec![
            "one ".into(),
            "two ".into(),
            "three".into(),
        ])),
    )]);
    let mut h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;
    v.request("session/prompt", prompt_params(&sid, "go", Some("r1")))
        .await;
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
    })
    .await;
    let cur = v.blocks.cursor.clone().unwrap();
    h.kill9();
    g.model().open("g"); // the turn finishes while honkd is down
    h.restart().await;
    let mut w = Viewer::connect(&h).await;
    w.blocks = v.blocks.clone(); // same client state, resuming
    w.blocks.resets = 0;
    w.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": [], "_meta": {"honk": {"after": cur}}}),
    )
    .await;
    assert_eq!(w.blocks.resets, 0, "patched resume keeps the epoch → delta");
    assert_eq!(w.blocks.agent_text(), g.turn_text(&sid).await);
    let r = w
        .call("session/prompt", prompt_params(&sid, "go", Some("r1")))
        .await; // retry
    assert_eq!(r["result"]["stopReason"], "end_turn");
    assert_eq!(r["result"]["_meta"]["honk"]["text"], "one two three");
    assert_eq!(
        g.model().turn_requests().len(),
        1,
        "the prompt reached the model once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ring_gap_mid_run_appends_then_rebuilds_once_at_turn_end() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let proxy = TcpProxy::start(std::net::SocketAddr::from(([127, 0, 0, 1], g.port())))
        .await
        .expect("start proxy");
    let proxy_url = format!("http://{}/acp", proxy.addr());

    let chunks: Vec<String> = (0..6000).map(|i| format!("c{i} ")).collect();
    g.model().push([
        Turn::Text(chunks),
        Turn::Gate(
            "end_gate".into(),
            Box::new(Turn::Text(vec!["final".into()])),
        ),
    ]);

    let h = Honkd::spawn(&proxy_url, g.secret_file(), &["--stream-dead-secs", "2"]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;
    assert_eq!(v.blocks.resets, 1, "initial load has 1 reset");

    let p_id = v
        .request("session/prompt", prompt_params(&sid, "go", None))
        .await;

    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    proxy.freeze();

    v.wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/gap"))
        .await;
    assert_eq!(v.blocks.resets, 1, "no reset while run is active");

    g.model().open("end_gate");

    v.wait_for(|f| {
        f.get("method").and_then(Value::as_str) == Some("_honk/replay_done")
            && f.pointer("/params/epoch").and_then(Value::as_u64) == Some(2)
    })
    .await;
    assert_eq!(v.blocks.resets, 2, "exactly one reset at turn end");
    assert_eq!(
        v.blocks.cursor.as_ref().map(|c| c.epoch),
        Some(2),
        "epoch is 2 after rebuild"
    );

    let resp = v.response_timeout(&p_id, Duration::from_secs(10)).await;
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    assert_eq!(v.blocks.agent_text(), g.turn_text(&sid).await);

    proxy.release().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_gap_rebuilds_immediately() {
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
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;
    assert_eq!(v.blocks.resets, 1, "initial load without cursor gets reset");

    fake.set_export_data(
        &sid,
        json!({
            "sessionId": sid,
            "conversation": [{
                "id": "msg_1",
                "role": "user",
                "created": 1,
                "content": [{"type": "text", "text": "hello"}]
            }, {
                "id": "msg_2",
                "role": "assistant",
                "created": 2,
                "content": [{"type": "text", "text": "world"}]
            }]
        }),
    );

    fake.gap_on_next_resume(&sid);

    v.wait_for(|f| {
        f.get("method").and_then(Value::as_str) == Some("_honk/replay_done")
            && f.pointer("/params/epoch").and_then(Value::as_u64) == Some(2)
    })
    .await;

    assert_eq!(
        v.blocks.resets, 2,
        "idle gap rebuilds immediately with second reset"
    );
    assert_eq!(v.blocks.cursor.as_ref().map(|c| c.epoch), Some(2));
    assert_eq!(v.blocks.agent_text(), "world");
}

#[tokio::test(flavor = "multi_thread")]
async fn half_open_stream_detected_by_byte_watchdog() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    fake.set_keepalive_interval(Duration::from_secs(60));
    let proxy = TcpProxy::start(fake.addr()).await.expect("start proxy");
    let proxy_url = format!("http://{}/acp", proxy.addr());

    let h = Honkd::spawn(&proxy_url, fake.secret_file(), &["--stream-dead-secs", "2"]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    fake.script_prompt(vec!["hello".into()]);
    let pid = v
        .request("session/prompt", prompt_params(&sid, "go", None))
        .await;

    proxy.freeze();

    let resp = v.response_timeout(&pid, Duration::from_secs(10)).await;
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    assert_eq!(v.blocks.agent_text(), "hello");
    let sid_conn_id = {
        let state_content =
            std::fs::read_to_string(h.state_dir.path().join("state.json")).expect("read state");
        let parsed: Value = serde_json::from_str(&state_content).expect("parse state");
        parsed["sessions"][&sid]["conn_id"]
            .as_str()
            .unwrap()
            .to_string()
    };

    assert!(
        !fake.deletes().contains(&sid_conn_id),
        "session connection must never be deleted on half-open reattach: deletes={:?}",
        fake.deletes()
    );

    proxy.release().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_never_posted_is_reported_not_delivered() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    let mut h = Honkd::spawn_with_env(
        &g.url(),
        g.secret_file(),
        &[],
        &[("HONKD_TEST_CRASH_BEFORE_POST", "1")],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    v.request(
        "session/prompt",
        prompt_params(&sid, "test_msg", Some("r1")),
    )
    .await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    h.restart_with_env(&[("HONKD_TEST_CRASH_BEFORE_POST", "0")])
        .await;

    let mut w = Viewer::connect(&h).await;
    let r = w
        .call(
            "session/prompt",
            prompt_params(&sid, "test_msg", Some("r1")),
        )
        .await;

    assert_eq!(r["result"]["stopReason"], "cancelled");
    assert_eq!(r["result"]["_meta"]["honk"]["reason"], "not_delivered");
    assert_eq!(
        g.model().turn_requests().len(),
        0,
        "the model saw no request"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn degraded_mode_reattach_rebuilds_from_export() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(false);
    fake.set_keepalive_interval(Duration::from_secs(60));

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--allow-unpatched-upstream", "--stream-dead-secs", "2"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;
    assert_eq!(v.blocks.resets, 1, "initial load has 1 reset");

    fake.set_export_data(
        &sid,
        json!({
            "sessionId": sid,
            "conversation": [{
                "id": "msg_1",
                "role": "user",
                "created": 1,
                "content": [{"type": "text", "text": "prompt text"}]
            }, {
                "id": "msg_2",
                "role": "assistant",
                "created": 2,
                "content": [{"type": "text", "text": "export text"}]
            }]
        }),
    );

    fake.lose_next(&sid, |_| true);
    fake.kick_stream(&sid);

    v.wait_for(|f| {
        f.get("method").and_then(Value::as_str) == Some("_honk/replay_done")
            && f.pointer("/params/epoch").and_then(Value::as_u64) == Some(2)
    })
    .await;

    assert_eq!(v.blocks.resets, 2);
    assert_eq!(v.blocks.agent_text(), "export text");
}
