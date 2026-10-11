use honk_testgoose::{StubGoose, Turn};
use honk_upstream::{Event, ReplayOutcome, SubscribeOpts, Upstream};
use serde_json::json;
use std::time::{Duration, Instant};

fn is_agent_chunk(frame: &serde_json::Value) -> bool {
    frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str())
        == Some("agent_message_chunk")
}

fn chunk_text(frame: &serde_json::Value) -> Option<&str> {
    if !is_agent_chunk(frame) {
        return None;
    }
    frame
        .pointer("/params/update/content/text")
        .and_then(|v| v.as_str())
}

fn is_run_end(frame: &serde_json::Value) -> bool {
    if frame
        .pointer("/params/update/sessionUpdate")
        .and_then(|v| v.as_str())
        == Some("session_info_update")
    {
        let active_run = frame.pointer("/params/update/_meta/goose/activeRunId");
        active_run == Some(&serde_json::Value::Null) || active_run.is_none()
    } else {
        false
    }
}

fn is_permission_request(frame: &serde_json::Value) -> bool {
    frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission")
}

#[tokio::test]
async fn check1_text_turn() -> anyhow::Result<()> {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return Ok(());
    };

    goose.model().push([Turn::Text(vec![
        "Hello from ".to_string(),
        "stub model!".to_string(),
    ])]);

    let mut up = Upstream::initialize(
        &goose.url(),
        goose.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let new_res = up
        .call(
            "session/new",
            json!({
                "cwd": goose.cwd().to_str().unwrap(),
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid)).await?;

    let prompt_id = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "say hello"}]
            }),
            Some(&sid),
        )
        .await?;

    let mut chunks = Vec::new();
    let mut run_ended = false;
    let mut prompt_replied = false;
    let timeout = Duration::from_secs(30);
    let start = Instant::now();

    while (!run_ended || !prompt_replied) && start.elapsed() < timeout {
        let Some(event) = up.next(Duration::from_millis(500)).await? else {
            continue;
        };
        if let Event::Frame(_, frame, _) = event {
            if let Some(text) = chunk_text(&frame) {
                chunks.push(text.to_string());
            }
            if is_run_end(&frame) {
                run_ended = true;
            }
            if frame
                .get("id")
                .is_some_and(|id| honk_upstream::rpc_id_matches(&prompt_id, id))
            {
                prompt_replied = true;
            }
        }
    }

    assert!(run_ended, "run should have ended");
    assert!(prompt_replied, "prompt should have received reply");
    let full_text = chunks.concat();
    assert_eq!(full_text, "Hello from stub model!");

    up.delete().await?;
    Ok(())
}

