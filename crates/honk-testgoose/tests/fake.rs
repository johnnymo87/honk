use honk_testgoose::{FakeGoose, TcpProxy};
use honk_upstream::{Event, ReplayOutcome, StreamFeatures, StreamKey, SubscribeOpts, Upstream};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn test_no_features_header() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;

    up.subscribe(None).await?;
    assert_eq!(
        up.stream_features(),
        Some(StreamFeatures {
            takeover: false,
            replay: false,
        })
    );
    assert!(!up.stream_features().unwrap().is_patched());

    assert!(
        fake.inbound()
            .iter()
            .any(|f| f.get("method").and_then(|m| m.as_str()) == Some("initialize")),
        "initialize frame should be recorded in inbound"
    );

    let conn_id = up.conn_id().to_string();
    up.delete().await?;
    assert!(
        fake.deletes().contains(&conn_id),
        "deleted connection should be in deletes()"
    );

    Ok(())
}

#[tokio::test]
async fn test_conflict_forever() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;

    fake.conflict_forever(up.conn_id());

    up.set_subscribe_deadline(Duration::from_millis(200));
    up.set_subscribe_backoff(Duration::from_millis(20), Duration::from_millis(50));

    let res = up.subscribe(None).await;
    assert!(res.is_err(), "subscribe should fail under conflict_forever");
    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("409 conflict") || err_msg.contains("409 Conflict"),
        "error should indicate 409 conflict timeout, got: {err_msg}"
    );

    Ok(())
}

#[tokio::test]
async fn test_lose_next() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let new_res = up
        .call(
            "session/new",
            json!({"cwd": "/tmp", "mcpServers": []}),
            None,
            Duration::from_secs(5),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid)).await?;

    fake.script_prompt(vec![
        "chunk1".to_string(),
        "drop_this_chunk".to_string(),
        "chunk2".to_string(),
    ]);

    fake.lose_next(&sid, |frame| {
        frame
            .pointer("/params/update/content/text")
            .and_then(|v| v.as_str())
            == Some("drop_this_chunk")
    });

    let prompt_id = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "run test"}]
            }),
            Some(&sid),
        )
        .await?;

    let mut received_chunks = Vec::new();
    let mut prompt_replied = false;
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(5);

    while (!prompt_replied || received_chunks.len() < 2) && start.elapsed() < timeout {
        let Some(event) = up.next(Duration::from_millis(100)).await? else {
            continue;
        };
        if let Event::Frame(_, frame, _) = event {
            if let Some(text) = frame
                .pointer("/params/update/content/text")
                .and_then(|v| v.as_str())
            {
                received_chunks.push(text.to_string());
            }
            if frame
                .get("id")
                .is_some_and(|id| honk_upstream::rpc_id_matches(&prompt_id, id))
            {
                prompt_replied = true;
            }
        }
    }

    assert!(prompt_replied, "prompt reply should have arrived");
    assert_eq!(
        received_chunks,
        vec!["chunk1".to_string(), "chunk2".to_string()],
        "drop_this_chunk should have been lost"
    );

    Ok(())
}

#[tokio::test]
async fn test_404_after_restart() -> anyhow::Result<()> {
    let mut fake = FakeGoose::start().await;

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    fake.restart().await;

    let err = up
        .call("session/list", json!({}), None, Duration::from_secs(2))
        .await
        .expect_err("call after restart must return 404");
    assert!(
        honk_upstream::is_gone(&err),
        "error after restart must be 404 (is_gone), got: {err:?}"
    );

    // After restart, new initialize works
    let mut up2 = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up2.subscribe(None).await?;
    assert_eq!(
        up2.stream_features(),
        Some(StreamFeatures {
            takeover: false,
            replay: false,
        })
    );

    Ok(())
}

