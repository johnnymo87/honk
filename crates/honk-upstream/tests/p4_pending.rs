mod common;
use common::*;

use std::time::{Duration, Instant};

use honk_upstream::{Event, Upstream, rpc_id_matches};
use serde_json::{Value, json};

fn is_permission_request(frame: &Value) -> bool {
    frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission")
}

fn get_permission_options(frame: &Value) -> Vec<String> {
    frame
        .pointer("/params/options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|opt| {
                    opt.get("id")
                        .and_then(|i| i.as_str())
                        .or_else(|| opt.get("kind").and_then(|k| k.as_str()))
                        .map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn is_tool_call_chunk(frame: &Value) -> bool {
    let update_type = frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str());
    update_type == Some("tool_call") || update_type == Some("tool_call_update")
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

fn allow_once_outcome() -> Value {
    json!({
        "outcome": {
            "outcome": "selected",
            "optionId": "allow_once"
        }
    })
}

fn check_export_tool_output(session_json: &Value, needle: &str) -> bool {
    if let Some(convo) = session_json.get("conversation").and_then(|v| v.as_array()) {
        for msg in convo {
            if let Some(contents) = msg.get("content").and_then(|c| c.as_array()) {
                for block in contents {
                    let b_type = block.get("type").and_then(|t| t.as_str());
                    if b_type == Some("toolResponse") && block.to_string().contains(needle) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[tokio::test]
#[ignore]
async fn p4a1_no_listener_90s() -> anyhow::Result<()> {
    run_p4a_case("p4a1", 90).await
}

#[tokio::test]
#[ignore]
async fn p4a2_no_listener_90s() -> anyhow::Result<()> {
    run_p4a_case("p4a2", 90).await
}

async fn run_p4a_case(case_name: &str, wait_seconds: u64) -> anyhow::Result<()> {
    let Some(mut ts) = setup_approve_session("p4a").await? else {
        return Ok(());
    };

    let start_total = Instant::now();
    println!(
        "[{case_name}] Session {} started on Conn A: {}",
        ts.sid, ts.conn_id
    );

    let prompt = "Run the shell command \"echo P4OK\", then reply with only the word FINISHED.";
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

    let prompt_wait_timeout = Duration::from_secs(60);
    let prompt_wait_start = Instant::now();

    let (orig_id, perm_options) = loop {
        if prompt_wait_start.elapsed() >= prompt_wait_timeout {
            panic!("[{case_name}] Timed out waiting for session/request_permission");
        }
        let remaining = prompt_wait_timeout - prompt_wait_start.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[{case_name}] Stream closed while waiting for session/request_permission");
        };

        if let Event::Frame(_, ref frame, _) = event
            && is_permission_request(frame)
        {
            let id = frame.get("id").cloned().expect("permission request id");
            let opts = get_permission_options(frame);
            println!(
                "[{case_name}] Received permission request: id={:?}, options={:?}",
                id, opts
            );
            break (id, opts);
        }
    };

    // 1. Hard drop WITHOUT answering
    println!(
        "[{case_name}] Hard-dropping connection WITHOUT answering, waiting {wait_seconds}s..."
    );
    ts.upstream.hard_drop();
    let drop_start = Instant::now();
    tokio::time::sleep(Duration::from_secs(wait_seconds)).await;
    let drop_elapsed = drop_start.elapsed();

    // 2. Re-attach (resume + subscribe)
    println!(
        "[{case_name}] Reconnecting after {:.1}s via resume...",
        drop_elapsed.as_secs_f64()
    );
    let mut upstream = Upstream::resume(
        &ts.url,
        ts.secret.clone(),
        ts.conn_id.clone(),
        ts.rec.clone(),
    )?;
    upstream.subscribe(None).await?;
    upstream.subscribe(Some(&ts.sid)).await?;

    // 3. Check if redelivered on stream
    println!("[{case_name}] Checking if permission request is redelivered on stream...");
    let mut redelivered_id: Option<Value> = None;
    let check_redelivery_start = Instant::now();
    while check_redelivery_start.elapsed() < Duration::from_secs(5) {
        let remaining = Duration::from_secs(5) - check_redelivery_start.elapsed();
        if let Ok(Some(Event::Frame(_, frame, _))) = upstream.next(remaining).await
            && is_permission_request(&frame)
        {
            redelivered_id = frame.get("id").cloned();
            println!(
                "[{case_name}] Permission request REDELIVERED! id={:?}",
                redelivered_id
            );
            break;
        }
    }

    let redelivered = redelivered_id.is_some();
    println!(
        "[{case_name}] Redelivery result: redelivered={redelivered}, redelivered_id={:?}",
        redelivered_id
    );

    // 4. If not redelivered, try session/load on the same connection
    let mut resent_on_load = false;
    let mut load_perm_id: Option<Value> = None;
    if !redelivered {
        println!(
            "[{case_name}] Request not redelivered on stream, issuing session/load on same connection..."
        );
        let load_res = upstream
            .call(
                "session/load",
                json!({
                    "sessionId": &ts.sid,
                    "cwd": ts.temp_dir.path().to_str().unwrap(),
                    "mcpServers": []
                }),
                Some(&ts.sid),
                Duration::from_secs(30),
            )
            .await;
        println!("[{case_name}] session/load result: {:?}", load_res.is_ok());

        let check_load_start = Instant::now();
        while check_load_start.elapsed() < Duration::from_secs(5) {
            let remaining = Duration::from_secs(5) - check_load_start.elapsed();
            if let Ok(Some(Event::Frame(_, frame, _))) = upstream.next(remaining).await
                && is_permission_request(&frame)
            {
                load_perm_id = frame.get("id").cloned();
                resent_on_load = true;
                println!(
                    "[{case_name}] Permission request RE-SENT via session/load! id={:?}",
                    load_perm_id
                );
                break;
            }
        }
    }

    // 5. Answer allow_once (to whichever request we have: load_perm_id, redelivered_id, or orig_id)
    let target_perm_id = load_perm_id
        .clone()
        .or_else(|| redelivered_id.clone())
        .unwrap_or_else(|| orig_id.clone());
    let is_stale_id = target_perm_id == orig_id && !redelivered && !resent_on_load;
    println!(
        "[{case_name}] Answering permission request target_id={:?} (stale_id={is_stale_id}) with allow_once...",
        target_perm_id
    );

    let answer_start = Instant::now();
    let respond_res = upstream
        .respond(target_perm_id.clone(), allow_once_outcome(), Some(&ts.sid))
        .await;
    let respond_ok = respond_res.is_ok();
    println!(
        "[{case_name}] respond result: ok={respond_ok}, err={:?}",
        respond_res.as_ref().err()
    );

    // 6. Wait for turn to finish (watch for prompt response or run end)
    println!("[{case_name}] Waiting for tool execution and turn finish...");
    let finish_timeout = Duration::from_secs(60);
    let finish_start = Instant::now();
    let mut saw_run_end = false;
    let mut saw_prompt_reply = false;

    while finish_start.elapsed() < finish_timeout {
        let remaining = finish_timeout - finish_start.elapsed();
        let Some(event) = upstream.next(remaining).await? else {
            println!("[{case_name}] Stream ended while waiting for turn finish");
            break;
        };

        if let Event::Frame(_, ref frame, _) = event {
            if is_run_end(frame) {
                saw_run_end = true;
                println!("[{case_name}] Saw run end");
            }
            if rpc_id_matches(&prompt_req_id, frame.get("id").unwrap_or(&Value::Null)) {
                saw_prompt_reply = true;
                println!("[{case_name}] Saw prompt reply: {:?}", frame);
                break;
            }
            if saw_run_end && finish_start.elapsed() > Duration::from_secs(5) {
                break;
            }
        }
    }

    let post_answer_duration = answer_start.elapsed();

    // 7. Export session to verify tool output and final assistant message
    let session_json = export_session(&mut upstream, &ts.sid).await?;
    let tool_ran = check_export_tool_output(&session_json, "P4OK");
    let assistant_text = extract_assistant_text(&session_json);
    let all_text = extract_all_assistant_text(&session_json);
    let turn_finished = assistant_text.contains("FINISHED") || all_text.contains("FINISHED");
    let total_duration = start_total.elapsed();

    let summary = json!({
        "case": case_name,
        "session_id": ts.sid,
        "conn_id": ts.conn_id,
        "original_perm_id": orig_id,
        "perm_options": perm_options,
        "wait_seconds": wait_seconds,
        "drop_elapsed_s": drop_elapsed.as_secs_f64(),
        "redelivered": redelivered,
        "redelivered_id": redelivered_id,
        "resent_on_load": resent_on_load,
        "load_perm_id": load_perm_id,
        "answered_id": target_perm_id,
        "is_stale_id": is_stale_id,
        "respond_ok": respond_ok,
        "tool_ran": tool_ran,
        "turn_finished": turn_finished,
        "saw_run_end": saw_run_end,
        "saw_prompt_reply": saw_prompt_reply,
        "post_answer_duration_s": post_answer_duration.as_secs_f64(),
        "total_duration_s": total_duration.as_secs_f64(),
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!(
        "[{case_name}] SUMMARY: {}",
        serde_json::to_string_pretty(&summary)?
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p4b1_lost_permission() -> anyhow::Result<()> {
    run_p4b_case("p4b1").await
}

#[tokio::test]
#[ignore]
async fn p4b2_lost_permission() -> anyhow::Result<()> {
    run_p4b_case("p4b2").await
}

async fn run_p4b_case(case_name: &str) -> anyhow::Result<()> {
    let Some(mut ts) = setup_approve_session("p4b").await? else {
        return Ok(());
    };

    let start_total = Instant::now();
    println!(
        "[{case_name}] Session {} started on Conn A: {}",
        ts.sid, ts.conn_id
    );

    let prompt = "Run the shell command \"echo P4OK\", then reply with only the word FINISHED.";
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

    let mut perm_read_before_drop = false;
    let timeout = Duration::from_secs(60);
    let start_wait = Instant::now();

    loop {
        if start_wait.elapsed() >= timeout {
            panic!("[{case_name}] Timed out waiting for tool_call");
        }
        let remaining = timeout - start_wait.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[{case_name}] Stream closed waiting for tool_call");
        };

        if let Event::Frame(_, ref frame, _) = event {
            if is_permission_request(frame) {
                perm_read_before_drop = true;
                println!("[{case_name}] Note: permission request arrived before tool_call drop!");
            }
            if is_tool_call_chunk(frame) {
                println!(
                    "[{case_name}] tool_call appeared! Immediately hard-dropping connection (best-effort lost-permission)..."
                );
                ts.upstream.hard_drop();
                break;
            }
        }
    }
    println!(
        "[{case_name}] Connection dropped. perm_read_before_drop={perm_read_before_drop}. Pausing 2s before resume..."
    );
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Resume + subscribe
    println!("[{case_name}] Reconnecting via resume...");
    let mut upstream = Upstream::resume(
        &ts.url,
        ts.secret.clone(),
        ts.conn_id.clone(),
        ts.rec.clone(),
    )?;
    upstream.subscribe(None).await?;
    upstream.subscribe(Some(&ts.sid)).await?;

    // Check if permission frame is delivered after resume
    println!("[{case_name}] Checking if permission request arrives on resumed stream...");
    let mut perm_id_after_resume: Option<Value> = None;
    let resume_check_start = Instant::now();
    while resume_check_start.elapsed() < Duration::from_secs(5) {
        let remaining = Duration::from_secs(5) - resume_check_start.elapsed();
        if let Ok(Some(Event::Frame(_, frame, _))) = upstream.next(remaining).await
            && is_permission_request(&frame)
        {
            perm_id_after_resume = frame.get("id").cloned();
            println!(
                "[{case_name}] Permission request received on resumed stream! id={:?}",
                perm_id_after_resume
            );
            break;
        }
    }

    // If not delivered on resume, try session/load on same connection
    let mut resent_on_load = false;
    let mut load_perm_id: Option<Value> = None;
    if perm_id_after_resume.is_none() {
        println!(
            "[{case_name}] Permission request not received on resume; issuing session/load on same connection..."
        );
        let load_res = upstream
            .call(
                "session/load",
                json!({
                    "sessionId": &ts.sid,
                    "cwd": ts.temp_dir.path().to_str().unwrap(),
                    "mcpServers": []
                }),
                Some(&ts.sid),
                Duration::from_secs(30),
            )
            .await;
        println!("[{case_name}] session/load result: {:?}", load_res.is_ok());

        let load_check_start = Instant::now();
        while load_check_start.elapsed() < Duration::from_secs(5) {
            let remaining = Duration::from_secs(5) - load_check_start.elapsed();
            if let Ok(Some(Event::Frame(_, frame, _))) = upstream.next(remaining).await
                && is_permission_request(&frame)
            {
                load_perm_id = frame.get("id").cloned();
                resent_on_load = true;
                println!(
                    "[{case_name}] Permission request RE-SENT via session/load! id={:?}",
                    load_perm_id
                );
                break;
            }
        }
    }

    // Answer allow_once if we have an ID
    let target_perm_id = load_perm_id
        .clone()
        .or_else(|| perm_id_after_resume.clone());
    let mut respond_ok = false;
    if let Some(ref pid) = target_perm_id {
        println!("[{case_name}] Answering permission request target_id={pid:?} with allow_once...");
        let respond_res = upstream
            .respond(pid.clone(), allow_once_outcome(), Some(&ts.sid))
            .await;
        respond_ok = respond_res.is_ok();
        println!(
            "[{case_name}] respond result: ok={respond_ok}, err={:?}",
            respond_res.as_ref().err()
        );
    } else {
        println!("[{case_name}] No permission request id available to answer!");
    }

    // Wait for turn completion
    println!("[{case_name}] Waiting for turn completion...");
    let finish_timeout = Duration::from_secs(60);
    let finish_start = Instant::now();
    let mut saw_run_end = false;
    let mut saw_prompt_reply = false;

    while finish_start.elapsed() < finish_timeout {
        let remaining = finish_timeout - finish_start.elapsed();
        let Some(event) = upstream.next(remaining).await? else {
            break;
        };
        if let Event::Frame(_, ref frame, _) = event {
            if is_run_end(frame) {
                saw_run_end = true;
            }
            if rpc_id_matches(&prompt_req_id, frame.get("id").unwrap_or(&Value::Null)) {
                saw_prompt_reply = true;
                break;
            }
            if saw_run_end && finish_start.elapsed() > Duration::from_secs(5) {
                break;
            }
        }
    }

    let session_json = export_session(&mut upstream, &ts.sid).await?;
    let tool_ran = check_export_tool_output(&session_json, "P4OK");
    let assistant_text = extract_assistant_text(&session_json);
    let all_text = extract_all_assistant_text(&session_json);
    let turn_finished = assistant_text.contains("FINISHED") || all_text.contains("FINISHED");
    let total_duration = start_total.elapsed();

    let summary = json!({
        "case": case_name,
        "session_id": ts.sid,
        "conn_id": ts.conn_id,
        "perm_read_before_drop": perm_read_before_drop,
        "perm_id_after_resume": perm_id_after_resume,
        "resent_on_load": resent_on_load,
        "load_perm_id": load_perm_id,
        "answered_id": target_perm_id,
        "respond_ok": respond_ok,
        "tool_ran": tool_ran,
        "turn_finished": turn_finished,
        "saw_run_end": saw_run_end,
        "saw_prompt_reply": saw_prompt_reply,
        "total_duration_s": total_duration.as_secs_f64(),
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!(
        "[{case_name}] SUMMARY: {}",
        serde_json::to_string_pretty(&summary)?
    );

    Ok(())
}

#[tokio::test]
#[ignore]
async fn p4c1_teardown_sm_off() -> anyhow::Result<()> {
    run_p4c_case("p4c1", false).await
}

#[tokio::test]
#[ignore]
async fn p4c2_teardown_sm_off() -> anyhow::Result<()> {
    run_p4c_case("p4c2", false).await
}

#[tokio::test]
#[ignore]
async fn p4c3_teardown_sm_on() -> anyhow::Result<()> {
    run_p4c_case("p4c3", true).await
}

#[tokio::test]
#[ignore]
async fn p4c4_teardown_sm_on() -> anyhow::Result<()> {
    run_p4c_case("p4c4", true).await
}

async fn run_p4c_case(case_name: &str, state_machine: bool) -> anyhow::Result<()> {
    let subfolder = if state_machine {
        "p4c_sm_on"
    } else {
        "p4c_sm_off"
    };
    let Some(mut ts) = setup_approve_session(subfolder).await? else {
        return Ok(());
    };

    let start_total = Instant::now();
    println!(
        "[{case_name}] Session {} started on Conn A: {} (state_machine={state_machine})",
        ts.sid, ts.conn_id
    );

    let prompt = "Run the shell command \"echo P4OK\", then reply with only the word FINISHED.";
    let prompt_body = if state_machine {
        json!({
            "sessionId": &ts.sid,
            "prompt": [{"type": "text", "text": prompt}],
            "_meta": {
                "goose": {
                    "unrolledAgentLoop": true
                }
            }
        })
    } else {
        json!({
            "sessionId": &ts.sid,
            "prompt": [{"type": "text", "text": prompt}],
            "_meta": {
                "goose": {
                    "unrolledAgentLoop": false
                }
            }
        })
    };

    let _prompt_req_id = ts
        .upstream
        .request("session/prompt", prompt_body, Some(&ts.sid))
        .await?;

    let timeout = Duration::from_secs(60);
    let start_wait = Instant::now();

    let (orig_id, perm_options) = loop {
        if start_wait.elapsed() >= timeout {
            panic!("[{case_name}] Timed out waiting for permission request on Conn A");
        }
        let remaining = timeout - start_wait.elapsed();
        let Some(event) = ts.upstream.next(remaining).await? else {
            panic!("[{case_name}] Stream closed on Conn A");
        };

        if let Event::Frame(_, ref frame, _) = event
            && is_permission_request(frame)
        {
            let id = frame.get("id").cloned().expect("permission id on Conn A");
            let opts = get_permission_options(frame);
            println!(
                "[{case_name}] Received permission request on Conn A: id={:?}, options={:?}",
                id, opts
            );
            break (id, opts);
        }
    };

    // Cancel-style teardown: DELETE connection A
    println!("[{case_name}] Sending DELETE to Conn A (cancelling run)...");
    ts.upstream.delete().await?;
    println!("[{case_name}] Conn A deleted.");

    // New connection Conn B
    println!("[{case_name}] Initializing Conn B...");
    let mut upstream_b = Upstream::initialize(&ts.url, ts.secret.clone(), ts.rec.clone()).await?;
    let conn_b_id = upstream_b.conn_id().to_string();
    println!("[{case_name}] Conn B initialized: {conn_b_id}");
    upstream_b.subscribe(None).await?;
    upstream_b.subscribe(Some(&ts.sid)).await?;

    // session/load on Conn B
    println!("[{case_name}] Calling session/load on Conn B...");
    let load_body = if state_machine {
        json!({
            "sessionId": &ts.sid,
            "cwd": ts.temp_dir.path().to_str().unwrap(),
            "mcpServers": [],
            "_meta": {
                "goose": {
                    "unrolledAgentLoop": true
                }
            }
        })
    } else {
        json!({
            "sessionId": &ts.sid,
            "cwd": ts.temp_dir.path().to_str().unwrap(),
            "mcpServers": []
        })
    };
    let load_res = upstream_b
        .call(
            "session/load",
            load_body,
            Some(&ts.sid),
            Duration::from_secs(30),
        )
        .await;
    println!(
        "[{case_name}] session/load on Conn B: {:?}",
        load_res.is_ok()
    );

    // Check if a persisted permission is re-sent on Conn B
    println!("[{case_name}] Reading Conn B stream for re-sent permission request...");
    let mut resent_perm_id: Option<Value> = None;
    let check_start = Instant::now();
    while check_start.elapsed() < Duration::from_secs(10) {
        let remaining = Duration::from_secs(10) - check_start.elapsed();
        if let Ok(Some(Event::Frame(_, frame, _))) = upstream_b.next(remaining).await
            && is_permission_request(&frame)
        {
            resent_perm_id = frame.get("id").cloned();
            println!(
                "[{case_name}] Persisted permission request RE-SENT on Conn B! id={:?}",
                resent_perm_id
            );
            break;
        }
    }

    let is_resent = resent_perm_id.is_some();
    println!(
        "[{case_name}] Persisted permission re-sent on Conn B: {is_resent} (id={:?})",
        resent_perm_id
    );

    let (answer_sent, answer_respond_ok, answered_id) = if let Some(pid) = resent_perm_id.clone() {
        println!("[{case_name}] Answering re-sent permission request with allow_once on Conn B...");
        let resp_res = upstream_b
            .respond(pid.clone(), allow_once_outcome(), Some(&ts.sid))
            .await;
        let ok = resp_res.is_ok();
        println!(
            "[{case_name}] respond to re-sent request: ok={ok}, err={:?}",
            resp_res.as_ref().err()
        );
        (true, ok, Some(pid))
    } else {
        // Attempt answering original perm id on Conn B to see if cross-connection answering works
        println!(
            "[{case_name}] Attempting to answer original perm id {:?} on Conn B...",
            orig_id
        );
        let resp_res = upstream_b
            .respond(orig_id.clone(), allow_once_outcome(), Some(&ts.sid))
            .await;
        let ok = resp_res.is_ok();
        println!(
            "[{case_name}] respond to orig_id on Conn B: ok={ok}, err={:?}",
            resp_res.as_ref().err()
        );
        (true, ok, Some(orig_id.clone()))
    };

    // Wait 10s for any activity on Conn B stream (runs, tools, etc.)
    println!("[{case_name}] Waiting 10s for any events on Conn B...");
    let wait_activity_start = Instant::now();
    let mut activity_events = 0usize;
    while wait_activity_start.elapsed() < Duration::from_secs(10) {
        let remaining = Duration::from_secs(10) - wait_activity_start.elapsed();
        if let Ok(Some(Event::Frame(_, ref frame, _))) = upstream_b.next(remaining).await {
            activity_events += 1;
            println!(
                "[{case_name}] Conn B received event: {:?}",
                frame.get("method")
            );
        }
    }

    // Export session from Conn B
    let session_json = export_session(&mut upstream_b, &ts.sid).await?;
    let tool_ran = check_export_tool_output(&session_json, "P4OK");
    let assistant_text = extract_assistant_text(&session_json);
    let all_text = extract_all_assistant_text(&session_json);
    let turn_finished = assistant_text.contains("FINISHED") || all_text.contains("FINISHED");
    let total_duration = start_total.elapsed();

    let summary = json!({
        "case": case_name,
        "state_machine": state_machine,
        "session_id": ts.sid,
        "conn_a_id": ts.conn_id,
        "conn_b_id": conn_b_id,
        "orig_perm_id": orig_id,
        "perm_options": perm_options,
        "is_resent_on_conn_b": is_resent,
        "resent_perm_id": resent_perm_id,
        "answer_sent": answer_sent,
        "answered_id": answered_id,
        "answer_respond_ok": answer_respond_ok,
        "tool_ran": tool_ran,
        "turn_finished": turn_finished,
        "conn_b_activity_events": activity_events,
        "total_duration_s": total_duration.as_secs_f64(),
        "fixture": ts.fixture_path.display().to_string(),
    });

    println!(
        "[{case_name}] SUMMARY: {}",
        serde_json::to_string_pretty(&summary)?
    );

    Ok(())
}

#[test]
fn p4d_elicitation_untested() {
    // Elicitation forms: marked untested per spike plan §P4.4.
    // The sandbox is configured only with the developer extension (command execution),
    // and has no eliciting MCP server installed.
    println!("[P4d] Elicitation marked UNTESTED (no eliciting MCP in sandbox)");
}
