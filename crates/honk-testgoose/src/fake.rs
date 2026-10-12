use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
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
    queue: VecDeque<(u64, Value)>,
    ring: VecDeque<(u64, Value)>,
    sender: Option<mpsc::UnboundedSender<Result<bytes::Bytes, std::convert::Infallible>>>,
    evict_tx: Option<oneshot::Sender<()>>,
}

impl StreamState {
    fn new() -> Self {
        Self {
            next_seq: 1,
            queue: VecDeque::new(),
            ring: VecDeque::new(),
            sender: None,
            evict_tx: None,
        }
    }

    fn push_frame(&mut self, frame: Value, patched: bool, is_lost: bool) {
        let seq = self.next_seq;
        self.next_seq += 1;

        if let Some(sender) = &self.sender {
            if is_lost {
                if patched {
                    self.ring.push_back((seq, frame));
                }
                return;
            }

            let sse_str = if patched {
                format!("id: {seq}\ndata: {frame}\n\n")
            } else {
                format!("data: {frame}\n\n")
            };

            if sender.send(Ok(bytes::Bytes::from(sse_str))).is_ok() {
                if patched {
                    self.ring.push_back((seq, frame));
                }
            } else {
                self.sender = None;
                self.queue.push_back((seq, frame));
            }
        } else {
            self.queue.push_back((seq, frame));
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
            while self.ring.front().is_some_and(|(seq, _)| *seq <= last_id) {
                self.ring.pop_front();
            }
            for (seq, frame) in &self.ring {
                if *seq > last_id {
                    let sse_str = format!("id: {seq}\ndata: {frame}\n\n");
                    let _ = tx.send(Ok(bytes::Bytes::from(sse_str)));
                }
            }
        }

        while let Some((seq, frame)) = self.queue.pop_front() {
            let sse_str = if patched {
                format!("id: {seq}\ndata: {frame}\n\n")
            } else {
                format!("data: {frame}\n\n")
            };
            let _ = tx.send(Ok(bytes::Bytes::from(sse_str)));
            if patched {
                self.ring.push_back((seq, frame));
            }
        }

        Ok((rx, evict_rx))
    }
}

struct ConnectionState {
    conn_stream: StreamState,
    sessions: HashMap<String, StreamState>,
}

