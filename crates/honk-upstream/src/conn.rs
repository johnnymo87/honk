use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::StreamExt;
use reqwest::{StatusCode, Url};
use serde_json::Value;

use crate::record::{Direction, Recorder};
use crate::sse::SseFrame;
use crate::{HttpStatusError, guard_url_with};

fn truncate_body(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let mut truncated: String = trimmed.chars().take(max_chars).collect();
        truncated.push_str("...");
        truncated
    }
}

fn http_status_error(op: &'static str, status: StatusCode, body: String) -> anyhow::Error {
    let err = anyhow::Error::new(HttpStatusError { op, status });
    let trimmed = body.trim();
    if trimmed.is_empty() {
        err
    } else {
        err.context(truncate_body(trimmed, 200))
    }
}

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

#[derive(Debug, Clone)]
pub struct UpstreamOpts {
    pub allow_production: bool,
    pub client_capabilities: serde_json::Value,
}

impl Default for UpstreamOpts {
    fn default() -> Self {
        Self {
            allow_production: false,
            client_capabilities: serde_json::json!({}),
        }
    }
}

pub(crate) struct UpstreamInner {
    pub(crate) client: reqwest::Client,
    pub(crate) url: Url,
    pub(crate) secret: String,
    pub(crate) conn_id: String,
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<Event>,
    pub(crate) stream_tasks: std::sync::Mutex<HashMap<StreamKey, tokio::task::JoinHandle<()>>>,
    pub(crate) generations: std::sync::Mutex<HashMap<StreamKey, u64>>,
    pub(crate) last_byte_at: std::sync::Arc<std::sync::Mutex<HashMap<StreamKey, Instant>>>,
    pub(crate) rec: Recorder,
    pub(crate) features: std::sync::Mutex<Option<StreamFeatures>>,
    pub(crate) subscribe_deadline: Duration,
    pub(crate) subscribe_initial_backoff: Duration,
    pub(crate) subscribe_max_backoff: Duration,
    pub(crate) subscribe_headers_timeout: Duration,
}

impl Drop for UpstreamInner {
    fn drop(&mut self) {
        if let Ok(mut gens) = self.generations.lock() {
            for generation in gens.values_mut() {
                *generation += 1;
            }
        }
        if let Ok(mut tasks) = self.stream_tasks.lock() {
            for (_, handle) in tasks.drain() {
                handle.abort();
            }
        }
        if let Ok(mut bytes) = self.last_byte_at.lock() {
            bytes.clear();
        }
    }
}

/// Cloneable; every method takes &self. Subscribing spawns the reader task, which sends into
/// the shared event channel owned by `EventRx`. Re-subscribing a key aborts that key's old reader.
#[derive(Clone)]
pub struct UpstreamHandle {
    pub(crate) inner: std::sync::Arc<UpstreamInner>,
}

struct HttpTarget<'a> {
    client: &'a reqwest::Client,
    url: &'a Url,
    secret: &'a str,
    conn_id: &'a str,
    rec: &'a Recorder,
}

struct SubscribeTimeouts {
    deadline: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
    headers_timeout: Duration,
}

