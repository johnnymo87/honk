mod common;
use common::proxy::TcpProxy;
use common::*;
use honk_upstream::{ReplayOutcome, SubscribeOpts};
use std::collections::HashMap;

const STREAMING_PROMPT: &str =
    "Write a 60-line markdown document about tides, including a 5-row table, then reply FINISHED.";

struct HalfOpen {
    ts: TestSession,
    proxy: TcpProxy,
    prompt_id: Value,
    pre_frames: Vec<Value>,
    /// Last PROCESSED event id per stream: what honkd would have persisted.
    last_ids: HashMap<StreamKey, u64>,
}

/// P2d's setup: all traffic through a proxy, streaming turn, freeze after 20 chunks.
async fn freeze_mid_stream(probe: &str) -> anyhow::Result<Option<HalfOpen>> {
    let proxy = TcpProxy::start(goose_port()).await?;
    let proxy_url = format!("http://127.0.0.1:{}/acp", proxy.port);
    let Some(mut ts) = setup_session_at(&proxy_url, FIXTURES_PATCHED_3, probe, "auto").await?
    else {
        return Ok(None);
    };
    let prompt_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({"sessionId": &ts.sid, "prompt": [{"type": "text", "text": STREAMING_PROMPT}]}),
            Some(&ts.sid),
        )
        .await?;
    let mut pre_frames = Vec::new();
    let mut last_ids = HashMap::new();
    let mut chunks = 0;
    while chunks < 20 {
        match ts.upstream.next(Duration::from_secs(90)).await? {
            Some(Event::Frame(key, frame, id)) => {
                if is_agent_chunk(&frame) {
                    chunks += 1;
                }
                if let Some(id) = id {
                    last_ids.insert(key, id);
                }
                pre_frames.push(frame);
            }
            other => anyhow::bail!("stream ended before 20 chunks: {other:?}"),
        }
    }
    proxy.freeze();
    ts.upstream.hard_drop();
    // Frames the reader task had already parsed were delivered; process them too.
    while let Some(Event::Frame(key, frame, id)) = ts.upstream.next(Duration::ZERO).await? {
        if let Some(id) = id {
            last_ids.insert(key, id);
        }
        pre_frames.push(frame);
    }
    Ok(Some(HalfOpen {
        ts,
        proxy,
        prompt_id,
        pre_frames,
        last_ids,
    }))
}

struct Tail {
    frames: Vec<Value>,
    prompt_reply: bool,
    run_end: bool,
}

async fn read_to_turn_end(
    up: &mut Upstream,
    prompt_id: &Value,
    within: Duration,
) -> anyhow::Result<Tail> {
    let deadline = Instant::now() + within;
    let mut tail = Tail {
        frames: Vec::new(),
        prompt_reply: false,
        run_end: false,
    };
    while !(tail.prompt_reply && tail.run_end) && Instant::now() < deadline {
        let Some(Event::Frame(_, frame, _)) = up.next(deadline - Instant::now()).await? else {
            break;
        };
        if frame
            .get("id")
            .is_some_and(|id| rpc_id_matches(prompt_id, id))
            && frame.get("result").is_some()
        {
            tail.prompt_reply = true;
        }
        if is_run_end(&frame) {
            tail.run_end = true;
        }
        tail.frames.push(frame);
    }
    Ok(tail)
}

async fn read_until_idle(
    up: &mut Upstream,
    prompt_id: &Value,
    idle_timeout: Duration,
) -> anyhow::Result<Tail> {
    let mut tail = Tail {
        frames: Vec::new(),
        prompt_reply: false,
        run_end: false,
    };
    while !(tail.prompt_reply && tail.run_end) {
        let Some(Event::Frame(_, frame, _)) = up.next(idle_timeout).await? else {
            break;
        };
        if frame
            .get("id")
            .is_some_and(|id| rpc_id_matches(prompt_id, id))
            && frame.get("result").is_some()
        {
            tail.prompt_reply = true;
        }
        if is_run_end(&frame) {
            tail.run_end = true;
        }
        tail.frames.push(frame);
    }
    Ok(tail)
}

fn streamed_text(frames: &[Value]) -> String {
    frames.iter().filter_map(chunk_text).collect()
}

