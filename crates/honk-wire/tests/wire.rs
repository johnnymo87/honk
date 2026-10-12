use honk_wire::export::{client_prompts, to_updates, turn_text};
use honk_wire::{BlockKey, Blocks, block_key, run_state};
use serde_json::Value;
use std::fs::read_to_string;

const EXPORT_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/goose-1.54.0-patched.2/canonical/export.json"
);
const LIVE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/goose-1.54.0-patched.2/canonical/live.jsonl"
);
const REPLAY_FULL_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/goose-1.54.0-patched.2/canonical/replay-full.jsonl"
);

fn load_jsonl(path: &str) -> Vec<Value> {
    let content = read_to_string(path).expect("read jsonl");
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse jsonl line"))
        .collect()
}

fn load_json(path: &str) -> Value {
    let content = read_to_string(path).expect("read json");
    serde_json::from_str(&content).expect("parse json")
}

#[test]
fn test_block_key_canonical_fixtures() {
    let live_frames = load_jsonl(LIVE_PATH);
    let mut live_content_updates = 0;
    let mut live_keys = Vec::new();

    for row in &live_frames {
        let frame = row.get("frame").unwrap_or(row);
        let Some(u) = frame.pointer("/params/update") else {
            continue;
        };
        let kind = u.get("sessionUpdate").and_then(Value::as_str);
        if matches!(
            kind,
            Some(
                "user_message_chunk"
                    | "agent_message_chunk"
                    | "agent_thought_chunk"
                    | "tool_call"
                    | "tool_call_update"
            )
        ) {
            live_content_updates += 1;
            let key = block_key(u).expect("every content update has a key");
            if !live_keys.contains(&key) {
                live_keys.push(key);
            }
        }
    }
    assert!(live_content_updates > 0);

    // Thought, text and tool of one message are distinct blocks
    let thought_key = BlockKey::Thought("msg_vrtx_011CfucR7pLauqNsvK8QQHqt".into());
    let text_key = BlockKey::Text("msg_vrtx_011CfucR7pLauqNsvK8QQHqt".into());
    let tool_key = BlockKey::Tool("toolu_vrtx_01UCgfne7TkMfcDo4ZDRQkxm".into());
    assert!(live_keys.contains(&thought_key));
    assert!(live_keys.contains(&text_key));
    assert!(live_keys.contains(&tool_key));
    assert_ne!(thought_key, text_key);

    // Replay keys follow live first-seen order (A7)
    // Note: User prompt is not echoed live to sender, so compare non-user keys.
    let replay_frames = load_jsonl(REPLAY_FULL_PATH);
    let mut replay_keys = Vec::new();
    for row in &replay_frames {
        let frame = row.get("frame").unwrap_or(row);
        let Some(u) = frame.pointer("/params/update") else {
            continue;
        };
        if let Some(key) = block_key(u)
            && !replay_keys.contains(&key)
        {
            replay_keys.push(key);
        }
    }

    let non_user_live: Vec<_> = live_keys
        .into_iter()
        .filter(|k| !matches!(k, BlockKey::User(_)))
        .collect();
    let non_user_replay: Vec<_> = replay_keys
        .into_iter()
        .filter(|k| !matches!(k, BlockKey::User(_)))
        .collect();

    assert_eq!(non_user_replay, non_user_live);
}

#[test]
fn test_run_state() {
    let start = serde_json::json!({
        "sessionUpdate": "session_info_update",
        "_meta": {
            "goose": {
                "activeRunId": "run_6f2cb817"
            }
        }
    });
    assert_eq!(run_state(&start), Some(Some("run_6f2cb817".to_string())));

    let end = serde_json::json!({
        "sessionUpdate": "session_info_update",
        "_meta": {
            "goose": {
                "activeRunId": null
            }
        }
    });
    assert_eq!(run_state(&end), Some(None));

    let queued_steer = serde_json::json!({
        "sessionUpdate": "session_info_update",
        "_meta": {
            "goose": {
                "queuedSteer": true
            }
        }
    });
    assert_eq!(run_state(&queued_steer), None);

    let title_update = serde_json::json!({
        "sessionUpdate": "session_info_update",
        "title": "Scratch file operations practice"
    });
    assert_eq!(run_state(&title_update), None);

    let non_info = serde_json::json!({
        "sessionUpdate": "user_message_chunk"
    });
    assert_eq!(run_state(&non_info), None);
}

