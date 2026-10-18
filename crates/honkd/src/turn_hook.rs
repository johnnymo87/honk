use crate::turn::TurnMeta;
use std::time::Duration;

pub const MAX_TEXT_BYTES: usize = 256 * 1024;

/// Caps a UTF-8 string to at most `max` bytes without splitting a UTF-8 character.
/// Returns the (possibly capped) string and a boolean indicating whether it was truncated.
pub fn cap_utf8(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        (s.to_string(), false)
    } else {
        let boundary = s.floor_char_boundary(max);
        (s[..boundary].to_string(), true)
    }
}

#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct TurnEvent {
    pub v: u8,
    pub r#type: &'static str,
    pub event_id: String,
    pub session_id: String,
    pub upstream_id: String,
    pub run_id: Option<String>,
    pub client_req_ids: Vec<String>,
    pub prompt_text: String,
    pub at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub honk_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_text_truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl TurnEvent {
    pub fn started(sid: impl Into<String>, meta: &TurnMeta, at_ms: u64) -> Self {
        let session_id = sid.into();
        let event_id = format!("{}:{}:started", session_id, meta.upstream_id);
        let (prompt_text, _) = cap_utf8(&meta.prompt_text, MAX_TEXT_BYTES);
        Self {
            v: 1,
            r#type: "turn.started",
            event_id,
            session_id,
            upstream_id: meta.upstream_id.clone(),
            run_id: meta.run_id.clone(),
            client_req_ids: meta.client_req_ids.clone(),
            prompt_text,
            at_ms,
            stop_reason: None,
            honk_reason: None,
            final_text: None,
            final_text_truncated: None,
            error: None,
        }
    }

    pub fn ended(
        sid: impl Into<String>,
        meta: &TurnMeta,
        stop_reason: Option<String>,
        final_text: Option<String>,
        error: Option<String>,
        at_ms: u64,
    ) -> Self {
        let session_id = sid.into();
        let event_id = format!("{}:{}:ended", session_id, meta.upstream_id);
        let (prompt_text, _) = cap_utf8(&meta.prompt_text, MAX_TEXT_BYTES);
        let (final_text, final_text_truncated) = match final_text {
            Some(t) => {
                let (capped, truncated) = cap_utf8(&t, MAX_TEXT_BYTES);
                (Some(capped), Some(truncated))
            }
            None => (None, Some(false)),
        };
        Self {
            v: 1,
            r#type: "turn.ended",
            event_id,
            session_id,
            upstream_id: meta.upstream_id.clone(),
            run_id: meta.run_id.clone(),
            client_req_ids: meta.client_req_ids.clone(),
            prompt_text,
            at_ms,
            stop_reason,
            honk_reason: meta.honk_reason.clone(),
            final_text,
            final_text_truncated,
            error,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HookOpts {
    pub queue: usize,
    pub request_timeout: Duration,
    pub retry_window: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
}

impl Default for HookOpts {
    fn default() -> Self {
        Self {
            queue: 256,
            request_timeout: Duration::from_secs(10),
            retry_window: Duration::from_secs(1200),
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
        }
    }
}

#[derive(Clone)]
pub struct TurnHook {
    tx: tokio::sync::mpsc::Sender<TurnEvent>,
}

impl TurnHook {
    pub fn spawn(url: String, token: Option<String>, opts: HookOpts) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(opts.queue);
        tokio::spawn(sender_loop(url, token, opts, rx));
        Self { tx }
    }

    pub fn emit(&self, ev: TurnEvent) {
        match self.tx.try_send(ev) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(ev)) => {
                tracing::warn!(
                    event_id = %ev.event_id,
                    "turn hook queue full; dropping event"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(ev)) => {
                tracing::warn!(
                    event_id = %ev.event_id,
                    "turn hook channel closed; dropping event"
                );
            }
        }
    }
}

async fn sender_loop(
    url: String,
    token: Option<String>,
    opts: HookOpts,
    mut rx: tokio::sync::mpsc::Receiver<TurnEvent>,
) {
    let client = match reqwest::Client::builder()
        .timeout(opts.request_timeout)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to create reqwest client for turn hook: {e:#}");
            return;
        }
    };

    while let Some(ev) = rx.recv().await {
        send_one_event(&client, &url, token.as_deref(), &opts, &ev).await;
    }
}

