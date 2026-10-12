use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use honk_upstream::{Event, Recorder, Redactor, StreamKey, Upstream};

#[tokio::test]
#[ignore]
async fn live_smoke() -> anyhow::Result<()> {
    let secret = match std::env::var("HONK_GOOSE_SECRET") {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            eprintln!("skipping live_smoke: HONK_GOOSE_SECRET unset");
            return Ok(());
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
    let fixture_path = repo_root.join(format!("fixtures/goose-1.54.0-patched.2/smoke/{ts}.jsonl"));

    let redactor = Redactor::from_env();
    let rec = Recorder::file(&fixture_path, redactor)?;

    println!(
        "Starting live_smoke against {url} recording to {}",
        fixture_path.display()
    );

    // 1. Initialize
    let mut upstream = Upstream::initialize(&url, secret, rec).await?;
    println!("Initialized connection: {}", upstream.conn_id());
    if let Some(init_res) = upstream.init_result() {
        println!("init_result: {}", serde_json::to_string_pretty(init_res)?);
    }

    // 2. Subscribe connection stream
    upstream.subscribe(None).await?;
    println!("Subscribed to connection stream");

    // 3. session/new
    let temp_dir = tempfile::tempdir()?;
    let cwd = temp_dir.path().to_str().unwrap().to_string();
    let new_res = upstream
        .call(
            "session/new",
            serde_json::json!({
                "cwd": cwd,
                "mcpServers": []
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;

    println!(
        "session/new reply: {}",
        serde_json::to_string_pretty(&new_res)?
    );

    // Assert reply arrived on the connection stream
    assert_eq!(
        upstream.last_call_stream(),
        Some(&StreamKey::Conn),
        "session/new reply must arrive on the CONN stream"
    );

    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("session/new reply must contain sessionId")
        .to_string();
    println!("Got sessionId: {sid}");

    // 4. Subscribe to session stream
    upstream.subscribe(Some(&sid)).await?;
    println!("Subscribed to session stream: {sid}");

    // 5. session/set_mode
    // Inspect offered modes in session/new or init_result
    let mode_id = "auto";
    println!("Setting mode to: {mode_id}");
    let mode_res = upstream
        .call(
            "session/set_mode",
            serde_json::json!({
                "sessionId": &sid,
                "modeId": mode_id
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;
    println!("session/set_mode response: {mode_res:?}");

    // 6. Inspect config options for models; set Haiku model if offered
    if let Some(config_options) = new_res
        .get("configOptions")
        .and_then(|v| v.as_array())
        .or_else(|| {
            upstream
                .init_result()
                .and_then(|r| r.get("configOptions"))
                .and_then(|v| v.as_array())
        })
    {
        println!("Inspecting {} config options", config_options.len());
        let mut model_found = None;
        for opt in config_options {
            if opt.get("id").and_then(|v| v.as_str()) == Some("model")
                || opt.get("category").and_then(|v| v.as_str()) == Some("model")
            {
                if let Some(current) =
                    opt.get("currentValue")
                        .and_then(|v| v.as_str())
                        .or_else(|| {
                            opt.pointer("/kind/select/current_value")
                                .and_then(|v| v.as_str())
                        })
                {
                    println!("Current model: {current}");
                }
                let options_arr = opt.get("options").and_then(|v| v.as_array()).or_else(|| {
                    opt.pointer("/kind/select/options")
                        .and_then(|v| v.as_array())
                });

                if let Some(options) = options_arr {
                    for option in options {
                        if let Some(val) = option.get("value").and_then(|v| v.as_str()) {
                            println!("Available model option: {val}");
                            if val.to_lowercase().contains("haiku") && model_found.is_none() {
                                model_found = Some(val.to_string());
                            }
                        }
                    }
                }
            }
        }

        if let Some(haiku_model) = model_found {
            println!("Setting model to Haiku model: {haiku_model}");
            let model_res = upstream
                .call(
                    "session/set_config_option",
                    serde_json::json!({
                        "sessionId": &sid,
                        "configId": "model",
                        "value": haiku_model
                    }),
                    Some(&sid),
                    Duration::from_secs(30),
                )
                .await?;
            println!("set_config_option model response: {model_res:?}");
        } else {
            println!("No Haiku model offered; leaving default model");
        }
    } else {
        println!("No configOptions returned; leaving default model");
    }

    // 7. session/prompt
    println!("Sending session/prompt");
    let prompt_req_id = upstream
        .request(
            "session/prompt",
            serde_json::json!({
                "sessionId": &sid,
                "prompt": [
                    {"type": "text", "text": "Reply with only the word PONG."}
                ]
            }),
            Some(&sid),
        )
        .await?;
    println!("Prompt request id: {prompt_req_id}");

    let mut accumulated_text = String::new();
    let timeout = Duration::from_secs(60);
    let start = Instant::now();

    loop {
        if start.elapsed() >= timeout {
            panic!("timed out waiting for prompt response and PONG");
        }
        let remaining = timeout - start.elapsed();
        let event_opt = upstream.next(remaining).await?;
        let Some(event) = event_opt else {
            panic!("timed out waiting for prompt frames");
        };

        let (stream_key, frame) = match event {
            Event::Frame(key, frame) => (key, frame),
            Event::StreamEnded { key, error } => {
                panic!("stream {key:?} ended unexpectedly: {error:?}");
            }
        };

        println!(
            "Received frame on {stream_key:?}: {}",
            serde_json::to_string(&frame)?
        );

        // Fail loudly if request_permission arrives
        if frame.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
            panic!("unexpected session/request_permission in auto mode: {frame}");
        }

        // Collect text chunks from agent_message_chunk
        if let Some(params) = frame.get("params")
            && let Some(update) = params.get("update")
            && update.get("sessionUpdate").and_then(|v| v.as_str()) == Some("agent_message_chunk")
        {
            if let Some(text) = update.pointer("/content/text").and_then(|v| v.as_str()) {
                accumulated_text.push_str(text);
            } else if let Some(text) = update.get("text").and_then(|v| v.as_str()) {
                accumulated_text.push_str(text);
            }
        }

        // Check if matching prompt response arrived
        if frame.get("id") == Some(&prompt_req_id) {
            if let Some(err) = frame.get("error") {
                panic!("prompt failed with RPC error: {err}");
            }
            let result = frame.get("result").expect("prompt response missing result");
            assert!(
                result.get("stopReason").is_some(),
                "prompt result must contain stopReason: {result}"
            );
            println!("Prompt completed with result: {result}");
            break;
        }
    }

    println!("All accumulated text: '{accumulated_text}'");
    assert!(
        accumulated_text.contains("PONG"),
        "agent_message_chunk text must contain 'PONG', got: '{accumulated_text}'"
    );

    // 8. delete()
    println!("Deleting connection");
    let del_status = upstream.delete().await?;
    println!("Delete status: {del_status}");
    assert!(del_status.is_success() || del_status == reqwest::StatusCode::ACCEPTED);

    println!("live_smoke test completed successfully!");
    Ok(())
}
