//! stdio MCP server exposing pigeon swarm tools for goose sessions.
//!
//! # Lifecycle and Persistence
//!
//! Goose persists the swarm extension's command and arguments per session in its database.
//! After changing honkd's `--swarm-*` flags, existing sessions need `/mcp disable swarm`
//! followed by `/mcp enable swarm` to pick up updated arguments or binary paths.
//!
//! Removing the flags leaves already-enabled sessions with the extension configured;
//! disable it with `/mcp disable swarm` before removing the flags.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

pub const ECONOMY_FOOTER: &str = "[pigeon] Message economy — the unit of cost is the message, not the word. \
    Reading this cost the receiving session a full reasoning turn, and so will \
    every follow-up. Do NOT ack, thank, heartbeat, or ping. Reply only if you \
    carry a decision only the recipient can make, new evidence, a retraction, \
    or the deliverable itself. Otherwise act silently — a quiet session making \
    progress is the healthy state. If you have several things to say, hold them \
    and send ONE message at your next checkpoint instead of a back-and-forth.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwarmMcpServerConfig {
    pub session: String,
    pub pigeon_url: String,
    pub pigeon_token_file: Option<PathBuf>,
    pub honk_url: String,
    pub honk_secret_file: Option<PathBuf>,
}

fn read_token(path: Option<&Path>) -> Option<String> {
    let path = path?;
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn check_caller_identity(config: &SwarmMcpServerConfig, params: &Value) -> Result<(), String> {
    let caller_id = params
        .get("_meta")
        .and_then(|m| m.get("agent-session-id"))
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| std::env::var("AGENT_SESSION_ID").ok())
        .filter(|s| !s.trim().is_empty());

    match caller_id {
        Some(id) if id == config.session => Ok(()),
        _ => Err(format!(
            "swarm tools belong to goose session {}; subagents cannot use them",
            config.session
        )),
    }
}

fn tool_ok(text: impl Into<String>) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": text.into()
            }
        ]
    })
}

fn tool_err(text: impl Into<String>) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": text.into()
            }
        ],
        "isError": true
    })
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn format_relative_time(updated_ms: i64, now_ms: i64) -> String {
    let diff_ms = now_ms.saturating_sub(updated_ms);
    let secs = diff_ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn resolve_call_cwd(meta: Option<&Value>) -> Option<String> {
    if let Some(agent_cwd) = meta
        .and_then(|m| m.get("agent-working-dir"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        Some(agent_cwd.to_string())
    } else {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    }
}

async fn ensure_mailbox(
    client: &reqwest::Client,
    config: &SwarmMcpServerConfig,
    cwd: Option<&str>,
) -> Result<String, String> {
    let endpoint = crate::pigeon::acp_endpoint(&config.honk_url)
        .map_err(|e| format!("invalid honk URL {}: {e}", config.honk_url))?;
    let secret = read_token(config.honk_secret_file.as_deref()).unwrap_or_default();
    let pigeon_token = read_token(config.pigeon_token_file.as_deref());

    let mut body = json!({
        "session_id": config.session,
        "endpoint": endpoint,
        "auth_token": secret,
        "label": config.session,
    });
    if let Some(dir) = cwd {
        body["cwd"] = json!(dir);
    }

    let mut req = client
        .post(format!(
            "{}/goose/swarm-mailbox",
            config.pigeon_url.trim_end_matches('/')
        ))
        .json(&body);
    if let Some(tok) = pigeon_token {
        req = req.bearer_auth(tok);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("pigeon /goose/swarm-mailbox unreachable: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let err_msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!(
            "pigeon /goose/swarm-mailbox failed (HTTP {status}): {err_msg}"
        ));
    }

    let body: Value = serde_json::from_str(&text)
        .map_err(|e| format!("pigeon /goose/swarm-mailbox returned non-JSON: {e}"))?;
    let session_id = body
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("pigeon response missing session_id: {body}"))?;
    if !session_id.starts_with("gse_") {
        return Err(format!(
            "session '{}' has legacy pigeon session id '{}' (expected 'gse_…'); re-register with `honk telegram {} --force`",
            config.session, session_id, config.session
        ));
    }
    Ok(session_id.to_string())
}

