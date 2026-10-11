use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};

#[derive(Clone, Debug)]
pub enum Turn {
    /// assistant text, streamed one chunk per SSE event
    Text(Vec<String>),
    /// one tool call (OpenAI `tool_calls` delta). `tool` is matched against the request's
    /// `tools[].function.name` (exact, else suffix match, e.g. "shell"); args {"command":"echo hi"}
    ToolCall {
        tool: String,
        arguments: serde_json::Value,
    },
    /// wait until `StubModel::open(name)` before answering the NEXT turn
    Gate(String, Box<Turn>),
}

#[derive(Clone)]
struct AppState {
    turns: Arc<Mutex<VecDeque<Turn>>>,
    gates: Arc<Mutex<HashSet<String>>>,
    gate_notify: Arc<Notify>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    turn_requests: Arc<Mutex<Vec<serde_json::Value>>>,
    classification_log: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    seen_tool_names: Arc<Mutex<HashSet<String>>>,
}

pub struct StubModel {
    pub(crate) addr: SocketAddr,
    pub(crate) turns: Arc<Mutex<VecDeque<Turn>>>,
    pub(crate) gates: Arc<Mutex<HashSet<String>>>,
    pub(crate) gate_notify: Arc<Notify>,
    pub(crate) requests: Arc<Mutex<Vec<serde_json::Value>>>,
    pub(crate) turn_requests: Arc<Mutex<Vec<serde_json::Value>>>,
    pub(crate) classification_log: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    pub(crate) seen_tool_names: Arc<Mutex<HashSet<String>>>,
    pub(crate) shutdown_tx: Option<oneshot::Sender<()>>,
}

impl StubModel {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub model listener");
        let addr = listener.local_addr().expect("local addr");

        let turns = Arc::new(Mutex::new(VecDeque::new()));
        let gates = Arc::new(Mutex::new(HashSet::new()));
        let gate_notify = Arc::new(Notify::new());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let turn_requests = Arc::new(Mutex::new(Vec::new()));
        let classification_log = Arc::new(Mutex::new(Vec::new()));
        let seen_tool_names = Arc::new(Mutex::new(HashSet::new()));

        let state = AppState {
            turns: turns.clone(),
            gates: gates.clone(),
            gate_notify: gate_notify.clone(),
            requests: requests.clone(),
            turn_requests: turn_requests.clone(),
            classification_log: classification_log.clone(),
            seen_tool_names: seen_tool_names.clone(),
        };

        let app = Router::new()
            .route("/v1/models", get(get_models))
            .route("/v1/chat/completions", post(chat_completions))
            .with_state(state);

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
            turns,
            gates,
            gate_notify,
            requests,
            turn_requests,
            classification_log,
            seen_tool_names,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn push(&self, turns: impl IntoIterator<Item = Turn>) {
        let mut q = self.turns.lock().unwrap();
        for t in turns {
            q.push_back(t);
        }
    }

    pub fn open(&self, gate: &str) {
        self.gates.lock().unwrap().insert(gate.to_string());
        self.gate_notify.notify_waiters();
    }

    pub fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }

    pub fn turn_requests(&self) -> Vec<serde_json::Value> {
        self.turn_requests.lock().unwrap().clone()
    }

    pub fn classification_log(&self) -> Vec<(String, serde_json::Value)> {
        self.classification_log.lock().unwrap().clone()
    }

    pub fn seen_tool_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .seen_tool_names
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        names.sort();
        names
    }
}

impl Drop for StubModel {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn get_models() -> impl IntoResponse {
    Json(serde_json::json!({
        "data": [
            { "id": "stub-model" }
        ]
    }))
}

async fn chat_completions(
    State(state): State<AppState>,
    Json(req): Json<serde_json::Value>,
) -> Response {
    // 1. Record raw request
    state.requests.lock().unwrap().push(req.clone());

    // 2. Extract tools and record seen tool names
    let available_tools: Vec<String> = req
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    t.pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();

    if !available_tools.is_empty() {
        let mut seen = state.seen_tool_names.lock().unwrap();
        for name in &available_tools {
            seen.insert(name.clone());
        }
    }

    // 3. Classify request:
    // "A request is a turn request when it carries the session's tools array and its
    // last message is a user or tool message. Turn requests pop the next scripted Turn.
    // Any other request is auxiliary and gets Text(["ok"])."
    let has_tools = req
        .get("tools")
        .and_then(|t| t.as_array())
        .is_some_and(|a| !a.is_empty());

    let last_role = req
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|a| a.last())
        .and_then(|msg| msg.get("role"))
        .and_then(|r| r.as_str());

    let is_user_or_tool = matches!(last_role, Some("user") | Some("tool"));
    let is_turn_request = has_tools && is_user_or_tool;