impl UpstreamHandle {
    fn target(&self) -> HttpTarget<'_> {
        HttpTarget {
            client: &self.inner.client,
            url: &self.inner.url,
            secret: &self.inner.secret,
            conn_id: &self.inner.conn_id,
            rec: &self.inner.rec,
        }
    }

    pub fn conn_id(&self) -> &str {
        &self.inner.conn_id
    }

    pub fn stream_features(&self) -> Option<StreamFeatures> {
        *self.inner.features.lock().unwrap()
    }

    pub fn last_byte_at(&self, key: &StreamKey) -> Option<std::time::Instant> {
        self.inner.last_byte_at.lock().unwrap().get(key).copied()
    }

    pub async fn request_with_id(
        &self,
        id: serde_json::Value,
        method: &str,
        params: serde_json::Value,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        do_request_with_id(&self.target(), id, method, params, session).await
    }

    pub async fn notify(
        &self,
        method: &str,
        params: serde_json::Value,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        do_notify(&self.target(), method, params, session).await
    }

    pub async fn respond(
        &self,
        id: serde_json::Value,
        result: serde_json::Value,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        do_respond(&self.target(), id, result, session).await
    }

    pub async fn subscribe_with(
        &self,
        session: Option<&str>,
        opts: SubscribeOpts,
    ) -> anyhow::Result<SubscribeOutcome> {
        let stream_key = session
            .map(|s| StreamKey::Session(s.to_string()))
            .unwrap_or(StreamKey::Conn);

        let my_gen = {
            let mut gens = self.inner.generations.lock().unwrap();
            let generation = gens.entry(stream_key.clone()).or_insert(0);
            *generation += 1;
            *generation
        };

        let timeouts = SubscribeTimeouts {
            deadline: self.inner.subscribe_deadline,
            initial_backoff: self.inner.subscribe_initial_backoff,
            max_backoff: self.inner.subscribe_max_backoff,
            headers_timeout: self.inner.subscribe_headers_timeout,
        };

        let (outcome, task_handle) = do_subscribe_with(
            &self.target(),
            &self.inner.tx,
            &self.inner.last_byte_at,
            session,
            opts,
            timeouts,
        )
        .await?;

        {
            let gens = self.inner.generations.lock().unwrap();
            let current_gen = gens.get(&stream_key).copied().unwrap_or(0);
            if current_gen != my_gen {
                task_handle.abort();
                anyhow::bail!(
                    "subscribe to {} superseded by newer operation",
                    stream_key.stream_record_name()
                );
            }

            let mut tasks = self.inner.stream_tasks.lock().unwrap();
            if let Some(displaced) = tasks.insert(stream_key.clone(), task_handle) {
                displaced.abort();
            }
        }

        self.inner
            .last_byte_at
            .lock()
            .unwrap()
            .insert(stream_key, Instant::now());

        *self.inner.features.lock().unwrap() = Some(outcome.features);
        Ok(outcome)
    }

    pub fn unsubscribe(&self, session: Option<&str>) {
        let stream_key = session
            .map(|s| StreamKey::Session(s.to_string()))
            .unwrap_or(StreamKey::Conn);

        {
            let mut gens = self.inner.generations.lock().unwrap();
            let generation = gens.entry(stream_key.clone()).or_insert(0);
            *generation += 1;
        }

        if let Some(prev) = self.inner.stream_tasks.lock().unwrap().remove(&stream_key) {
            prev.abort();
            self.inner.rec.record_marker(
                stream_key.stream_record_name(),
                format!("unsubscribed from {}", stream_key.stream_record_name()),
            );
        }
        self.inner.last_byte_at.lock().unwrap().remove(&stream_key);
    }

    pub async fn delete(&self) -> anyhow::Result<reqwest::StatusCode> {
        {
            let mut gens = self.inner.generations.lock().unwrap();
            for generation in gens.values_mut() {
                *generation += 1;
            }
        }
        if let Ok(mut tasks) = self.inner.stream_tasks.lock() {
            for (_, handle) in tasks.drain() {
                handle.abort();
            }
        }
        self.inner.last_byte_at.lock().unwrap().clear();
        self.inner
            .rec
            .record_marker("conn", "delete: aborted all SSE stream tasks");
        do_delete(&self.target()).await
    }
}

pub struct EventRx {
    pub(crate) rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    pub(crate) buffered: VecDeque<Event>,
}

impl EventRx {
    pub async fn recv(&mut self) -> Option<Event> {
        if let Some(event) = self.buffered.pop_front() {
            return Some(event);
        }
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Result<Event, tokio::sync::mpsc::error::TryRecvError> {
        if let Some(event) = self.buffered.pop_front() {
            return Ok(event);
        }
        self.rx.try_recv()
    }
}

async fn do_request_with_id(
    target: &HttpTarget<'_>,
    id: Value,
    method: &str,
    params: Value,
    session: Option<&str>,
) -> anyhow::Result<()> {
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });

    let mut req = target
        .client
        .post(target.url.clone())
        .timeout(POST_TIMEOUT)
        .header("x-secret-key", target.secret)
        .header("acp-connection-id", target.conn_id)
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
    target.rec.record(
        Direction::Out,
        "post",
        Some(status.as_u16()),
        Some(frame),
        None,
    );

    if status != StatusCode::ACCEPTED {
        let err_text = res.text().await.unwrap_or_default();
        return Err(http_status_error("POST request", status, err_text));
    }

    Ok(())
}

async fn do_notify(
    target: &HttpTarget<'_>,
    method: &str,
    params: Value,
    session: Option<&str>,
) -> anyhow::Result<()> {
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });

    let mut req = target
        .client
        .post(target.url.clone())
        .timeout(POST_TIMEOUT)
        .header("x-secret-key", target.secret)
        .header("acp-connection-id", target.conn_id)
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
    target.rec.record(
        Direction::Out,
        "post",
        Some(status.as_u16()),
        Some(frame),
        None,
    );

    if status != StatusCode::ACCEPTED {
        let err_text = res.text().await.unwrap_or_default();
        return Err(http_status_error("POST notify", status, err_text));
    }

    Ok(())
}

async fn do_respond(
    target: &HttpTarget<'_>,
    id: Value,
    result: Value,
    session: Option<&str>,
) -> anyhow::Result<()> {
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });

    let mut req = target
        .client
        .post(target.url.clone())
        .timeout(POST_TIMEOUT)
        .header("x-secret-key", target.secret)
        .header("acp-connection-id", target.conn_id)
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
    target.rec.record(
        Direction::Out,
        "post",
        Some(status.as_u16()),
        Some(frame),
        None,
    );

    if status != StatusCode::ACCEPTED {
        let err_text = res.text().await.unwrap_or_default();
        return Err(http_status_error("POST respond", status, err_text));
    }

    Ok(())
}