impl ConnectionState {
    fn new() -> Self {
        Self {
            conn_stream: StreamState::new(),
            sessions: HashMap::new(),
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
    scripted_frames: Mutex<Vec<Value>>,
    export_data: Mutex<HashMap<String, Value>>,
    active_prompt_cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    active_runs: Mutex<HashMap<String, String>>,
    all_sessions: Mutex<HashMap<String, String>>,
    keepalive_interval: Mutex<Duration>,
    drop_prompt_reply: AtomicBool,
    drop_prompt_announcement: AtomicBool,
    probe_overtakes_reply: AtomicBool,
    steer_status: Mutex<Option<StatusCode>>,
    respond_status: Mutex<Option<StatusCode>>,
    prompt_status: Mutex<Option<StatusCode>>,
    prompt_error_reply: Mutex<Option<Value>>,
    export_delay: Mutex<Option<Duration>>,
    run_announcement_delay: Mutex<Option<Duration>>,
    export_commit_delay: Mutex<Option<Duration>>,
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
        let secret = "fake-secret-key-1234-0123456789abcdef".to_string();
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
            scripted_frames: Mutex::new(Vec::new()),
            export_data: Mutex::new(HashMap::new()),
            active_prompt_cancels: Mutex::new(HashMap::new()),
            active_runs: Mutex::new(HashMap::new()),
            all_sessions: Mutex::new(HashMap::new()),
            keepalive_interval: Mutex::new(Duration::from_secs(15)),
            drop_prompt_reply: AtomicBool::new(false),
            drop_prompt_announcement: AtomicBool::new(false),
            probe_overtakes_reply: AtomicBool::new(false),
            steer_status: Mutex::new(None),
            respond_status: Mutex::new(None),
            prompt_status: Mutex::new(None),
            prompt_error_reply: Mutex::new(None),
            export_delay: Mutex::new(None),
            run_announcement_delay: Mutex::new(None),
            export_commit_delay: Mutex::new(None),
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

    pub fn remove_session(&self, sid: &str) {
        self.state.all_sessions.lock().unwrap().remove(sid);
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

    pub fn script_frames(&self, frames: Vec<Value>) {
        *self.state.scripted_frames.lock().unwrap() = frames;
    }

    pub fn set_export_data(&self, sid: &str, data: Value) {
        self.state
            .export_data
            .lock()
            .unwrap()
            .insert(sid.to_string(), data);
    }

    pub fn set_export_delay(&self, delay: Option<Duration>) {
        *self.state.export_delay.lock().unwrap() = delay;
    }

    pub fn set_keepalive_interval(&self, interval: Duration) {
        *self.state.keepalive_interval.lock().unwrap() = interval;
    }

    pub fn kick_stream(&self, sid: &str) {
        let mut conns = self.state.connections.lock().unwrap();
        for conn in conns.values_mut() {
            if let Some(stream) = conn.sessions.get_mut(sid) {
                if let Some(evict) = stream.evict_tx.take() {
                    let _ = evict.send(());
                }
                stream.sender = None;
            }
            if let Some(evict) = conn.conn_stream.evict_tx.take() {
                let _ = evict.send(());
            }
            conn.conn_stream.sender = None;
        }
    }

    pub fn clear_ring(&self, sid: &str) {
        let mut conns = self.state.connections.lock().unwrap();
        for conn in conns.values_mut() {
            if let Some(stream) = conn.sessions.get_mut(sid) {
                stream.ring.clear();
            }
        }
    }

    pub fn set_drop_prompt_reply(&self, drop: bool) {
        self.state.drop_prompt_reply.store(drop, Ordering::SeqCst);
    }

    pub fn set_drop_prompt_announcement(&self, drop: bool) {
        self.state
            .drop_prompt_announcement
            .store(drop, Ordering::SeqCst);
    }

    pub fn set_probe_overtakes_reply(&self, overtakes: bool) {
        self.state
            .probe_overtakes_reply
            .store(overtakes, Ordering::SeqCst);
    }

    pub fn set_steer_status(&self, status: Option<StatusCode>) {
        *self.state.steer_status.lock().unwrap() = status;
    }

    pub fn set_respond_status(&self, status: Option<StatusCode>) {
        *self.state.respond_status.lock().unwrap() = status;
    }

    pub fn set_prompt_status(&self, status: Option<StatusCode>) {
        *self.state.prompt_status.lock().unwrap() = status;
    }

    pub fn set_prompt_error_reply(&self, err: Option<Value>) {
        *self.state.prompt_error_reply.lock().unwrap() = err;
    }

    pub fn set_run_announcement_delay(&self, delay: Option<Duration>) {
        *self.state.run_announcement_delay.lock().unwrap() = delay;
    }

    pub fn set_export_commit_delay(&self, delay: Option<Duration>) {
        *self.state.export_commit_delay.lock().unwrap() = delay;
    }

    pub fn received_prompts(&self, sid: &str) -> Vec<Value> {
        let frames = self.state.inbound_frames.lock().unwrap();
        frames
            .iter()
            .filter(|f| {
                f.get("method").and_then(Value::as_str) == Some("session/prompt")
                    && (f.pointer("/params/sessionId").and_then(Value::as_str) == Some(sid)
                        || f.get("sessionId").and_then(Value::as_str) == Some(sid))
            })
            .cloned()
            .collect()
    }

    pub fn emit_frame(&self, sid: &str, frame: Value) {
        let conn_id = {
            let conns = self.state.connections.lock().unwrap();
            conns
                .iter()
                .find(|(_, conn)| conn.sessions.contains_key(sid))
                .map(|(cid, _)| cid.clone())
        };
        if let Some(cid) = conn_id {
            emit_session_frame(&self.state, &cid, sid, frame);
        }
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

fn method_requires_session(method: &str) -> bool {
    matches!(
        method,
        "session/prompt"
            | "session/cancel"
            | "session/close"
            | "session/delete"
            | "session/fork"
            | "session/load"
            | "session/resume"
            | "session/set_config_option"
            | "session/set_mode"
            | "session/set_model"
            | "_goose/unstable/session/export"
            | "_goose/unstable/session/steer"
    )
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

    let method = frame.get("method").and_then(|m| m.as_str());
    let req_id = frame.get("id").cloned();

    if method == Some("initialize") {
        state.inbound_frames.lock().unwrap().push(frame.clone());
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

    let mut recorded_frame = frame.clone();
    if let Some(ref sid) = target_session
        && recorded_frame.get("sessionId").is_none()
        && recorded_frame.pointer("/params/sessionId").is_none()
    {
        recorded_frame["sessionId"] = json!(sid);
    }
    state.inbound_frames.lock().unwrap().push(recorded_frame);

    if method.is_some_and(method_requires_session) && target_session.is_none() {
        return (StatusCode::BAD_REQUEST, "Acp-Session-Id header required").into_response();
    }

    if let Some(ref sid) = target_session {
        let mut conns = state.connections.lock().unwrap();
        if let Some(conn) = conns.get_mut(&conn_id) {
            conn.sessions
                .entry(sid.clone())
                .or_insert_with(StreamState::new);
        }
        state
            .all_sessions
            .lock()
            .unwrap()
            .entry(sid.clone())
            .or_insert_with(|| "/tmp".to_string());
    }

    let patched = state.advertise_patched.load(Ordering::SeqCst);

    match method {
        Some("session/new") => {
            let sid = format!(
                "ses_{}",
                state.next_session_num.fetch_add(1, Ordering::SeqCst)
            );
            let cwd = frame
                .pointer("/params/cwd")
                .and_then(|v| v.as_str())
                .unwrap_or("/tmp")
                .to_string();

            state.all_sessions.lock().unwrap().insert(sid.clone(), cwd);

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
                    conn.conn_stream.push_frame(resp, patched, false);
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        Some("session/list") => {
            let sessions_guard = state.all_sessions.lock().unwrap();
            let sessions_list: Vec<Value> = sessions_guard
                .iter()
                .map(|(s, cwd)| {
                    json!({
                        "sessionId": s,
                        "cwd": cwd,
                        "title": Value::Null,
                        "updatedAt": Value::Null
                    })
                })
                .collect();

            let mut conns = state.connections.lock().unwrap();
            if let Some(conn) = conns.get_mut(&conn_id)
                && let Some(id) = req_id
            {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "sessions": sessions_list
                    }
                });
                conn.conn_stream.push_frame(resp, patched, false);
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
                    let stream = conn
                        .sessions
                        .entry(sid.clone())
                        .or_insert_with(StreamState::new);
                    stream.push_frame(resp, patched, false);
                } else {
                    conn.conn_stream.push_frame(resp, patched, false);
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        Some("_goose/unstable/session/export") => {
            let delay_opt = *state.export_delay.lock().unwrap();
            if let Some(d) = delay_opt {
                tokio::time::sleep(d).await;
            }

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
                    let stream = conn
                        .sessions
                        .entry(sid.clone())
                        .or_insert_with(StreamState::new);
                    stream.push_frame(resp, patched, false);
                } else {
                    conn.conn_stream.push_frame(resp, patched, false);
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        Some("_goose/unstable/session/steer") => {
            if let Some(status) = *state.steer_status.lock().unwrap() {
                return status.into_response();
            }
            let Some(sid) = target_session
                .as_deref()
                .or_else(|| frame.pointer("/params/sessionId").and_then(Value::as_str))
            else {
                return (StatusCode::BAD_REQUEST, "sessionId required").into_response();
            };
            let expected_run_id = frame
                .pointer("/params/expectedRunId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let active_run = state.active_runs.lock().unwrap().get(sid).cloned();

            let force_idle = state.probe_overtakes_reply.load(Ordering::SeqCst);
            let resp = match active_run {
                _ if force_idle => json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32602,
                        "message": "Invalid params",
                        "data": "no active run to steer"
                    }
                }),
                None => json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32602,
                        "message": "Invalid params",
                        "data": "no active run to steer"
                    }
                }),
                Some(actual_run_id) if actual_run_id != expected_run_id => json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32602,
                        "message": "Invalid params",
                        "data": {
                            "message": format!(
                                "expected active run id `{expected_run_id}` but found `{actual_run_id}`"
                            ),
                            "expectedRunId": expected_run_id,
                            "actualRunId": actual_run_id
                        }
                    }
                }),
                Some(actual_run_id) => {
                    let msg_id = format!(
                        "steer_{}",
                        state.next_session_num.fetch_add(1, Ordering::SeqCst)
                    );
                    json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "runId": actual_run_id,
                            "messageId": msg_id
                        }
                    })
                }
            };

            let mut conns = state.connections.lock().unwrap();
            if let Some(conn) = conns.get_mut(&conn_id) {
                let stream = conn
                    .sessions
                    .entry(sid.to_string())
                    .or_insert_with(StreamState::new);
                stream.push_frame(resp, patched, false);
            }
            StatusCode::ACCEPTED.into_response()
        }
        Some("session/prompt") => {
            if let Some(status) = *state.prompt_status.lock().unwrap() {
                return status.into_response();
            }

            let Some(sid) = target_session else {
                return (StatusCode::BAD_REQUEST, "Acp-Session-Id header required").into_response();
            };

            if let Some(err_val) = state.prompt_error_reply.lock().unwrap().clone() {
                let state_clone = state.clone();
                let conn_id_clone = conn_id.clone();
                let sid_clone = sid.clone();
                let req_id_clone = req_id.clone();
                tokio::spawn(async move {
                    if let Some(id) = req_id_clone {
                        emit_session_frame(
                            &state_clone,
                            &conn_id_clone,
                            &sid_clone,
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": err_val
                            }),
                        );
                    }
                });
                return StatusCode::ACCEPTED.into_response();
            }

            if state.drop_prompt_announcement.load(Ordering::SeqCst) {
                return StatusCode::ACCEPTED.into_response();
            }

            let cancel_flag = Arc::new(AtomicBool::new(false));
            state
                .active_prompt_cancels
                .lock()
                .unwrap()
                .insert(sid.clone(), cancel_flag.clone());

            let user_text = frame
                .pointer("/params/prompt")
                .and_then(Value::as_array)
                .and_then(|arr| {
                    arr.iter().find_map(|b| {
                        if b.get("type").and_then(Value::as_str) == Some("text") {
                            b.get("text").and_then(Value::as_str)
                        } else {
                            None
                        }
                    })
                })
                .map(String::from);

            let commit_delay = *state.export_commit_delay.lock().unwrap();
            if let Some(delay) = commit_delay {
                let state_c = state.clone();
                let sid_c = sid.clone();
                let text_c = user_text.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    if let Some(text) = text_c {
                        let mut data = state_c.export_data.lock().unwrap();
                        let entry = data
                            .entry(sid_c)
                            .or_insert_with(|| json!({ "conversation": [] }));
                        if let Some(conv) =
                            entry.get_mut("conversation").and_then(Value::as_array_mut)
                        {
                            conv.push(json!({
                                "role": "user",
                                "content": [{
                                    "type": "text",
                                    "text": text
                                }]
                            }));
                        }
                    }
                });
            }

            let state_clone = state.clone();
            let conn_id_clone = conn_id.clone();
            let sid_clone = sid.clone();
            let req_id_clone = req_id.clone();

            tokio::spawn(async move {
                let run_delay = *state_clone.run_announcement_delay.lock().unwrap();
                if let Some(delay) = run_delay {
                    tokio::time::sleep(delay).await;
                }

                let custom_frames = state_clone.scripted_frames.lock().unwrap().clone();
                let run_id = "run_fake_001".to_string();

                if !custom_frames.is_empty() {
                    state_clone
                        .active_runs
                        .lock()
                        .unwrap()
                        .insert(sid_clone.clone(), run_id.clone());
                    for f in custom_frames {
                        if cancel_flag.load(Ordering::SeqCst) {
                            break;
                        }
                        emit_session_frame(&state_clone, &conn_id_clone, &sid_clone, f);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    state_clone.active_runs.lock().unwrap().remove(&sid_clone);
                    if state_clone.probe_overtakes_reply.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    if state_clone.drop_prompt_reply.load(Ordering::SeqCst) {
                        return;
                    }
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
                    return;
                }

                let chunks = state_clone.prompt_chunks.lock().unwrap().clone();
                let message_id = format!("msg_fake_{run_id}");

                // 1. Run start
                state_clone
                    .active_runs
                    .lock()
                    .unwrap()
                    .insert(sid_clone.clone(), run_id.clone());
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

                // 2. Chunks carrying messageId and _meta.goose.messageId
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
                                    "messageId": &message_id,
                                    "content": {
                                        "type": "text",
                                        "text": chunk
                                    },
                                    "_meta": {
                                        "goose": {
                                            "messageId": &message_id
                                        }
                                    }
                                }
                            }
                        }),
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }

                // 3. Run end
                state_clone.active_runs.lock().unwrap().remove(&sid_clone);
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
                if state_clone.probe_overtakes_reply.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                if state_clone.drop_prompt_reply.load(Ordering::SeqCst) {
                    return;
                }
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

                state_clone
                    .active_prompt_cancels
                    .lock()
                    .unwrap()
                    .remove(&sid_clone);
            });

            StatusCode::ACCEPTED.into_response()
        }
        Some("session/cancel") => {
            if let Some(sid) = target_session
                && let Some(cancel) = state.active_prompt_cancels.lock().unwrap().get(&sid)
            {
                cancel.store(true, Ordering::SeqCst);
            }
            StatusCode::ACCEPTED.into_response()
        }
        None => {
            if let Some(status) = *state.respond_status.lock().unwrap() {
                return status.into_response();
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

    let patched = state.advertise_patched.load(Ordering::SeqCst);
    let mut conns = state.connections.lock().unwrap();
    if let Some(conn) = conns.get_mut(conn_id) {
        let stream = conn
            .sessions
            .entry(sid.to_string())
            .or_insert_with(StreamState::new);
        stream.push_frame(frame, patched, should_lose);
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

    let interval_dur = *state.keepalive_interval.lock().unwrap();
    let mut ticker = tokio::time::interval(interval_dur);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.reset();

    let stream = futures::stream::unfold(
        (rx, evict_rx, ticker),
        |(mut rx, mut evict_rx, mut ticker)| async move {
            tokio::select! {
                biased;
                _ = &mut evict_rx => None,
                item = rx.recv() => item.map(|val| (val, (rx, evict_rx, ticker))),
                _ = ticker.tick() => {
                    Some((Ok(bytes::Bytes::from("data:\n\n")), (rx, evict_rx, ticker)))
                }
            }
        },
    );

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