async fn handle_swarm_send(
    client: &reqwest::Client,
    config: &SwarmMcpServerConfig,
    args: &Value,
    cwd: Option<&str>,
) -> Value {
    let to = match args.get("to").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => t.trim(),
        _ => return tool_err("missing or empty 'to' argument"),
    };
    let message = match args.get("message").and_then(Value::as_str) {
        Some(m) => m,
        _ => return tool_err("missing or empty 'message' argument"),
    };
    let kind = args.get("kind").and_then(Value::as_str);
    let priority = args.get("priority").and_then(Value::as_str);
    let reply_to = args.get("reply_to").and_then(Value::as_str);

    let from = match ensure_mailbox(client, config, cwd).await {
        Ok(f) => f,
        Err(e) => return tool_err(e),
    };

    let mut send_body = json!({
        "from": from,
        "to": to,
        "payload": message,
    });
    if let Some(k) = kind {
        send_body["kind"] = json!(k);
    }
    if let Some(p) = priority {
        send_body["priority"] = json!(p);
    }
    if let Some(r) = reply_to {
        send_body["reply_to"] = json!(r);
    }

    let pigeon_token = read_token(config.pigeon_token_file.as_deref());
    let mut req = client
        .post(format!(
            "{}/swarm/send",
            config.pigeon_url.trim_end_matches('/')
        ))
        .json(&send_body);
    if let Some(tok) = &pigeon_token {
        req = req.bearer_auth(tok);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return tool_err(format!("pigeon /swarm/send unreachable: {e}")),
    };

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let err_msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return tool_err(err_msg);
    }

    let send_res: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return tool_err(format!("pigeon returned non-JSON: {e}")),
    };

    let msg_id = send_res
        .get("msg_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let banked = send_res
        .get("banked")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut result_text = format!("Queued {msg_id} -> {to}");
    if banked {
        result_text.push_str(" (waiting in its mailbox)");
    }

    // Query pending count for sender
    let mut pending_req = client
        .get(format!(
            "{}/pull/pending",
            config.pigeon_url.trim_end_matches('/')
        ))
        .query(&[("session", &from)]);
    if let Some(tok) = &pigeon_token {
        pending_req = pending_req.bearer_auth(tok);
    }
    if let Ok(pending_resp) = pending_req.send().await
        && pending_resp.status().is_success()
        && let Ok(pending_val) = pending_resp.json::<Value>().await
    {
        let unread = pending_val
            .get("by_source")
            .and_then(|bs| bs.get("swarm"))
            .and_then(Value::as_u64)
            .or_else(|| pending_val.get("pending").and_then(Value::as_u64))
            .unwrap_or(0);
        if unread > 0 {
            result_text.push_str(&format!(
                "\n\nYou have {unread} unread swarm messages — call swarm_read."
            ));
        }
    }

    tool_ok(result_text)
}