async fn do_delete(target: &HttpTarget<'_>) -> anyhow::Result<StatusCode> {
    let res = target
        .client
        .delete(target.url.clone())
        .timeout(POST_TIMEOUT)
        .header("x-secret-key", target.secret)
        .header("acp-connection-id", target.conn_id)
        .send()
        .await
        .with_context(|| "failed to send DELETE /acp")?;

    let status = res.status();
    target.rec.record(
        Direction::Out,
        "post",
        Some(status.as_u16()),
        None,
        Some("DELETE /acp".into()),
    );

    if !status.is_success() {
        let err_text = res.text().await.unwrap_or_default();
        return Err(http_status_error("DELETE /acp", status, err_text));
    }

    Ok(status)
}

async fn do_subscribe_with(
    target: &HttpTarget<'_>,
    tx: &tokio::sync::mpsc::UnboundedSender<Event>,
    last_byte_at: &std::sync::Arc<std::sync::Mutex<HashMap<StreamKey, Instant>>>,
    session: Option<&str>,
    opts: SubscribeOpts,
    timeouts: SubscribeTimeouts,
) -> anyhow::Result<(SubscribeOutcome, tokio::task::JoinHandle<()>)> {
    let stream_key = session
        .map(|s| StreamKey::Session(s.to_string()))
        .unwrap_or(StreamKey::Conn);

    let stream_name = stream_key.stream_record_name();
    let start = Instant::now();
    let mut first_409: Option<Instant> = None;
    let mut backoff = timeouts.initial_backoff;

    loop {
        let mut req = target
            .client
            .get(target.url.clone())
            .header("accept", "text/event-stream")
            .header("x-secret-key", target.secret)
            .header("acp-connection-id", target.conn_id);

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
        let res = tokio::time::timeout(timeouts.headers_timeout, send_fut)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "timed out after {:?} waiting for headers when subscribing to {stream_name}",
                    timeouts.headers_timeout
                )
            })?
            .with_context(|| format!("failed to send GET request for {stream_name}"))?;

        let features = StreamFeatures::from_headers(res.headers());
        let status = res.status();
        if status == StatusCode::CONFLICT {
            if first_409.is_none() {
                first_409 = Some(Instant::now());
            }
            target.rec.record_marker(
                &stream_name,
                format!("409 Conflict subscribing to {stream_name}"),
            );
            if start.elapsed() >= timeouts.deadline {
                anyhow::bail!(
                    "timed out after {:?} waiting for 409 conflict to resolve on {stream_name}",
                    timeouts.deadline
                );
            }
            tokio::time::sleep(backoff).await;
            backoff = std::cmp::min(backoff * 2, timeouts.max_backoff);
            continue;
        }

        if !status.is_success() {
            let err_text = res.text().await.unwrap_or_default();
            target.rec.record(
                Direction::Marker,
                &stream_name,
                Some(status.as_u16()),
                None,
                Some(format!("GET stream failed with status {status}")),
            );
            return Err(http_status_error("GET stream", status, err_text));
        }

        let conflict_window = first_409.map(|t| t.elapsed()).unwrap_or(Duration::ZERO);
        let replay = ReplayOutcome::from_headers(opts.resume_after.is_some(), res.headers());
        let took_over = opts.takeover && features.takeover;

        target.rec.record_marker(
            &stream_name,
            format!("subscribed to {stream_name} (409 window: {conflict_window:?})"),
        );

        if opts.takeover || opts.resume_after.is_some() {
            target.rec.record_marker(
                &stream_name,
                format!(
                    "stream options: took_over={took_over} resume_after={:?} replay={replay:?} features={features:?}",
                    opts.resume_after
                ),
            );
        }

        let last_byte_at_map = last_byte_at.clone();
        let stamp_key = stream_key.clone();
        let stamped_stream = res.bytes_stream().inspect(move |_| {
            if let Ok(mut map) = last_byte_at_map.lock()
                && let Some(entry) = map.get_mut(&stamp_key)
            {
                *entry = Instant::now();
            }
        });

        let sse_stream = crate::sse::sse_to_frames(stamped_stream);
        let tx = tx.clone();
        let rec = target.rec.clone();
        let key = stream_key.clone();
        let s_name = stream_name.clone();

        let handle = tokio::spawn(async move {
            let mut stream = sse_stream;
            let mut stream_err = None;
            let mut floor = opts.resume_after;
            loop {
                tokio::select! {
                    biased;
                    _ = tx.closed() => {
                        break;
                    }
                    item = stream.next() => {
                        match item {
                            Some(Ok(SseFrame { id, data })) => {
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
                                    break;
                                }
                            }
                            Some(Err(err)) => {
                                let err_str = err.to_string();
                                rec.record_marker(&s_name, format!("stream error: {err_str}"));
                                stream_err = Some(err_str);
                                break;
                            }
                            None => {
                                break;
                            }
                        }
                    }
                }
            }
            rec.record_marker(&s_name, format!("stream {s_name} ended"));
            let _ = tx.send(Event::StreamEnded {
                key,
                error: stream_err,
            });
        });

        return Ok((
            SubscribeOutcome {
                conflict_window,
                took_over,
                replay,
                features,
            },
            handle,
        ));
    }
}