async fn send_one_event(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    opts: &HookOpts,
    ev: &TurnEvent,
) {
    let first_attempt = tokio::time::Instant::now();
    let mut current_backoff = opts.backoff_initial;

    loop {
        let mut req = client.post(url).json(ev);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }

        let send_res = req.send().await;
        match send_res {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return;
                } else if status.is_client_error() {
                    tracing::warn!(
                        event_id = %ev.event_id,
                        status = %status.as_u16(),
                        "turn hook receiver returned 4xx client error; dropping event"
                    );
                    return;
                } else {
                    tracing::warn!(
                        event_id = %ev.event_id,
                        status = %status.as_u16(),
                        "turn hook receiver returned transient error; will retry"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    event_id = %ev.event_id,
                    error = %e,
                    "turn hook request failed; will retry"
                );
            }
        }

        if first_attempt.elapsed() >= opts.retry_window {
            tracing::warn!(
                event_id = %ev.event_id,
                "turn hook retry window exceeded; dropping event"
            );
            return;
        }

        tokio::time::sleep(current_backoff).await;
        current_backoff = (current_backoff * 2).min(opts.backoff_max);

        if first_attempt.elapsed() >= opts.retry_window {
            tracing::warn!(
                event_id = %ev.event_id,
                "turn hook retry window exceeded; dropping event"
            );
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    #[test]
    fn test_cap_utf8_under_limit() {
        let (s, truncated) = cap_utf8("hello", 10);
        assert_eq!(s, "hello");
        assert!(!truncated);
    }

    #[test]
    fn test_cap_utf8_exact_limit() {
        let (s, truncated) = cap_utf8("hello", 5);
        assert_eq!(s, "hello");
        assert!(!truncated);
    }

    #[test]
    fn test_cap_utf8_ascii_over_limit() {
        let (s, truncated) = cap_utf8("hello world", 5);
        assert_eq!(s, "hello");
        assert!(truncated);
    }

    #[test]
    fn test_cap_utf8_multibyte_boundary() {
        // "🦀" is 4 bytes: [0xF0, 0x9F, 0xA6, 0x80]
        // "hi🦀there": "hi" is 2 bytes, 🦀 is 4 bytes (indices 2..6), "there" is 5 bytes.
        // If max is 3, boundary falls inside 🦀, so it must cap to "hi" (2 bytes).
        let (s, truncated) = cap_utf8("hi🦀there", 3);
        assert_eq!(s, "hi");
        assert!(truncated);

        // If max is 5, boundary falls inside 🦀, so it must cap to "hi" (2 bytes).
        let (s, truncated) = cap_utf8("hi🦀there", 5);
        assert_eq!(s, "hi");
        assert!(truncated);

        // If max is 6, 🦀 fits completely: "hi🦀" (6 bytes).
        let (s, truncated) = cap_utf8("hi🦀there", 6);
        assert_eq!(s, "hi🦀");
        assert!(truncated);
    }

    fn sample_meta() -> TurnMeta {
        TurnMeta {
            upstream_id: "hk1_2-3".to_string(),
            client_req_ids: vec!["crid-1".to_string()],
            prompt_text: "run prompt".to_string(),
            run_id: Some("run-42".to_string()),
            honk_reason: None,
        }
    }

    #[test]
    fn test_turn_event_started_fields_and_json() {
        let meta = sample_meta();
        let ev = TurnEvent::started("sid-1", &meta, 123456789);
        assert_eq!(ev.v, 1);
        assert_eq!(ev.r#type, "turn.started");
        assert_eq!(ev.event_id, "sid-1:hk1_2-3:started");
        assert_eq!(ev.session_id, "sid-1");
        assert_eq!(ev.upstream_id, "hk1_2-3");
        assert_eq!(ev.run_id.as_deref(), Some("run-42"));
        assert_eq!(ev.client_req_ids, vec!["crid-1"]);
        assert_eq!(ev.prompt_text, "run prompt");
        assert_eq!(ev.at_ms, 123456789);
        assert_eq!(ev.stop_reason, None);
        assert_eq!(ev.honk_reason, None);
        assert_eq!(ev.final_text, None);
        assert_eq!(ev.final_text_truncated, None);
        assert_eq!(ev.error, None);

        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["v"], 1);
        assert_eq!(json["type"], "turn.started");
        assert_eq!(json["event_id"], "sid-1:hk1_2-3:started");
        assert_eq!(json["session_id"], "sid-1");
        assert_eq!(json["upstream_id"], "hk1_2-3");
        assert_eq!(json["run_id"], "run-42");
        assert_eq!(json["client_req_ids"], serde_json::json!(["crid-1"]));
        assert_eq!(json["prompt_text"], "run prompt");
        assert_eq!(json["at_ms"], 123456789);
        // ended-only fields must be omitted
        assert!(json.get("stop_reason").is_none());
        assert!(json.get("honk_reason").is_none());
        assert!(json.get("final_text").is_none());
        assert!(json.get("final_text_truncated").is_none());
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_turn_event_ended_fields_and_json() {
        let mut meta = sample_meta();
        meta.honk_reason = Some("cancelled".to_string());
        let ev = TurnEvent::ended(
            "sid-1",
            &meta,
            Some("cancelled".to_string()),
            Some("answer text".to_string()),
            Some("something went wrong".to_string()),
            987654321,
        );
        assert_eq!(ev.v, 1);
        assert_eq!(ev.r#type, "turn.ended");
        assert_eq!(ev.event_id, "sid-1:hk1_2-3:ended");
        assert_eq!(ev.session_id, "sid-1");
        assert_eq!(ev.upstream_id, "hk1_2-3");
        assert_eq!(ev.run_id.as_deref(), Some("run-42"));
        assert_eq!(ev.client_req_ids, vec!["crid-1"]);
        assert_eq!(ev.prompt_text, "run prompt");
        assert_eq!(ev.at_ms, 987654321);
        assert_eq!(ev.stop_reason.as_deref(), Some("cancelled"));
        assert_eq!(ev.honk_reason.as_deref(), Some("cancelled"));
        assert_eq!(ev.final_text.as_deref(), Some("answer text"));
        assert_eq!(ev.final_text_truncated, Some(false));
        assert_eq!(ev.error.as_deref(), Some("something went wrong"));

        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["v"], 1);
        assert_eq!(json["type"], "turn.ended");
        assert_eq!(json["event_id"], "sid-1:hk1_2-3:ended");
        assert_eq!(json["stop_reason"], "cancelled");
        assert_eq!(json["honk_reason"], "cancelled");
        assert_eq!(json["final_text"], "answer text");
        assert_eq!(json["final_text_truncated"], false);
        assert_eq!(json["error"], "something went wrong");
    }

    #[test]
    fn test_turn_event_ended_none_final_text() {
        let meta = sample_meta();
        let ev = TurnEvent::ended("sid-1", &meta, None, None, None, 100);
        assert_eq!(ev.final_text, None);
        assert_eq!(ev.final_text_truncated, Some(false));
        let json = serde_json::to_value(&ev).expect("serialize");
        assert!(json.get("final_text").is_none());
        assert_eq!(json["final_text_truncated"], false);
        assert!(json.get("stop_reason").is_none());
        assert!(json.get("honk_reason").is_none());
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_turn_event_capping_over_max_text_bytes() {
        let large_prompt = "a".repeat(MAX_TEXT_BYTES + 100);
        let large_final = "b".repeat(MAX_TEXT_BYTES + 200);
        let meta = TurnMeta {
            upstream_id: "hk1_2-3".to_string(),
            client_req_ids: vec![],
            prompt_text: large_prompt,
            run_id: None,
            honk_reason: None,
        };

        let ev_start = TurnEvent::started("sid-1", &meta, 1);
        assert_eq!(ev_start.prompt_text.len(), MAX_TEXT_BYTES);

        let ev_end = TurnEvent::ended("sid-1", &meta, None, Some(large_final), None, 2);
        assert_eq!(ev_end.prompt_text.len(), MAX_TEXT_BYTES);
        assert_eq!(ev_end.final_text.as_ref().unwrap().len(), MAX_TEXT_BYTES);
        assert_eq!(ev_end.final_text_truncated, Some(true));
    }

    #[tokio::test]
    async fn test_hook_delivers_in_order_with_bearer_token() {
        type ReceivedEvent = (Option<String>, serde_json::Value);

        #[derive(Clone)]
        struct ServerState {
            received: Arc<Mutex<Vec<ReceivedEvent>>>,
        }
        let state = ServerState {
            received: Arc::new(Mutex::new(Vec::new())),
        };

        let app = Router::new()
            .route(
                "/hook",
                post(
                    |State(s): State<ServerState>,
                     headers: HeaderMap,
                     body: axum::extract::Json<serde_json::Value>| async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|h| h.to_str().ok())
                            .map(|s| s.to_string());
                        s.received.lock().await.push((auth, body.0));
                        StatusCode::OK
                    },
                ),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let opts = HookOpts {
            queue: 16,
            request_timeout: Duration::from_secs(2),
            retry_window: Duration::from_secs(10),
            backoff_initial: Duration::from_millis(10),
            backoff_max: Duration::from_millis(50),
        };
        let hook = TurnHook::spawn(
            format!("http://{addr}/hook"),
            Some("my-secret-token".to_string()),
            opts,
        );

        let meta = sample_meta();
        let ev1 = TurnEvent::started("sid-1", &meta, 1);
        let ev2 = TurnEvent::ended("sid-1", &meta, Some("end_turn".to_string()), None, None, 2);

        hook.emit(ev1);
        hook.emit(ev2);

        // Wait for delivery
        for _ in 0..50 {
            if state.received.lock().await.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let rec = state.received.lock().await;
        assert_eq!(rec.len(), 2);
        assert_eq!(rec[0].0.as_deref(), Some("Bearer my-secret-token"));
        assert_eq!(rec[0].1["type"], "turn.started");
        assert_eq!(rec[1].0.as_deref(), Some("Bearer my-secret-token"));
        assert_eq!(rec[1].1["type"], "turn.ended");
    }

    #[tokio::test]
    async fn test_hook_4xx_dropped_then_next_delivered() {
        #[derive(Clone)]
        struct ServerState {
            received: Arc<Mutex<Vec<String>>>,
        }
        let state = ServerState {
            received: Arc::new(Mutex::new(Vec::new())),
        };

        let app =
            Router::new()
                .route(
                    "/hook",
                    post(
                        |State(s): State<ServerState>,
                         body: axum::extract::Json<serde_json::Value>| async move {
                            let ev_id = body.0["event_id"].as_str().unwrap().to_string();
                            s.received.lock().await.push(ev_id.clone());
                            if ev_id.ends_with(":started") {
                                StatusCode::BAD_REQUEST
                            } else {
                                StatusCode::OK
                            }
                        },
                    ),
                )
                .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let opts = HookOpts {
            queue: 16,
            request_timeout: Duration::from_secs(2),
            retry_window: Duration::from_secs(5),
            backoff_initial: Duration::from_millis(10),
            backoff_max: Duration::from_millis(50),
        };
        let hook = TurnHook::spawn(format!("http://{addr}/hook"), None, opts);

        let meta = sample_meta();
        let ev1 = TurnEvent::started("sid-1", &meta, 1);
        let ev2 = TurnEvent::ended("sid-1", &meta, Some("end_turn".to_string()), None, None, 2);

        hook.emit(ev1);
        hook.emit(ev2);

        for _ in 0..50 {
            if state.received.lock().await.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let rec = state.received.lock().await;
        assert_eq!(rec.len(), 2);
        assert!(rec[0].ends_with(":started"));
        assert!(rec[1].ends_with(":ended"));
    }

    #[tokio::test]
    async fn test_hook_503_twice_then_200_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_clone = attempts.clone();

        let app = Router::new().route(
            "/hook",
            post(move || {
                let attempts = attempts_clone.clone();
                async move {
                    let count = attempts.fetch_add(1, Ordering::SeqCst);
                    if count < 2 {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let opts = HookOpts {
            queue: 16,
            request_timeout: Duration::from_secs(2),
            retry_window: Duration::from_secs(5),
            backoff_initial: Duration::from_millis(10),
            backoff_max: Duration::from_millis(50),
        };
        let hook = TurnHook::spawn(format!("http://{addr}/hook"), None, opts);

        let meta = sample_meta();
        let ev = TurnEvent::started("sid-1", &meta, 1);
        hook.emit(ev);

        for _ in 0..50 {
            if attempts.load(Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_hook_unreachable_then_reachable_within_window() {
        // Bind to get a free local port, then drop listener so port is unreachable
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let opts = HookOpts {
            queue: 16,
            request_timeout: Duration::from_millis(200),
            retry_window: Duration::from_secs(5),
            backoff_initial: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
        };
        let hook = TurnHook::spawn(format!("http://{addr}/hook"), None, opts);

        let meta = sample_meta();
        hook.emit(TurnEvent::started("sid-1", &meta, 1));

        // Sleep briefly while server is unreachable
        tokio::time::sleep(Duration::from_millis(60)).await;

        let delivered = Arc::new(AtomicUsize::new(0));
        let delivered_clone = delivered.clone();
        let app = Router::new().route(
            "/hook",
            post(move || {
                let delivered = delivered_clone.clone();
                async move {
                    delivered.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        for _ in 0..50 {
            if delivered.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        assert_eq!(delivered.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_hook_window_exceeded_dropped_and_next_delivered() {
        #[derive(Clone)]
        struct ServerState {
            ev1_attempts: Arc<AtomicUsize>,
            ev2_delivered: Arc<AtomicUsize>,
        }
        let state = ServerState {
            ev1_attempts: Arc::new(AtomicUsize::new(0)),
            ev2_delivered: Arc::new(AtomicUsize::new(0)),
        };

        let app =
            Router::new()
                .route(
                    "/hook",
                    post(
                        |State(s): State<ServerState>,
                         body: axum::extract::Json<serde_json::Value>| async move {
                            let ev_id = body.0["event_id"].as_str().unwrap().to_string();
                            if ev_id.ends_with(":started") {
                                s.ev1_attempts.fetch_add(1, Ordering::SeqCst);
                                StatusCode::INTERNAL_SERVER_ERROR
                            } else {
                                s.ev2_delivered.fetch_add(1, Ordering::SeqCst);
                                StatusCode::OK
                            }
                        },
                    ),
                )
                .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let opts = HookOpts {
            queue: 16,
            request_timeout: Duration::from_millis(50),
            retry_window: Duration::from_millis(150),
            backoff_initial: Duration::from_millis(15),
            backoff_max: Duration::from_millis(30),
        };
        let hook = TurnHook::spawn(format!("http://{addr}/hook"), None, opts);

        let meta = sample_meta();
        let ev1 = TurnEvent::started("sid-1", &meta, 1);
        let ev2 = TurnEvent::ended("sid-1", &meta, Some("end_turn".to_string()), None, None, 2);

        hook.emit(ev1);
        hook.emit(ev2);

        // Wait until ev2 is delivered after ev1's retry window has expired
        for _ in 0..100 {
            if state.ev2_delivered.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(state.ev1_attempts.load(Ordering::SeqCst) >= 2);
        assert_eq!(state.ev2_delivered.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_hook_emit_on_full_queue_returns_immediately() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let hook = TurnHook { tx };
        let meta = sample_meta();

        // 1st emit succeeds
        hook.emit(TurnEvent::started("sid-1", &meta, 1));
        // 2nd emit hits full queue, must return immediately without blocking
        hook.emit(TurnEvent::started("sid-1", &meta, 2));
    }
}
