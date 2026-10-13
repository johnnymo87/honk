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

/// Helper that iterates through conversation and yields `(conversation_idx, client_prompt_text)`
/// for all user messages that qualify as client prompts.
pub fn client_prompt_entries<'a>(
    conversation: &'a [Value],
) -> impl Iterator<Item = (usize, String)> + 'a {
    conversation.iter().enumerate().filter_map(|(i, msg)| {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            return None;
        }
        if !is_message_user_visible(msg) {
            return None;
        }
        let is_steer = msg
            .pointer("/metadata/steer")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_steer {
            return None;
        }
        let content = msg.get("content").and_then(Value::as_array)?;
        let has_tool_response = content.iter().any(|block| {
            let b_type = block.get("type").and_then(Value::as_str);
            b_type == Some("toolResponse")
                || b_type == Some("toolResult")
                || block.get("toolResult").is_some()
                || block.get("toolResponse").is_some()
        });
        if has_tool_response {
            return None;
        }

        Some((i, get_user_visible_text(msg)))
    })
}

pub fn turn_text_at_index(
    export: &Value,
    prompt_index: usize,
    expected_text: &str,
) -> Option<String> {
    if expected_text.is_empty() {
        return None;
    }
    let conversation = export.get("conversation").and_then(Value::as_array)?;
    let (idx, text) = client_prompt_entries(conversation).nth(prompt_index)?;
    if text != expected_text {
        return None;
    }
    extract_assistant_text_after(conversation, idx)
}

pub fn turn_text(export: &Value, user_text: &str) -> Option<String> {
    if user_text.is_empty() {
        return None;
    }
    let conversation = export.get("conversation").and_then(Value::as_array)?;
    let mut last_idx = None;
    for (idx, text) in client_prompt_entries(conversation) {
        if text == user_text {
            last_idx = Some(idx);
        }
    }
    let idx = last_idx?;
    extract_assistant_text_after(conversation, idx)
}

fn extract_assistant_text_after(conversation: &[Value], idx: usize) -> Option<String> {
    let mut out = String::new();
    for msg in &conversation[idx + 1..] {
        if !is_message_user_visible(msg) {
            continue;
        }
        // A turn's text stops at the next non-steer client user prompt!
        if msg.get("role").and_then(Value::as_str) == Some("user") {
            let is_steer = msg
                .pointer("/metadata/steer")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let has_tool_response =
                msg.get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|content| {
                        content.iter().any(|block| {
                            let b_type = block.get("type").and_then(Value::as_str);
                            b_type == Some("toolResponse")
                                || b_type == Some("toolResult")
                                || block.get("toolResult").is_some()
                                || block.get("toolResponse").is_some()
                        })
                    });
            if !is_steer && !has_tool_response {
                break;
            }
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

    Some(out)
}

/// Extracts client user prompts from an export transcript.
///
/// Filters for:
/// - `role == "user"`
/// - `is_message_user_visible(msg)` is true (drops synthetic turnContext messages)
/// - content contains no `toolResponse` or `toolResult` blocks (drops tool confirmations)
/// - message metadata has no `steer == true` (drops consumed steer messages)
///
/// Returns the concatenated user-visible text for each client prompt message in order.
pub fn client_prompts(export: &Value) -> Vec<String> {
    let conversation = match export.get("conversation").and_then(Value::as_array) {
        Some(c) => c,
        None => return Vec::new(),
    };
    client_prompt_entries(conversation)
        .map(|(_, text)| text)
        .collect()
}
