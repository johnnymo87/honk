//! A minimal stand-in for the pigeon daemon's `/goose/sessions` and `/stop`
//! routes, shared by the pigeon tests.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub struct Seen {
    pub path: String,
    pub auth: Option<String>,
    pub body: Value,
}

#[derive(Clone)]
pub struct Behaviour {
    pub register: (StatusCode, Value),
    pub stop: (StatusCode, Value),
    /// Body for `GET /sessions`.
    pub sessions: Value,
    pub swarm_mailbox: (StatusCode, Value),
    pub swarm_send: (StatusCode, Value),
    pub pull_drain: (StatusCode, Value),
    pub pull_ack: (StatusCode, Value),
    pub pull_pending: (StatusCode, Value),
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            register: (
                StatusCode::OK,
                json!({"ok": true, "session_id": "gse_test", "backend": "goose-acp"}),
            ),
            stop: (
                StatusCode::ACCEPTED,
                json!({"ok": true, "deliveryState": "queued", "notificationId": "s:gse_test:1"}),
            ),
            sessions: json!({"ok": true, "sessions": []}),
            swarm_mailbox: (
                StatusCode::OK,
                json!({"ok": true, "session_id": "gse_stub", "backend_session_id": "20261020_1", "created": false, "notify": false}),
            ),
            swarm_send: (
                StatusCode::ACCEPTED,
                json!({"accepted": true, "msg_id": "msg_stub_1"}),
            ),
            pull_drain: (
                StatusCode::OK,
                json!({"ok": true, "session_id": "gse_stub", "pending_total": 0, "messages": []}),
            ),
            pull_ack: (
                StatusCode::OK,
                json!({"ok": true, "acked": [], "rejected": []}),
            ),
            pull_pending: (
                StatusCode::OK,
                json!({"ok": true, "session_id": "gse_stub", "session_known": true, "backend_kind": "goose-acp", "pending": 0, "by_source": {"swarm": 0}}),
            ),
        }
    }
}

#[derive(Clone)]
struct St {
    seen: Arc<Mutex<Vec<Seen>>>,
    behaviour: Behaviour,
}

pub struct StubPigeon {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl StubPigeon {
    pub async fn start(behaviour: Behaviour) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let st = St {
            seen: seen.clone(),
            behaviour,
        };
        let app = Router::new()
            .route("/goose/sessions", post(register))
            .route("/stop", post(stop))
            .route("/sessions", axum::routing::get(sessions))
            .route("/goose/swarm-mailbox", post(swarm_mailbox))
            .route("/swarm/send", post(swarm_send))
            .route("/pull/drain", post(pull_drain))
            .route("/pull/ack", post(pull_ack))
            .route("/pull/pending", axum::routing::get(pull_pending))
            .with_state(st);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url: format!("http://{addr}"),
            seen,
        }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn record(st: &St, path: &str, headers: &HeaderMap, body: Value) {
    st.seen.lock().unwrap().push(Seen {
        path: path.into(),
        auth: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body,
    });
}

async fn register(
    State(st): State<St>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    record(&st, "/goose/sessions", &headers, body);
    let (s, v) = st.behaviour.register.clone();
    (s, axum::Json(v))
}

async fn stop(
    State(st): State<St>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    record(&st, "/stop", &headers, body);
    let (s, v) = st.behaviour.stop.clone();
    (s, axum::Json(v))
}

async fn sessions(State(st): State<St>, headers: HeaderMap) -> impl IntoResponse {
    record(&st, "/sessions", &headers, Value::Null);
    axum::Json(st.behaviour.sessions.clone())
}

async fn swarm_mailbox(
    State(st): State<St>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    record(&st, "/goose/swarm-mailbox", &headers, body);
    let (s, v) = st.behaviour.swarm_mailbox.clone();
    (s, axum::Json(v))
}

async fn swarm_send(
    State(st): State<St>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    record(&st, "/swarm/send", &headers, body);
    let (s, v) = st.behaviour.swarm_send.clone();
    (s, axum::Json(v))
}

async fn pull_drain(
    State(st): State<St>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    record(&st, "/pull/drain", &headers, body);
    let (s, v) = st.behaviour.pull_drain.clone();
    (s, axum::Json(v))
}

async fn pull_ack(
    State(st): State<St>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    record(&st, "/pull/ack", &headers, body);
    let (s, v) = st.behaviour.pull_ack.clone();
    (s, axum::Json(v))
}

async fn pull_pending(
    State(st): State<St>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    record(
        &st,
        "/pull/pending",
        &headers,
        serde_json::to_value(params).unwrap_or(Value::Null),
    );
    let (s, v) = st.behaviour.pull_pending.clone();
    (s, axum::Json(v))
}