/// Upstream ACP HTTP+SSE client.
pub struct Upstream {
    pub(crate) url: Url,
    pub(crate) secret: String,
    pub(crate) conn_id: String,
    init_result: Option<Value>,
    client: reqwest::Client,
    next_request_id: AtomicU64,
    rec: Recorder,
    pub(crate) stream_tasks: HashMap<StreamKey, tokio::task::JoinHandle<()>>,
    pub(crate) generations: HashMap<StreamKey, u64>,
    pub(crate) last_byte_at: std::sync::Arc<std::sync::Mutex<HashMap<StreamKey, Instant>>>,
    pub(crate) rx: Option<tokio::sync::mpsc::UnboundedReceiver<Event>>,
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<Event>,
    pub(crate) buffered_events: VecDeque<Event>,
    subscribe_deadline: Duration,
    subscribe_initial_backoff: Duration,
    subscribe_max_backoff: Duration,
    subscribe_headers_timeout: Duration,
    last_call_stream: Option<StreamKey>,
    features: Option<StreamFeatures>,
    split_done: bool,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        if !self.split_done {
            self.hard_drop();
        }
    }
}

impl Upstream {
    /// Connects and initializes a new ACP connection.
    ///
    /// Validates `url` via [`guard_url`], sends `initialize` JSON-RPC POST,
    /// stores the resulting `Acp-Connection-Id`, and records all frames.
    pub async fn initialize(url: &str, secret: String, rec: Recorder) -> anyhow::Result<Self> {
        Self::initialize_with(url, secret, rec, &UpstreamOpts::default()).await
    }

    pub async fn initialize_with(
        url: &str,
        secret: String,
        rec: Recorder,
        opts: &UpstreamOpts,
    ) -> anyhow::Result<Self> {
        let parsed_url = guard_url_with(url, opts.allow_production)?;
        let trimmed_secret = secret.trim().to_string();
        let client = build_http_client()?;

        let client_caps = if opts.client_capabilities.is_null() {
            serde_json::json!({})
        } else {
            opts.client_capabilities.clone()
        };

        let init_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": client_caps,
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
            .context("failed to send POST initialize")?;

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
            return Err(http_status_error("initialize", status, err_text));
        }

        let conn_id = res
            .headers()
            .get("acp-connection-id")
            .ok_or_else(|| anyhow::anyhow!("initialize response missing Acp-Connection-Id header"))?
            .to_str()
            .with_context(|| "invalid Acp-Connection-Id header")?
            .trim()
            .to_string();

