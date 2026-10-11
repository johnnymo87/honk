use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};

type Predicate = Box<dyn Fn(&Value) -> bool + Send + Sync>;
type StreamSubscriber = (
    mpsc::UnboundedReceiver<Result<bytes::Bytes, std::convert::Infallible>>,
    oneshot::Receiver<()>,
);

struct StreamState {
    next_seq: u64,
    ring: Vec<(u64, Value)>,
    sender: Option<mpsc::UnboundedSender<Result<bytes::Bytes, std::convert::Infallible>>>,
    evict_tx: Option<oneshot::Sender<()>>,
}

impl StreamState {
    fn new() -> Self {
        Self {
            next_seq: 1,
            ring: Vec::new(),
            sender: None,
            evict_tx: None,
        }
    }

    fn push_frame(&mut self, frame: Value, patched: bool) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.ring.push((seq, frame.clone()));

        if let Some(sender) = &self.sender {
            let sse_str = if patched {
                format!("id: {seq}\ndata: {frame}\n\n")
            } else {
                format!("data: {frame}\n\n")
            };
            let _ = sender.send(Ok(bytes::Bytes::from(sse_str)));
        }
    }

    fn subscribe(
        &mut self,
        patched: bool,
        takeover: bool,
        last_event_id: Option<u64>,
    ) -> Result<StreamSubscriber, ()> {
        if self.sender.is_some() {
            if patched && takeover {
                if let Some(evict) = self.evict_tx.take() {
                    let _ = evict.send(());
                }
                self.sender = None;
            } else {
                return Err(());
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let (evict_tx, evict_rx) = oneshot::channel();
        self.sender = Some(tx.clone());
        self.evict_tx = Some(evict_tx);

        if patched && let Some(last_id) = last_event_id {
            for (seq, frame) in &self.ring {
                if *seq > last_id {
                    let sse_str = format!("id: {seq}\ndata: {frame}\n\n");
                    let _ = tx.send(Ok(bytes::Bytes::from(sse_str)));
                }
            }
        }

        Ok((rx, evict_rx))
    }
}

struct ConnectionState {
    conn_stream: StreamState,
    sessions: HashMap<String, StreamState>,
    active_prompt_cancels: HashMap<String, Arc<AtomicBool>>,
}

impl ConnectionState {
    fn new() -> Self {
        Self {
            conn_stream: StreamState::new(),
            sessions: HashMap::new(),
            active_prompt_cancels: HashMap::new(),
        }
    }
}

struct FakeGooseInner {
    secret: String,
    secret_path: PathBuf,
    advertise_patched: AtomicBool,
    conflict_connections: Mutex<HashSet<String>>,
    lose_next_predicates: Mutex<HashMap<String, Vec<Predicate>>>,
    gap_on_next_resume: Mutex<HashSet<String>>,
    inbound_frames: Mutex<Vec<Value>>,
    deleted_connections: Mutex<Vec<String>>,
    connections: Mutex<HashMap<String, ConnectionState>>,
    next_conn_num: AtomicU64,
    next_session_num: AtomicU64,
    prompt_chunks: Mutex<Vec<String>>,
    export_data: Mutex<HashMap<String, Value>>,
}

pub struct FakeGoose {
    addr: SocketAddr,
    state: Arc<FakeGooseInner>,
    _temp_dir: Arc<TempDir>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

#[derive(Deserialize)]
struct AuthQuery {
    token: Option<String>,
}

impl FakeGoose {
    pub async fn start() -> Self {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let secret = "fake-secret-key-1234".to_string();
        let secret_path = temp_dir.path().join("secret");
        std::fs::write(&secret_path, &secret).expect("write secret file");

        let inner = Arc::new(FakeGooseInner {
            secret,
            secret_path,
            advertise_patched: AtomicBool::new(false),
            conflict_connections: Mutex::new(HashSet::new()),
            lose_next_predicates: Mutex::new(HashMap::new()),
            gap_on_next_resume: Mutex::new(HashSet::new()),
            inbound_frames: Mutex::new(Vec::new()),
            deleted_connections: Mutex::new(Vec::new()),
            connections: Mutex::new(HashMap::new()),
            next_conn_num: AtomicU64::new(1),
            next_session_num: AtomicU64::new(1),
            prompt_chunks: Mutex::new(vec!["fake chunk".to_string()]),
            export_data: Mutex::new(HashMap::new()),
        });

        let app = Router::new()
            .route(
                "/acp",
                post(handle_post).get(handle_get).delete(handle_delete),
            )
            .with_state(inner.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake goose listener");
        let addr = listener.local_addr().expect("local addr");

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        Self {
            addr,
            state: inner,
            _temp_dir: Arc::new(temp_dir),
            shutdown_tx: Some(shutdown_tx),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}/acp", self.addr)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn secret(&self) -> &str {
        &self.state.secret
    }

    pub fn secret_file(&self) -> &Path {
        &self.state.secret_path
    }

    pub fn advertise_patched(&self, patched: bool) {
        self.state
            .advertise_patched
            .store(patched, Ordering::SeqCst);
    }

    pub fn gap_on_next_resume(&self, sid: &str) {
        self.state
            .gap_on_next_resume
            .lock()
            .unwrap()
            .insert(sid.to_string());
    }

    pub fn conflict_forever(&self, conn: &str) {
        self.state
            .conflict_connections
            .lock()
            .unwrap()
            .insert(conn.to_string());
    }

    pub fn lose_next(&self, sid: &str, pred: impl Fn(&Value) -> bool + Send + Sync + 'static) {
        self.state
            .lose_next_predicates
            .lock()
            .unwrap()
            .entry(sid.to_string())
            .or_default()
            .push(Box::new(pred));
    }

    pub fn inbound(&self) -> Vec<Value> {
        self.state.inbound_frames.lock().unwrap().clone()
    }

    pub fn deletes(&self) -> Vec<String> {
        self.state.deleted_connections.lock().unwrap().clone()
    }

    pub fn script_prompt(&self, chunks: Vec<String>) {
        *self.state.prompt_chunks.lock().unwrap() = chunks;
    }

    pub fn set_export_data(&self, sid: &str, data: Value) {
        self.state
            .export_data
            .lock()
            .unwrap()
            .insert(sid.to_string(), data);
    }

    pub async fn restart(&mut self) {
        self.state.connections.lock().unwrap().clear();
    }
}

impl Drop for FakeGoose {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn check_auth(state: &FakeGooseInner, headers: &HeaderMap, query: &AuthQuery) -> bool {
    if let Some(key) = headers.get("x-secret-key").and_then(|v| v.to_str().ok())
        && key == state.secret
    {
        return true;
    }
    if let Some(token) = &query.token
        && token == &state.secret
    {
        return true;
    }
    false
}

async fn handle_post(
    State(state): State<Arc<FakeGooseInner>>,
    Query(query): Query<AuthQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !check_auth(&state, &headers, &query) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let frame: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    state.inbound_frames.lock().unwrap().push(frame.clone());

    let method = frame.get("method").and_then(|m| m.as_str());
    let req_id = frame.get("id").cloned();

    if method == Some("initialize") {
        let conn_id = format!(
            "conn-{}",
            state.next_conn_num.fetch_add(1, Ordering::SeqCst)
        );
        {
            let mut conns = state.connections.lock().unwrap();
            conns.insert(conn_id.clone(), ConnectionState::new());
        }

        let resp_json = json!({
            "jsonrpc": "2.0",
            "id": req_id.unwrap_or(json!(1)),
            "result": {
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": true
                },
                "serverInfo": {
                    "name": "fake-goose",
                    "version": "1.54.0"
                }
            }
        });

        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("acp-connection-id", conn_id)
            .body(Body::from(resp_json.to_string()))
            .unwrap();
    }

    let conn_id = match headers
        .get("acp-connection-id")
        .and_then(|v| v.to_str().ok())
    {
        Some(cid) => cid.to_string(),
        None => {
            return (StatusCode::BAD_REQUEST, "Acp-Connection-Id header required").into_response();
        }
    };

    {
        let conns = state.connections.lock().unwrap();
        if !conns.contains_key(&conn_id) {
            return StatusCode::NOT_FOUND.into_response();
        }
    }

    let session_header = headers
        .get("acp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let session_param = frame
        .pointer("/params/sessionId")
        .and_then(|v| v.as_str())
        .map(String::from);
    let target_session = session_header.or(session_param);

    let patched = state.advertise_patched.load(Ordering::SeqCst);

    match method {
        Some("session/new") => {
            let sid = format!(
                "ses_{}",
                state.next_session_num.fetch_add(1, Ordering::SeqCst)
            );
            let mut conns = state.connections.lock().unwrap();
            if let Some(conn) = conns.get_mut(&conn_id) {
                conn.sessions.insert(sid.clone(), StreamState::new());
                if let Some(id) = req_id {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "sessionId": sid
                        }
                    });
                    conn.conn_stream.push_frame(resp, patched);
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        Some("session/list") => {
            let mut conns = state.connections.lock().unwrap();
            if let Some(conn) = conns.get_mut(&conn_id) {
                let list: Vec<Value> = conn
                    .sessions
                    .keys()
                    .map(|s| json!({"sessionId": s}))
                    .collect();
                if let Some(id) = req_id {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": list
                    });
                    conn.conn_stream.push_frame(resp, patched);
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        Some("session/load") => {
            let mut conns = state.connections.lock().unwrap();
            if let Some(conn) = conns.get_mut(&conn_id)
                && let Some(id) = req_id
            {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {}
                });
                if let Some(sid) = &target_session {
                    if let Some(stream) = conn.sessions.get_mut(sid) {
                        stream.push_frame(resp, patched);
                    }
                } else {
                    conn.conn_stream.push_frame(resp, patched);
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        Some("_goose/unstable/session/export") => {
            let export_val = target_session
                .as_ref()
                .and_then(|sid| state.export_data.lock().unwrap().get(sid).cloned())
                .unwrap_or_else(|| json!({"conversation": []}));

            let mut conns = state.connections.lock().unwrap();
            if let Some(conn) = conns.get_mut(&conn_id)
                && let Some(id) = req_id
            {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "data": export_val.to_string()
                    }
                });
                if let Some(sid) = &target_session {
                    if let Some(stream) = conn.sessions.get_mut(sid) {
                        stream.push_frame(resp, patched);
                    }
                } else {
                    conn.conn_stream.push_frame(resp, patched);
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        Some("session/prompt") => {
            let Some(sid) = target_session else {
                return (StatusCode::BAD_REQUEST, "sessionId required").into_response();
            };

            let cancel_flag = Arc::new(AtomicBool::new(false));
            {
                let mut conns = state.connections.lock().unwrap();
                if let Some(conn) = conns.get_mut(&conn_id) {
                    conn.active_prompt_cancels
                        .insert(sid.clone(), cancel_flag.clone());
                }
            }

            let state_clone = state.clone();
            let conn_id_clone = conn_id.clone();
            let sid_clone = sid.clone();
            let req_id_clone = req_id.clone();

            tokio::spawn(async move {
                let chunks = state_clone.prompt_chunks.lock().unwrap().clone();
                let run_id = "run_fake_001".to_string();

                // 1. Run start
                emit_session_frame(
                    &state_clone,
                    &conn_id_clone,
                    &sid_clone,
                    json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": &sid_clone,
                            "update": {
                                "sessionUpdate": "session_info_update",
                                "_meta": {
                                    "goose": {
                                        "activeRunId": run_id
                                    }
                                }
                            }
                        }
                    }),
                );

                tokio::time::sleep(Duration::from_millis(10)).await;

                // 2. Chunks
                for chunk in chunks {
                    if cancel_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    emit_session_frame(
                        &state_clone,
                        &conn_id_clone,
                        &sid_clone,
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": &sid_clone,
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {
                                        "type": "text",
                                        "text": chunk
                                    }
                                }
                            }
                        }),
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }

                // 3. Run end
                emit_session_frame(
                    &state_clone,
                    &conn_id_clone,
                    &sid_clone,
                    json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": &sid_clone,
                            "update": {
                                "sessionUpdate": "session_info_update",
                                "_meta": {
                                    "goose": {
                                        "activeRunId": Value::Null
                                    }
                                }
                            }
                        }
                    }),
                );

                // 4. Prompt reply
                if let Some(id) = req_id_clone {
                    let stop_reason = if cancel_flag.load(Ordering::SeqCst) {
                        "cancelled"
                    } else {
                        "end_turn"
                    };
                    emit_session_frame(
                        &state_clone,
                        &conn_id_clone,
                        &sid_clone,
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "stopReason": stop_reason
                            }
                        }),
                    );
                }

                let mut conns = state_clone.connections.lock().unwrap();
                if let Some(conn) = conns.get_mut(&conn_id_clone) {
                    conn.active_prompt_cancels.remove(&sid_clone);
                }
            });

            StatusCode::ACCEPTED.into_response()
        }
        Some("session/cancel") => {
            if let Some(sid) = target_session {
                let conns = state.connections.lock().unwrap();
                if let Some(conn) = conns.get(&conn_id)
                    && let Some(cancel) = conn.active_prompt_cancels.get(&sid)
                {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        _ => StatusCode::ACCEPTED.into_response(),
    }
}

fn emit_session_frame(state: &FakeGooseInner, conn_id: &str, sid: &str, frame: Value) {
    let should_lose = {
        let mut preds = state.lose_next_predicates.lock().unwrap();
        if let Some(list) = preds.get_mut(sid) {
            if let Some(pos) = list.iter().position(|pred| pred(&frame)) {
                let _ = list.remove(pos);
                true
            } else {
                false
            }
        } else {
            false
        }
    };

    if should_lose {
        return;
    }

    let patched = state.advertise_patched.load(Ordering::SeqCst);
    let mut conns = state.connections.lock().unwrap();
    if let Some(conn) = conns.get_mut(conn_id)
        && let Some(stream) = conn.sessions.get_mut(sid)
    {
        stream.push_frame(frame, patched);
    }
}

async fn handle_get(
    State(state): State<Arc<FakeGooseInner>>,
    Query(query): Query<AuthQuery>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&state, &headers, &query) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let conn_id = match headers
        .get("acp-connection-id")
        .and_then(|v| v.to_str().ok())
    {
        Some(cid) => cid.to_string(),
        None => {
            return (StatusCode::BAD_REQUEST, "Acp-Connection-Id header required").into_response();
        }
    };

    let patched = state.advertise_patched.load(Ordering::SeqCst);

    if state
        .conflict_connections
        .lock()
        .unwrap()
        .contains(&conn_id)
    {
        let mut res = (
            StatusCode::CONFLICT,
            "outbound stream already has a subscriber",
        )
            .into_response();
        if patched {
            res.headers_mut().insert(
                "acp-stream-features",
                HeaderValue::from_static("takeover, replay"),
            );
        }
        return res;
    }

    let session_id = headers
        .get("acp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let takeover = headers
        .get("acp-takeover")
        .is_some_and(|v| v.as_bytes() == b"1");
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());

    let (rx, evict_rx) = {
        let mut conns = state.connections.lock().unwrap();
        let Some(conn) = conns.get_mut(&conn_id) else {
            return StatusCode::NOT_FOUND.into_response();
        };

        let stream = match &session_id {
            Some(sid) => conn
                .sessions
                .entry(sid.clone())
                .or_insert_with(StreamState::new),
            None => &mut conn.conn_stream,
        };

        match stream.subscribe(patched, takeover, last_event_id) {
            Ok(pair) => pair,
            Err(()) => {
                let mut res = (
                    StatusCode::CONFLICT,
                    "outbound stream already has a subscriber",
                )
                    .into_response();
                if patched {
                    res.headers_mut().insert(
                        "acp-stream-features",
                        HeaderValue::from_static("takeover, replay"),
                    );
                }
                return res;
            }
        }
    };

    let replay_header = if patched && last_event_id.is_some() {
        let is_gap = session_id
            .as_ref()
            .is_some_and(|sid| state.gap_on_next_resume.lock().unwrap().remove(sid));
        if is_gap {
            Some("gap")
        } else {
            Some("complete")
        }
    } else {
        None
    };

    let stream = futures::stream::unfold((rx, evict_rx), |(mut rx, mut evict_rx)| async move {
        tokio::select! {
            biased;
            _ = &mut evict_rx => None,
            item = rx.recv() => item.map(|val| (val, (rx, evict_rx))),
        }
    });

    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    response_headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    response_headers.insert(header::CONNECTION, "keep-alive".parse().unwrap());
    response_headers.insert(
        "acp-connection-id",
        HeaderValue::from_str(&conn_id).unwrap(),
    );

    if let Some(sid) = &session_id
        && let Ok(val) = HeaderValue::from_str(sid)
    {
        response_headers.insert("acp-session-id", val);
    }

    if patched {
        response_headers.insert(
            "acp-stream-features",
            HeaderValue::from_static("takeover, replay"),
        );
        if let Some(replay) = replay_header {
            response_headers.insert("acp-replay", HeaderValue::from_static(replay));
        }
    }

    (StatusCode::OK, response_headers, Body::from_stream(stream)).into_response()
}

async fn handle_delete(
    State(state): State<Arc<FakeGooseInner>>,
    Query(query): Query<AuthQuery>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&state, &headers, &query) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let conn_id = match headers
        .get("acp-connection-id")
        .and_then(|v| v.to_str().ok())
    {
        Some(cid) => cid.to_string(),
        None => {
            return (StatusCode::BAD_REQUEST, "Acp-Connection-Id header required").into_response();
        }
    };

    let removed = {
        let mut conns = state.connections.lock().unwrap();
        conns.remove(&conn_id).is_some()
    };

    if !removed {
        return StatusCode::NOT_FOUND.into_response();
    }

    state.deleted_connections.lock().unwrap().push(conn_id);
    StatusCode::ACCEPTED.into_response()
}
