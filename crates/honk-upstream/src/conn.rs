use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::StreamExt;
use reqwest::{StatusCode, Url};
use serde_json::Value;

use crate::guard_url;
use crate::record::{Direction, Recorder};
use crate::sse::SseFrame;

/// Timeout applied to all JSON-RPC POST and DELETE requests.
pub const POST_TIMEOUT: Duration = Duration::from_secs(30);

/// Connection timeout applied when establishing HTTP connections.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Timeout applied to subscribe GET request waiting for response headers.
pub const SUBSCRIBE_HEADERS_TIMEOUT: Duration = Duration::from_secs(5);

fn build_http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .context("failed to build reqwest client")
}

/// Identifies whether a stream belongs to the connection mailbox or a specific session mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StreamKey {
    Conn,
    Session(String),
}

impl StreamKey {
    pub fn stream_record_name(&self) -> String {
        match self {
            StreamKey::Conn => "conn".to_string(),
            StreamKey::Session(id) => format!("session:{id}"),
        }
    }
}

/// Lifecycle and payload events delivered across SSE streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A JSON-RPC frame received from an SSE stream, with its SSE event id if goose
    /// sent one (patched-1.54.0.3+). Persist the id only after processing the frame.
    Frame(StreamKey, Value, Option<u64>),
    /// Indicates an SSE stream has terminated, either cleanly (error is None) or with an error.
    StreamEnded {
        key: StreamKey,
        error: Option<String>,
    },
}

impl Event {
    pub fn key(&self) -> &StreamKey {
        match self {
            Event::Frame(key, _, _) => key,
            Event::StreamEnded { key, .. } => key,
        }
    }

    pub fn frame(&self) -> Option<&Value> {
        match self {
            Event::Frame(_, val, _) => Some(val),
            Event::StreamEnded { .. } => None,
        }
    }

    pub fn event_id(&self) -> Option<u64> {
        match self {
            Event::Frame(_, _, id) => *id,
            Event::StreamEnded { .. } => None,
        }
    }
}

/// Compares two JSON-RPC ids for equality, handling number-string equivalence.
pub fn rpc_id_matches(expected: &Value, actual: &Value) -> bool {
    if expected == actual {
        return true;
    }
    match (expected, actual) {
        (Value::Number(n), Value::String(s)) => n.to_string() == *s,
        (Value::String(s), Value::Number(n)) => *s == n.to_string(),
        _ => false,
    }
}

/// What a goose `GET /acp` advertises in `Acp-Stream-Features` (patched-1.54.0.3+).
/// Unpatched goose sends no header: both false.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamFeatures {
    pub takeover: bool,
    pub replay: bool,
}

impl StreamFeatures {
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        let mut features = Self::default();
        if let Some(value) = headers
            .get("acp-stream-features")
            .and_then(|v| v.to_str().ok())
        {
            for token in value.split(',').map(str::trim) {
                match token {
                    "takeover" => features.takeover = true,
                    "replay" => features.replay = true,
                    _ => {}
                }
            }
        }
        features
    }

    /// Everything honkd's lossless-reattach design needs.
    pub fn is_patched(self) -> bool {
        self.takeover && self.replay
    }
}

/// Per-call subscribe options. Safe against unpatched goose: it ignores both headers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscribeOpts {
    /// Send `Acp-Takeover: 1`: evict a stuck subscriber instead of retrying 409.
    pub takeover: bool,
    /// Send `Last-Event-ID: <n>`: replay what follows the last *processed* id.
    /// `None` sends nothing; use `Some(0)` to replay everything retained.
    pub resume_after: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayOutcome {
    NotRequested,
    /// Resume was requested but the server did not answer `Acp-Replay` (unpatched).
    Unsupported,
    Complete,
    Gap,
}

impl ReplayOutcome {
    pub fn from_headers(requested: bool, headers: &reqwest::header::HeaderMap) -> Self {
        if !requested {
            return Self::NotRequested;
        }
        match headers
            .get("acp-replay")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
        {
            Some("complete") => Self::Complete,
            Some("gap") => Self::Gap,
            _ => Self::Unsupported,
        }
    }