        let body_text = res
            .text()
            .await
            .context("failed to read initialize response body")?;
        let init_response: Value = serde_json::from_str(&body_text).with_context(|| {
            format!(
                "failed to parse initialize response JSON body: {}",
                truncate_body(&body_text, 200)
            )
        })?;

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
            generations: HashMap::new(),
            last_byte_at: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            rx: Some(rx),
            tx,
            buffered_events: VecDeque::new(),
            subscribe_deadline: Duration::from_secs(60),
            subscribe_initial_backoff: Duration::from_millis(100),
            subscribe_max_backoff: Duration::from_secs(2),
            subscribe_headers_timeout: SUBSCRIBE_HEADERS_TIMEOUT,
            last_call_stream: None,
            features: None,
            split_done: false,
        })
    }

    /// Resumes an existing connection id without re-running initialize.
    pub fn resume(
        url: &str,
        secret: String,
        conn_id: String,
        rec: Recorder,
    ) -> anyhow::Result<Self> {
        Self::resume_with(url, secret, conn_id, rec, &UpstreamOpts::default())
    }

    pub fn resume_with(
        url: &str,
        secret: String,
        conn_id: String,
        rec: Recorder,
        opts: &UpstreamOpts,
    ) -> anyhow::Result<Self> {
        let parsed_url = guard_url_with(url, opts.allow_production)?;
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
            generations: HashMap::new(),
            last_byte_at: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            rx: Some(rx),
            tx,
            buffered_events: VecDeque::new(),
            subscribe_deadline: Duration::from_secs(60),
            subscribe_initial_backoff: Duration::from_millis(100),
            subscribe_max_backoff: Duration::from_secs(2),
            subscribe_headers_timeout: SUBSCRIBE_HEADERS_TIMEOUT,
            last_call_stream: None,
            features: None,
            split_done: false,
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

    pub fn last_byte_at(&self, key: &StreamKey) -> Option<std::time::Instant> {
        self.last_byte_at.lock().unwrap().get(key).copied()
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
        self.request_with_id(id.clone(), method, params, session)
            .await?;
        Ok(id)
    }

    fn target(&self) -> HttpTarget<'_> {
        HttpTarget {
            client: &self.client,
            url: &self.url,
            secret: &self.secret,
            conn_id: &self.conn_id,
            rec: &self.rec,
        }
    }

    pub async fn request_with_id(
        &self,
        id: serde_json::Value,
        method: &str,
        params: serde_json::Value,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        do_request_with_id(&self.target(), id, method, params, session).await
    }

    /// Sends a JSON-RPC notification frame (no `id`).
    /// Expects HTTP 202 Accepted.
    pub async fn notify(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        do_notify(&self.target(), method, params, session).await
    }

    /// Sends a JSON-RPC response frame back to the server.
    /// Expects HTTP 202 Accepted.
    pub async fn respond(
        &self,
        id: Value,
        result: Value,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        do_respond(&self.target(), id, result, session).await
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

        let my_gen = {
            let generation = self.generations.entry(stream_key.clone()).or_insert(0);
            *generation += 1;
            *generation
        };

        let timeouts = SubscribeTimeouts {
            deadline: self.subscribe_deadline,
            initial_backoff: self.subscribe_initial_backoff,
            max_backoff: self.subscribe_max_backoff,
            headers_timeout: self.subscribe_headers_timeout,
        };

        let (outcome, task_handle) = do_subscribe_with(
            &self.target(),
            &self.tx,
            &self.last_byte_at,
            session,
            opts,
            timeouts,
        )
        .await?;

        let current_gen = self.generations.get(&stream_key).copied().unwrap_or(0);
        if current_gen != my_gen {
            task_handle.abort();
            anyhow::bail!(
                "subscribe to {} superseded by newer operation",
                stream_key.stream_record_name()
            );
        }

        if let Some(displaced) = self.stream_tasks.insert(stream_key.clone(), task_handle) {
            displaced.abort();
        }

        self.last_byte_at
            .lock()
            .unwrap()
            .insert(stream_key, Instant::now());

        self.features = Some(outcome.features);
        Ok(outcome)
    }

    /// Cancel-safe: buffered events first, then the channel.
    pub async fn recv(&mut self) -> Option<Event> {
        if let Some(event) = self.buffered_events.pop_front() {
            return Some(event);
        }
        if let Some(rx) = self.rx.as_mut() {
            rx.recv().await
        } else {
            None
        }
    }

    pub fn unsubscribe(&mut self, session: Option<&str>) {
        let stream_key = session
            .map(|s| StreamKey::Session(s.to_string()))
            .unwrap_or(StreamKey::Conn);

        let generation = self.generations.entry(stream_key.clone()).or_insert(0);
        *generation += 1;

        if let Some(prev) = self.stream_tasks.remove(&stream_key) {
            prev.abort();
            self.rec.record_marker(
                stream_key.stream_record_name(),
                format!("unsubscribed from {}", stream_key.stream_record_name()),
            );
        }
        self.last_byte_at.lock().unwrap().remove(&stream_key);
    }

    /// I2: split into a cloneable handle (POST/DELETE/subscribe) and the single event receiver.
    pub fn split(mut self) -> (UpstreamHandle, EventRx) {
        self.split_done = true;
        let tasks = std::mem::take(&mut self.stream_tasks);
        let gens = std::mem::take(&mut self.generations);
        let rx = self.rx.take().expect("Upstream::split called once");
        let buffered = std::mem::take(&mut self.buffered_events);

        let handle = UpstreamHandle {
            inner: std::sync::Arc::new(UpstreamInner {
                client: self.client.clone(),
                url: self.url.clone(),
                secret: std::mem::take(&mut self.secret),
                conn_id: std::mem::take(&mut self.conn_id),
                tx: self.tx.clone(),
                stream_tasks: std::sync::Mutex::new(tasks),
                generations: std::sync::Mutex::new(gens),
                last_byte_at: self.last_byte_at.clone(),
                rec: self.rec.clone(),
                features: std::sync::Mutex::new(self.features),
                subscribe_deadline: self.subscribe_deadline,
                subscribe_initial_backoff: self.subscribe_initial_backoff,
                subscribe_max_backoff: self.subscribe_max_backoff,
                subscribe_headers_timeout: self.subscribe_headers_timeout,
            }),
        };
        let event_rx = EventRx { rx, buffered };
        (handle, event_rx)
    }

    /// Reads the next event, checking buffered events first, then waiting on SSE streams up to `timeout`.
    /// Returns `Ok(None)` if `timeout` expires.
    pub async fn next(&mut self, timeout: Duration) -> anyhow::Result<Option<Event>> {
        if let Some(buffered) = self.buffered_events.pop_front() {
            return Ok(Some(buffered));
        }

        if let Some(rx) = self.rx.as_mut() {
            match tokio::time::timeout(timeout, rx.recv()).await {
                Ok(Some(item)) => Ok(Some(item)),
                Ok(None) => Ok(None),
                Err(_) => Ok(None),
            }
        } else {
            Ok(None)
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
        for generation in self.generations.values_mut() {
            *generation += 1;
        }
        for (_, handle) in self.stream_tasks.drain() {
            handle.abort();
        }
        self.last_byte_at.lock().unwrap().clear();
        self.rec
            .record_marker("conn", "hard_drop: aborted all SSE stream tasks");
    }

    /// Terminates the connection via DELETE /acp.
    pub async fn delete(mut self) -> anyhow::Result<StatusCode> {
        self.split_done = true;
        self.hard_drop();
        do_delete(&self.target()).await
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

    /// Accepts one GET, returns its lowercased request head, answers with an SSE body.
    async fn one_shot_sse_server(
        response_headers: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/acp", listener.local_addr().unwrap());
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
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n{response_headers}\r\n{body}"
            );
            sock.write_all(response.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
            head.to_ascii_lowercase()
        });
        (url, handle)
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
    async fn recv_drains_buffer_then_channel() {
        let mut up = Upstream::resume(
            "http://127.0.0.1:4180/acp",
            "s".into(),
            "c".into(),
            Recorder::disabled(),
        )
        .unwrap();
        up.buffered_events.push_back(Event::Frame(
            StreamKey::Conn,
            serde_json::json!({"a": 1}),
            None,
        ));
        up.tx
            .send(Event::Frame(
                StreamKey::Conn,
                serde_json::json!({"b": 2}),
                None,
            ))
            .unwrap();
        assert_eq!(up.recv().await.unwrap().frame().unwrap()["a"], 1);
        assert_eq!(up.recv().await.unwrap().frame().unwrap()["b"], 2);
    }

    #[tokio::test]
    async fn unsubscribe_aborts_only_that_stream() {
        let mut up = Upstream::resume(
            "http://127.0.0.1:4180/acp",
            "s".into(),
            "c".into(),
            Recorder::disabled(),
        )
        .unwrap();

        let (conn_tx, mut conn_rx) = tokio::sync::oneshot::channel::<()>();
        let conn_task = tokio::spawn(async move {
            let _guard = conn_tx;
            std::future::pending::<()>().await
        });

        let (session_tx, session_rx) = tokio::sync::oneshot::channel::<()>();
        let session_task = tokio::spawn(async move {
            let _guard = session_tx;
            std::future::pending::<()>().await
        });

        up.stream_tasks.insert(StreamKey::Conn, conn_task);
        up.stream_tasks
            .insert(StreamKey::Session("x".into()), session_task);

        up.unsubscribe(Some("x"));

        assert!(up.stream_tasks.contains_key(&StreamKey::Conn));
        assert!(
            !up.stream_tasks
                .contains_key(&StreamKey::Session("x".into()))
        );

        // Session task must have been aborted (drop guard dropped)
        let session_aborted = tokio::time::timeout(Duration::from_secs(1), session_rx).await;
        assert!(session_aborted.is_ok(), "session task abort timed out");
        assert!(
            session_aborted.unwrap().is_err(),
            "session task should have been aborted"
        );

        // Conn task must NOT have been aborted
        assert!(
            matches!(
                conn_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "conn task was unexpectedly aborted"
        );
    }

    #[test]
    fn resume_with_opt_in_accepts_production_port() {
        let opts = UpstreamOpts {
            allow_production: true,
            ..Default::default()
        };
        assert!(
            Upstream::resume_with(
                "http://127.0.0.1:4080/acp",
                "s".into(),
                "c".into(),
                Recorder::disabled(),
                &opts
            )
            .is_ok()
        );
        let default_opts = UpstreamOpts::default();
        assert!(
            Upstream::resume_with(
                "http://127.0.0.1:4080/acp",
                "s".into(),
                "c".into(),
                Recorder::disabled(),
                &default_opts
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn split_handle_concurrent_post_and_unified_events() {
        use axum::Router;
        use axum::http::HeaderMap;
        use axum::routing::post;

        let app = Router::new().route(
            "/acp",
            post(|| async { StatusCode::ACCEPTED }).get(|headers: HeaderMap| async move {
                let is_session = headers.get("acp-session-id").is_some();
                let body = if is_session {
                    "id: 20\ndata: {\"from\":\"session\"}\n\n"
                } else {
                    "id: 10\ndata: {\"from\":\"conn\"}\n\n"
                };
                (
                    [
                        ("content-type", "text/event-stream"),
                        ("cache-control", "no-cache"),
                    ],
                    body,
                )
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://127.0.0.1:{port}/acp");
        let upstream =
            Upstream::resume(&url, "sec".into(), "c1".into(), Recorder::disabled()).unwrap();
        let (h1, mut rx) = upstream.split();
        let h2 = h1.clone();

        h1.subscribe_with(None, SubscribeOpts::default())
            .await
            .unwrap();
        h2.subscribe_with(Some("s1"), SubscribeOpts::default())
            .await
            .unwrap();

        let post1 = h1.request_with_id(
            serde_json::json!(1),
            "test_method1",
            serde_json::json!({}),
            None,
        );
        let post2 = h2.request_with_id(
            serde_json::json!(2),
            "test_method2",
            serde_json::json!({}),
            Some("s1"),
        );
        let (res1, res2) = tokio::join!(post1, post2);
        res1.unwrap();
        res2.unwrap();

        let mut frames = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while frames.len() < 2 {
                let ev = rx.recv().await.expect("recv event");
                if let Event::Frame(key, data, _) = ev {
                    frames.push((key, data));
                }
            }
        })
        .await
        .expect("timed out waiting for frames from both streams");

        let mut keys: Vec<StreamKey> = frames.into_iter().map(|(k, _)| k).collect();
        keys.sort_by_key(|k| k.stream_record_name());
        assert_eq!(keys, vec![StreamKey::Conn, StreamKey::Session("s1".into())]);
    }

    #[tokio::test]
    async fn drop_all_handles_aborts_readers_and_closes_event_rx() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;

        let (_tx, rx) =
            futures::channel::mpsc::unbounded::<Result<bytes::Bytes, std::convert::Infallible>>();
        let body_holder = std::sync::Arc::new(std::sync::Mutex::new(Some(Body::from_stream(rx))));

        let app = Router::new().route(
            "/acp",
            get(move || {
                let body_holder = body_holder.clone();
                async move {
                    let body = body_holder.lock().unwrap().take().unwrap_or_default();
                    (
                        [
                            ("content-type", "text/event-stream"),
                            ("cache-control", "no-cache"),
                        ],
                        body,
                    )
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://127.0.0.1:{port}/acp");
        let upstream =
            Upstream::resume(&url, "sec".into(), "c1".into(), Recorder::disabled()).unwrap();
        let (handle, mut event_rx) = upstream.split();

        handle
            .subscribe_with(None, SubscribeOpts::default())
            .await
            .unwrap();

        let handle_clone = handle.clone();

        // Drop first handle; handle_clone is still alive, so rx stays open
        drop(handle);

        // Drop last handle; UpstreamInner drops, aborting reader task and dropping tx
        drop(handle_clone);

        // event_rx should close and return None within timeout
        let res = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timed out waiting for event_rx.recv()");
        assert_eq!(res, None);
    }

    #[tokio::test]
    async fn drop_unsplit_upstream_aborts_readers() {
        let mut up = Upstream::resume(
            "http://127.0.0.1:4180/acp",
            "s".into(),
            "c".into(),
            Recorder::disabled(),
        )
        .unwrap();

        let (task_tx, task_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _guard = task_tx;
            std::future::pending::<()>().await
        });
        up.stream_tasks.insert(StreamKey::Conn, task);

        drop(up);

        let res = tokio::time::timeout(Duration::from_secs(1), task_rx).await;
        assert!(res.is_ok(), "task drop guard not closed in time");
        assert!(
            res.unwrap().is_err(),
            "task was not aborted on Upstream drop"
        );
    }

    #[tokio::test]
    async fn unsubscribe_during_subscribe_aborts_stale_task() {
        use axum::Router;
        use axum::extract::Request;
        use axum::routing::get;

        let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
        let gate = std::sync::Arc::new(tokio::sync::Mutex::new(Some(gate_rx)));

        let app = Router::new().route(
            "/acp",
            get({
                let gate = gate.clone();
                move |_: Request| {
                    let gate = gate.clone();
                    async move {
                        let rx = gate.lock().await.take();
                        if let Some(rx) = rx {
                            let _ = rx.await;
                        }
                        (
                            [
                                ("content-type", "text/event-stream"),
                                ("cache-control", "no-cache"),
                            ],
                            "id: 1\ndata: {}\n\n",
                        )
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://127.0.0.1:{port}/acp");
        let upstream =
            Upstream::resume(&url, "sec".into(), "c1".into(), Recorder::disabled()).unwrap();
        let (handle, _rx) = upstream.split();

        let h = handle.clone();
        let join_sub =
            tokio::spawn(async move { h.subscribe_with(None, SubscribeOpts::default()).await });

        // Allow subscribe_with to connect and block on the gate
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Unsubscribe while subscribe_with is in-flight
        handle.unsubscribe(None);

        // Release the gate
        let _ = gate_tx.send(());

        let sub_res = tokio::time::timeout(Duration::from_secs(2), join_sub)
            .await
            .expect("subscribe_with timed out")
            .expect("join failed");

        assert!(sub_res.is_err(), "expected in-flight subscribe to fail");
        assert!(handle.inner.stream_tasks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn last_byte_at_advances_on_keepalive_only_chunk() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;

        let (tx, rx) =
            futures::channel::mpsc::unbounded::<Result<bytes::Bytes, std::convert::Infallible>>();
        let body_holder = std::sync::Arc::new(std::sync::Mutex::new(Some(Body::from_stream(rx))));

        let app = Router::new().route(
            "/acp",
            get(move || {
                let body_holder = body_holder.clone();
                async move {
                    let body = body_holder.lock().unwrap().take().unwrap_or_default();
                    (
                        [
                            ("content-type", "text/event-stream"),
                            ("cache-control", "no-cache"),
                        ],
                        body,
                    )
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://127.0.0.1:{port}/acp");
        let upstream =
            Upstream::resume(&url, "sec".into(), "c1".into(), Recorder::disabled()).unwrap();
        let (handle, mut rx) = upstream.split();

        handle
            .subscribe_with(None, SubscribeOpts::default())
            .await
            .unwrap();

        let t0 = handle.last_byte_at(&StreamKey::Conn).expect("t0 present");

        tokio::time::sleep(Duration::from_millis(20)).await;

        tx.unbounded_send(Ok(bytes::Bytes::from(": keepalive\n\n")))
            .unwrap();

        let start = Instant::now();
        let mut advanced = false;
        while start.elapsed() < Duration::from_secs(1) {
            if handle
                .last_byte_at(&StreamKey::Conn)
                .is_some_and(|t1| t1 > t0)
            {
                advanced = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(advanced, "last_byte_at did not advance on keepalive chunk");

        // Keepalive comments should yield no events
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn concurrent_subscribes_newer_survives_when_older_completes_last() {
        use axum::Router;
        use axum::body::Body;
        use axum::extract::Request;
        use axum::routing::get;

        let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
        let gate = std::sync::Arc::new(tokio::sync::Mutex::new(Some(gate_rx)));
        let request_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let (_sub2_body_tx, sub2_body_rx) =
            futures::channel::mpsc::unbounded::<Result<bytes::Bytes, std::convert::Infallible>>();
        let sub2_body_holder =
            std::sync::Arc::new(std::sync::Mutex::new(Some(Body::from_stream(sub2_body_rx))));

        let app = Router::new().route(
            "/acp",
            get({
                let gate = gate.clone();
                let request_count = request_count.clone();
                let sub2_body_holder = sub2_body_holder.clone();
                move |_: Request| {
                    let gate = gate.clone();
                    let count = request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let sub2_body_holder = sub2_body_holder.clone();
                    async move {
                        if count == 0 {
                            // First request (older subscribe): wait on gate
                            let rx = gate.lock().await.take();
                            if let Some(rx) = rx {
                                let _ = rx.await;
                            }
                            (
                                [
                                    ("content-type", "text/event-stream"),
                                    ("cache-control", "no-cache"),
                                ],
                                Body::from("id: 1\ndata: {}\n\n"),
                            )
                        } else {
                            // Second request (newer subscribe): stream stays open
                            let body = sub2_body_holder.lock().unwrap().take().unwrap_or_default();
                            (
                                [
                                    ("content-type", "text/event-stream"),
                                    ("cache-control", "no-cache"),
                                ],
                                body,
                            )
                        }
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://127.0.0.1:{port}/acp");
        let upstream =
            Upstream::resume(&url, "sec".into(), "c1".into(), Recorder::disabled()).unwrap();
        let (handle, _rx) = upstream.split();

        let h1 = handle.clone();
        let task1 =
            tokio::spawn(async move { h1.subscribe_with(None, SubscribeOpts::default()).await });

        // Ensure task1 has started and is waiting on gate
        tokio::time::sleep(Duration::from_millis(50)).await;

        let h2 = handle.clone();
        // Second subscribe (newer, higher generation): completes immediately
        let sub2_res = h2.subscribe_with(None, SubscribeOpts::default()).await;
        assert!(sub2_res.is_ok());

        // Release the gate for task1 so older subscribe completes second
        let _ = gate_tx.send(());

        let sub1_res = tokio::time::timeout(Duration::from_secs(2), task1)
            .await
            .expect("task1 timed out")
            .expect("join failed");

        // The older subscribe must fail because its generation was superseded
        assert!(
            sub1_res.is_err(),
            "expected older subscribe to fail as superseded"
        );

        // The newer subscribe's task in stream_tasks must survive and not be finished
        let tasks = handle.inner.stream_tasks.lock().unwrap();
        assert_eq!(tasks.len(), 1);
        let task = tasks.get(&StreamKey::Conn).expect("Conn task present");
        assert!(
            !task.is_finished(),
            "newer task was unexpectedly aborted or finished"
        );
    }

    #[tokio::test]
    async fn delete_invalidates_in_flight_subscribes() {
        use axum::Router;
        use axum::extract::Request;
        use axum::routing::{delete, get};

        let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
        let gate = std::sync::Arc::new(tokio::sync::Mutex::new(Some(gate_rx)));

        let app = Router::new()
            .route(
                "/acp",
                get({
                    let gate = gate.clone();
                    move |_: Request| {
                        let gate = gate.clone();
                        async move {
                            let rx = gate.lock().await.take();
                            if let Some(rx) = rx {
                                let _ = rx.await;
                            }
                            (
                                [
                                    ("content-type", "text/event-stream"),
                                    ("cache-control", "no-cache"),
                                ],
                                "id: 1\ndata: {}\n\n",
                            )
                        }
                    }
                }),
            )
            .route("/acp", delete(|| async { StatusCode::OK }));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://127.0.0.1:{port}/acp");
        let upstream =
            Upstream::resume(&url, "sec".into(), "c1".into(), Recorder::disabled()).unwrap();
        let (handle, _rx) = upstream.split();

        let h = handle.clone();
        let join_sub =
            tokio::spawn(async move { h.subscribe_with(None, SubscribeOpts::default()).await });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Delete while subscribe is in-flight
        let del_res = handle.delete().await;
        assert!(del_res.is_ok());

        // Release gate
        let _ = gate_tx.send(());

        let sub_res = tokio::time::timeout(Duration::from_secs(2), join_sub)
            .await
            .expect("subscribe_with timed out")
            .expect("join failed");

        assert!(
            sub_res.is_err(),
            "in-flight subscribe should fail after delete"
        );
        assert!(handle.inner.stream_tasks.lock().unwrap().is_empty());
        assert!(handle.inner.last_byte_at.lock().unwrap().is_empty());
    }
}