#[tokio::test]
async fn check2_tool_turn() -> anyhow::Result<()> {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return Ok(());
    };

    goose.model().push([
        Turn::ToolCall {
            tool: "shell".to_string(),
            arguments: json!({"command": "echo hi"}),
        },
        Turn::Text(vec!["tool finished".to_string()]),
    ]);

    let mut up = Upstream::initialize(
        &goose.url(),
        goose.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let new_res = up
        .call(
            "session/new",
            json!({
                "cwd": goose.cwd().to_str().unwrap(),
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid)).await?;

    up.call(
        "session/set_mode",
        json!({"sessionId": &sid, "modeId": "auto"}),
        Some(&sid),
        Duration::from_secs(30),
    )
    .await?;

    let prompt_id = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "run echo"}]
            }),
            Some(&sid),
        )
        .await?;

    let mut saw_tool_call = false;
    let mut saw_tool_completed = false;
    let mut text_chunks = Vec::new();
    let mut run_ended = false;
    let mut prompt_replied = false;
    let timeout = Duration::from_secs(30);
    let start = Instant::now();

    while (!run_ended || !prompt_replied) && start.elapsed() < timeout {
        let Some(event) = up.next(Duration::from_millis(500)).await? else {
            continue;
        };
        if let Event::Frame(_, frame, _) = event {
            let update_type = frame
                .pointer("/params/update/sessionUpdate")
                .and_then(|v| v.as_str());
            if update_type == Some("tool_call") {
                saw_tool_call = true;
            }
            if update_type == Some("tool_call_update") {
                let status = frame
                    .pointer("/params/update/status")
                    .and_then(|v| v.as_str());
                if status == Some("completed") {
                    saw_tool_completed = true;
                }
            }
            if let Some(text) = chunk_text(&frame) {
                text_chunks.push(text.to_string());
            }
            if is_run_end(&frame) {
                run_ended = true;
            }
            if frame
                .get("id")
                .is_some_and(|id| honk_upstream::rpc_id_matches(&prompt_id, id))
            {
                prompt_replied = true;
            }
        }
    }

    assert!(saw_tool_call, "tool_call frame expected");
    assert!(saw_tool_completed, "tool_call_update completed expected");
    assert_eq!(text_chunks.concat(), "tool finished");

    let export = goose.export(&sid).await?;
    let export_str = export.to_string();
    assert!(
        export_str.contains("echo hi"),
        "export should contain tool invocation: {export_str}"
    );
    assert!(
        export_str.contains("tool finished"),
        "export should contain assistant text: {export_str}"
    );

    up.delete().await?;
    Ok(())
}