#[test]
fn test_to_updates_and_blocks_reduce() {
    let export = load_json(EXPORT_PATH);
    let updates = to_updates(&export, "20261011_85");

    let mut export_blocks = Blocks::default();
    for u in &updates {
        export_blocks.apply(u);
    }

    let replay_frames = load_jsonl(REPLAY_FULL_PATH);
    let mut replay_blocks = Blocks::default();
    for row in &replay_frames {
        let frame = row.get("frame").unwrap_or(row);
        replay_blocks.apply(frame);
    }

    assert_eq!(export_blocks.agent_text(), replay_blocks.agent_text());
    assert_eq!(export_blocks.tool_title.len(), 3);
    assert_eq!(
        export_blocks.tool_title.len(),
        replay_blocks.tool_title.len()
    );

    // Second user message in export.json (the turnContext injection) is excluded
    // exactly when goose's replay excludes it; assert against replay-full's user chunks
    assert_eq!(export_blocks.user_texts(), replay_blocks.user_texts());
    assert_eq!(export_blocks.user_texts().len(), 1);
}

#[test]
fn test_turn_text_equals_live_agent_chunks() {
    let export = load_json(EXPORT_PATH);
    let canonical_prompt = export
        .pointer("/conversation/0/content/0/text")
        .and_then(Value::as_str)
        .expect("canonical prompt");

    let live_frames = load_jsonl(LIVE_PATH);
    let mut live_agent_text = String::new();
    for row in &live_frames {
        let frame = row.get("frame").unwrap_or(row);
        let Some(u) = frame.pointer("/params/update") else {
            continue;
        };
        if u.get("sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk")
            && let Some(t) = u.pointer("/content/text").and_then(Value::as_str)
        {
            live_agent_text.push_str(t);
        }
    }

    let computed_turn_text = turn_text(&export, canonical_prompt);
    assert_eq!(computed_turn_text, live_agent_text);
    assert!(!computed_turn_text.is_empty());
}

#[test]
fn test_client_prompts_canonical_fixture() {
    let export = load_json(EXPORT_PATH);
    let prompts = client_prompts(&export);

    // Canonical export.json has 1 client prompt:
    // conversation[0] = User message with text
    // conversation[1] = User message with turnContext (userVisible: false)
    // conversation[2] = Assistant message with toolRequest
    // conversation[3] = User message with toolResponse
    // conversation[4] = Assistant message with toolRequest
    // conversation[5] = User message with toolResponse
    // conversation[6] = Assistant message with toolRequest
    // conversation[7] = User message with toolResponse
    // conversation[8] = Assistant message with final text
    // Only conversation[0] is a client user prompt!
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].starts_with("Think first about what to do."));
}

#[test]
fn test_client_prompts_multiturn_fixture() {
    // Test with a multi-prompt fixture or custom json containing:
    // - normal user prompt
    // - turn-context message
    // - toolResponse message
    // - second user prompt
    let session = serde_json::json!({
        "conversation": [
            {
                "role": "user",
                "metadata": { "userVisible": true },
                "content": [{ "type": "text", "text": "first prompt" }]
            },
            {
                "role": "user",
                "metadata": { "userVisible": false, "turnContext": true },
                "content": [{ "type": "text", "text": "<turn-context>..." }]
            },
            {
                "role": "assistant",
                "content": [{ "type": "toolRequest", "toolCall": { "name": "shell" } }]
            },
            {
                "role": "user",
                "metadata": { "userVisible": true },
                "content": [{ "type": "toolResponse", "toolResult": { "output": "ok" } }]
            },
            {
                "role": "assistant",
                "content": [{ "type": "text", "text": "done 1" }]
            },
            {
                "role": "user",
                "metadata": { "userVisible": true },
                "content": [{ "type": "text", "text": "second prompt" }]
            }
        ]
    });

    let prompts = client_prompts(&session);
    assert_eq!(
        prompts,
        vec!["first prompt".to_string(), "second prompt".to_string()]
    );
}
