mod common;
use common::*;

use std::time::{Duration, Instant};

use common::proxy::TcpProxy;
use honk_upstream::{Event, rpc_id_matches};
use serde_json::json;

#[tokio::test]
#[ignore]
async fn p2a_quiet_drop() -> anyhow::Result<()> {
    let Some(mut ts) = setup_session("p2a").await? else {
        return Ok(());
    };

    println!(
        "[P2a] Session {} started on connection {}, fixture: {}",
        ts.sid,
        ts.conn_id,
        ts.fixture_path.display()
    );

    let prompt =
        "Run the shell command \"sleep 40 && echo done\", then reply with only the word FINISHED.";
    let prompt_req_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": prompt}]
            }),
            Some(&ts.sid),
        )
        .await?;

    // Wait until tool_call appears
    let wait_start = Instant::now();
    let timeout = Duration::from_secs(60);
    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[P2a] timed out waiting for tool_call");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[P2a] stream ended or timed out waiting for tool_call");
        };

        if let Event::Frame(_, frame, _) = event {
            if frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
                panic!("[P2a] unexpected session/request_permission in auto mode: {frame}");
            }

            if let Some(params) = frame.get("params")
                && let Some(update) = params.get("update")
            {
                let su = update.get("sessionUpdate").and_then(|v| v.as_str());
                if su == Some("tool_call")
                    || su == Some("tool_call_update")
                    || update.get("toolCall").is_some()
                {
                    println!("[P2a] Observed tool_call frame, performing hard_drop");
                    break;
                }
            }
        }
    }

    // Immediately hard_drop
    ts.upstream.hard_drop();

    // Immediately resume and subscribe
    println!("[P2a] Resuming connection {}", ts.conn_id);
    let mut resumed = Upstream::resume(
        &ts.url,
        ts.secret.clone(),
        ts.conn_id.clone(),
        ts.rec.clone(),
    )?;

    let conn_409 = resumed.subscribe(None).await?;
    let session_409 = resumed.subscribe(Some(&ts.sid)).await?;

    println!(
        "[P2a] Re-subscribed: conn 409 window = {conn_409:?}, session 409 window = {session_409:?}"
    );

    // Read until session/prompt reply arrives (generous timeout e.g. 180s)
    let read_start = Instant::now();
    let read_timeout = Duration::from_secs(180);
    let mut prompt_reply_received = false;
    let mut run_end_received = false;

    while read_start.elapsed() < read_timeout {
        let remaining = read_timeout - read_start.elapsed();
        let Some(event) = resumed.next(remaining).await? else {
            break;
        };

        if let Event::Frame(_, frame, _) = event {
            if frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
                panic!("[P2a] unexpected session/request_permission in auto mode: {frame}");
            }

            if let Some(params) = frame.get("params")
                && let Some(update) = params.get("update")
                && update.get("sessionUpdate").and_then(|v| v.as_str())
                    == Some("session_info_update")
                && update.pointer("/_meta/goose/activeRunId") == Some(&Value::Null)
            {
                run_end_received = true;
            }

            if frame
                .get("id")
                .is_some_and(|id| rpc_id_matches(&prompt_req_id, id))
            {
                prompt_reply_received = true;
                break;
            }
        }
    }

    // Export session to assert FINISHED present
    let export_val = export_session(&mut resumed, &ts.sid).await?;
    let assistant_text = extract_assistant_text(&export_val);
    let finished_present = assistant_text.contains("FINISHED");

    let _ = resumed.delete().await;

    let summary = json!({
        "test": "p2a_quiet_drop",
        "session_id": ts.sid,
        "conn_id": ts.conn_id,
        "conn_409_ms": conn_409.as_millis(),
        "session_409_ms": session_409.as_millis(),
        "prompt_reply_received": prompt_reply_received,
        "run_end_received": run_end_received,
        "finished_present": finished_present,
        "fixture_path": ts.fixture_path.display().to_string(),
    });

    println!("P2A_SUMMARY: {}", serde_json::to_string(&summary)?);
    assert!(
        finished_present,
        "P2a failed: 'FINISHED' not present in exported assistant text: '{assistant_text}'"
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p2b_drop_streaming() -> anyhow::Result<()> {
    let Some(mut ts) = setup_session("p2b").await? else {
        return Ok(());
    };

    println!(
        "[P2b] Session {} started on connection {}, fixture: {}",
        ts.sid,
        ts.conn_id,
        ts.fixture_path.display()
    );

    let prompt = "Write a 60-line markdown document about tides, including a 5-row table, then reply FINISHED.";
    let prompt_req_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": prompt}]
            }),
            Some(&ts.sid),
        )
        .await?;

    let mut chunks_before = 0usize;
    let mut streamed_text_before = String::new();
    let wait_start = Instant::now();
    let timeout = Duration::from_secs(60);

    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[P2b] timed out waiting for 20 agent_message_chunks");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[P2b] stream ended or timed out waiting for chunks");
        };

        if let Event::Frame(_, frame, _) = event {
            if frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
                panic!("[P2b] unexpected session/request_permission in auto mode: {frame}");
            }

            if let Some(params) = frame.get("params")
                && let Some(update) = params.get("update")
                && update.get("sessionUpdate").and_then(|v| v.as_str())
                    == Some("agent_message_chunk")
            {
                let text = update
                    .pointer("/content/text")
                    .or_else(|| update.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                streamed_text_before.push_str(text);
                chunks_before += 1;
                if chunks_before >= 20 {
                    println!("[P2b] Reached 20 chunks before drop, dropping now");
                    break;
                }
            }
        }
    }

    ts.upstream.hard_drop();

    println!("[P2b] Resuming connection {}", ts.conn_id);
    let mut resumed = Upstream::resume(
        &ts.url,
        ts.secret.clone(),
        ts.conn_id.clone(),
        ts.rec.clone(),
    )?;

    let conn_409 = resumed.subscribe(None).await?;
    let session_409 = resumed.subscribe(Some(&ts.sid)).await?;

    println!(
        "[P2b] Re-subscribed: conn 409 window = {conn_409:?}, session 409 window = {session_409:?}"
    );

    let mut chunks_after = 0usize;
    let mut streamed_text_after = String::new();
    let mut prompt_reply_received = false;
    let mut run_end_received = false;

    let read_start = Instant::now();
    let read_timeout = Duration::from_secs(180);

    while read_start.elapsed() < read_timeout {
        let remaining = read_timeout - read_start.elapsed();
        let Some(event) = resumed.next(remaining).await? else {
            break;
        };

        if let Event::Frame(_, frame, _) = event {
            if frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
                panic!("[P2b] unexpected session/request_permission in auto mode: {frame}");
            }

            if let Some(params) = frame.get("params")
                && let Some(update) = params.get("update")
            {
                let su = update.get("sessionUpdate").and_then(|v| v.as_str());
                if su == Some("agent_message_chunk") {
                    let text = update
                        .pointer("/content/text")
                        .or_else(|| update.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    streamed_text_after.push_str(text);
                    chunks_after += 1;
                } else if su == Some("session_info_update")
                    && update.pointer("/_meta/goose/activeRunId") == Some(&Value::Null)
                {
                    run_end_received = true;
                }
            }

            if frame
                .get("id")
                .is_some_and(|id| rpc_id_matches(&prompt_req_id, id))
            {
                prompt_reply_received = true;
                break;
            }
        }
    }

    let export_val = export_session(&mut resumed, &ts.sid).await?;
    let final_assistant_text = extract_assistant_text(&export_val);
    let all_assistant_text = extract_all_assistant_text(&export_val);
    let finished_present =
        final_assistant_text.contains("FINISHED") || all_assistant_text.contains("FINISHED");

    let total_streamed_chars = streamed_text_before.len() + streamed_text_after.len();
    let exported_final_chars = final_assistant_text.len();
    let exported_all_chars = all_assistant_text.len();
    let chars_missing = exported_all_chars.saturating_sub(total_streamed_chars);
    let chars_abs_diff = total_streamed_chars.abs_diff(exported_all_chars);
    let streamed_equals_exported_all = total_streamed_chars == exported_all_chars;
    let streamed_equals_exported_final = total_streamed_chars == exported_final_chars;

    let _ = resumed.delete().await;

    let summary = json!({
        "test": "p2b_drop_streaming",
        "session_id": ts.sid,
        "conn_id": ts.conn_id,
        "conn_409_ms": conn_409.as_millis(),
        "session_409_ms": session_409.as_millis(),
        "chunks_before": chunks_before,
        "chunks_after": chunks_after,
        "streamed_chars_total": total_streamed_chars,
        "exported_final_chars": exported_final_chars,
        "exported_all_chars": exported_all_chars,
        "chars_missing": chars_missing,
        "chars_abs_diff": chars_abs_diff,
        "streamed_equals_exported_all": streamed_equals_exported_all,
        "streamed_equals_exported_final": streamed_equals_exported_final,
        "prompt_reply_received": prompt_reply_received,
        "run_end_received": run_end_received,
        "finished_present": finished_present,
        "fixture_path": ts.fixture_path.display().to_string(),
    });

    println!("P2B_SUMMARY: {}", serde_json::to_string(&summary)?);
    assert!(
        finished_present,
        "P2b failed: turn did not finish per export (missing FINISHED in '{final_assistant_text}')"
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p2c_long_gap() -> anyhow::Result<()> {
    let Some(mut ts) = setup_session("p2c").await? else {
        return Ok(());
    };

    println!(
        "[P2c] Session {} started on connection {}, fixture: {}",
        ts.sid,
        ts.conn_id,
        ts.fixture_path.display()
    );

    let prompt =
        "Run the shell command \"sleep 40 && echo done\", then reply with only the word FINISHED.";
    let prompt_req_id = ts
        .upstream
        .request(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": prompt}]
            }),
            Some(&ts.sid),
        )
        .await?;

    let wait_start = Instant::now();
    let timeout = Duration::from_secs(60);
    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[P2c] timed out waiting for tool_call");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[P2c] stream ended or timed out waiting for tool_call");
        };

        if let Event::Frame(_, frame, _) = event {
            if frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
                panic!("[P2c] unexpected session/request_permission in auto mode: {frame}");
            }

            if let Some(params) = frame.get("params")
                && let Some(update) = params.get("update")
            {
                let su = update.get("sessionUpdate").and_then(|v| v.as_str());
                if su == Some("tool_call")
                    || su == Some("tool_call_update")
                    || update.get("toolCall").is_some()
                {
                    println!("[P2c] Observed tool_call frame, performing hard_drop");
                    break;
                }
            }
        }
    }

    ts.upstream.hard_drop();

    let gap_secs = 600;
    println!("[P2c] Waiting {gap_secs}s before reattaching...");
    tokio::time::sleep(Duration::from_secs(gap_secs)).await;
    println!(
        "[P2c] Gap wait complete. Resuming connection {}",
        ts.conn_id
    );

    let mut resumed = Upstream::resume(
        &ts.url,
        ts.secret.clone(),
        ts.conn_id.clone(),
        ts.rec.clone(),
    )?;

    let conn_409 = resumed.subscribe(None).await?;
    let session_409 = resumed.subscribe(Some(&ts.sid)).await?;

    println!(
        "[P2c] Re-subscribed: conn 409 window = {conn_409:?}, session 409 window = {session_409:?}"
    );

    // Read any pending frames (e.g. up to 30s)
    let read_start = Instant::now();
    let read_timeout = Duration::from_secs(30);
    let mut prompt_reply_received = false;
    let mut run_end_received = false;

    while read_start.elapsed() < read_timeout {
        let remaining = read_timeout - read_start.elapsed();
        let Some(event) = resumed.next(remaining).await? else {
            break;
        };

        if let Event::Frame(_, frame, _) = event {
            if frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
                panic!("[P2c] unexpected session/request_permission in auto mode: {frame}");
            }

            if let Some(params) = frame.get("params")
                && let Some(update) = params.get("update")
                && update.get("sessionUpdate").and_then(|v| v.as_str())
                    == Some("session_info_update")
                && update.pointer("/_meta/goose/activeRunId") == Some(&Value::Null)
            {
                run_end_received = true;
            }

            if frame
                .get("id")
                .is_some_and(|id| rpc_id_matches(&prompt_req_id, id))
            {
                prompt_reply_received = true;
                break;
            }
        }
    }

    let export_val = export_session(&mut resumed, &ts.sid).await?;
    let assistant_text = extract_assistant_text(&export_val);
    let finished_present = assistant_text.contains("FINISHED");

    let _ = resumed.delete().await;

    let summary = json!({
        "test": "p2c_long_gap",
        "session_id": ts.sid,
        "conn_id": ts.conn_id,
        "gap_secs": gap_secs,
        "conn_409_ms": conn_409.as_millis(),
        "session_409_ms": session_409.as_millis(),
        "prompt_reply_received": prompt_reply_received,
        "run_end_received": run_end_received,
        "finished_present": finished_present,
        "fixture_path": ts.fixture_path.display().to_string(),
    });

    println!("P2C_SUMMARY: {}", serde_json::to_string(&summary)?);
    assert!(
        finished_present,
        "P2c failed: turn did not finish per export (missing FINISHED in '{assistant_text}')"
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p2d_half_open_drop() -> anyhow::Result<()> {
    let secret = match std::env::var("HONK_GOOSE_SECRET") {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            eprintln!("skipping p2d: HONK_GOOSE_SECRET unset");
            return Ok(());
        }
    };

    let direct_url =
        std::env::var("HONK_GOOSE_URL").unwrap_or_else(|_| "http://127.0.0.1:4180/acp".to_string());

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let fixture_path = fixtures_root().join(format!("{FIXTURES_PATCHED_2}/p2d/{ts}.jsonl"));
    if let Some(parent) = fixture_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let redactor = Redactor::from_env();
    let rec = Recorder::file(&fixture_path, redactor)?;

    // 1. Start TCP forwarding proxy in front of Goose sandbox
    let proxy = TcpProxy::start(goose_port()).await?;
    let proxy_url = format!("http://127.0.0.1:{}/acp", proxy.port);
    println!(
        "[P2d] Started TCP forwarding proxy on port {} pointing to {}",
        proxy.port,
        goose_port()
    );

    // 2. Initialize Upstream via proxy
    let mut upstream = Upstream::initialize(&proxy_url, secret.clone(), rec.clone()).await?;
    let conn_id = upstream.conn_id().to_string();

    upstream.subscribe(None).await?;

    let temp_dir = tempfile::tempdir()?;
    let cwd = temp_dir.path().to_str().unwrap().to_string();
    let new_res = upstream
        .call(
            "session/new",
            json!({
                "cwd": cwd,
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;

    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("session/new reply missing sessionId")
        .to_string();

    upstream.subscribe(Some(&sid)).await?;

    upstream
        .call(
            "session/set_mode",
            json!({
                "sessionId": &sid,
                "modeId": "auto"
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;

    upstream
        .call(
            "session/set_config_option",
            json!({
                "sessionId": &sid,
                "configId": "model",
                "value": "claude-haiku-4-5@20251001"
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;

    println!(
        "[P2d] Session {} started on connection {} via proxy port {}, fixture: {}",
        sid,
        conn_id,
        proxy.port,
        fixture_path.display()
    );

    // 3. Send streaming prompt
    let prompt = "Write a 60-line markdown document about tides, including a 5-row table, then reply FINISHED.";
    let prompt_req_id = upstream
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": prompt}]
            }),
            Some(&sid),
        )
        .await?;

    let mut chunks_before = 0usize;
    let mut streamed_text_before = String::new();
    let wait_start = Instant::now();
    let timeout = Duration::from_secs(60);

    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[P2d] timed out waiting for 20 agent_message_chunks");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = upstream.next(remaining).await? else {
            panic!("[P2d] stream ended or timed out waiting for chunks");
        };

        if let Event::Frame(_, frame, _) = event {
            if frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
                panic!("[P2d] unexpected session/request_permission in auto mode: {frame}");
            }

            if let Some(params) = frame.get("params")
                && let Some(update) = params.get("update")
                && update.get("sessionUpdate").and_then(|v| v.as_str())
                    == Some("agent_message_chunk")
            {
                let text = update
                    .pointer("/content/text")
                    .or_else(|| update.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                streamed_text_before.push_str(text);
                chunks_before += 1;
                if chunks_before >= 20 {
                    println!(
                        "[P2d] Reached 20 chunks before drop ({chunks_before} chunks, {} chars)",
                        streamed_text_before.len()
                    );
                    break;
                }
            }
        }
    }

    // 4. Freeze proxy (stop reading/writing, keep sockets OPEN) and hard_drop client tasks
    println!("[P2d] Freezing proxy (holding sockets open without reading/writing)...");
    proxy.freeze();
    upstream.hard_drop();

    // 5. Immediately reattach DIRECTLY to 127.0.0.1:4180 (resume same conn id, no proxy)
    // and subscribe, retrying 409 with a deadline of 180s.
    println!("[P2d] Reattaching directly to 127.0.0.1:4180 with 180s deadline...");
    let mut probe_conn =
        Upstream::resume(&direct_url, secret.clone(), conn_id.clone(), rec.clone())?;
    probe_conn.set_subscribe_deadline(Duration::from_secs(180));

    let mut probe_sess =
        Upstream::resume(&direct_url, secret.clone(), conn_id.clone(), rec.clone())?;
    probe_sess.set_subscribe_deadline(Duration::from_secs(180));

    let probe_start = Instant::now();
    let (conn_sub_res, sess_sub_res) =
        tokio::join!(probe_conn.subscribe(None), probe_sess.subscribe(Some(&sid)),);
    let half_open_probe_elapsed = probe_start.elapsed();

    let conn_cleared_while_frozen = conn_sub_res.is_ok();
    let sess_cleared_while_frozen = sess_sub_res.is_ok();
    let conn_409_ms = conn_sub_res
        .as_ref()
        .map(|d| d.as_millis())
        .unwrap_or(half_open_probe_elapsed.as_millis());
    let sess_409_ms = sess_sub_res
        .as_ref()
        .map(|d| d.as_millis())
        .unwrap_or(half_open_probe_elapsed.as_millis());

    println!(
        "[P2d] Half-open 409 probe finished in {half_open_probe_elapsed:?}: conn cleared={conn_cleared_while_frozen}, sess cleared={sess_cleared_while_frozen}"
    );

    let time_to_subscribe_after_fin;
    let mut resumed;

    if !conn_cleared_while_frozen || !sess_cleared_while_frozen {
        println!(
            "[P2d] 409 did not clear while half-open. Unfreezing/closing proxy sockets (sending FIN)..."
        );
        let fin_start = Instant::now();
        proxy.close().await;

        resumed = Upstream::resume(&direct_url, secret.clone(), conn_id.clone(), rec.clone())?;
        resumed.set_subscribe_deadline(Duration::from_secs(30));
        let _ = resumed.subscribe(None).await?;
        let _ = resumed.subscribe(Some(&sid)).await?;
        time_to_subscribe_after_fin = fin_start.elapsed();
        println!("[P2d] Subscribed successfully after FIN in {time_to_subscribe_after_fin:?}");
    } else {
        println!("[P2d] 409 cleared unexpectedly while half-open");
        time_to_subscribe_after_fin = Duration::ZERO;
        proxy.close().await;
        resumed = probe_sess;
    }

    // 6. Read to prompt reply (if it arrives)
    println!("[P2d] Reading events on resumed connection...");
    let mut chunks_after = 0usize;
    let mut streamed_text_after = String::new();
    let mut prompt_reply_received = false;
    let mut run_end_received = false;

    let read_start = Instant::now();
    let read_timeout = Duration::from_secs(60);

    while read_start.elapsed() < read_timeout {
        let remaining = read_timeout - read_start.elapsed();
        let Some(event) = resumed.next(remaining).await? else {
            break;
        };

        if let Event::Frame(_, frame, _) = event {
            if frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
                panic!("[P2d] unexpected session/request_permission in auto mode: {frame}");
            }

            if let Some(params) = frame.get("params")
                && let Some(update) = params.get("update")
            {
                let su = update.get("sessionUpdate").and_then(|v| v.as_str());
                if su == Some("agent_message_chunk") {
                    let text = update
                        .pointer("/content/text")
                        .or_else(|| update.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    streamed_text_after.push_str(text);
                    chunks_after += 1;
                } else if su == Some("session_info_update")
                    && update.pointer("/_meta/goose/activeRunId") == Some(&Value::Null)
                {
                    run_end_received = true;
                }
            }

            if frame
                .get("id")
                .is_some_and(|id| rpc_id_matches(&prompt_req_id, id))
            {
                prompt_reply_received = true;
                break;
            }
        }
    }

    // 7. Export session and compute frame loss
    println!("[P2d] Exporting session {}...", sid);
    let export_val = export_session(&mut resumed, &sid).await?;
    let final_assistant_text = extract_assistant_text(&export_val);
    let all_assistant_text = extract_all_assistant_text(&export_val);
    let finished_present =
        final_assistant_text.contains("FINISHED") || all_assistant_text.contains("FINISHED");

    let total_streamed_text = format!("{streamed_text_before}{streamed_text_after}");
    let total_streamed_chars = total_streamed_text.len();
    let exported_all_chars = all_assistant_text.len();
    let exported_final_chars = final_assistant_text.len();
    let chars_missing_vs_all = exported_all_chars.saturating_sub(total_streamed_chars);
    let chars_missing_vs_final = exported_final_chars.saturating_sub(total_streamed_chars);
    let chars_abs_diff = total_streamed_chars.abs_diff(exported_all_chars);
    let streamed_is_prefix = all_assistant_text.starts_with(&total_streamed_text);
    let streamed_is_subsequence = is_subsequence(&total_streamed_text, &all_assistant_text);

    let _ = resumed.delete().await;

    let summary = json!({
        "test": "p2d_half_open_drop",
        "session_id": sid,
        "conn_id": conn_id,
        "half_open_probe_elapsed_ms": half_open_probe_elapsed.as_millis(),
        "conn_cleared_while_frozen": conn_cleared_while_frozen,
        "sess_cleared_while_frozen": sess_cleared_while_frozen,
        "conn_409_ms": conn_409_ms,
        "session_409_ms": sess_409_ms,
        "subscribe_ms_after_fin": time_to_subscribe_after_fin.as_millis(),
        "chunks_before": chunks_before,
        "chunks_after": chunks_after,
        "streamed_chars_total": total_streamed_chars,
        "exported_final_chars": exported_final_chars,
        "exported_all_chars": exported_all_chars,
        "chars_missing_vs_all": chars_missing_vs_all,
        "chars_missing_vs_final": chars_missing_vs_final,
        "chars_abs_diff": chars_abs_diff,
        "streamed_is_prefix": streamed_is_prefix,
        "streamed_is_subsequence": streamed_is_subsequence,
        "prompt_reply_received": prompt_reply_received,
        "run_end_received": run_end_received,
        "finished_present": finished_present,
        "fixture_path": fixture_path.display().to_string(),
    });

    println!("P2D_SUMMARY: {}", serde_json::to_string(&summary)?);
    assert!(
        finished_present,
        "P2d failed: turn did not finish per export (missing FINISHED in '{final_assistant_text}')"
    );

    Ok(())
}