    let turn = if is_turn_request {
        state.turn_requests.lock().unwrap().push(req.clone());
        state
            .classification_log
            .lock()
            .unwrap()
            .push(("turn".to_string(), req.clone()));

        // "A turn request with the queue empty also answers Text(["ok"]), so nothing hangs."
        state
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Turn::Text(vec!["ok".to_string()]))
    } else {
        state
            .classification_log
            .lock()
            .unwrap()
            .push(("auxiliary".to_string(), req.clone()));

        Turn::Text(vec!["ok".to_string()])
    };

    // 4. Resolve gates: race-free wait armed before checking gate set, bounded by 60s timeout
    let mut resolved_turn = turn;
    let gate_deadline = std::time::Instant::now() + Duration::from_secs(60);
    while let Turn::Gate(gate_name, inner) = resolved_turn {
        loop {
            let notified = state.gate_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if state.gates.lock().unwrap().contains(&gate_name) {
                break;
            }

            let now = std::time::Instant::now();
            if now >= gate_deadline {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(serde_json::json!({
                        "error": {
                            "message": format!("Timed out waiting for gate '{gate_name}'")
                        }
                    })),
                )
                    .into_response();
            }

            let remaining = gate_deadline - now;
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(serde_json::json!({
                        "error": {
                            "message": format!("Timed out waiting for gate '{gate_name}'")
                        }
                    })),
                )
                    .into_response();
            }
        }
        resolved_turn = *inner;
    }

    // 5. Match tool name if ToolCall
    let resolved_turn = match resolved_turn {
        Turn::ToolCall { tool, arguments } => {
            let matched_name = available_tools
                .iter()
                .find(|name| *name == &tool)
                .cloned()
                .or_else(|| {
                    available_tools
                        .iter()
                        .find(|name| name.ends_with(&format!("__{tool}")) || name.ends_with(&tool))
                        .cloned()
                })
                .unwrap_or(tool);
            Turn::ToolCall {
                tool: matched_name,
                arguments,
            }
        }
        other => other,
    };

    let stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    if stream {
        build_sse_response(resolved_turn).into_response()
    } else {
        build_json_response(resolved_turn).into_response()
    }
}

fn build_json_response(turn: Turn) -> (StatusCode, Json<serde_json::Value>) {
    match turn {
        Turn::Text(chunks) => {
            let full_text = chunks.concat();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "chatcmpl-stub",
                    "object": "chat.completion",
                    "created": 1770000000,
                    "model": "stub-model",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": full_text
                            },
                            "finish_reason": "stop"
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 10,
                        "total_tokens": 20
                    }
                })),
            )
        }
        Turn::ToolCall { tool, arguments } => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": "chatcmpl-stub",
                "object": "chat.completion",
                "created": 1770000000,
                "model": "stub-model",
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": serde_json::Value::Null,
                            "tool_calls": [
                                {
                                    "id": "call_stub_001",
                                    "type": "function",
                                    "function": {
                                        "name": tool,
                                        "arguments": arguments.to_string()
                                    }
                                }
                            ]
                        },
                        "finish_reason": "tool_calls"
                    }
                ],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 10,
                    "total_tokens": 20
                }
            })),
        ),
        Turn::Gate(_, _) => unreachable!("gates must be resolved"),
    }
}

fn build_sse_response(turn: Turn) -> Response {
    let mut frames: Vec<String> = Vec::new();

    match turn {
        Turn::Text(chunks) => {
            frames.push(format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-stub",
                    "object": "chat.completion.chunk",
                    "created": 1770000000,
                    "model": "stub-model",
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "content": ""
                            },
                            "finish_reason": serde_json::Value::Null
                        }
                    ]
                })
            ));

            for chunk in chunks {
                frames.push(format!(
                    "data: {}\n\n",
                    serde_json::json!({
                        "id": "chatcmpl-stub",
                        "object": "chat.completion.chunk",
                        "created": 1770000000,
                        "model": "stub-model",
                        "choices": [
                            {
                                "index": 0,
                                "delta": {
                                    "content": chunk
                                },
                                "finish_reason": serde_json::Value::Null
                            }
                        ]
                    })
                ));
            }

            frames.push(format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-stub",
                    "object": "chat.completion.chunk",
                    "created": 1770000000,
                    "model": "stub-model",
                    "choices": [
                        {
                            "index": 0,
                            "delta": {},
                            "finish_reason": "stop"
                        }
                    ]
                })
            ));

            frames.push(format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-stub",
                    "object": "chat.completion.chunk",
                    "created": 1770000000,
                    "model": "stub-model",
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 10,
                        "total_tokens": 20
                    }
                })
            ));

            frames.push("data: [DONE]\n\n".to_string());
        }
        Turn::ToolCall { tool, arguments } => {
            frames.push(format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-stub",
                    "object": "chat.completion.chunk",
                    "created": 1770000000,
                    "model": "stub-model",
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "content": serde_json::Value::Null,
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call_stub_001",
                                        "type": "function",
                                        "function": {
                                            "name": tool,
                                            "arguments": arguments.to_string()
                                        }
                                    }
                                ]
                            },
                            "finish_reason": "tool_calls"
                        }
                    ]
                })
            ));

            frames.push(format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-stub",
                    "object": "chat.completion.chunk",
                    "created": 1770000000,
                    "model": "stub-model",
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 10,
                        "total_tokens": 20
                    }
                })
            ));

            frames.push("data: [DONE]\n\n".to_string());
        }
        Turn::Gate(_, _) => unreachable!("gates must be resolved"),
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::convert::Infallible>>(16);
    tokio::spawn(async move {
        for frame in frames {
            let _ = tx.send(Ok(bytes::Bytes::from(frame))).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    headers.insert(header::CONNECTION, "keep-alive".parse().unwrap());

    (
        StatusCode::OK,
        headers,
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}