#[tokio::test]
async fn test_advertise_patched_resume_complete() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    let outcome = up.subscribe_with(None, SubscribeOpts::default()).await?;
    assert!(outcome.features.is_patched());

    let new_res = up
        .call(
            "session/new",
            json!({"cwd": "/tmp", "mcpServers": []}),
            None,
            Duration::from_secs(5),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid)).await?;

    fake.script_prompt(vec!["part1".to_string(), "part2".to_string()]);

    let _prompt_id = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "test prompt"}]
            }),
            Some(&sid),
        )
        .await?;

    let mut session_frames: Vec<(u64, serde_json::Value)> = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let Some(event) = up.next(Duration::from_millis(100)).await? else {
            continue;
        };
        if let Event::Frame(key, frame, id) = event {
            if let honk_upstream::StreamKey::Session(s) = &key
                && s == &sid
                && let Some(frame_id) = id
            {
                session_frames.push((frame_id, frame.clone()));
            }
            if frame.pointer("/result/stopReason").is_some() {
                break;
            }
        }
    }

    assert!(
        session_frames.len() >= 2,
        "should have received multiple frames with ids on session stream"
    );
    let resume_from = session_frames[0].0;
    let expected_replayed_count = session_frames.len() - 1;

    let mut resumed = Upstream::resume(
        &fake.url(),
        fake.secret().to_string(),
        up.conn_id().to_string(),
        honk_upstream::Recorder::disabled(),
    )?;

    let sub_outcome = resumed
        .subscribe_with(
            Some(&sid),
            SubscribeOpts {
                takeover: true,
                resume_after: Some(resume_from),
            },
        )
        .await?;

    assert_eq!(sub_outcome.replay, ReplayOutcome::Complete);
    assert!(sub_outcome.took_over);

    let mut replayed_frames: Vec<(u64, serde_json::Value)> = Vec::new();
    let r_start = std::time::Instant::now();
    while r_start.elapsed() < Duration::from_secs(3) {
        let Some(event) = resumed.next(Duration::from_millis(100)).await? else {
            continue;
        };
        if let Event::Frame(key, frame, id) = event
            && let honk_upstream::StreamKey::Session(s) = &key
            && s == &sid
            && let Some(frame_id) = id
        {
            replayed_frames.push((frame_id, frame));
            if replayed_frames.len() == expected_replayed_count {
                break;
            }
        }
    }

    assert_eq!(replayed_frames.len(), expected_replayed_count);
    for (orig, replayed) in session_frames[1..].iter().zip(&replayed_frames) {
        assert_eq!(orig.0, replayed.0);
        assert_eq!(orig.1, replayed.1);
    }

    Ok(())
}

#[tokio::test]
async fn test_advertise_patched_resume_gap() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let new_res = up
        .call(
            "session/new",
            json!({"cwd": "/tmp", "mcpServers": []}),
            None,
            Duration::from_secs(5),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid)).await?;

    fake.script_prompt(vec!["hello".to_string()]);
    let _ = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "hi"}]
            }),
            Some(&sid),
        )
        .await?;

    // Drain until prompt response
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let Some(event) = up.next(Duration::from_millis(100)).await? else {
            continue;
        };
        if let Event::Frame(_, frame, _) = event
            && frame.pointer("/result/stopReason").is_some()
        {
            break;
        }
    }

    fake.gap_on_next_resume(&sid);

    let mut resumed = Upstream::resume(
        &fake.url(),
        fake.secret().to_string(),
        up.conn_id().to_string(),
        honk_upstream::Recorder::disabled(),
    )?;

    let sub_outcome = resumed
        .subscribe_with(
            Some(&sid),
            SubscribeOpts {
                takeover: true,
                resume_after: Some(1),
            },
        )
        .await?;

    assert_eq!(sub_outcome.replay, ReplayOutcome::Gap);

    Ok(())
}