async fn handle_swarm_read(
    client: &reqwest::Client,
    config: &SwarmMcpServerConfig,
    args: &Value,
    cwd: Option<&str>,
) -> Value {
    let limit = args.get("limit").and_then(Value::as_u64);

    let session_id = match ensure_mailbox(client, config, cwd).await {
        Ok(s) => s,
        Err(e) => return tool_err(e),
    };

    let mut drain_body = json!({
        "session_id": session_id,
    });
    if let Some(l) = limit {
        drain_body["limit"] = json!(l);
    }

    let pigeon_token = read_token(config.pigeon_token_file.as_deref());
    let mut drain_req = client
        .post(format!(
            "{}/pull/drain",
            config.pigeon_url.trim_end_matches('/')
        ))
        .json(&drain_body);
    if let Some(tok) = &pigeon_token {
        drain_req = drain_req.bearer_auth(tok);
    }

    let resp = match drain_req.send().await {
        Ok(r) => r,
        Err(e) => return tool_err(format!("pigeon /pull/drain unreachable: {e}")),
    };

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let err_msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return tool_err(err_msg);
    }

    let drain_res: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return tool_err(format!("pigeon returned non-JSON: {e}")),
    };

    let messages = drain_res
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if messages.is_empty() {
        return tool_ok("Inbox is empty.");
    }

    // Ack returned messages
    let msg_ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.get("msg_id").and_then(Value::as_str))
        .map(String::from)
        .collect();

    let mut ack_req = client
        .post(format!(
            "{}/pull/ack",
            config.pigeon_url.trim_end_matches('/')
        ))
        .json(&json!({
            "session_id": session_id,
            "msg_ids": msg_ids,
        }));
    if let Some(tok) = &pigeon_token {
        ack_req = ack_req.bearer_auth(tok);
    }
    let mut ack_warning: Option<String> = None;
    match ack_req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                let n = msg_ids.len();
                let s = if n == 1 {
                    "1 message"
                } else {
                    &format!("{n} messages")
                };
                let msg = format!("warning: {s} could not be acknowledged and may be shown again");
                eprintln!(
                    "honk-swarm: warning: failed to ack drained messages (HTTP {status}): {text}"
                );
                ack_warning = Some(msg);
            } else if let Ok(val) = serde_json::from_str::<Value>(&text) {
                if let Some(rej) = val.get("rejected").and_then(Value::as_array)
                    && !rej.is_empty()
                {
                    let n = rej.len();
                    let s = if n == 1 {
                        "1 message"
                    } else {
                        &format!("{n} messages")
                    };
                    let msg =
                        format!("warning: {s} could not be acknowledged and may be shown again");
                    eprintln!("honk-swarm: warning: {n} messages rejected on ack: {rej:?}");
                    ack_warning = Some(msg);
                }
            }
        }
        Err(e) => {
            let n = msg_ids.len();
            let s = if n == 1 {
                "1 message"
            } else {
                &format!("{n} messages")
            };
            let msg = format!("warning: {s} could not be acknowledged and may be shown again");
            eprintln!("honk-swarm: warning: failed to ack drained messages: {e}");
            ack_warning = Some(msg);
        }
    }

    let pending_total = drain_res
        .get("pending_total")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut blocks = Vec::new();
    for m in &messages {
        let kind = m.get("kind").and_then(Value::as_str).unwrap_or("chat");
        let from = m
            .get("sender_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let msg_id = m.get("msg_id").and_then(Value::as_str).unwrap_or("");
        let priority = m.get("priority").and_then(Value::as_str);
        let reply_to = m.get("reply_to").and_then(Value::as_str);
        let redelivered = m
            .get("redelivered")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let payload = m.get("payload").and_then(Value::as_str).unwrap_or("");

        let mut attrs = format!(
            r#"v="1" kind="{}" from="{}" msg_id="{}""#,
            xml_escape(kind),
            xml_escape(from),
            xml_escape(msg_id),
        );
        if let Some(p) = priority {
            attrs.push_str(&format!(r#" priority="{}""#, xml_escape(p)));
        }
        if let Some(r) = reply_to {
            attrs.push_str(&format!(r#" reply_to="{}""#, xml_escape(r)));
        }
        if redelivered {
            attrs.push_str(r#" redelivered="true""#);
        }
        let block =
            format!("<swarm_message {attrs}>\n{payload}\n</swarm_message>\n\n{ECONOMY_FOOTER}");
        blocks.push(block);
    }

    let mut result_text = blocks.join("\n\n");
    let returned_count = messages.len() as u64;
    if pending_total > returned_count {
        let remaining = pending_total - returned_count;
        let s = if remaining == 1 { "" } else { "s" };
        result_text.push_str(&format!(
            "\n\n({remaining} more unread message{s} in inbox — call swarm_read again to drain.)"
        ));
    }
    if let Some(warn) = ack_warning {
        result_text.push_str(&format!("\n\n{warn}"));
    }

    tool_ok(result_text)
}

async fn handle_swarm_list(
    client: &reqwest::Client,
    config: &SwarmMcpServerConfig,
    args: &Value,
) -> Value {
    let limit = args.get("limit").and_then(Value::as_u64);

    let pigeon_token = read_token(config.pigeon_token_file.as_deref());
    let mut list_req = client
        .get(format!(
            "{}/sessions",
            config.pigeon_url.trim_end_matches('/')
        ))
        .query(&[("active", "true")]);
    if let Some(tok) = &pigeon_token {
        list_req = list_req.bearer_auth(tok);
    }

    let resp = match list_req.send().await {
        Ok(r) => r,
        Err(e) => return tool_err(format!("pigeon /sessions unreachable: {e}")),
    };

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let err_msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return tool_err(err_msg);
    }

    let body: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return tool_err(format!("pigeon returned non-JSON: {e}")),
    };

    let mut sessions = body
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if let Some(l) = limit {
        sessions.truncate(l as usize);
    }

    if sessions.is_empty() {
        return tool_ok("No active sessions found.");
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let mut lines = Vec::new();
    for s in &sessions {
        let session_id = s.get("session_id").and_then(Value::as_str).unwrap_or("");
        let backend_session_id = s.get("backend_session_id").and_then(Value::as_str);
        let backend_kind = s.get("backend_kind").and_then(Value::as_str);
        let pull_sources: Vec<&str> = s
            .get("pull_sources")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let cwd = s.get("cwd").and_then(Value::as_str).unwrap_or("");
        let title = s.get("title").and_then(Value::as_str).unwrap_or("");

        let is_you = backend_session_id == Some(&config.session) || session_id == config.session;
        let you = if is_you { " (you)" } else { "" };

        let kind = match backend_kind {
            Some("goose-acp") | Some("goose-pull") => {
                if pull_sources.contains(&"swarm") {
                    "goose: mailbox"
                } else {
                    "goose: no mailbox"
                }
            }
            _ => "opencode",
        };

        let updated_ts = s
            .get("updated_at")
            .or_else(|| s.get("last_seen"))
            .and_then(Value::as_i64);
        let updated = match updated_ts {
            Some(ts) => {
                let ts_ms = if ts < 10_000_000_000 { ts * 1000 } else { ts };
                format_relative_time(ts_ms, now_ms)
            }
            None => "-".to_string(),
        };

        lines.push(
            format!("{session_id}{you}  {kind}  {updated}  {cwd}  {title}")
                .trim_end()
                .to_string(),
        );
    }

    tool_ok(lines.join("\n"))
}

fn get_tools() -> Value {
    json!([
        {
            "name": "swarm_send",
            "description": "Send a swarm message to another session via pigeon. One substantial message beats several; no acks or heartbeats; reply only with decisions, evidence, or deliverables. The message payload must NOT contain the literal </swarm_message> tag, and do NOT write the <swarm_message> envelope yourself. The sender ('from') is filled in automatically. 'to' must be 'ses_…' (opencode) or 'gse_…' (goose).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient session id (starts with 'ses_' for opencode or 'gse_' for goose)."
                    },
                    "message": {
                        "type": "string",
                        "description": "Raw message text. Do NOT include <swarm_message> tags."
                    },
                    "kind": {
                        "type": "string",
                        "description": "Message kind (default 'chat'). Examples: chat, task.assign, result, status.update, clarification.request."
                    },
                    "priority": {
                        "type": "string",
                        "description": "Priority: 'urgent', 'normal' (default), or 'low'."
                    },
                    "reply_to": {
                        "type": "string",
                        "description": "A prior msg_id to thread this message under."
                    }
                },
                "required": ["to", "message"]
            }
        },
        {
            "name": "swarm_read",
            "description": "Read and drain incoming swarm messages from this session's pigeon mailbox. Each message is wrapped in a <swarm_message> envelope with message-economy reminders. Messages are acknowledged upon drain.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of messages to return (default 10)."
                    }
                }
            }
        },
        {
            "name": "swarm_list",
            "description": "List active sessions across all projects to discover peer session IDs for swarm_send. Identifies opencode sessions and goose sessions (noting whether a goose session has a mailbox).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of sessions to return."
                    }
                }
            }
        }
    ])
}

