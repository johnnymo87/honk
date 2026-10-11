use honk_testgoose::{FakeGoose, TcpProxy};
use honk_upstream::{Event, ReplayOutcome, StreamFeatures, SubscribeOpts, Upstream};
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

    Ok(())
}
