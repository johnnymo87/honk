use serde_json::Value;

pub fn is_message_user_visible(msg: &Value) -> bool {
    let meta = match msg.get("metadata") {
        Some(m) => m,
        None => return true,
    };
    if meta.get("userVisible").and_then(Value::as_bool) == Some(false) {
        return false;
    }
    if meta.get("user_visible").and_then(Value::as_bool) == Some(false) {
        return false;
    }
    true
}

fn is_content_user_visible(block: &Value) -> bool {
    let audience = block
        .pointer("/annotations/audience")
        .and_then(Value::as_array);
    if let Some(aud) = audience {
        return aud.iter().any(|v| v.as_str() == Some("user"));
    }
    true
}

fn get_user_visible_text(msg: &Value) -> String {
    let mut text = String::new();
    if let Some(content) = msg.get("content").and_then(Value::as_array) {
        for block in content {
            if !is_content_user_visible(block) {
                continue;
            }
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(t) = block.get("text").and_then(Value::as_str)
            {
                text.push_str(t);
            }
        }
    }
    text
}

pub fn to_updates(export: &Value, sid: &str) -> Vec<Value> {
    let conversation = match export.get("conversation").and_then(Value::as_array) {
        Some(c) => c,
        None => return Vec::new(),
    };

    let mut updates = Vec::new();

    for msg in conversation {
        if !is_message_user_visible(msg) {
            continue;
        }

        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let message_id = msg.get("id").and_then(Value::as_str).unwrap_or("");

        let mut goose_meta = serde_json::Map::new();
        goose_meta.insert(
            "messageId".to_string(),
            Value::String(message_id.to_string()),
        );
        if let Some(created) = msg.get("created") {
            goose_meta.insert("created".to_string(), created.clone());
        }

        let content = match msg.get("content").and_then(Value::as_array) {
            Some(c) => c,
            None => continue,
        };

        for block in content {
            if !is_content_user_visible(block) {
                continue;
            }

            let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");

            match block_type {
                "text" => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    let session_update = if role == "user" {
                        "user_message_chunk"
                    } else {
                        "agent_message_chunk"
                    };

                    let update = serde_json::json!({
                        "sessionUpdate": session_update,
                        "messageId": message_id,
                        "content": {
                            "type": "text",
                            "text": text,
                        },
                        "_meta": {
                            "goose": goose_meta.clone(),
                        }
                    });

                    updates.push(serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": update,
                        }
                    }));
                }
                "thinking" => {
                    let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                    let update = serde_json::json!({
                        "sessionUpdate": "agent_thought_chunk",
                        "messageId": message_id,
                        "content": {
                            "type": "text",
                            "text": thinking,
                        },
                        "_meta": {
                            "goose": goose_meta.clone(),
                        }
                    });

                    updates.push(serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": update,
                        }
                    }));
                }
                "toolRequest" => {
                    let tool_call_id = block.get("id").and_then(Value::as_str).unwrap_or("");
                    let title = block
                        .pointer("/toolCall/value/name")
                        .or_else(|| block.pointer("/toolCall/name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");

                    let update = serde_json::json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": tool_call_id,
                        "title": title,
                        "_meta": {
                            "goose": goose_meta.clone(),
                        }
                    });

                    updates.push(serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": update,
                        }
                    }));
                }
                "toolResponse" => {
                    let tool_call_id = block.get("id").and_then(Value::as_str).unwrap_or("");
                    let status = if block.pointer("/toolResult/status").and_then(Value::as_str)
                        != Some("success")
                        || block
                            .pointer("/toolResult/value/isError")
                            .and_then(Value::as_bool)
                            == Some(true)
                        || block
                            .pointer("/toolResult/isError")
                            .and_then(Value::as_bool)
                            == Some(true)
                    {
                        "failed"
                    } else {
                        "completed"
                    };

                    let update = serde_json::json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": tool_call_id,
                        "status": status,
                        "_meta": {
                            "goose": goose_meta.clone(),
                        }
                    });

                    updates.push(serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": update,
                        }
                    }));
                }
                _ => {}
            }
        }
    }

    updates
}

pub fn turn_text(export: &Value, user_text: &str) -> String {
    let conversation = match export.get("conversation").and_then(Value::as_array) {
        Some(c) => c,
        None => return String::new(),
    };

    let mut last_matching_user_idx = None;

    for (i, msg) in conversation.iter().enumerate() {
        if !is_message_user_visible(msg) {
            continue;
        }
        if msg.get("role").and_then(Value::as_str) == Some("user") {
            let user_msg_text = get_user_visible_text(msg);
            if user_msg_text == user_text {
                last_matching_user_idx = Some(i);
            }
        }
    }

    let Some(idx) = last_matching_user_idx else {
        return String::new();
    };

    let mut out = String::new();
    for msg in &conversation[idx + 1..] {
        if !is_message_user_visible(msg) {
            continue;
        }
        if msg.get("role").and_then(Value::as_str) == Some("assistant") {
            let Some(content) = msg.get("content").and_then(Value::as_array) else {
                continue;
            };
            for block in content {
                if !is_content_user_visible(block) {
                    continue;
                }
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = block.get("text").and_then(Value::as_str)
                {
                    out.push_str(t);
                }
            }
        }
    }

    out
}

/// Extracts client user prompts from an export transcript.
///
/// Filters for:
/// - `role == "user"`
/// - `is_message_user_visible(msg)` is true (drops synthetic turnContext messages)
/// - content contains no `toolResponse` or `toolResult` blocks (drops tool confirmations)
///
/// Returns the concatenated user-visible text for each client prompt message in order.
pub fn client_prompts(export: &Value) -> Vec<String> {
    let conversation = match export.get("conversation").and_then(Value::as_array) {
        Some(c) => c,
        None => return Vec::new(),
    };

    let mut prompts = Vec::new();

    for msg in conversation {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if !is_message_user_visible(msg) {
            continue;
        }
        let Some(content) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        let has_tool_response = content.iter().any(|block| {
            let b_type = block.get("type").and_then(Value::as_str);
            b_type == Some("toolResponse")
                || b_type == Some("toolResult")
                || block.get("toolResult").is_some()
                || block.get("toolResponse").is_some()
        });
        if has_tool_response {
            continue;
        }

        let mut text = String::new();
        for block in content {
            if !is_content_user_visible(block) {
                continue;
            }
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(t) = block.get("text").and_then(Value::as_str)
            {
                text.push_str(t);
            }
        }
        prompts.push(text);
    }

    prompts
}
