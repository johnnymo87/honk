mod common;
use common::*;

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use honk_upstream::{Event, Recorder, Redactor, StreamKey, Upstream, rpc_id_matches};
use serde_json::{Value, json};

async fn get_session_cwd(upstream: &mut Upstream, sid: &str) -> anyhow::Result<Option<String>> {
    let list_res = upstream
        .call("session/list", json!({}), None, Duration::from_secs(30))
        .await?;

    if let Some(sessions) = list_res.get("sessions").and_then(|v| v.as_array()) {
        for s in sessions {
            let session_id = s.get("sessionId").and_then(|v| {
                v.as_str()
                    .or_else(|| v.pointer("/id").and_then(|i| i.as_str()))
            });
            if session_id == Some(sid)
                && let Some(cwd) = s.get("cwd").and_then(|v| v.as_str())
            {
                return Ok(Some(cwd.to_string()));
            }
        }
    }
    Ok(None)
}

fn get_chunk_text(frame: &Value) -> Option<String> {
    let params = frame.get("params")?;
    let update = params.get("update")?;
    let update_type = update.get("sessionUpdate").and_then(|v| v.as_str())?;
    if update_type == "agent_message_chunk" || update_type == "user_message_chunk" {
        let text = update
            .pointer("/content/text")
            .or_else(|| update.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        Some(text.to_string())
    } else {
        None
    }
}

fn is_agent_chunk(frame: &Value) -> bool {
    frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str())
        == Some("agent_message_chunk")
}

fn is_user_chunk(frame: &Value) -> bool {
    frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str())
        == Some("user_message_chunk")
}

fn is_run_end(frame: &Value) -> bool {
    if frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str())
        == Some("session_info_update")
    {
        let active_run = frame.pointer("/params/update/_meta/goose/activeRunId");
        active_run == Some(&Value::Null) || active_run.is_none()
    } else {
        false
    }
}

fn check_no_permission_request(frame: &Value, context: &str) {
    if frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
        panic!("[{context}] unexpected session/request_permission in auto mode: {frame}");
    }
}

fn find_overlap(a: &str, b: &str) -> usize {
    let max_possible = std::cmp::min(a.len(), b.len());
    for len in (1..=max_possible).rev() {
        if a.ends_with(&b[..len]) {
            return len;
        }
    }
    0
}