#[tokio::test]
async fn test_tcp_proxy_freeze_and_release() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;
    let proxy = TcpProxy::start(fake.addr()).await?;
    let proxy_url = format!("http://{}/acp", proxy.addr());

    let mut up1 = Upstream::initialize(
        &proxy_url,
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up1.subscribe(None).await?;

    // Freeze currently active proxy sockets (half-open, no FIN, P2d)
    proxy.freeze();

    // Assert that up1 stream yields nothing while frozen
    let event_while_frozen = up1.next(Duration::from_millis(200)).await?;
    assert!(
        event_while_frozen.is_none(),
        "up1 should yield nothing while proxy is frozen"
    );

    // New connection through proxy should pass through and succeed (P2f)
    let mut up2 = Upstream::initialize(
        &proxy_url,
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up2.subscribe(None).await?;

    // Release frozen sockets (sends TCP FIN, P2d)
    proxy.release().await;

    // Assert that up1 stream ends (yields StreamEnded or None/error) after release
    let mut up1_ended = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        match up1.next(Duration::from_millis(200)).await {
            Ok(Some(Event::StreamEnded { .. })) => {
                up1_ended = true;
                break;
            }
            Ok(None) => {}
            Err(_) => {
                up1_ended = true;
                break;
            }
            _ => {}
        }
    }
    assert!(up1_ended, "up1 stream must end after proxy release");

    Ok(())
}

#[tokio::test]
async fn test_mailbox_queuing_before_subscribe() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let new_res = up
        .call(
            "session/new",
            json!({"cwd": "/tmp", "mcpServers": []}),
            None,
            Duration::from_secs(5),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    fake.script_prompt(vec!["queued1".to_string(), "queued2".to_string()]);

    // Send prompt BEFORE subscribing to session stream
    let prompt_id = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "start run"}]
            }),
            Some(&sid),
        )
        .await?;

    // Allow background prompt task to emit chunks into the mailbox
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now subscribe WITHOUT Last-Event-ID; queued frames must be delivered
    up.subscribe(Some(&sid)).await?;

    let mut received_chunks = Vec::new();
    let mut prompt_replied = false;
    let start = std::time::Instant::now();
    while (!prompt_replied || received_chunks.len() < 2) && start.elapsed() < Duration::from_secs(5)
    {
        let Some(event) = up.next(Duration::from_millis(100)).await? else {
            continue;
        };
        if let Event::Frame(_, frame, _) = event {
            if let Some(text) = frame
                .pointer("/params/update/content/text")
                .and_then(|v| v.as_str())
            {
                received_chunks.push(text.to_string());
            }
            if frame
                .get("id")
                .is_some_and(|id| honk_upstream::rpc_id_matches(&prompt_id, id))
            {
                prompt_replied = true;
            }
        }
    }

    assert!(
        prompt_replied,
        "prompt reply should be delivered from mailbox queue"
    );
    assert_eq!(
        received_chunks,
        vec!["queued1".to_string(), "queued2".to_string()],
        "queued chunks should be delivered upon subscription"
    );

    Ok(())
}

#[tokio::test]
async fn test_mailbox_queuing_before_subscribe_patched() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;
    fake.advertise_patched(true);

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let new_res = up
        .call(
            "session/new",
            json!({"cwd": "/tmp", "mcpServers": []}),
            None,
            Duration::from_secs(5),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    fake.script_prompt(vec!["p_queued1".to_string(), "p_queued2".to_string()]);

    let prompt_id = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "start run"}]
            }),
            Some(&sid),
        )
        .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // In patched mode, subscribing delivers all queued frames and attaches IDs
    let outcome = up
        .subscribe_with(Some(&sid), SubscribeOpts::default())
        .await?;
    assert!(outcome.features.is_patched());

    let mut received_chunks = Vec::new();
    let mut prompt_replied = false;
    let start = std::time::Instant::now();
    while (!prompt_replied || received_chunks.len() < 2) && start.elapsed() < Duration::from_secs(5)
    {
        let Some(event) = up.next(Duration::from_millis(100)).await? else {
            continue;
        };
        if let Event::Frame(_, frame, id) = event {
            assert!(id.is_some(), "patched frames must carry event IDs");
            if let Some(text) = frame
                .pointer("/params/update/content/text")
                .and_then(|v| v.as_str())
            {
                received_chunks.push(text.to_string());
            }
            if frame
                .get("id")
                .is_some_and(|id| honk_upstream::rpc_id_matches(&prompt_id, id))
            {
                prompt_replied = true;
            }
        }
    }

    assert!(prompt_replied);
    assert_eq!(
        received_chunks,
        vec!["p_queued1".to_string(), "p_queued2".to_string()]
    );

    Ok(())
}