#[tokio::test]
async fn check3_approve_mode() -> anyhow::Result<()> {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return Ok(());
    };

    goose.model().push([
        Turn::ToolCall {
            tool: "shell".to_string(),
            arguments: json!({"command": "echo approve_success"}),
        },
        Turn::Text(vec!["approved and done".to_string()]),
    ]);

    let mut up = Upstream::initialize(
        &goose.url(),
        goose.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let new_res = up
        .call(
            "session/new",
            json!({
                "cwd": goose.cwd().to_str().unwrap(),
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid)).await?;

    up.call(
        "session/set_mode",
        json!({"sessionId": &sid, "modeId": "approve"}),
        Some(&sid),
        Duration::from_secs(30),
    )
    .await?;

    let prompt_id = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "run echo in approve mode"}]
            }),
            Some(&sid),
        )
        .await?;

    let mut answered_permission = false;
    let mut saw_tool_completed = false;
    let mut text_chunks = Vec::new();
    let mut run_ended = false;
    let mut prompt_replied = false;
    let timeout = Duration::from_secs(30);
    let start = Instant::now();

    while (!run_ended || !prompt_replied) && start.elapsed() < timeout {
        let Some(event) = up.next(Duration::from_millis(500)).await? else {
            continue;
        };
        if let Event::Frame(_, frame, _) = event {
            if is_permission_request(&frame) && !answered_permission {
                let req_id = frame.get("id").cloned().expect("permission request id");
                up.respond(
                    req_id,
                    json!({
                        "outcome": {
                            "outcome": "selected",
                            "optionId": "allow_once"
                        }
                    }),
                    Some(&sid),
                )
                .await?;
                answered_permission = true;
            }
            if frame
                .pointer("/params/update/sessionUpdate")
                .and_then(|v| v.as_str())
                == Some("tool_call_update")
            {
                let status = frame
                    .pointer("/params/update/status")
                    .and_then(|v| v.as_str());
                if status == Some("completed") {
                    saw_tool_completed = true;
                }
            }
            if let Some(text) = chunk_text(&frame) {
                text_chunks.push(text.to_string());
            }
            if is_run_end(&frame) {
                run_ended = true;
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
        answered_permission,
        "request_permission should have arrived and been answered"
    );
    assert!(
        saw_tool_completed,
        "tool should have completed after permission"
    );
    assert_eq!(text_chunks.concat(), "approved and done");

    up.delete().await?;
    Ok(())
}

#[tokio::test]
async fn check4_gate_busy_steer_cancel() -> anyhow::Result<()> {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return Ok(());
    };

    goose.model().push([
        Turn::ToolCall {
            tool: "shell".to_string(),
            arguments: json!({"command": "echo gate_step1"}),
        },
        Turn::Gate(
            "hold_turn2".to_string(),
            Box::new(Turn::Text(vec!["after gate".to_string()])),
        ),
    ]);

    let mut up = Upstream::initialize(
        &goose.url(),
        goose.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let new_res = up
        .call(
            "session/new",
            json!({
                "cwd": goose.cwd().to_str().unwrap(),
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid)).await?;

    let prompt_id = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "start gated run"}]
            }),
            Some(&sid),
        )
        .await?;

    // Wait until the gate is reached (2 turn requests received by StubModel)
    // and record activeRunId from session_info_update
    let mut active_run_id = None;
    let start = Instant::now();
    loop {
        while let Some(event) = up.next(Duration::from_millis(50)).await? {
            if let Event::Frame(_, frame, _) = event
                && let Some(rid) = frame
                    .pointer("/params/update/_meta/goose/activeRunId")
                    .and_then(|v| v.as_str())
            {
                active_run_id = Some(rid.to_string());
            }
        }
        if goose.model().turn_requests().len() >= 2 {
            break;
        }
        anyhow::ensure!(
            start.elapsed() < Duration::from_secs(15),
            "gate not reached in time"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 1. Session is busy: a concurrent prompt must receive error code -32602
    let busy_res = up
        .call(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "concurrent prompt"}]
            }),
            Some(&sid),
            Duration::from_secs(10),
        )
        .await;
    assert!(
        busy_res.is_err(),
        "concurrent prompt must fail while run is active"
    );
    let err_str = busy_res.unwrap_err().to_string();
    assert!(
        err_str.contains("-32602") || err_str.contains("already has active run"),
        "error must indicate busy active run: {err_str}"
    );

    // 2. Steer is accepted
    let run_id = active_run_id.expect("active run id observed before gate");
    let steer_res = up
        .call(
            "_goose/unstable/session/steer",
            json!({
                "sessionId": &sid,
                "expectedRunId": run_id,
                "prompt": [{"type": "text", "text": "steer instruction"}]
            }),
            Some(&sid),
            Duration::from_secs(10),
        )
        .await?;
    assert!(
        steer_res.get("messageId").is_some() || steer_res.get("runId").is_some(),
        "steer response should carry messageId or runId: {steer_res}"
    );

    // 3. Cancel the run
    up.notify("session/cancel", json!({"sessionId": &sid}), Some(&sid))
        .await?;

    // Open gate so held model request can release if needed
    goose.model().open("hold_turn2");

    // Wait for prompt reply with stopReason: "cancelled"
    let mut stop_reason = None;
    let cancel_start = Instant::now();
    while stop_reason.is_none() && cancel_start.elapsed() < Duration::from_secs(15) {
        let Some(event) = up.next(Duration::from_millis(500)).await? else {
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

    up.delete().await?;
    Ok(())
}

#[tokio::test]
async fn check5_takeover_and_replay() -> anyhow::Result<()> {
    let Some(goose) = StubGoose::start_or_skip().await else {
        return Ok(());
    };

    goose.model().push([Turn::Text(vec![
        "frame 1 ".to_string(),
        "frame 2".to_string(),
    ])]);

    let mut up = Upstream::initialize(
        &goose.url(),
        goose.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;
    let features = up
        .stream_features()
        .expect("features present after subscribe");
    assert!(
        features.is_patched(),
        "stub goose should advertise takeover and replay: {features:?}"
    );

    let new_res = up
        .call(
            "session/new",
            json!({
                "cwd": goose.cwd().to_str().unwrap(),
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid)).await?;

    let _prompt_id = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "replay test"}]
            }),
            Some(&sid),
        )
        .await?;

    let mut session_frame_ids = Vec::new();
    let timeout = Duration::from_secs(30);
    let start = Instant::now();

    while start.elapsed() < timeout {
        let Some(event) = up.next(Duration::from_millis(500)).await? else {
            continue;
        };
        if let Event::Frame(key, frame, id) = event {
            if let honk_upstream::StreamKey::Session(s) = &key
                && s == &sid
                && let Some(frame_id) = id
            {
                session_frame_ids.push(frame_id);
            }
            if is_run_end(&frame) {
                break;
            }
        }
    }

    assert!(
        session_frame_ids.len() >= 2,
        "expected multiple frame ids on session stream, got: {session_frame_ids:?}"
    );
    let resume_from = session_frame_ids[0]; // drop from first id, expect replay of subsequent frames

    // Resume the existing connection to test takeover & replay
    let mut resumed = Upstream::resume(
        &goose.url(),
        goose.secret().to_string(),
        up.conn_id().to_string(),
        honk_upstream::Recorder::disabled(),
    )?;

    let sess_outcome = resumed
        .subscribe_with(
            Some(&sid),
            SubscribeOpts {
                takeover: true,
                resume_after: Some(resume_from),
            },
        )
        .await?;

    assert_eq!(
        sess_outcome.replay,
        ReplayOutcome::Complete,
        "replay outcome should be Complete"
    );

    let mut replayed_ids = Vec::new();
    let r_start = Instant::now();
    while r_start.elapsed() < Duration::from_secs(5) {
        let Some(event) = resumed.next(Duration::from_millis(200)).await? else {
            continue;
        };
        if let Event::Frame(key, _frame, id) = event
            && let honk_upstream::StreamKey::Session(s) = &key
            && s == &sid
            && let Some(frame_id) = id
        {
            replayed_ids.push(frame_id);
        }
    }

    assert!(
        !replayed_ids.is_empty(),
        "should receive replayed frames after {resume_from}"
    );
    assert!(
        replayed_ids.iter().all(|id| *id > resume_from),
        "all replayed frames must have id > {resume_from}, got: {replayed_ids:?}"
    );

    resumed.delete().await?;
    Ok(())
}

