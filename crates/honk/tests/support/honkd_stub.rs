use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub struct SeenHonk {
    pub path: String,
    pub secret: Option<String>,
    pub body: Value,
}

#[derive(Clone)]
pub struct HonkdBehaviour {
    pub self_grant: StatusCode,
    pub list: (StatusCode, Value),
    pub enable: (StatusCode, Value),
    pub disable: (StatusCode, Value),
}

impl Default for HonkdBehaviour {
    fn default() -> Self {
        Self {
            self_grant: StatusCode::NO_CONTENT,
            list: (
                StatusCode::OK,
                json!({
                    "servers": [
                        {
                            "name": "slack",
                            "enabled": false,
                            "supported": true,
                            "confirm": true,
                            "sibling": "slack-ro"
                        },
                        {
                            "name": "slack-ro",
                            "enabled": true,
                            "supported": true,
                            "confirm": false,
                            "sibling": "slack"
                        }
                    ],
                    "other": [
                        { "key": "developer" }
                    ]
                }),
            ),
            enable: (
                StatusCode::OK,
                json!({
                    "summary": "enabled slack, disabled slack-ro",
                    "results": [
                        {
                            "name": "slack",
                            "ok": true,
                            "replaced": "slack-ro",
                            "changed": true
                        }
                    ],
                    "servers": [],
                    "other": [],
                    "enabled": ["slack"]
                }),
            ),
            disable: (
                StatusCode::OK,
                json!({
                    "summary": "disabled slack",
                    "results": [
                        {
                            "name": "slack",
                            "ok": true,
                            "changed": true
                        }
                    ],
                    "servers": [],
                    "other": [],
                    "enabled": []
                }),
            ),
        }
    }
}

#[derive(Clone)]
struct HonkSt {
    seen: Arc<Mutex<Vec<SeenHonk>>>,
    behaviour: HonkdBehaviour,
}

pub struct StubHonkd {
    pub url: String,
    pub seen: Arc<Mutex<Vec<SeenHonk>>>,
}

impl StubHonkd {
    pub async fn start(behaviour: HonkdBehaviour) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let st = HonkSt {
            seen: seen.clone(),
            behaviour,
        };
        let app = Router::new()
            .route("/mcp/self-grant", get(mcp_self_grant))
            .route("/mcp/list", post(mcp_list))
            .route("/mcp/enable", post(mcp_enable))
            .route("/mcp/disable", post(mcp_disable))
            .with_state(st);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url: format!("http://{addr}/acp"),
            seen,
        }
    }
}

async fn mcp_list(
    State(st): State<HonkSt>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<Value>,
) -> impl IntoResponse {
    let secret = headers
        .get("x-secret-key")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    st.seen.lock().unwrap().push(SeenHonk {
        path: "/mcp/list".to_string(),
        secret,
        body,
    });
    let (status, val) = st.behaviour.list.clone();
    (status, axum::Json(val))
}

async fn mcp_enable(
    State(st): State<HonkSt>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<Value>,
) -> impl IntoResponse {
    let secret = headers
        .get("x-secret-key")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    st.seen.lock().unwrap().push(SeenHonk {
        path: "/mcp/enable".to_string(),
        secret,
        body,
    });
    let (status, val) = st.behaviour.enable.clone();
    (status, axum::Json(val))
}

async fn mcp_self_grant(State(st): State<HonkSt>, headers: HeaderMap) -> impl IntoResponse {
    let secret = headers
        .get("x-secret-key")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    st.seen.lock().unwrap().push(SeenHonk {
        path: "/mcp/self-grant".to_string(),
        secret,
        body: Value::Null,
    });
    st.behaviour.self_grant
}

async fn mcp_disable(
    State(st): State<HonkSt>,
    headers: HeaderMap,
    axum::extract::Json(body): axum::extract::Json<Value>,
) -> impl IntoResponse {
    let secret = headers
        .get("x-secret-key")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    st.seen.lock().unwrap().push(SeenHonk {
        path: "/mcp/disable".to_string(),
        secret,
        body,
    });
    let (status, val) = st.behaviour.disable.clone();
    (status, axum::Json(val))
}
