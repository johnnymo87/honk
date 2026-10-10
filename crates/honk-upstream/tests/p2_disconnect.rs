use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use honk_upstream::{Event, Recorder, Redactor, Upstream};
use serde_json::{Value, json};

struct TestSession {
    upstream: Upstream,
    url: String,
    secret: String,
    conn_id: String,
    sid: String,
    rec: Recorder,
    fixture_path: PathBuf,
    _temp_dir: tempfile::TempDir,
}

async fn setup_session(probe_name: &str) -> anyhow::Result<Option<TestSession>> {
    let secret = match std::env::var("HONK_GOOSE_SECRET") {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            eprintln!("skipping {probe_name}: HONK_GOOSE_SECRET unset");
            return Ok(None);
        }
    };

    let url =
        std::env::var("HONK_GOOSE_URL").unwrap_or_else(|_| "http://127.0.0.1:4180/acp".to_string());

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("finding repo root");
    let fixture_path = repo_root.join(format!(
        "fixtures/goose-1.54.0-patched.2/{probe_name}/{ts}.jsonl"
    ));
    if let Some(parent) = fixture_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let redactor = Redactor::from_env();
    let rec = Recorder::file(&fixture_path, redactor)?;

    let mut upstream = Upstream::initialize(&url, secret.clone(), rec.clone()).await?;
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

    Ok(Some(TestSession {
        upstream,
        url,
        secret,
        conn_id,
        sid,
        rec,
        fixture_path,
        _temp_dir: temp_dir,
    }))
}

async fn export_session(upstream: &mut Upstream, sid: &str) -> anyhow::Result<Value> {
    let export_res = upstream
        .call(
            "_goose/unstable/session/export",
            json!({
                "sessionId": sid,
                "format": "json"
            }),
            Some(sid),
            Duration::from_secs(30),
        )
        .await?;

    let data_str = export_res
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("export response missing data string: {export_res}"))?;

    let session_val: Value = serde_json::from_str(data_str)?;
    Ok(session_val)
}

fn extract_assistant_text(session_json: &Value) -> String {
    let mut text = String::new();
    if let Some(convo) = session_json.get("conversation").and_then(|v| v.as_array()) {
        for msg in convo.iter().rev() {
            if msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
                && let Some(contents) = msg.get("content").and_then(|c| c.as_array())
            {
                for block in contents {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
                break;
            }
        }
    }
    text
}

fn extract_all_assistant_text(session_json: &Value) -> String {
    let mut text = String::new();
    if let Some(convo) = session_json.get("conversation").and_then(|v| v.as_array()) {
        for msg in convo {
            if msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
                && let Some(contents) = msg.get("content").and_then(|c| c.as_array())
            {
                for block in contents {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    text
}

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

        if let Event::Frame(_, frame) = event {
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

        if let Event::Frame(_, frame) = event {
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

            if frame.get("id") == Some(&prompt_req_id) {
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

        if let Event::Frame(_, frame) = event {
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

        if let Event::Frame(_, frame) = event {
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

            if frame.get("id") == Some(&prompt_req_id) {
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

        if let Event::Frame(_, frame) = event {
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

        if let Event::Frame(_, frame) = event {
            if let Some(params) = frame.get("params")
                && let Some(update) = params.get("update")
                && update.get("sessionUpdate").and_then(|v| v.as_str())
                    == Some("session_info_update")
                && update.pointer("/_meta/goose/activeRunId") == Some(&Value::Null)
            {
                run_end_received = true;
            }

            if frame.get("id") == Some(&prompt_req_id) {
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