#[tokio::test]
async fn check6_kill_restart_export() -> anyhow::Result<()> {
    let Some(mut goose) = StubGoose::start_or_skip().await else {
        return Ok(());
    };

    goose
        .model()
        .push([Turn::Text(vec!["committed text".to_string()])]);

    let mut up = Upstream::initialize(
        &goose.url(),
        goose.secret().to_string(),
        honk_upstream::Recorder::disabled(),
    )
    .await?;
    up.subscribe(None).await?;

    let new_res = up
        .call(
            "session/new",
            json!({
                "cwd": goose.cwd().to_str().unwrap(),
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;
    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new")
        .to_string();

    up.subscribe(Some(&sid)).await?;

    let prompt_id = up
        .request(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{"type": "text", "text": "persist me"}]
            }),
            Some(&sid),
        )
        .await?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        let Some(event) = up.next(Duration::from_millis(500)).await? else {
            continue;
        };
        if let Event::Frame(_, frame, _) = event
            && frame
                .get("id")
                .is_some_and(|id| honk_upstream::rpc_id_matches(&prompt_id, id))
        {
            break;
        }
    }

    // Verify export works before kill
    let export_before = goose.export(&sid).await?;
    assert!(export_before.to_string().contains("committed text"));

    // 1. Kill child
    goose.kill9();

    // 2. Old connection requests fail / return error
    let fail_res = up
        .call("session/list", json!({}), None, Duration::from_secs(2))
        .await;
    assert!(
        fail_res.is_err(),
        "requests on old connection must fail after goose kill"
    );

    // 3. Restart goose
    goose.restart().await;

    // 4. Export works on respawned goose
    let export_after = goose.export(&sid).await?;
    let export_after_str = export_after.to_string();
    assert!(
        export_after_str.contains("committed text"),
        "export after restart should contain committed text: {export_after_str}"
    );

    Ok(())
}