/// P2e: half-open subscriber + takeover. Question: is the 409 window gone?
#[tokio::test]
#[ignore]
async fn p2e_half_open_takeover() -> anyhow::Result<()> {
    let Some(h) = freeze_mid_stream("p2e").await? else {
        return Ok(());
    };
    let waited = wait_until_turn_committed(&h.ts, Duration::from_secs(150)).await?;
    let mut resumed = Upstream::resume(
        &goose_url(),
        h.ts.secret.clone(),
        h.ts.conn_id.clone(),
        h.ts.rec.clone(),
    )?;
    resumed.set_subscribe_deadline(Duration::from_secs(5));
    let opts = SubscribeOpts {
        takeover: true,
        resume_after: None,
    };
    let t0 = Instant::now();
    let conn = resumed.subscribe_with(None, opts).await?;
    let sess = resumed.subscribe_with(Some(&h.ts.sid), opts).await?;
    let reattach = t0.elapsed();
    let tail = read_until_idle(&mut resumed, &h.prompt_id, Duration::from_secs(10)).await?;
    let export = export_session(&mut resumed, &h.ts.sid).await?;
    let streamed = format!(
        "{}{}",
        streamed_text(&h.pre_frames),
        streamed_text(&tail.frames)
    );
    let exported = extract_all_assistant_text(&export);
    let missing = exported
        .chars()
        .count()
        .saturating_sub(streamed.chars().count());
    h.ts.rec.record_marker(
        "probe",
        json!({
            "probe": "p2e", "waited_for_commit_ms": waited.as_millis(),
            "reattach_ms": reattach.as_millis(),
            "conn_409_ms": conn.conflict_window.as_millis(), "session_409_ms": sess.conflict_window.as_millis(),
            "took_over": [conn.took_over, sess.took_over],
            "prompt_reply": tail.prompt_reply, "run_end": tail.run_end,
            "streamed_chars": streamed.chars().count(), "exported_chars": exported.chars().count(),
            "chars_missing": missing, "streamed_is_subsequence": is_subsequence(&streamed, &exported),
        })
        .to_string(),
    );
    assert_eq!(conn.conflict_window, Duration::ZERO);
    assert_eq!(sess.conflict_window, Duration::ZERO);
    assert!(conn.took_over && sess.took_over);
    assert!(
        reattach < Duration::from_millis(100),
        "takeover reattach took {reattach:?}"
    );
    // P2e is the CONTROL for P2f: takeover without Last-Event-ID must still lose what the
    // frozen socket had dequeued (P2d). If nothing was lost, the freeze landed too late and
    // P2f's `chars_missing == 0` would prove nothing.
    assert!(
        missing > 0,
        "control failed: no loss without replay (freeze landed after goose finished writing?)"
    );
    assert!(
        !tail.prompt_reply,
        "control failed: prompt reply arrived without replay"
    );
    drop(h.proxy);
    Ok(())
}

/// P2f: half-open + wait for the turn to finish behind the frozen socket, then
/// takeover + Last-Event-ID. Every post-reattach frame is then a replay of something
/// goose already wrote into the dead socket (P2d: those were all lost).
#[tokio::test]
#[ignore]
async fn p2f_half_open_takeover_with_replay() -> anyhow::Result<()> {
    let Some(h) = freeze_mid_stream("p2f").await? else {
        return Ok(());
    };
    let waited = wait_until_turn_committed(&h.ts, Duration::from_secs(150)).await?;
    let mut resumed = Upstream::resume(
        &goose_url(),
        h.ts.secret.clone(),
        h.ts.conn_id.clone(),
        h.ts.rec.clone(),
    )?;
    let conn_last = h.last_ids.get(&StreamKey::Conn).copied();
    let sess_last = h
        .last_ids
        .get(&StreamKey::Session(h.ts.sid.clone()))
        .copied();
    anyhow::ensure!(
        sess_last.is_some(),
        "no event id was processed on the session stream before the freeze"
    );
    let conn = resumed
        .subscribe_with(
            None,
            SubscribeOpts {
                takeover: true,
                resume_after: Some(conn_last.unwrap_or(0)),
            },
        )
        .await?;
    let sess = resumed
        .subscribe_with(
            Some(&h.ts.sid),
            SubscribeOpts {
                takeover: true,
                resume_after: sess_last,
            },
        )
        .await?;
    let tail = read_to_turn_end(&mut resumed, &h.prompt_id, Duration::from_secs(30)).await?;
    let export = export_session(&mut resumed, &h.ts.sid).await?;
    let streamed = format!(
        "{}{}",
        streamed_text(&h.pre_frames),
        streamed_text(&tail.frames)
    );
    let exported = extract_all_assistant_text(&export);
    let missing = exported
        .chars()
        .count()
        .saturating_sub(streamed.chars().count());
    h.ts.rec.record_marker(
        "probe",
        json!({
            "probe": "p2f", "waited_for_commit_ms": waited.as_millis(),
            "last_ids": {"conn": conn_last, "session": sess_last},
            "conn_replay": format!("{:?}", conn.replay), "session_replay": format!("{:?}", sess.replay),
            "replayed_frames": tail.frames.len(),
            "prompt_reply": tail.prompt_reply, "run_end": tail.run_end,
            "streamed_chars": streamed.chars().count(), "exported_chars": exported.chars().count(),
            "chars_missing": missing,
        })
        .to_string(),
    );
    assert_eq!(conn.replay, ReplayOutcome::Complete);
    assert_eq!(sess.replay, ReplayOutcome::Complete);
    assert!(
        !tail.frames.is_empty(),
        "replay delivered nothing: the probe did not exercise the ring"
    );
    assert!(
        tail.prompt_reply,
        "the session/prompt reply must be replayed"
    );
    assert!(
        tail.run_end,
        "the run-end session_info_update must be replayed"
    );
    assert_eq!(
        missing, 0,
        "streamed text must cover the exported assistant text"
    );
    assert!(is_subsequence(&streamed, &exported));
    drop(h.proxy);
    Ok(())
}
