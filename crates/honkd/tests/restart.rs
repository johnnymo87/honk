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
    assert_eq!(v.blocks.resets, 1, "initial load has 1 reset");

    fake.set_export_data(
        &sid,
        json!({
            "sessionId": sid,
            "conversation": [{
                "id": "msg_1",
                "role": "user",
                "created": 1,
                "content": [{"type": "text", "text": "go"}]
            }, {
                "id": "msg_2",
                "role": "assistant",
                "created": 2,
                "content": [{"type": "text", "text": "turn output text"}]
            }]
        }),
    );

    // Tell fake to return Gap on next resume
    fake.gap_on_next_resume(&sid);

    fake.script_prompt(vec!["chunk1".into(), "chunk2".into()]);
    let p_id = v
        .request("session/prompt", prompt_params(&sid, "go", None))
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

    // Kick the stream mid-run -> honkd reattaches with takeover, fake returns Gap
    fake.kick_stream(&sid);

    // Honk-aware viewer gets _honk/gap while run is active and NO reset yet
    v.wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/gap"))
        .await;
    assert_eq!(v.blocks.resets, 1, "no reset while run is active");

    // Turn finishes, activeRunId: null processed -> triggers rebuild at turn end
    // Viewer receives _honk/replay_done for epoch 2
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
    assert_eq!(v.blocks.agent_text(), "turn output text");
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

    h.wait_for_exit(Duration::from_secs(5)).await;

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
async fn prompt_with_repeated_text_after_completed_turn_reported_not_delivered_on_crash() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Text(vec!["first response".into()])]);

    let mut h = Honkd::spawn_with_env(&g.url(), g.secret_file(), &[], &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Turn 1 completes with prompt text "go"
    let r1 = v
        .call("session/prompt", prompt_params(&sid, "go", Some("r1")))
        .await;
    assert_eq!(r1["result"]["stopReason"], "end_turn");

    // Restart honkd with CRASH_BEFORE_POST=1
    h.restart_with_env(&[("HONKD_TEST_CRASH_BEFORE_POST", "1")])
        .await;
    let mut v2 = Viewer::connect(&h).await;

    // Turn 2 sends prompt "go" again with client_req_id "r2". Crashes before post!
    v2.request("session/prompt", prompt_params(&sid, "go", Some("r2")))
        .await;

    h.wait_for_exit(Duration::from_secs(5)).await;

    // Restart without crash hook
    h.restart_with_env(&[("HONKD_TEST_CRASH_BEFORE_POST", "0")])
        .await;

    let mut w = Viewer::connect(&h).await;
    let r2 = w
        .call("session/prompt", prompt_params(&sid, "go", Some("r2")))
        .await;

    assert_eq!(r2["result"]["stopReason"], "cancelled");
    assert_eq!(r2["result"]["_meta"]["honk"]["reason"], "not_delivered");
    assert_eq!(g.model().turn_requests().len(), 1, "model only saw turn 1");
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

#[tokio::test(flavor = "multi_thread")]
async fn rebuild_with_log_larger_than_viewer_queue_does_not_evict_and_dedupes_old_epoch() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    fake.set_keepalive_interval(Duration::from_secs(60));

    // viewer_queue is only 8!
    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &["--viewer-queue", "8", "--stream-dead-secs", "2"],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Set export with 50 messages (> 8 viewer_queue)
    let mut conversation = Vec::new();
    for i in 0..50 {
        conversation.push(json!({
            "id": format!("msg_{i}"),
            "role": if i % 2 == 0 { "user" } else { "assistant" },
            "created": i,
            "content": [{"type": "text", "text": format!("text_{i}")}]
        }));
    }
    fake.set_export_data(
        &sid,
        json!({
            "sessionId": sid,
            "conversation": conversation
        }),
    );

    // Trigger gap resume while idle -> triggers rebuild
    fake.gap_on_next_resume(&sid);

    // Wait for replay_done with epoch 2
    v.wait_for(|f| {
        f.get("method").and_then(Value::as_str) == Some("_honk/replay_done")
            && f.pointer("/params/epoch").and_then(Value::as_u64) == Some(2)
    })
    .await;

    // Viewer MUST NOT be evicted (closed must be None)
    assert!(
        v.closed.is_none(),
        "viewer was evicted with queue overflow: {:?}",
        v.closed
    );
    assert_eq!(v.blocks.resets, 2);
    assert_eq!(v.blocks.cursor.as_ref().map(|c| c.epoch), Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_retry_while_reply_finalizing_parks_and_answers_cached_done() {
    let Some(g) = StubGoose::start_or_skip().await else {
        return;
    };
    g.model().push([Turn::Text(vec!["hello world".into()])]);

    let h = Honkd::spawn(&g.url(), g.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Send prompt with client_req_id "r1"
    let p1 = v
        .call("session/prompt", prompt_params(&sid, "test", Some("r1")))
        .await;
    assert_eq!(p1["result"]["stopReason"], "end_turn");
    assert_eq!(p1["result"]["_meta"]["honk"]["text"], "hello world");

    // Retry with same client_req_id "r1" -> answered from ledger Done immediately
    let mut w = Viewer::connect(&h).await;
    let p2 = w
        .call("session/prompt", prompt_params(&sid, "test", Some("r1")))
        .await;
    assert_eq!(p2["result"]["stopReason"], "end_turn");
    assert_eq!(p2["result"]["_meta"]["honk"]["text"], "hello world");
    assert_eq!(
        g.model().turn_requests().len(),
        1,
        "only 1 request reached model"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reply_lost_in_gap_synthesizes_end_turn_after_rebuild() {
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

    fake.set_export_data(
        &sid,
        json!({
            "sessionId": sid,
            "conversation": [{
                "id": "msg_1",
                "role": "user",
                "created": 1,
                "content": [{"type": "text", "text": "test"}]
            }, {
                "id": "msg_2",
                "role": "assistant",
                "created": 2,
                "content": [{"type": "text", "text": "resp"}]
            }]
        }),
    );

    fake.gap_on_next_resume(&sid);

    fake.script_prompt(vec!["resp".into()]);
    let pid = v
        .request("session/prompt", prompt_params(&sid, "test", None))
        .await;

    fake.kick_stream(&sid);

    let resp = v.response_timeout(&pid, Duration::from_secs(10)).await;
    assert_eq!(resp["result"]["stopReason"], "end_turn");
}

#[tokio::test(flavor = "multi_thread")]
async fn inconsistent_inflight_client_req_id_returns_32000() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let mut h = Honkd::spawn(&fake.url(), fake.secret_file(), &[]).await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    // Mutate state.json to add an InFlight ledger entry for this session with no turn inflight
    {
        let state_path = h.state_dir.path().join("state.json");
        let content = std::fs::read_to_string(&state_path).expect("read state");
        let mut parsed: Value = serde_json::from_str(&content).expect("parse state");
        parsed["ledger"]["lost_crid"] = json!({
            "state": "in_flight",
            "session": sid,
            "upstream_id": "hk_lost",
        });
        std::fs::write(&state_path, serde_json::to_string_pretty(&parsed).unwrap())
            .expect("write state");
    }

    h.restart().await;

    let mut w = Viewer::connect(&h).await;
    let resp = w
        .call(
            "session/prompt",
            prompt_params(&sid, "test", Some("lost_crid")),
        )
        .await;

    assert_eq!(resp["error"]["code"], -32000);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("honkd lost track of this client_req_id; not resending")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_ended_in_gap_synthesizes_end_turn_after_rebuild_without_hang() {
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

    // The turn committed on goose with assistant text, but the reply and activeRunId:null were in the gap
    fake.set_export_data(
        &sid,
        json!({
            "sessionId": sid,
            "conversation": [{
                "id": "msg_1",
                "role": "user",
                "created": 1,
                "content": [{"type": "text", "text": "gap_prompt"}]
            }, {
                "id": "msg_2",
                "role": "assistant",
                "created": 2,
                "content": [{"type": "text", "text": "assistant_gap_reply"}]
            }]
        }),
    );

    fake.gap_on_next_resume(&sid);
    fake.set_drop_prompt_reply(true);

    let pid = v
        .request("session/prompt", prompt_params(&sid, "gap_prompt", None))
        .await;

    // Wait until run starts so prompt is in flight
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // Clear ring to simulate that frames including the prompt reply were evicted from the ring
    fake.clear_ring(&sid);
    fake.kick_stream(&sid);

    // Reattach detects turn finished in gap from export, rebuilds immediately, synthesizes end_turn without hang
    let resp = v.response_timeout(&pid, Duration::from_secs(10)).await;
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    assert_eq!(
        resp.pointer("/result/_meta/honk/synthesized"),
        Some(&json!(true))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_probe_overtakes_reply_real_reply_wins_no_synthesis() {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);
    fake.set_keepalive_interval(Duration::from_secs(60));

    let h = Honkd::spawn(
        &fake.url(),
        fake.secret_file(),
        &[
            "--stream-dead-secs",
            "2",
            "--gap-synthesis-grace-ms",
            "1000",
        ],
    )
    .await;
    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, ".").await;
    v.call(
        "session/load",
        json!({"sessionId": sid, "cwd": ".", "mcpServers": []}),
    )
    .await;

    fake.set_export_data(
        &sid,
        json!({
            "conversation": [{
                "id": "msg_1",
                "role": "user",
                "created": 1,
                "content": [{"type": "text", "text": "overtake_prompt"}]
            }, {
                "id": "msg_2",
                "role": "assistant",
                "created": 2,
                "content": [{"type": "text", "text": "assistant_real_reply"}]
            }]
        }),
    );

    fake.gap_on_next_resume(&sid);
    fake.set_probe_overtakes_reply(true);

    let pid = v
        .request(
            "session/prompt",
            prompt_params(&sid, "overtake_prompt", None),
        )
        .await;

    // Wait until run starts so prompt is in flight
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("session_info_update")
            && f.pointer("/params/update/_meta/goose/activeRunId")
                .and_then(Value::as_str)
                .is_some()
    })
    .await;

    // Kick stream while turn is in flight -> gap on resume -> steer probe issued
    fake.kick_stream(&sid);

    // The idle probe overtakes the reply. Honkd rebuilds from export and waits in grace window.
    // The real reply arrives during the grace window and wins.
    let resp = v.response_timeout(&pid, Duration::from_secs(10)).await;
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    assert_eq!(
        resp.pointer("/result/_meta/honk/synthesized"),
        None,
        "real reply must be used when arriving during grace window (no synthesis)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_turn_mid_run_gap_never_synthesizes_and_uses_real_reply() {
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
    assert_eq!(v.blocks.resets, 1, "initial load has 1 reset");

    fake.set_export_data(
        &sid,
        json!({
            "sessionId": sid,
            "conversation": [{
                "id": "msg_1",
                "role": "user",
                "created": 1,
                "content": [{"type": "text", "text": "run_tool"}]
            }, {
                "id": "msg_2",
                "role": "assistant",
                "created": 2,
                "content": [{"type": "text", "text": "final_turn_response"}]
            }]
        }),
    );

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
                            "activeRunId": "run_fake_tool"
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
                    "toolCallId": "tool_1",
                    "title": "shell"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": &sid,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "tool_1",
                    "status": "completed"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": &sid,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {
                        "type": "text",
                        "text": "final_turn_response"
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
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "goose": {
                            "activeRunId": Value::Null
                        }
                    }
                }
            }
        }),
    ]);

    // Gap on resume
    fake.gap_on_next_resume(&sid);

    let pid = v
        .request("session/prompt", prompt_params(&sid, "run_tool", None))
        .await;

    // Wait until tool_call appears
    v.wait_for(|f| {
        f.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("tool_call")
    })
    .await;

    // Kick stream while tool turn is in flight
    fake.kick_stream(&sid);

    // Watchdog reattaches via takeover, probe sees actualRunId -> stays Live, NO mid-run reset!
    v.wait_for(|f| f.get("method").and_then(Value::as_str) == Some("_honk/gap"))
        .await;
    assert_eq!(v.blocks.resets, 1, "NO reset while tool turn is running");

    // Turn completes live: real prompt response arrives, rebuild runs at turn end
    let resp = v.response_timeout(&pid, Duration::from_secs(10)).await;
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    assert_eq!(
        resp.pointer("/result/_meta/honk/synthesized"),
        None,
        "real reply used, must NOT be synthesized"
    );

    // Rebuild at turn end delivered epoch 2
    v.wait_for(|f| {
        f.get("method").and_then(Value::as_str) == Some("_honk/replay_done")
            && f.pointer("/params/epoch").and_then(Value::as_u64) == Some(2)
    })
    .await;
    assert_eq!(v.blocks.resets, 2, "exactly one reset at turn end");
    assert!(v.blocks.agent_text().contains("final_turn_response"));
}