#[tokio::test]
#[ignore]
async fn p3a1_same_conn_reload_tides() -> anyhow::Result<()> {
    let Some(mut ts) = setup_session("p3a").await? else {
        return Ok(());
    };

    let goose_pid = get_goose_sandbox_pid()?;
    let stats_init = get_proc_stats(goose_pid)?;
    println!(
        "[P3a1] Session {} started on Conn A: {}, pid={}, threads={:?}, rss={:?} kB",
        ts.sid, ts.conn_id, goose_pid, stats_init.threads, stats_init.vm_rss_kb
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

    let mut chunks_before_drop = 0usize;
    let mut text_before_drop = String::new();
    let wait_start = Instant::now();
    let timeout = Duration::from_secs(60);

    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[P3a1] timed out waiting for 10 agent_message_chunks");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[P3a1] stream ended or timed out waiting for chunks");
        };

        if let Event::Frame(_, ref frame) = event {
            check_no_permission_request(frame, "P3a1 pre-drop");
            if is_agent_chunk(frame) {
                if let Some(t) = get_chunk_text(frame) {
                    text_before_drop.push_str(&t);
                }
                chunks_before_drop += 1;
                if chunks_before_drop >= 10 {
                    println!(
                        "[P3a1] Reached {} chunks before drop ({} chars), hard-dropping connection",
                        chunks_before_drop,
                        text_before_drop.len()
                    );
                    break;
                }
            }
        }
    }

    // 1. Hard drop connection A
    ts.upstream.hard_drop();

    // 2. Immediately resume connection A and subscribe
    let mut resumed = Upstream::resume(
        &ts.url,
        ts.secret.clone(),
        ts.conn_id.clone(),
        ts.rec.clone(),
    )?;
    resumed.subscribe(None).await?;
    let sub_window = resumed.subscribe(Some(&ts.sid)).await?;
    println!(
        "[P3a1] Resumed conn A and subscribed (409 window: {:?})",
        sub_window
    );

    // Get stored cwd from session/list
    let stored_cwd = get_session_cwd(&mut resumed, &ts.sid)
        .await?
        .unwrap_or_else(|| ts.temp_dir.path().to_str().unwrap().to_string());
    println!("[P3a1] Stored cwd from session/list: {stored_cwd}");

    // Check mid-run export
    let mid_run_export_res = export_session(&mut resumed, &ts.sid).await;
    let (mid_run_export_ok, mid_run_export_assistant_chars, mid_run_convo_len) =
        match &mid_run_export_res {
            Ok(v) => {
                let ast = extract_all_assistant_text(v);
                let len = v
                    .get("conversation")
                    .and_then(|c| c.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                (true, ast.len(), len)
            }
            Err(e) => {
                println!("[P3a1] mid-run export error: {e}");
                (false, 0, 0)
            }
        };
    println!(
        "[P3a1] Mid-run export ok: {}, in-flight assistant chars in export: {}, convo len: {}",
        mid_run_export_ok, mid_run_export_assistant_chars, mid_run_convo_len
    );

    let stats_before_load = get_proc_stats(goose_pid)?;

    // Send session/load on Conn A
    println!("[P3a1] Sending session/load on connection A...");
    let load_req_id = resumed
        .request(
            "session/load",
            json!({
                "sessionId": &ts.sid,
                "cwd": &stored_cwd,
                "mcpServers": []
            }),
            Some(&ts.sid),
        )
        .await?;

    let mut replay_frames: Vec<Value> = Vec::new();
    let mut live_frames_after_load: Vec<Value> = Vec::new();
    let mut load_reply_stream: Option<StreamKey> = None;
    let mut load_reply_frame: Option<Value> = None;
    let mut stats_after_load: Option<ProcStats> = None;
    let mut prompt_reply_frame: Option<Value> = None;
    let mut run_end_frame: Option<Value> = None;

    let post_load_start = Instant::now();
    let post_load_timeout = Duration::from_secs(90);

    while post_load_start.elapsed() < post_load_timeout {
        let remaining = post_load_timeout - post_load_start.elapsed();
        let Some(event) = resumed.next(remaining).await? else {
            println!("[P3a1] Stream ended or timed out waiting for frames");
            break;
        };

        if let Event::Frame(key, frame) = event {
            check_no_permission_request(&frame, "P3a1 post-load");

            if frame
                .get("id")
                .is_some_and(|id| rpc_id_matches(&load_req_id, id))
            {
                println!("[P3a1] Received session/load reply on stream: {:?}", key);
                load_reply_stream = Some(key.clone());
                load_reply_frame = Some(frame.clone());
                stats_after_load = Some(get_proc_stats(goose_pid)?);
                continue;
            }

            if frame
                .get("id")
                .is_some_and(|id| rpc_id_matches(&prompt_req_id, id))
            {
                println!("[P3a1] Received prompt reply frame!");
                prompt_reply_frame = Some(frame.clone());
            }

            if is_run_end(&frame) {
                println!("[P3a1] Received run-end session_info_update!");
                run_end_frame = Some(frame.clone());
            }

            if load_reply_frame.is_none() {
                replay_frames.push(frame);
            } else {
                live_frames_after_load.push(frame);
            }

            if prompt_reply_frame.is_some() && run_end_frame.is_some() {
                let _ = tokio::time::sleep(Duration::from_millis(500)).await;
                while let Ok(Some(Event::Frame(_, extra))) =
                    resumed.next(Duration::from_millis(200)).await
                {
                    live_frames_after_load.push(extra);
                }
                break;
            }
        }
    }

    let load_success = load_reply_frame
        .as_ref()
        .is_some_and(|f| f.get("error").is_none());
    let load_error = load_reply_frame
        .as_ref()
        .and_then(|f| f.get("error"))
        .cloned();

    // Analyze replay frames
    let mut replay_user_chunks = 0usize;
    let mut replay_agent_chunks = 0usize;
    let mut replay_assistant_text = String::new();
    let mut replay_meta_fields = std::collections::HashSet::new();

    for f in &replay_frames {
        if is_user_chunk(f) {
            replay_user_chunks += 1;
        }
        if is_agent_chunk(f) {
            replay_agent_chunks += 1;
            if let Some(t) = get_chunk_text(f) {
                replay_assistant_text.push_str(&t);
            }
        }
        if let Some(meta) = f.pointer("/params/update/_meta").or_else(|| {
            f.pointer("/params/update/chunk/_meta")
                .or_else(|| f.pointer("/params/update/meta"))
        }) && let Some(obj) = meta.as_object()
        {
            for k in obj.keys() {
                replay_meta_fields.insert(k.clone());
            }
        }
    }

    // Analyze live frames after load
    let mut live_agent_chunks = 0usize;
    let mut live_assistant_text = String::new();
    for f in &live_frames_after_load {
        if is_agent_chunk(f) {
            live_agent_chunks += 1;
            if let Some(t) = get_chunk_text(f) {
                live_assistant_text.push_str(&t);
            }
        }
    }

    let stats_post = stats_after_load.unwrap_or(stats_before_load.clone());
    let thread_diff =
        stats_post.threads.unwrap_or(0) as i64 - stats_before_load.threads.unwrap_or(0) as i64;
    let rss_diff_kb =
        stats_post.vm_rss_kb.unwrap_or(0) as i64 - stats_before_load.vm_rss_kb.unwrap_or(0) as i64;
    let children_changed = stats_post.children != stats_before_load.children;

    // Export final session
    let final_export = export_session(&mut resumed, &ts.sid).await?;
    let final_all_text = extract_all_assistant_text(&final_export);
    let _final_last_text = extract_assistant_text(&final_export);

    let combined_streamed = format!("{text_before_drop}{live_assistant_text}");
    let overlap = find_overlap(&text_before_drop, &live_assistant_text);
    let missing_chars = final_all_text.len().saturating_sub(combined_streamed.len());
    let duplicate_chars = overlap;
    let is_prefix = final_all_text.starts_with(&combined_streamed);
    let is_subseq = is_subsequence(&combined_streamed, &final_all_text);
    let finished_present = final_all_text.contains("FINISHED");

    println!("[P3a1] Replay frames count: {}", replay_frames.len());
    println!(
        "[P3a1] Replay user chunks: {}, agent chunks: {}, text len: {}",
        replay_user_chunks,
        replay_agent_chunks,
        replay_assistant_text.len()
    );
    println!(
        "[P3a1] Replay meta fields: {:?}",
        replay_meta_fields.iter().collect::<Vec<_>>()
    );
    println!(
        "[P3a1] Live frames after load: {}, live agent chunks: {}, live text len: {}",
        live_frames_after_load.len(),
        live_agent_chunks,
        live_assistant_text.len()
    );
    println!(
        "[P3a1] Proc stats: thread diff = {}, rss diff = {} kB, children changed = {}",
        thread_diff, rss_diff_kb, children_changed
    );
    println!(
        "[P3a1] Combined streamed len = {}, final export len = {}, missing = {}, overlap = {}",
        combined_streamed.len(),
        final_all_text.len(),
        missing_chars,
        overlap
    );

    let summary = json!({
        "probe": "p3a1_tides",
        "sessionId": ts.sid,
        "conn_id": ts.conn_id,
        "load_success": load_success,
        "load_error": load_error,
        "load_reply_stream": load_reply_stream.map(|s| s.stream_record_name()),
        "mid_run_export_ok": mid_run_export_ok,
        "mid_run_export_assistant_chars": mid_run_export_assistant_chars,
        "chunks_before_drop": chunks_before_drop,
        "text_before_drop_chars": text_before_drop.len(),
        "replay_frames_count": replay_frames.len(),
        "replay_user_chunks": replay_user_chunks,
        "replay_agent_chunks": replay_agent_chunks,
        "replay_assistant_chars": replay_assistant_text.len(),
        "live_agent_chunks": live_agent_chunks,
        "live_assistant_chars": live_assistant_text.len(),
        "combined_streamed_chars": combined_streamed.len(),
        "final_export_chars": final_all_text.len(),
        "missing_chars": missing_chars,
        "duplicate_chars": duplicate_chars,
        "is_prefix": is_prefix,
        "is_subsequence": is_subseq,
        "finished_present": finished_present,
        "thread_diff": thread_diff,
        "rss_diff_kb": rss_diff_kb,
        "children_changed": children_changed,
        "prompt_reply_received": prompt_reply_frame.is_some(),
        "run_end_received": run_end_frame.is_some(),
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!("[P3a1_JSON_SUMMARY] {}", summary);
    assert!(load_success, "session/load during active run must succeed");
    assert!(
        finished_present,
        "turn must complete successfully and include FINISHED"
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p3a2_same_conn_reload_sleep() -> anyhow::Result<()> {
    let Some(mut ts) = setup_session("p3a_sleep").await? else {
        return Ok(());
    };

    let goose_pid = get_goose_sandbox_pid()?;
    let stats_init = get_proc_stats(goose_pid)?;
    println!(
        "[P3a2] Session {} started on Conn A: {}, pid={}, threads={:?}, rss={:?} kB",
        ts.sid, ts.conn_id, goose_pid, stats_init.threads, stats_init.vm_rss_kb
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

    // Wait until tool call starts executing
    let wait_start = Instant::now();
    let timeout = Duration::from_secs(30);
    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[P3a2] timed out waiting for tool_call to start");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[P3a2] stream ended waiting for tool_call");
        };

        if let Event::Frame(_, ref frame) = event {
            check_no_permission_request(frame, "P3a2 pre-drop");
            let update_type = frame
                .pointer("/params/update/sessionUpdate")
                .and_then(|v| v.as_str());
            if update_type == Some("tool_call") || update_type == Some("tool_call_update") {
                println!("[P3a2] Tool call started: {update_type:?}, hard-dropping connection");
                break;
            }
        }
    }

    // 1. Hard drop connection A
    ts.upstream.hard_drop();

    // 2. Immediately resume connection A and subscribe
    let mut resumed = Upstream::resume(
        &ts.url,
        ts.secret.clone(),
        ts.conn_id.clone(),
        ts.rec.clone(),
    )?;
    resumed.subscribe(None).await?;
    let sub_window = resumed.subscribe(Some(&ts.sid)).await?;
    println!(
        "[P3a2] Resumed conn A and subscribed (409 window: {:?})",
        sub_window
    );

    let stored_cwd = get_session_cwd(&mut resumed, &ts.sid)
        .await?
        .unwrap_or_else(|| ts.temp_dir.path().to_str().unwrap().to_string());

    // Mid-run export check
    let mid_run_export = export_session(&mut resumed, &ts.sid).await;
    let mid_run_ok = mid_run_export.is_ok();
    println!("[P3a2] Mid-run export ok: {mid_run_ok}");

    let stats_before_load = get_proc_stats(goose_pid)?;
    println!("[P3a2] Pre-load children: {:?}", stats_before_load.children);

    // Send session/load on Conn A while sleep 40 is running
    println!("[P3a2] Sending session/load on connection A mid-sleep...");
    let load_req_id = resumed
        .request(
            "session/load",
            json!({
                "sessionId": &ts.sid,
                "cwd": &stored_cwd,
                "mcpServers": []
            }),
            Some(&ts.sid),
        )
        .await?;

    let mut replay_frames: Vec<Value> = Vec::new();
    let mut live_frames_after_load: Vec<Value> = Vec::new();
    let mut load_reply_stream: Option<StreamKey> = None;
    let mut load_reply_frame: Option<Value> = None;
    let mut stats_after_load: Option<ProcStats> = None;
    let mut prompt_reply_frame: Option<Value> = None;
    let mut run_end_frame: Option<Value> = None;

    let post_load_start = Instant::now();
    let post_load_timeout = Duration::from_secs(90);

    while post_load_start.elapsed() < post_load_timeout {
        let remaining = post_load_timeout - post_load_start.elapsed();
        let Some(event) = resumed.next(remaining).await? else {
            println!("[P3a2] Stream ended or timed out waiting for frames");
            break;
        };

        if let Event::Frame(key, frame) = event {
            check_no_permission_request(&frame, "P3a2 post-load");

            if frame
                .get("id")
                .is_some_and(|id| rpc_id_matches(&load_req_id, id))
            {
                println!("[P3a2] Received session/load reply on stream: {:?}", key);
                load_reply_stream = Some(key.clone());
                load_reply_frame = Some(frame.clone());
                stats_after_load = Some(get_proc_stats(goose_pid)?);
                continue;
            }

            if frame
                .get("id")
                .is_some_and(|id| rpc_id_matches(&prompt_req_id, id))
            {
                println!("[P3a2] Received prompt reply frame!");
                prompt_reply_frame = Some(frame.clone());
            }

            if is_run_end(&frame) {
                println!("[P3a2] Received run-end session_info_update!");
                run_end_frame = Some(frame.clone());
            }

            if load_reply_frame.is_none() {
                replay_frames.push(frame);
            } else {
                live_frames_after_load.push(frame);
            }

            if prompt_reply_frame.is_some() && run_end_frame.is_some() {
                let _ = tokio::time::sleep(Duration::from_millis(500)).await;
                while let Ok(Some(Event::Frame(_, extra))) =
                    resumed.next(Duration::from_millis(200)).await
                {
                    live_frames_after_load.push(extra);
                }
                break;
            }
        }
    }

    let load_success = load_reply_frame
        .as_ref()
        .is_some_and(|f| f.get("error").is_none());
    let final_export = export_session(&mut resumed, &ts.sid).await?;
    let final_text = extract_all_assistant_text(&final_export);
    let finished_present = final_text.contains("FINISHED");

    let summary = json!({
        "probe": "p3a2_sleep",
        "sessionId": ts.sid,
        "conn_id": ts.conn_id,
        "load_success": load_success,
        "load_reply_stream": load_reply_stream.map(|s| s.stream_record_name()),
        "mid_run_export_ok": mid_run_ok,
        "replay_frames_count": replay_frames.len(),
        "live_frames_after_load_count": live_frames_after_load.len(),
        "prompt_reply_received": prompt_reply_frame.is_some(),
        "run_end_received": run_end_frame.is_some(),
        "finished_present": finished_present,
        "stats_before_children": stats_before_load.children,
        "stats_after_children": stats_after_load.as_ref().map(|s| s.children.clone()),
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!("[P3a2_JSON_SUMMARY] {}", summary);
    assert!(load_success, "session/load during active run must succeed");
    assert!(finished_present, "turn must complete with FINISHED");

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p3b_reload_different_cwd() -> anyhow::Result<()> {
    let Some(mut ts) = setup_session("p3b").await? else {
        return Ok(());
    };

    // Run short prompt to ensure session has conversation in DB
    let prompt_res = ts
        .upstream
        .call(
            "session/prompt",
            json!({
                "sessionId": &ts.sid,
                "prompt": [{"type": "text", "text": "Reply with only the word HELLO."}]
            }),
            Some(&ts.sid),
            Duration::from_secs(30),
        )
        .await?;
    println!("[P3b] Initial prompt reply: {prompt_res:?}");

    let initial_cwd = get_session_cwd(&mut ts.upstream, &ts.sid)
        .await?
        .expect("session must be found in session/list");
    println!("[P3b] Initial stored cwd from session/list: {initial_cwd}");

    let new_temp = tempfile::tempdir()?;
    let new_cwd = new_temp.path().to_str().unwrap().to_string();
    println!("[P3b] New target cwd: {new_cwd}");
    assert_ne!(
        initial_cwd, new_cwd,
        "Target cwd must differ from initial cwd"
    );

    // Call session/load with new_cwd
    let load_res = ts
        .upstream
        .call(
            "session/load",
            json!({
                "sessionId": &ts.sid,
                "cwd": &new_cwd,
                "mcpServers": []
            }),
            Some(&ts.sid),
            Duration::from_secs(30),
        )
        .await?;
    println!("[P3b] session/load response: {load_res:?}");

    // Query session/list again
    let updated_cwd = get_session_cwd(&mut ts.upstream, &ts.sid)
        .await?
        .expect("session must still be found in session/list");
    println!("[P3b] Updated stored cwd from session/list: {updated_cwd}");

    let cwd_rewritten = updated_cwd == new_cwd;
    println!("[P3b] CWD rewritten by load: {cwd_rewritten}");
    assert_eq!(
        updated_cwd, new_cwd,
        "session/load with different cwd MUST rewrite stored cwd in session/list"
    );

    println!(
        "[P3b_JSON_SUMMARY] {}",
        json!({
            "probe": "p3b_cwd",
            "sessionId": ts.sid,
            "initial_cwd": initial_cwd,
            "new_cwd": new_cwd,
            "updated_cwd": updated_cwd,
            "cwd_rewritten": cwd_rewritten,
            "fixture": ts.fixture_path.display().to_string(),
        })
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p3c_diff_conn_reload_mid_run() -> anyhow::Result<()> {
    let Some(mut ts) = setup_session("p3c").await? else {
        return Ok(());
    };

    println!("[P3c] Session {} started on Conn A: {}", ts.sid, ts.conn_id);
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

    let mut a_chunks_before = 0usize;
    let wait_start = Instant::now();
    let timeout = Duration::from_secs(60);

    loop {
        if wait_start.elapsed() >= timeout {
            panic!("[P3c] timed out waiting for chunks on Conn A");
        }
        let remaining = timeout - wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[P3c] stream ended waiting for chunks on Conn A");
        };

        if let Event::Frame(_, ref frame) = event {
            check_no_permission_request(frame, "P3c Conn A pre");
            if is_agent_chunk(frame) {
                a_chunks_before += 1;
                if a_chunks_before >= 10 {
                    println!("[P3c] Conn A reached 10 chunks, spinning up Conn B");
                    break;
                }
            }
        }
    }

    // While run is active on Conn A:
    // Initialize Conn B
    let ts_num = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root");
    let fixture_b = repo_root.join(format!(
        "fixtures/goose-1.54.0-patched.2/p3c_b/{ts_num}.jsonl"
    ));
    if let Some(p) = fixture_b.parent() {
        std::fs::create_dir_all(p)?;
    }
    let rec_b = Recorder::file(&fixture_b, Redactor::from_env())?;
    let mut upstream_b = Upstream::initialize(&ts.url, ts.secret.clone(), rec_b).await?;
    println!("[P3c] Initialized Conn B: {}", upstream_b.conn_id());

    // Subscribe B to Conn stream
    upstream_b.subscribe(None).await?;

    // Probe subscribing B to session stream (short deadline, check 409 because A holds it)
    upstream_b.set_subscribe_deadline(Duration::from_millis(500));
    let b_sub_session_res = upstream_b.subscribe(Some(&ts.sid)).await;
    let b_session_409 = b_sub_session_res.is_err();
    println!(
        "[P3c] Conn B subscribing to session stream returned 409 error: {}",
        b_session_409
    );

    let stored_cwd = ts.temp_dir.path().to_str().unwrap().to_string();

    // Send session/load on Conn B
    println!("[P3c] Sending session/load on Conn B...");
    let b_load_req_id = upstream_b
        .request(
            "session/load",
            json!({
                "sessionId": &ts.sid,
                "cwd": &stored_cwd,
                "mcpServers": []
            }),
            None,
        )
        .await?;

    let mut a_chunks_after = 0usize;
    let mut b_chunks_after = 0usize;
    let mut a_got_prompt_reply = false;
    let mut b_got_prompt_reply = false;
    let mut a_got_run_end = false;
    let mut b_got_run_end = false;
    let mut b_load_reply: Option<Value> = None;
    let mut b_load_reply_stream: Option<StreamKey> = None;

    let post_start = Instant::now();
    let post_timeout = Duration::from_secs(60);

    while post_start.elapsed() < post_timeout {
        tokio::select! {
            ev_a = ts.upstream.next(Duration::from_millis(100)) => {
                if let Ok(Some(Event::Frame(_, frame))) = ev_a {
                    check_no_permission_request(&frame, "P3c Conn A post");
                    if is_agent_chunk(&frame) {
                        a_chunks_after += 1;
                    }
                    if frame.get("id").is_some_and(|id| rpc_id_matches(&prompt_req_id, id)) {
                        a_got_prompt_reply = true;
                    }
                    if is_run_end(&frame) {
                        a_got_run_end = true;
                    }
                }
            }
            ev_b = upstream_b.next(Duration::from_millis(100)) => {
                if let Ok(Some(Event::Frame(key, frame))) = ev_b {
                    check_no_permission_request(&frame, "P3c Conn B post");
                    if frame.get("id").is_some_and(|id| rpc_id_matches(&b_load_req_id, id)) {
                        b_load_reply_stream = Some(key);
                        b_load_reply = Some(frame.clone());
                        println!("[P3c] Conn B received session/load reply: {:?}", b_load_reply_stream);
                    }
                    if is_agent_chunk(&frame) {
                        b_chunks_after += 1;
                    }
                    if frame.get("id").is_some_and(|id| rpc_id_matches(&prompt_req_id, id)) {
                        b_got_prompt_reply = true;
                    }
                    if is_run_end(&frame) {
                        b_got_run_end = true;
                    }
                }
            }
        }

        if (a_got_prompt_reply || a_got_run_end) && b_load_reply.is_some() {
            let drain_until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < drain_until {
                if let Ok(Some(Event::Frame(_, frame))) =
                    ts.upstream.next(Duration::from_millis(200)).await
                    && is_agent_chunk(&frame)
                {
                    a_chunks_after += 1;
                }
                if let Ok(Some(Event::Frame(_, frame))) =
                    upstream_b.next(Duration::from_millis(200)).await
                    && is_agent_chunk(&frame)
                {
                    b_chunks_after += 1;
                }
            }
            break;
        }
    }

    let b_load_success = b_load_reply
        .as_ref()
        .is_some_and(|r| r.get("error").is_none());
    println!(
        "[P3c] Results: Conn A chunks after={a_chunks_after}, Conn B chunks after={b_chunks_after}"
    );
    println!(
        "[P3c] Conn A prompt reply={a_got_prompt_reply}, Conn B prompt reply={b_got_prompt_reply}"
    );
    println!("[P3c] Conn A run-end={a_got_run_end}, Conn B run-end={b_got_run_end}");
    println!(
        "[P3c] Conn B load reply received={}",
        b_load_reply.is_some()
    );

    println!(
        "[P3c_JSON_SUMMARY] {}",
        json!({
            "probe": "p3c_cross_conn",
            "sessionId": ts.sid,
            "conn_a": ts.conn_id,
            "conn_b": upstream_b.conn_id(),
            "b_session_sub_409": b_session_409,
            "b_load_reply_stream": b_load_reply_stream.map(|s| s.stream_record_name()),
            "b_load_success": b_load_success,
            "a_chunks_before": a_chunks_before,
            "a_chunks_after": a_chunks_after,
            "b_chunks_after": b_chunks_after,
            "a_got_prompt_reply": a_got_prompt_reply,
            "b_got_prompt_reply": b_got_prompt_reply,
            "a_got_run_end": a_got_run_end,
            "b_got_run_end": b_got_run_end,
            "fixture_a": ts.fixture_path.display().to_string(),
            "fixture_b": fixture_b.display().to_string(),
        })
    );

    Ok(())
}
