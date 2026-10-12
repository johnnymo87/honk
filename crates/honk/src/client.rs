//! WebSocket client and JSON-RPC transport for honkd.

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    pub session_id: String,
    pub held: bool,
    pub running: bool,
    pub title: String,
    pub cwd: String,
}

impl SessionEntry {
    /// Formats one line per session: `{sessionId}  {held|    }{ running}  {title}  {cwd}`
    pub fn format_line(&self) -> String {
        format_session_line(
            &self.session_id,
            self.held,
            self.running,
            &self.title,
            &self.cwd,
        )
    }
}

pub fn format_session_line(
    session_id: &str,
    held: bool,
    running: bool,
    title: &str,
    cwd: &str,
) -> String {
    let held_str = if held { "held" } else { "    " };
    let running_str = if running { " running" } else { "" };
    format!("{session_id}  {held_str}{running_str}  {title}  {cwd}")
}

/// Normalizes a URL to a WebSocket URL ending in `/acp`.
pub fn normalize_ws_url(url: &str) -> String {
    let mut u = url.trim().to_string();
    if u.starts_with("http://") {
        u = format!("ws://{}", &u["http://".len()..]);
    } else if u.starts_with("https://") {
        u = format!("wss://{}", &u["https://".len()..]);
    } else if !u.starts_with("ws://") && !u.starts_with("wss://") {
        u = format!("ws://{u}");
    }
    if !u.ends_with("/acp") {
        if u.ends_with('/') {
            u.push_str("acp");
        } else {
            u.push_str("/acp");
        }
    }
    u
}

/// Resolves the honkd secret from:
/// 1. explicit `--secret` argument
/// 2. `$HONK_SECRET` environment variable
/// 3. `$XDG_STATE_HOME/honk/secret`
/// 4. `~/.local/state/honk/secret`
pub fn resolve_secret(secret_arg: Option<&str>) -> anyhow::Result<String> {
    if let Some(s) = secret_arg
        && !s.trim().is_empty()
    {
        return Ok(s.trim().to_string());
    }
    if let Ok(s) = std::env::var("HONK_SECRET")
        && !s.trim().is_empty()
    {
        return Ok(s.trim().to_string());
    }
    let secret_path = if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
        && !xdg.trim().is_empty()
    {
        PathBuf::from(xdg.trim()).join("honk/secret")
    } else if let Ok(home) = std::env::var("HOME")
        && !home.trim().is_empty()
    {
        PathBuf::from(home.trim()).join(".local/state/honk/secret")
    } else {
        anyhow::bail!("cannot find honk secret: HONK_SECRET unset and HOME unset");
    };

    let content = std::fs::read_to_string(&secret_path).map_err(|e| {
        anyhow::anyhow!("failed to read secret from {}: {e}", secret_path.display())
    })?;
    let secret = content.trim().to_string();
    if secret.is_empty() {
        anyhow::bail!("secret file at {} is empty", secret_path.display());
    }
    Ok(secret)
}

pub struct Client {
    ws: WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    next_req_id: u64,
}

impl Client {
    pub async fn connect(url: &str, secret: &str) -> anyhow::Result<Self> {
        let ws_url = normalize_ws_url(url);
        let mut req = ws_url.into_client_request()?;
        req.headers_mut().insert(
            "x-secret-key",
            secret
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid secret header: {e}"))?,
        );

        let (ws, _) = tokio_tungstenite::connect_async(req).await?;
        let mut client = Self { ws, next_req_id: 1 };

        // Send honk-aware initialize
        let init_res = client
            .call(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "_meta": { "honk": {} }
                }),
            )
            .await?;

        if let Some(err) = init_res.get("error") {
            anyhow::bail!("honkd initialize failed: {err}");
        }

        Ok(client)
    }

    pub async fn request(&mut self, method: &str, params: Value) -> anyhow::Result<u64> {
        let id = self.next_req_id;
        self.next_req_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.ws
            .send(Message::Text(req.to_string().into()))
            .await
            .map_err(|e| anyhow::anyhow!("failed to send {method} request: {e}"))?;
        Ok(id)
    }

    pub async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.request(method, params).await?;
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t)
                        && v.get("id") == Some(&json!(id))
                    {
                        return Ok(v);
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    let _ = self.ws.send(Message::Pong(payload)).await;
                }
                Some(Ok(Message::Close(_))) => {
                    anyhow::bail!("connection closed while awaiting response for request {id}");
                }
                Some(Err(e)) => return Err(e.into()),
                None => anyhow::bail!("stream ended while awaiting response for request {id}"),
                _ => {}
            }
        }
    }

    pub async fn next_frame(&mut self) -> anyhow::Result<Option<Value>> {
        while let Some(msg_res) = self.ws.next().await {
            match msg_res {
                Ok(Message::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        return Ok(Some(v));
                    }
                }
                Ok(Message::Ping(payload)) => {
                    let _ = self.ws.send(Message::Pong(payload)).await;
                }
                Ok(Message::Close(_)) => return Ok(None),
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    pub async fn list_sessions(&mut self) -> anyhow::Result<Vec<SessionEntry>> {
        let res = self.call("session/list", json!({})).await?;
        if let Some(err) = res.get("error") {
            anyhow::bail!("session/list returned error: {err}");
        }

        let sessions_val = res
            .pointer("/result/sessions")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("missing sessions array in response: {res}"))?;

        let mut entries = Vec::new();
        for s in sessions_val {
            let session_id = s
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let title = s
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let cwd = s
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let held = s
                .pointer("/_meta/honk/held")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let running = s
                .pointer("/_meta/honk/running")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            entries.push(SessionEntry {
                session_id,
                held,
                running,
                title,
                cwd,
            });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_session_line() {
        // held and running
        let line1 = format_session_line("ses_1", true, true, "Build project", "/path/to/work");
        assert_eq!(line1, "ses_1  held running  Build project  /path/to/work");

        // held and not running
        let line2 = format_session_line("ses_2", true, false, "Idle session", "/path/to/work");
        assert_eq!(line2, "ses_2  held  Idle session  /path/to/work");

        // not held and running
        let line3 = format_session_line("ses_3", false, true, "Running detached", "/path/to/work");
        assert_eq!(
            line3,
            "ses_3       running  Running detached  /path/to/work"
        );

        // not held and not running
        let line4 = format_session_line("ses_4", false, false, "Old session", "/path/to/work");
        assert_eq!(line4, "ses_4        Old session  /path/to/work");
    }

    #[test]
    fn test_normalize_ws_url() {
        assert_eq!(
            normalize_ws_url("http://127.0.0.1:4280/acp"),
            "ws://127.0.0.1:4280/acp"
        );
        assert_eq!(
            normalize_ws_url("ws://127.0.0.1:4280/acp"),
            "ws://127.0.0.1:4280/acp"
        );
        assert_eq!(
            normalize_ws_url("127.0.0.1:4280"),
            "ws://127.0.0.1:4280/acp"
        );
        assert_eq!(
            normalize_ws_url("http://127.0.0.1:4280"),
            "ws://127.0.0.1:4280/acp"
        );
        assert_eq!(
            normalize_ws_url("https://example.com/acp"),
            "wss://example.com/acp"
        );
    }
}