    /// Whether the caller must reconcile (export / same-connection session/load, A3/A5).
    pub fn needs_reconcile(self) -> bool {
        !matches!(self, Self::Complete)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscribeOutcome {
    pub conflict_window: Duration,
    /// The server honoured `Acp-Takeover` (it advertises takeover and we asked).
    pub took_over: bool,
    pub replay: ReplayOutcome,
    pub features: StreamFeatures,
}

/// Upstream ACP HTTP+SSE client.
pub struct Upstream {
    url: Url,
    secret: String,
    conn_id: String,
    init_result: Option<Value>,
    client: reqwest::Client,
    next_request_id: AtomicU64,
    rec: Recorder,
    stream_tasks: HashMap<StreamKey, tokio::task::JoinHandle<()>>,
    rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    tx: tokio::sync::mpsc::UnboundedSender<Event>,
    buffered_events: VecDeque<Event>,
    subscribe_deadline: Duration,
    subscribe_initial_backoff: Duration,
    subscribe_max_backoff: Duration,
    subscribe_headers_timeout: Duration,
    last_call_stream: Option<StreamKey>,
    features: Option<StreamFeatures>,
}

impl Upstream {
    /// Connects and initializes a new ACP connection.
    ///
    /// Validates `url` via [`guard_url`], sends `initialize` JSON-RPC POST,
    /// stores the resulting `Acp-Connection-Id`, and records all frames.
    pub async fn initialize(url: &str, secret: String, rec: Recorder) -> anyhow::Result<Self> {
        let parsed_url = guard_url(url)?;
        let trimmed_secret = secret.trim().to_string();
        let client = build_http_client()?;

        let init_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {}
            }
        });

        let res = client
            .post(parsed_url.clone())
            .timeout(POST_TIMEOUT)
            .header("x-secret-key", &trimmed_secret)
            .header("content-type", "application/json")
            .json(&init_body)
            .send()
            .await
            .with_context(|| format!("failed to send POST initialize to {url}"))?;

        let status = res.status();
        rec.record(
            Direction::Out,
            "post",
            Some(status.as_u16()),
            Some(init_body),
            None,
        );

        if !status.is_success() {
            let err_text = res.text().await.unwrap_or_default();
            anyhow::bail!("initialize POST failed with status {status}: {err_text}");
        }

        let conn_id = res
            .headers()
            .get("acp-connection-id")
            .ok_or_else(|| anyhow::anyhow!("initialize response missing Acp-Connection-Id header"))?
            .to_str()
            .with_context(|| "invalid Acp-Connection-Id header")?
            .trim()
            .to_string();

        let init_response: Value = res
            .json()
            .await
            .with_context(|| "failed to parse initialize response JSON body")?;