#[tokio::test]
async fn test_ensure_session_lazily_on_export() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;

    let mut up1 = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up1.subscribe(None).await?;

    let new_res = up1
        .call(
            "session/new",
            json!({"cwd": "/tmp", "mcpServers": []}),
            None,
            Duration::from_secs(5),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    fake.set_export_data(
        &sid,
        json!({
            "conversation": [
                {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "exported text"}]
                }
            ]
        }),
    );

    // Open a fresh connection (like honkd's export side connection)
    let mut up2 = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;

    // Subscribing to session sid on up2 lazily ensures the session stream
    up2.subscribe(Some(&sid)).await?;

    let export_res = up2
        .call(
            "_goose/unstable/session/export",
            json!({"sessionId": &sid, "format": "json"}),
            Some(&sid),
            Duration::from_secs(5),
        )
        .await?;

    let data_str = export_res
        .get("data")
        .and_then(|v| v.as_str())
        .expect("data field in export response");
    assert!(data_str.contains("exported text"));

    Ok(())
}

#[tokio::test]
async fn test_keepalives() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;
    fake.set_keepalive_interval(Duration::from_millis(50));

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let t0 = up
        .last_byte_at(&StreamKey::Conn)
        .expect("last_byte_at recorded on subscribe");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Read to ensure bytes are processed by the stream reader
    let _ = up.next(Duration::from_millis(50)).await;

    let t1 = up
        .last_byte_at(&StreamKey::Conn)
        .expect("last_byte_at after keepalives");
    assert!(
        t1 > t0,
        "keepalive must advance last_byte_at (t0: {t0:?}, t1: {t1:?})"
    );

    Ok(())
}

#[tokio::test]
async fn test_cancel_from_any_connection() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;

    let mut up1 = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up1.subscribe(None).await?;

    let new_res = up1
        .call(
            "session/new",
            json!({"cwd": "/tmp", "mcpServers": []}),
            None,
            Duration::from_secs(5),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up1.subscribe(Some(&sid)).await?;

    fake.script_prompt(vec![
        "step1".to_string(),
        "step2".to_string(),
        "step3".to_string(),
        "step4".to_string(),
    ]);

    let prompt_id = up1
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "start long run"}]
            }),
            Some(&sid),
        )
        .await?;

    // Fresh connection cancels the run
    let up2 = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;

    up2.notify("session/cancel", json!({"sessionId": &sid}), Some(&sid))
        .await?;

    // Drain up1 and assert stopReason is cancelled
    let mut stop_reason = None;
    let start = std::time::Instant::now();
    while stop_reason.is_none() && start.elapsed() < Duration::from_secs(5) {
        let Some(event) = up1.next(Duration::from_millis(100)).await? else {
            continue;
        };
        if let Event::Frame(_, frame, _) = event
            && frame
                .get("id")
                .is_some_and(|id| honk_upstream::rpc_id_matches(&prompt_id, id))
        {
            stop_reason = frame
                .pointer("/result/stopReason")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
        }
    }

    assert_eq!(stop_reason.as_deref(), Some("cancelled"));

    Ok(())
}

#[tokio::test]
async fn test_session_list_shape() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let _ = up
        .call(
            "session/new",
            json!({"cwd": "/tmp/test-dir", "mcpServers": []}),
            None,
            Duration::from_secs(5),
        )
        .await?;

    let list_res = up
        .call("session/list", json!({}), None, Duration::from_secs(5))
        .await?;

    let sessions = list_res
        .get("sessions")
        .and_then(|v| v.as_array())
        .expect("sessions array in session/list response");
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].get("sessionId").is_some());
    assert_eq!(
        sessions[0].get("cwd").and_then(|v| v.as_str()),
        Some("/tmp/test-dir")
    );

    Ok(())
}

#[tokio::test]
async fn test_missing_session_id_error() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;

    let up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;

    let client = reqwest::Client::new();
    let res = client
        .post(fake.url())
        .header("x-secret-key", fake.secret())
        .header("content-type", "application/json")
        .header("acp-connection-id", up.conn_id())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/load",
            "params": {}
        }))
        .send()
        .await?;

    assert_eq!(res.status(), reqwest::StatusCode::BAD_REQUEST);
    let body = res.text().await?;
    assert!(
        body.contains("Acp-Session-Id header required"),
        "body should indicate Acp-Session-Id header required: {body}"
    );

    Ok(())
}

