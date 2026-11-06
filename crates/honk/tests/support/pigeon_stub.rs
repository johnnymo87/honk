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
    pub query: Option<String>,
    pub auth: Option<String>,
    pub content_type: Option<String>,
    pub origin: Option<String>,
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
    pub swarm_inbox: (StatusCode, Value),
    pub pull_drain: (StatusCode, Value),
    pub pull_ack: (StatusCode, Value),
    pub pull_pending: (StatusCode, Value),
    pub swarm_launch: (StatusCode, Value),
    pub swarm_launch_responses: Arc<Mutex<Vec<(StatusCode, Value)>>>,
    pub swarm_schedule: (StatusCode, Value),
    pub swarm_schedule_responses: Arc<Mutex<Vec<(StatusCode, Value)>>>,
    pub swarm_scheduled: (StatusCode, Value),
    pub swarm_scheduled_cancel: (StatusCode, Value),
    pub swarm_remote_sessions: (StatusCode, Value),
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
            swarm_inbox: (StatusCode::OK, json!({"messages": [], "has_more": false})),
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
            swarm_launch: (
                StatusCode::OK,
                json!({"status": "accepted", "launch_id": "rl_stub_1", "to_machine": "cloudbox"}),
            ),
            swarm_launch_responses: Arc::new(Mutex::new(Vec::new())),
            swarm_schedule: (
                StatusCode::ACCEPTED,
                json!({"accepted": true, "msg_id": "msg_stub_sched_1", "deliver_at": 1730736000000_i64, "expires_at": 1730757600000_i64}),
            ),
            swarm_schedule_responses: Arc::new(Mutex::new(Vec::new())),
            swarm_scheduled: (StatusCode::OK, json!({"scheduled": []})),
            swarm_scheduled_cancel: (
                StatusCode::OK,
                json!({"cancelled": true, "msg_id": "msg_stub_1"}),
            ),
            swarm_remote_sessions: (
                StatusCode::OK,
                json!({
                    "machine_id": "devbox",
                    "sessions": [],
                    "machines": [],
                    "truncated": false,
                    "relay_send": true
                }),
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
    pub behaviour: Behaviour,
}

impl StubPigeon {
    pub async fn start(behaviour: Behaviour) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let st = St {
            seen: seen.clone(),
            behaviour: behaviour.clone(),
        };
        let app = Router::new()
            .route("/goose/sessions", post(register))
            .route("/stop", post(stop))
            .route("/sessions", axum::routing::get(sessions))
            .route("/goose/swarm-mailbox", post(swarm_mailbox))
            .route("/swarm/send", post(swarm_send))
            .route("/swarm/inbox", axum::routing::get(swarm_inbox))
            .route("/swarm/launch", post(swarm_launch))
            .route("/swarm/schedule", post(swarm_schedule))
            .route("/swarm/scheduled", axum::routing::get(swarm_scheduled))
            .route("/swarm/scheduled/{id}/cancel", post(swarm_scheduled_cancel))
            .route(
                "/swarm/remote-sessions",
                axum::routing::get(swarm_remote_sessions),
            )
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
            behaviour,
        }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn launch_responses(&self) -> std::sync::MutexGuard<'_, Vec<(StatusCode, Value)>> {
        self.behaviour.swarm_launch_responses.lock().unwrap()
    }

    pub fn schedule_responses(&self) -> std::sync::MutexGuard<'_, Vec<(StatusCode, Value)>> {
        self.behaviour.swarm_schedule_responses.lock().unwrap()
    }
}

fn record(st: &St, path: &str, headers: &HeaderMap, body: Value) {
    record_with_query(st, path, None, headers, body);
}

fn record_with_query(st: &St, path: &str, query: Option<String>, headers: &HeaderMap, body: Value) {
    st.seen.lock().unwrap().push(Seen {
        path: path.into(),
        query,
        auth: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        content_type: headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        origin: headers
            .get("origin")
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

async fn swarm_inbox(
    State(st): State<St>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    record_with_query(
        &st,
        "/swarm/inbox",
        uri.query().map(str::to_string),
        &headers,
        serde_json::to_value(params).unwrap_or(Value::Null),
    );
    let (s, v) = st.behaviour.swarm_inbox.clone();
    if v.is_null() {
        (s, axum::body::Body::empty()).into_response()
    } else {
        (s, axum::Json(v)).into_response()
    }
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

async fn swarm_launch(
    State(st): State<St>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    record(&st, "/swarm/launch", &headers, body);
    let resp = {
        let mut list = st.behaviour.swarm_launch_responses.lock().unwrap();
        if !list.is_empty() {
            list.remove(0)
        } else {
            st.behaviour.swarm_launch.clone()
        }
    };
    (resp.0, axum::Json(resp.1))
}

async fn swarm_schedule(
    State(st): State<St>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    record(&st, "/swarm/schedule", &headers, body.clone());
    let mut resp = {
        let mut list = st.behaviour.swarm_schedule_responses.lock().unwrap();
        if !list.is_empty() {
            list.remove(0)
        } else {
            st.behaviour.swarm_schedule.clone()
        }
    };
    if resp.1.get("msg_id") == Some(&json!("$REQ_MSG_ID"))
        && let Some(req_id) = body.get("msg_id")
    {
        resp.1["msg_id"] = req_id.clone();
    }
    (resp.0, axum::Json(resp.1))
}

async fn swarm_scheduled(
    State(st): State<St>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    record(
        &st,
        "/swarm/scheduled",
        &headers,
        serde_json::to_value(params).unwrap_or(Value::Null),
    );
    let (s, v) = st.behaviour.swarm_scheduled.clone();
    (s, axum::Json(v))
}

async fn swarm_scheduled_cancel(
    State(st): State<St>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    record(&st, uri.path(), &headers, body);
    let (s, v) = st.behaviour.swarm_scheduled_cancel.clone();
    (s, axum::Json(v))
}

async fn swarm_remote_sessions(
    State(st): State<St>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    record(
        &st,
        "/swarm/remote-sessions",
        &headers,
        serde_json::to_value(params).unwrap_or(Value::Null),
    );
    let (s, v) = st.behaviour.swarm_remote_sessions.clone();
    (s, axum::Json(v))
}