        rec.record_in("conn", init_response.clone());

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        Ok(Self {
            url: parsed_url,
            secret: trimmed_secret,
            conn_id,
            init_result: Some(init_response),
            client,
            next_request_id: AtomicU64::new(2),
            rec,
            stream_tasks: HashMap::new(),
            rx,
            tx,
            buffered_events: VecDeque::new(),
            subscribe_deadline: Duration::from_secs(60),
            subscribe_initial_backoff: Duration::from_millis(100),
            subscribe_max_backoff: Duration::from_secs(2),
            subscribe_headers_timeout: SUBSCRIBE_HEADERS_TIMEOUT,
            last_call_stream: None,
            features: None,
        })
    }

    /// Resumes an existing connection id without re-running initialize.
    pub fn resume(
        url: &str,
        secret: String,
        conn_id: String,
        rec: Recorder,
    ) -> anyhow::Result<Self> {
        let parsed_url = guard_url(url)?;
        let trimmed_secret = secret.trim().to_string();
        let trimmed_conn_id = conn_id.trim().to_string();
        let client = build_http_client()?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        rec.record_marker("conn", format!("resumed connection {trimmed_conn_id}"));

        Ok(Self {
            url: parsed_url,
            secret: trimmed_secret,
            conn_id: trimmed_conn_id,
            init_result: None,
            client,
            next_request_id: AtomicU64::new(1000),
            rec,
            stream_tasks: HashMap::new(),
            rx,
            tx,
            buffered_events: VecDeque::new(),
            subscribe_deadline: Duration::from_secs(60),
            subscribe_initial_backoff: Duration::from_millis(100),
            subscribe_max_backoff: Duration::from_secs(2),
            subscribe_headers_timeout: SUBSCRIBE_HEADERS_TIMEOUT,
            last_call_stream: None,
            features: None,
        })
    }

    pub fn conn_id(&self) -> &str {
        &self.conn_id
    }

    pub fn init_result(&self) -> Option<&Value> {
        self.init_result.as_ref()
    }

    pub fn stream_features(&self) -> Option<StreamFeatures> {
        self.features
    }

    pub fn last_call_stream(&self) -> Option<&StreamKey> {
        self.last_call_stream.as_ref()
    }

    pub fn set_subscribe_deadline(&mut self, deadline: Duration) {
        self.subscribe_deadline = deadline;
    }

    pub fn set_subscribe_headers_timeout(&mut self, timeout: Duration) {
        self.subscribe_headers_timeout = timeout;
    }

    pub fn set_subscribe_backoff(&mut self, initial: Duration, max: Duration) {
        self.subscribe_initial_backoff = initial;
        self.subscribe_max_backoff = max;
    }

    pub fn buffered_event_count(&self) -> usize {
        self.buffered_events.len()
    }

    pub fn buffered_frame_count(&self) -> usize {
        self.buffered_events
            .iter()
            .filter(|e| matches!(e, Event::Frame(_, _, _)))
            .count()
    }

    /// Sends a JSON-RPC request frame with a monotonic integer id.
    /// Expects HTTP 202 Accepted.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> anyhow::Result<Value> {
        let id_num = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let id = Value::from(id_num);
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let mut req = self
            .client
            .post(self.url.clone())
            .timeout(POST_TIMEOUT)
            .header("x-secret-key", &self.secret)
            .header("acp-connection-id", &self.conn_id)
            .header("content-type", "application/json");

        if let Some(sid) = session {
            req = req.header("acp-session-id", sid);
        }

        let res = req
            .json(&frame)
            .send()
            .await
            .with_context(|| format!("failed to send POST request '{method}'"))?;

        let status = res.status();
        self.rec.record(
            Direction::Out,
            "post",
            Some(status.as_u16()),
            Some(frame),
            None,
        );

        if status != StatusCode::ACCEPTED {
            let err_text = res.text().await.unwrap_or_default();
            anyhow::bail!("request '{method}' failed with status {status}: {err_text}");
        }

        Ok(id)
    }

    /// Sends a JSON-RPC notification frame (no `id`).
    /// Expects HTTP 202 Accepted.
    pub async fn notify(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let mut req = self
            .client
            .post(self.url.clone())
            .timeout(POST_TIMEOUT)
            .header("x-secret-key", &self.secret)
            .header("acp-connection-id", &self.conn_id)
            .header("content-type", "application/json");

        if let Some(sid) = session {
            req = req.header("acp-session-id", sid);
        }

        let res = req
            .json(&frame)
            .send()
            .await
            .with_context(|| format!("failed to send POST notification '{method}'"))?;

        let status = res.status();
        self.rec.record(
            Direction::Out,
            "post",
            Some(status.as_u16()),
            Some(frame),
            None,
        );

        if status != StatusCode::ACCEPTED {
            let err_text = res.text().await.unwrap_or_default();
            anyhow::bail!("notification '{method}' failed with status {status}: {err_text}");
        }

        Ok(())
    }

    /// Sends a JSON-RPC response frame back to the server.
    /// Expects HTTP 202 Accepted.
    pub async fn respond(
        &self,
        id: Value,
        result: Value,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });

        let mut req = self
            .client
            .post(self.url.clone())
            .timeout(POST_TIMEOUT)
            .header("x-secret-key", &self.secret)
            .header("acp-connection-id", &self.conn_id)
            .header("content-type", "application/json");

        if let Some(sid) = session {
            req = req.header("acp-session-id", sid);
        }

        let res = req
            .json(&frame)
            .send()
            .await
            .with_context(|| "failed to send POST respond")?;

        let status = res.status();
        self.rec.record(
            Direction::Out,
            "post",
            Some(status.as_u16()),
            Some(frame),
            None,
        );

        if status != StatusCode::ACCEPTED {
            let err_text = res.text().await.unwrap_or_default();
            anyhow::bail!("respond failed with status {status}: {err_text}");
        }

        Ok(())
    }

    /// Subscribes to the connection stream (`session == None`) or a session stream (`session == Some(...)`).
    ///
    /// On HTTP 409 Conflict, retries with backoff up to `subscribe_deadline`.
    /// Returns the duration of the 409 conflict window (from first 409 to successful subscribe,
    /// or `Duration::ZERO` if no 409 occurred).
    /// Spawns a background task that streams parsed JSON frames into the client receiver.
    pub async fn subscribe(&mut self, session: Option<&str>) -> anyhow::Result<Duration> {
        Ok(self
            .subscribe_with(session, SubscribeOpts::default())
            .await?
            .conflict_window)
    }

    /// Subscribes to the stream with the specified options (takeover and resume).
    pub async fn subscribe_with(
        &mut self,
        session: Option<&str>,
        opts: SubscribeOpts,
    ) -> anyhow::Result<SubscribeOutcome> {
        let stream_key = session
            .map(|s| StreamKey::Session(s.to_string()))
            .unwrap_or(StreamKey::Conn);

        // Cancel previous task for this key if one was active
        if let Some(prev) = self.stream_tasks.remove(&stream_key) {
            prev.abort();
        }

        let stream_name = stream_key.stream_record_name();
        let start = Instant::now();
        let mut first_409: Option<Instant> = None;
        let mut backoff = self.subscribe_initial_backoff;

        loop {
            let mut req = self
                .client
                .get(self.url.clone())
                .header("accept", "text/event-stream")
                .header("x-secret-key", &self.secret)
                .header("acp-connection-id", &self.conn_id);

            if let Some(sid) = session {
                req = req.header("acp-session-id", sid);
            }
            if opts.takeover {
                req = req.header("acp-takeover", "1");
            }
            if let Some(last) = opts.resume_after {
                req = req.header("last-event-id", last.to_string());
            }

            let send_fut = req.send();
            let res = tokio::time::timeout(self.subscribe_headers_timeout, send_fut)
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "timed out after {:?} waiting for headers when subscribing to {stream_name}",
                        self.subscribe_headers_timeout
                    )
                })?
                .with_context(|| format!("failed to send GET request for {stream_name}"))?;

            let status = res.status();
            if status == StatusCode::OK || status == StatusCode::CONFLICT {
                self.features = Some(StreamFeatures::from_headers(res.headers()));
            }

            if status == StatusCode::CONFLICT {
                if first_409.is_none() {
                    first_409 = Some(Instant::now());
                }
                self.rec.record_marker(
                    &stream_name,
                    format!("409 Conflict subscribing to {stream_name}"),
                );
                if start.elapsed() >= self.subscribe_deadline {
                    anyhow::bail!(
                        "timed out after {:?} waiting for 409 conflict to resolve on {stream_name}",
                        self.subscribe_deadline
                    );
                }
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, self.subscribe_max_backoff);
                continue;
            }

            if !status.is_success() {
                self.rec.record(
                    Direction::Marker,
                    &stream_name,
                    Some(status.as_u16()),
                    None,
                    Some(format!("GET stream failed with status {status}")),
                );
                anyhow::bail!("GET stream for {stream_name} failed with status {status}");
            }

            let conflict_window = first_409.map(|t| t.elapsed()).unwrap_or(Duration::ZERO);
            let replay = ReplayOutcome::from_headers(opts.resume_after.is_some(), res.headers());
            let features = self.features.unwrap_or_default();
            let took_over = opts.takeover && features.takeover;

            self.rec.record_marker(
                &stream_name,
                format!("subscribed to {stream_name} (409 window: {conflict_window:?})"),
            );

            if opts.takeover || opts.resume_after.is_some() {
                self.rec.record_marker(
                    &stream_name,
                    format!(
                        "stream options: took_over={took_over} resume_after={:?} replay={replay:?} features={features:?}",
                        opts.resume_after
                    ),
                );
            }

            let sse_stream = crate::sse::sse_to_frames(res.bytes_stream());
            let tx = self.tx.clone();
            let rec = self.rec.clone();
            let key = stream_key.clone();
            let s_name = stream_name.clone();

            let handle = tokio::spawn(async move {
                let mut stream = sse_stream;
                let mut stream_err = None;
                let mut floor = if replay == ReplayOutcome::Complete {
                    opts.resume_after
                } else {
                    None
                };
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(SseFrame { id, data }) => {
                            if let Some(id) = id {
                                if floor.is_some_and(|f| id <= f) {
                                    rec.record_marker(
                                        &s_name,
                                        format!("dropped duplicate event id {id}"),
                                    );
                                    continue;
                                }
                                floor = Some(id);
                            }
                            rec.record(
                                Direction::In,
                                &s_name,
                                None,
                                Some(data.clone()),
                                id.map(|id| format!("event id {id}")),
                            );
                            if tx.send(Event::Frame(key.clone(), data, id)).is_err() {
                                return;
                            }
                        }
                        Err(err) => {
                            let err_str = err.to_string();
                            rec.record_marker(&s_name, format!("stream error: {err_str}"));
                            stream_err = Some(err_str);
                            break;
                        }
                    }
                }
                rec.record_marker(&s_name, format!("stream {s_name} ended"));
                let _ = tx.send(Event::StreamEnded {
                    key,
                    error: stream_err,
                });
            });

            self.stream_tasks.insert(stream_key, handle);
            return Ok(SubscribeOutcome {
                conflict_window,
                took_over,
                replay,
                features,
            });
        }
    }

    /// Reads the next event, checking buffered events first, then waiting on SSE streams up to `timeout`.
    /// Returns `Ok(None)` if `timeout` expires.
    pub async fn next(&mut self, timeout: Duration) -> anyhow::Result<Option<Event>> {
        if let Some(buffered) = self.buffered_events.pop_front() {
            return Ok(Some(buffered));
        }

        match tokio::time::timeout(timeout, self.rx.recv()).await {
            Ok(Some(item)) => Ok(Some(item)),
            Ok(None) => Ok(None),
            Err(_) => Ok(None),
        }
    }

    /// Convenience method: sends a request and waits on `next()` for the matching response id.
    ///
    /// Any non-matching events received while waiting are buffered into internal storage and
    /// drained by subsequent calls to `next()` or `call()`.
    /// Returns the `"result"` field of the response, or fails if the response contains an `"error"`.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let (stream_key, frame) = self.call_full(method, params, session, timeout).await?;
        self.last_call_stream = Some(stream_key);

        if let Some(err) = frame.get("error") {
            anyhow::bail!("RPC error calling '{method}': {err}");
        }

        if let Some(result) = frame.get("result") {
            Ok(result.clone())
        } else {
            Ok(frame)
        }
    }

    /// Like [`call`](Self::call), but returns both the [`StreamKey`] the reply arrived on
    /// and the full JSON-RPC response frame.
    pub async fn call_full(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<(StreamKey, Value)> {
        let req_id = self.request(method, params, session).await?;
        let deadline = Instant::now() + timeout;
        let mut unmatched = Vec::new();

        loop {
            let now = Instant::now();
            if now >= deadline {
                for item in unmatched.into_iter().rev() {
                    self.buffered_events.push_front(item);
                }
                anyhow::bail!("timed out waiting for response to request id {req_id}");
            }
            let remaining = deadline - now;
            match self.next(remaining).await {
                Ok(Some(Event::Frame(stream_key, frame, id))) => {
                    if frame
                        .get("id")
                        .is_some_and(|id| rpc_id_matches(&req_id, id))
                    {
                        for item in unmatched.into_iter().rev() {
                            self.buffered_events.push_front(item);
                        }
                        return Ok((stream_key, frame));
                    } else {
                        unmatched.push(Event::Frame(stream_key, frame, id));
                    }
                }
                Ok(Some(Event::StreamEnded { key, error })) => {
                    let expected_stream = session
                        .map(|s| StreamKey::Session(s.to_string()))
                        .unwrap_or(StreamKey::Conn);
                    if key == expected_stream || key == StreamKey::Conn {
                        for item in unmatched.into_iter().rev() {
                            self.buffered_events.push_front(item);
                        }
                        anyhow::bail!(
                            "stream {key:?} ended while waiting for response to request id {req_id}: {error:?}"
                        );
                    } else {
                        unmatched.push(Event::StreamEnded { key, error });
                    }
                }
                Ok(None) => {
                    for item in unmatched.into_iter().rev() {
                        self.buffered_events.push_front(item);
                    }
                    anyhow::bail!("timed out waiting for response to request id {req_id}");
                }
                Err(err) => {
                    for item in unmatched.into_iter().rev() {
                        self.buffered_events.push_front(item);
                    }
                    return Err(err);
                }
            }
        }
    }

    /// Aborts all running SSE stream tasks without sending a DELETE request to the server.
    pub fn hard_drop(&mut self) {
        for (_, handle) in self.stream_tasks.drain() {
            handle.abort();
        }
        self.rec
            .record_marker("conn", "hard_drop: aborted all SSE stream tasks");
    }

    /// Terminates the connection via DELETE /acp.
    pub async fn delete(mut self) -> anyhow::Result<StatusCode> {
        self.hard_drop();
        let res = self
            .client
            .delete(self.url.clone())
            .timeout(POST_TIMEOUT)
            .header("x-secret-key", &self.secret)
            .header("acp-connection-id", &self.conn_id)
            .send()
            .await
            .with_context(|| "failed to send DELETE /acp")?;

        let status = res.status();
        self.rec.record(
            Direction::Out,
            "post",
            Some(status.as_u16()),
            None,
            Some("DELETE /acp".into()),
        );

        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_refuses_production_port() {
        let res = Upstream::resume(
            "http://127.0.0.1:4080/acp",
            "secret".into(),
            "conn-1".into(),
            Recorder::disabled(),
        );
        assert!(res.is_err());
    }

    #[test]
    fn resume_trims_secret_and_conn_id() {
        let upstream = Upstream::resume(
            "http://127.0.0.1:4180/acp",
            "  secret-123 \n".into(),
            " conn-abc \r\n".into(),
            Recorder::disabled(),
        )
        .expect("resume ok");
        assert_eq!(upstream.secret, "secret-123");
        assert_eq!(upstream.conn_id(), "conn-abc");
    }

    #[test]
    fn rpc_id_matching_handles_number_and_string_equivalence() {
        use serde_json::json;
        assert!(rpc_id_matches(&json!(42), &json!(42)));
        assert!(rpc_id_matches(&json!("42"), &json!("42")));
        assert!(rpc_id_matches(&json!(42), &json!("42")));
        assert!(rpc_id_matches(&json!("42"), &json!(42)));
        assert!(!rpc_id_matches(&json!(42), &json!(43)));
        assert!(!rpc_id_matches(&json!(42), &json!("43")));
        assert!(!rpc_id_matches(&json!(42), &json!("not_a_number")));
        assert!(!rpc_id_matches(&json!(true), &json!("true")));
    }

    #[tokio::test]
    async fn next_returns_stream_ended_event() {
        let mut upstream = Upstream::resume(
            "http://127.0.0.1:4180/acp",
            "secret".into(),
            "conn-test".into(),
            Recorder::disabled(),
        )
        .expect("resume ok");

        // Simulate sending a StreamEnded event
        upstream
            .tx
            .send(Event::StreamEnded {
                key: StreamKey::Conn,
                error: Some("test error".into()),
            })
            .unwrap();

        let event = upstream
            .next(Duration::from_millis(50))
            .await
            .unwrap()
            .unwrap();
        match event {
            Event::StreamEnded { key, error } => {
                assert_eq!(key, StreamKey::Conn);
                assert_eq!(error.as_deref(), Some("test error"));
            }
            _ => panic!("expected StreamEnded event, got {event:?}"),
        }
    }

    #[tokio::test]
    async fn initialize_refuses_production_port() {
        let res = Upstream::initialize(
            "http://127.0.0.1:4080/acp",
            "secret".into(),
            Recorder::disabled(),
        )
        .await;
        assert!(res.is_err());
    }

    #[test]
    fn stream_key_record_names() {
        assert_eq!(StreamKey::Conn.stream_record_name(), "conn");
        assert_eq!(
            StreamKey::Session("s123".into()).stream_record_name(),
            "session:s123"
        );
    }

    #[tokio::test]
    async fn next_drains_buffered_frames_first() {
        let mut upstream = Upstream::resume(
            "http://127.0.0.1:4180/acp",
            "secret".into(),
            "conn-test".into(),
            Recorder::disabled(),
        )
        .expect("resume ok");

        upstream.buffered_events.push_back(Event::Frame(
            StreamKey::Conn,
            serde_json::json!({"buffered": 1}),
            None,
        ));
        upstream.buffered_events.push_back(Event::Frame(
            StreamKey::Session("s1".into()),
            serde_json::json!({"buffered": 2}),
            None,
        ));

        let first = upstream
            .next(Duration::from_millis(50))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first,
            Event::Frame(StreamKey::Conn, serde_json::json!({"buffered": 1}), None)
        );

        let second = upstream
            .next(Duration::from_millis(50))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            second,
            Event::Frame(
                StreamKey::Session("s1".into()),
                serde_json::json!({"buffered": 2}),
                None,
            )
        );

        let third = upstream.next(Duration::from_millis(10)).await.unwrap();
        assert!(third.is_none());
    }

    #[tokio::test]
    async fn hard_drop_aborts_tasks_and_records_marker() {
        let mut upstream = Upstream::resume(
            "http://127.0.0.1:4180/acp",
            "secret".into(),
            "conn-test".into(),
            Recorder::disabled(),
        )
        .expect("resume ok");

        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        upstream.stream_tasks.insert(StreamKey::Conn, handle);

        upstream.hard_drop();
        assert!(upstream.stream_tasks.is_empty());
    }

    #[tokio::test]
    async fn subscribe_times_out_when_server_does_not_send_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Server accepts connection and holds it open without sending response headers
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
            use tokio::io::AsyncWriteExt;
            let _ = socket.shutdown().await;
        });

        let mut upstream = Upstream::resume(
            &format!("http://127.0.0.1:{port}/acp"),
            "secret".into(),
            "conn-test".into(),
            Recorder::disabled(),
        )
        .expect("resume ok");
        upstream.set_subscribe_headers_timeout(Duration::from_millis(50));

        let start = Instant::now();
        let res = upstream.subscribe(None).await;
        let elapsed = start.elapsed();

        assert!(
            res.is_err(),
            "expected subscribe to error on header timeout"
        );
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("timed out") && err_msg.contains("50ms"),
            "expected error to mention timeout after 50ms, got: {err_msg}"
        );
        assert!(
            elapsed >= Duration::from_millis(30) && elapsed < Duration::from_millis(400),
            "elapsed time was {elapsed:?}, expected ~50ms"
        );
    }

    #[tokio::test]
    async fn subscribe_does_not_follow_redirect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let resp = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/redirected\r\nContent-Length: 0\r\n\r\n";
            let _ = socket.write_all(resp.as_bytes()).await;
        });

        let mut upstream = Upstream::resume(
            &format!("http://127.0.0.1:{port}/acp"),
            "secret".into(),
            "conn-test".into(),
            Recorder::disabled(),
        )
        .expect("resume ok");

        let res = upstream.subscribe(None).await;
        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("302"),
            "expected error to mention 302, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn initialize_does_not_follow_redirect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let resp = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/redirected\r\nContent-Length: 0\r\n\r\n";
            let _ = socket.write_all(resp.as_bytes()).await;
        });

        let res = Upstream::initialize(
            &format!("http://127.0.0.1:{port}/acp"),
            "secret".into(),
            Recorder::disabled(),
        )
        .await;

        let Err(err) = res else {
            panic!("expected initialize to fail on 302");
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("302"),
            "expected initialize error to mention 302, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn subscribe_streams_body_without_overall_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
            socket.write_all(headers.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let frame1 = "data: {\"frame\": 1}\n\n";
            socket.write_all(frame1.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let frame2 = "data: {\"frame\": 2}\n\n";
            socket.write_all(frame2.as_bytes()).await.unwrap();
        });

        let mut upstream = Upstream::resume(
            &format!("http://127.0.0.1:{port}/acp"),
            "secret".into(),
            "conn-test".into(),
            Recorder::disabled(),
        )
        .expect("resume ok");

        upstream.subscribe(None).await.expect("subscribe ok");

        let first = upstream
            .next(Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first,
            Event::Frame(StreamKey::Conn, serde_json::json!({"frame": 1}), None)
        );

        let second = upstream
            .next(Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            second,
            Event::Frame(StreamKey::Conn, serde_json::json!({"frame": 2}), None)
        );
    }

    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn stream_features_parse_from_header() {
        let mut h = HeaderMap::new();
        assert_eq!(StreamFeatures::from_headers(&h), StreamFeatures::default());
        assert!(!StreamFeatures::from_headers(&h).is_patched());
        h.insert("acp-stream-features", HeaderValue::from_static("takeover"));
        assert_eq!(
            StreamFeatures::from_headers(&h),
            StreamFeatures {
                takeover: true,
                replay: false
            }
        );
        h.insert(
            "acp-stream-features",
            HeaderValue::from_static("takeover, replay, later-thing"),
        );
        assert!(StreamFeatures::from_headers(&h).is_patched());
    }

    #[test]
    fn replay_outcome_trusts_only_an_explicit_complete() {
        let mut h = HeaderMap::new();
        assert_eq!(
            ReplayOutcome::from_headers(false, &h),
            ReplayOutcome::NotRequested
        );
        assert_eq!(
            ReplayOutcome::from_headers(true, &h),
            ReplayOutcome::Unsupported
        );
        h.insert("acp-replay", HeaderValue::from_static("gap"));
        assert_eq!(ReplayOutcome::from_headers(true, &h), ReplayOutcome::Gap);
        h.insert("acp-replay", HeaderValue::from_static("complete"));
        assert_eq!(
            ReplayOutcome::from_headers(true, &h),
            ReplayOutcome::Complete
        );
        assert!(!ReplayOutcome::Complete.needs_reconcile());
        for outcome in [
            ReplayOutcome::NotRequested,
            ReplayOutcome::Unsupported,
            ReplayOutcome::Gap,
        ] {
            assert!(outcome.needs_reconcile());
        }
    }

    async fn one_shot_http_server(
        status_line: &str,
        response_headers: &str,
        body: &str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/acp", listener.local_addr().unwrap());
        let status_line = status_line.to_string();
        let response_headers = response_headers.to_string();
        let body = body.to_string();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut head = String::new();
            while !head.contains("\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                head.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            let response = format!(
                "HTTP/1.1 {status_line}\r\nconnection: close\r\n{response_headers}\r\n{body}"
            );
            sock.write_all(response.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
            head.to_ascii_lowercase()
        });
        (url, handle)
    }

    /// Accepts one GET, returns its lowercased request head, answers with an SSE body.
    async fn one_shot_sse_server(
        response_headers: &str,
        body: &str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let headers = format!("content-type: text/event-stream\r\n{response_headers}");
        one_shot_http_server("200 OK", &headers, body).await
    }

    #[tokio::test]
    async fn subscribe_with_sends_takeover_and_resume_and_drops_replayed_duplicates() {
        let (url, server) = one_shot_sse_server(
            "acp-stream-features: takeover, replay\r\nacp-replay: complete\r\n",
            "id: 42\ndata: {\"n\":42}\n\nid: 43\ndata: {\"n\":43}\n\n",
        )
        .await;
        let mut up = Upstream::resume(&url, "s".into(), "c".into(), Recorder::disabled()).unwrap();
        let out = up
            .subscribe_with(
                None,
                SubscribeOpts {
                    takeover: true,
                    resume_after: Some(42),
                },
            )
            .await
            .unwrap();
        let head = server.await.unwrap();
        assert!(head.contains("acp-takeover: 1"), "{head}");
        assert!(head.contains("last-event-id: 42"), "{head}");
        assert!(out.took_over);
        assert_eq!(out.replay, ReplayOutcome::Complete);
        assert_eq!(
            up.stream_features(),
            Some(StreamFeatures {
                takeover: true,
                replay: true
            })
        );
        let first = up.next(Duration::from_secs(1)).await.unwrap().unwrap();
        assert_eq!(
            first.frame(),
            Some(&serde_json::json!({"n": 43})),
            "id 42 was already processed"
        );
        assert_eq!(first.event_id(), Some(43));
    }

    #[tokio::test]
    async fn subscribe_without_resume_sends_no_last_event_id() {
        let (url, server) = one_shot_sse_server("", "data: {\"n\":1}\n\n").await;
        let mut up = Upstream::resume(&url, "s".into(), "c".into(), Recorder::disabled()).unwrap();
        let out = up
            .subscribe_with(
                None,
                SubscribeOpts {
                    takeover: true,
                    resume_after: None,
                },
            )
            .await
            .unwrap();
        let head = server.await.unwrap();
        assert!(!head.contains("last-event-id"), "{head}");
        assert!(!out.took_over, "an unpatched server cannot honour takeover");
        assert_eq!(out.replay, ReplayOutcome::NotRequested);
        assert_eq!(up.stream_features(), Some(StreamFeatures::default()));
        let first = up.next(Duration::from_secs(1)).await.unwrap().unwrap();
        assert_eq!(first.event_id(), None);
    }

    #[tokio::test]
    async fn subscribe_with_gap_does_not_drop_live_frames_below_resume_after() {
        let (url, server) = one_shot_sse_server(
            "acp-stream-features: takeover, replay\r\nacp-replay: gap\r\n",
            "id: 1\ndata: {\"n\":1}\n\nid: 2\ndata: {\"n\":2}\n\n",
        )
        .await;
        let mut up = Upstream::resume(&url, "s".into(), "c".into(), Recorder::disabled()).unwrap();
        let out = up
            .subscribe_with(
                None,
                SubscribeOpts {
                    takeover: true,
                    resume_after: Some(42),
                },
            )
            .await
            .unwrap();
        let _head = server.await.unwrap();
        assert_eq!(out.replay, ReplayOutcome::Gap);
        let first = up.next(Duration::from_secs(1)).await.unwrap().unwrap();
        assert_eq!(
            first.frame(),
            Some(&serde_json::json!({"n": 1})),
            "id 1 must not be dropped when replay is Gap"
        );
        assert_eq!(first.event_id(), Some(1));
    }

    #[tokio::test]
    async fn subscribe_error_does_not_overwrite_features_but_409_does() {
        // 1. Initial 404 does not set stream_features to Some(unpatched).
        let (url404, srv404) = one_shot_http_server("404 Not Found", "", "not found").await;
        let mut up =
            Upstream::resume(&url404, "s".into(), "c".into(), Recorder::disabled()).unwrap();
        let res = up.subscribe(None).await;
        assert!(res.is_err());
        let _ = srv404.await.unwrap();
        assert_eq!(up.stream_features(), None, "404 must not set features");

        // 2. 409 conflict DOES update features from headers.
        let (url409, srv409) = one_shot_http_server(
            "409 Conflict",
            "acp-stream-features: takeover, replay\r\n",
            "conflict",
        )
        .await;
        let mut up409 =
            Upstream::resume(&url409, "s".into(), "c".into(), Recorder::disabled()).unwrap();
        up409.set_subscribe_deadline(Duration::ZERO);
        let res409 = up409.subscribe(None).await;
        assert!(res409.is_err());
        let _ = srv409.await.unwrap();
        assert_eq!(
            up409.stream_features(),
            Some(StreamFeatures {
                takeover: true,
                replay: true,
            }),
            "409 must update features"
        );

        // 3. A subsequent 404 does not overwrite previously established patched features.
        let (url404_subsequent, srv404_subsequent) =
            one_shot_http_server("404 Not Found", "", "not found").await;
        let mut up_patched = Upstream::resume(
            &url404_subsequent,
            "s".into(),
            "c".into(),
            Recorder::disabled(),
        )
        .unwrap();
        // Manually prime features as if from prior 200/409
        up_patched.features = Some(StreamFeatures {
            takeover: true,
            replay: true,
        });
        let res_subsequent = up_patched.subscribe(None).await;
        assert!(res_subsequent.is_err());
        let _ = srv404_subsequent.await.unwrap();
        assert_eq!(
            up_patched.stream_features(),
            Some(StreamFeatures {
                takeover: true,
                replay: true,
            }),
            "subsequent 404 must not overwrite existing features"
        );
    }
}
