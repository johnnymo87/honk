//! WebSocket client and JSON-RPC transport for honkd.

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListOpts {
    pub cwd: Option<String>,
    pub max_pages: usize,
}

impl Default for ListOpts {
    fn default() -> Self {
        Self {
            cwd: None,
            max_pages: Self::DEFAULT_MAX_PAGES,
        }
    }
}

impl ListOpts {
    pub const DEFAULT_MAX_PAGES: usize = 100;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_max_pages(mut self, max_pages: usize) -> Self {
        self.max_pages = max_pages;
        self
    }
}

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

use url::Url;

/// Normalizes a URL to a WebSocket URL ending in `/acp`, preserving queries and handling trailing slashes.
pub fn normalize_ws_url(url: &str) -> String {
    let trimmed = url.trim();
    let has_scheme = trimmed.contains("://");
    let url_str = if has_scheme {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };

    let Ok(mut parsed) = Url::parse(&url_str) else {
        return trimmed.to_string();
    };

    let new_scheme = match parsed.scheme() {
        "https" | "wss" => "wss",
        _ => "ws",
    };
    let _ = parsed.set_scheme(new_scheme);

    let path = parsed.path().trim_end_matches('/').to_string();
    if path.is_empty() || path == "/" {
        parsed.set_path("/acp");
    } else if path.ends_with("/acp") {
        parsed.set_path(&path);
    } else {
        parsed.set_path(&format!("{path}/acp"));
    }

    parsed.to_string()
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

pub async fn connect_and_initialize(
    url: &str,
    secret: &str,
) -> anyhow::Result<(
    WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Value,
)> {
    let ws_url = normalize_ws_url(url);
    let mut req = ws_url.into_client_request()?;
    req.headers_mut().insert(
        "x-secret-key",
        secret
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid secret header: {e}"))?,
    );

    let (mut ws, _) = match tokio_tungstenite::connect_async(req).await {
        Ok(pair) => pair,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp))
            if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 =>
        {
            anyhow::bail!("fatal: authentication failed (HTTP {})", resp.status());
        }
        Err(e) => return Err(e.into()),
    };

    // Send honk-aware initialize
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {},
            "_meta": { "honk": {} }
        }
    });
    ws.send(Message::Text(init_req.to_string().into()))
        .await
        .map_err(|e| anyhow::anyhow!("failed to send initialize request: {e}"))?;

    let init_res = loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t)
                    && v.get("id") == Some(&json!(1))
                {
                    break v;
                }
            }
            Some(Ok(Message::Ping(payload))) => {
                let _ = ws.send(Message::Pong(payload)).await;
            }
            Some(Ok(Message::Close(_))) => {
                anyhow::bail!("connection closed while awaiting initialize response");
            }
            Some(Err(e)) => return Err(e.into()),
            None => anyhow::bail!("stream ended while awaiting initialize response"),
            _ => {}
        }
    };

    if let Some(err) = init_res.get("error") {
        anyhow::bail!("fatal: honkd initialize failed: {err}");
    }

    let init_result = init_res.get("result").cloned().unwrap_or(init_res);
    Ok((ws, init_result))
}

impl Client {
    pub async fn connect(url: &str, secret: &str) -> anyhow::Result<Self> {
        let (ws, _init_result) = connect_and_initialize(url, secret).await?;
        Ok(Self { ws, next_req_id: 2 })
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

    /// Sends a JSON-RPC request and awaits the matching response by id.
    ///
    /// NOTE: Preflight-only. Any session notifications or unsolicited frames
    /// received while awaiting the response are dropped. For full session streaming
    /// and frame processing, use `request` followed by `next_frame`.
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
        self.list_sessions_with(ListOpts::default()).await
    }

    pub async fn list_sessions_with(
        &mut self,
        opts: ListOpts,
    ) -> anyhow::Result<Vec<SessionEntry>> {
        let max_pages = if opts.max_pages == 0 {
            ListOpts::DEFAULT_MAX_PAGES
        } else {
            opts.max_pages
        };

        let mut entries = Vec::new();
        let mut current_cursor: Option<String> = None;
        let mut seen_cursors = std::collections::HashSet::new();
        let mut pages_fetched = 0;

        loop {
            if pages_fetched >= max_pages {
                break;
            }

            let mut params = serde_json::Map::new();
            if let Some(ref cwd) = opts.cwd {
                params.insert("cwd".to_string(), json!(cwd));
            }
            if let Some(ref cursor) = current_cursor {
                params.insert("cursor".to_string(), json!(cursor));
            }

            let res = self.call("session/list", Value::Object(params)).await?;
            if let Some(err) = res.get("error") {
                anyhow::bail!("session/list returned error: {err}");
            }

            let sessions_val = res
                .pointer("/result/sessions")
                .or_else(|| res.get("sessions"))
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("missing sessions array in response: {res}"))?;

            for s in sessions_val {
                let session_id = s
                    .get("sessionId")
                    .or_else(|| s.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let title = s
                    .get("title")
                    .or_else(|| s.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let cwd = s
                    .get("cwd")
                    .or_else(|| s.pointer("/_meta/workingDir"))
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

            pages_fetched += 1;

            let next_cursor = res
                .pointer("/result/nextCursor")
                .or_else(|| res.get("nextCursor"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from);

            match next_cursor {
                Some(cursor) => {
                    if !seen_cursors.insert(cursor.clone()) {
                        break;
                    }
                    current_cursor = Some(cursor);
                }
                None => break,
            }
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
        assert_eq!(
            normalize_ws_url("http://127.0.0.1:4280/"),
            "ws://127.0.0.1:4280/acp"
        );
        assert_eq!(
            normalize_ws_url("http://127.0.0.1:4280/acp/"),
            "ws://127.0.0.1:4280/acp"
        );
        assert_eq!(
            normalize_ws_url("http://127.0.0.1:4280?token=abc"),
            "ws://127.0.0.1:4280/acp?token=abc"
        );
        assert_eq!(
            normalize_ws_url("http://127.0.0.1:4280/?token=abc"),
            "ws://127.0.0.1:4280/acp?token=abc"
        );
        assert_eq!(
            normalize_ws_url("http://127.0.0.1:4280/acp?token=abc"),
            "ws://127.0.0.1:4280/acp?token=abc"
        );
        assert_eq!(
            normalize_ws_url("http://127.0.0.1:4280/acp/?token=abc"),
            "ws://127.0.0.1:4280/acp?token=abc"
        );
    }
}
