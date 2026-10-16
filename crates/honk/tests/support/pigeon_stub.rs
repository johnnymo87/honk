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