pub async fn run_server<R, W>(
    config: SwarmMcpServerConfig,
    mut reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let mut line = String::new();
    while reader.read_line(&mut line).await? != 0 {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }

        let parsed: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("honk-swarm: invalid JSON: {e}");
                line.clear();
                continue;
            }
        };

        let id = parsed.get("id").cloned();
        let method = parsed.get("method").and_then(Value::as_str);

        // Notification: frames without an id must not receive a response
        let Some(req_id) = id else {
            line.clear();
            continue;
        };

        let params = parsed.get("params").unwrap_or(&Value::Null);

        let resp = match method {
            Some("initialize") => {
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2024-11-05");
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "protocolVersion": version,
                        "capabilities": {
                            "tools": {}
                        },
                        "serverInfo": {
                            "name": "honk-swarm",
                            "version": "0.1.0"
                        }
                    }
                })
            }
            Some("ping") => {
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {}
                })
            }
            Some("tools/list") => {
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "tools": get_tools()
                    }
                })
            }
            Some("tools/call") => {
                let tool_result = match check_caller_identity(&config, params) {
                    Err(e) => tool_err(e),
                    Ok(()) => {
                        let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                        let arguments = params.get("arguments").unwrap_or(&Value::Null);
                        let meta = params.get("_meta");
                        let cwd = resolve_call_cwd(meta);
                        match tool_name {
                            "swarm_send" => {
                                handle_swarm_send(&client, &config, arguments, cwd.as_deref()).await
                            }
                            "swarm_read" => {
                                handle_swarm_read(&client, &config, arguments, cwd.as_deref()).await
                            }
                            "swarm_list" => handle_swarm_list(&client, &config, arguments).await,
                            other => tool_err(format!("unknown tool '{other}'")),
                        }
                    }
                };
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": tool_result
                })
            }
            Some(unknown) => {
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": format!("Method not found: {unknown}")
                    }
                })
            }
            None => {
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32600,
                        "message": "Missing method"
                    }
                })
            }
        };

        let out_line = format!("{}\n", serde_json::to_string(&resp)?);
        writer.write_all(out_line.as_bytes()).await?;
        writer.flush().await?;

        line.clear();
    }

    Ok(())
}

pub async fn run_stdio(config: SwarmMcpServerConfig) -> anyhow::Result<()> {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    run_server(config, stdin, stdout).await
}