#[tokio::test]
async fn test_agent_message_chunk_carries_message_id() -> anyhow::Result<()> {
    let fake = FakeGoose::start().await;

    let mut up = Upstream::initialize(
        &fake.url(),
        fake.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let new_res = up
        .call(
            "session/new",
            json!({"cwd": "/tmp", "mcpServers": []}),
            None,
            Duration::from_secs(5),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid)).await?;

    fake.script_prompt(vec!["chunk_a".to_string()]);

    let _ = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "test"}]
            }),
            Some(&sid),
        )
        .await?;

    let mut saw_chunk = false;
    let start = std::time::Instant::now();
    while !saw_chunk && start.elapsed() < Duration::from_secs(5) {
        let Some(event) = up.next(Duration::from_millis(100)).await? else {
            continue;
        };
        if let Event::Frame(_, frame, _) = event {
            let update_type = frame
                .pointer("/params/update/sessionUpdate")
                .and_then(|v| v.as_str());
            if update_type == Some("agent_message_chunk") {
                let top_mid = frame
                    .pointer("/params/update/messageId")
                    .and_then(|v| v.as_str());
                let meta_mid = frame
                    .pointer("/params/update/_meta/goose/messageId")
                    .and_then(|v| v.as_str());
                assert!(
                    top_mid.is_some(),
                    "top-level messageId missing in chunk: {frame}"
                );
                assert!(
                    meta_mid.is_some(),
                    "_meta messageId missing in chunk: {frame}"
                );
                assert_eq!(top_mid, meta_mid, "top-level and meta messageId must match");
                saw_chunk = true;
            }
        }
    }

    assert!(saw_chunk, "agent_message_chunk must be observed");

    Ok(())
}

#[tokio::test]
async fn test_stub_model_gate_queued_before_restart_consumed_after_waits() {
    use honk_testgoose::{StubModel, Turn};

    let model = StubModel::start().await;
    // Queue gate "g1" and gate "g2" before restart
    model.push([
        Turn::Gate("g1".into(), Box::new(Turn::Text(vec!["one".into()]))),
        Turn::Gate("g2".into(), Box::new(Turn::Text(vec!["two".into()]))),
    ]);

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", model.base_url());

    // Request 1 arrives and enters gate "g1" wait
    let client1 = client.clone();
    let url1 = url.clone();
    let req1_fut = tokio::spawn(async move {
        client1
            .post(&url1)
            .json(&json!({
                "model": "stub-model",
                "messages": [{"role": "user", "content": "hi 1"}],
                "tools": [{"type": "function", "function": {"name": "developer"}}],
            }))
            .send()
            .await
    });

    // Give req1 a moment to begin waiting on "g1"
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Goose restart occurs: open_all() releases currently-waiting gates
    model.open_all();

    // req1 must resolve successfully
    let res1 = req1_fut.await.expect("join req1").expect("req1 response");
    assert!(res1.status().is_success());

    // Request 2 (for "g2", which was queued BEFORE restart) is sent AFTER restart
    let client2 = client.clone();
    let url2 = url.clone();
    let mut req2_fut = tokio::spawn(async move {
        client2
            .post(&url2)
            .json(&json!({
                "model": "stub-model",
                "messages": [{"role": "user", "content": "hi 2"}],
                "tools": [{"type": "function", "function": {"name": "developer"}}],
            }))
            .send()
            .await
    });

    // Verify req2 DOES NOT resolve immediately (it must still wait on "g2")
    let wait_res = tokio::time::timeout(Duration::from_millis(150), &mut req2_fut).await;
    assert!(
        wait_res.is_err(),
        "req2 must still wait on gate g2 even though it was queued before restart"
    );

    // Explicitly open gate "g2"
    model.open("g2");

    // req2 now resolves!
    let res2 = tokio::time::timeout(Duration::from_secs(2), req2_fut)
        .await
        .expect("req2 completes")
        .expect("join req2")
        .expect("res2 ok");
    assert!(res2.status().is_success());
}
