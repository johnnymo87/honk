use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

pub type RecordedEvent = (Option<String>, Value);
type EventStore = Arc<Mutex<Vec<RecordedEvent>>>;

#[derive(Clone)]
pub struct HookStub {
    pub addr: SocketAddr,
    received: EventStore,
    status: Arc<AtomicU16>,
}

impl HookStub {
    pub async fn start() -> Self {
        let received = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(AtomicU16::new(StatusCode::OK.as_u16()));

        #[derive(Clone)]
        struct AppState {
            received: EventStore,
            status: Arc<AtomicU16>,
        }

        let app_state = AppState {
            received: received.clone(),
            status: status.clone(),
        };

        let app = Router::new()
            .route(
                "/hook",
                post(
                    |State(s): State<AppState>,
                     headers: HeaderMap,
                     body: axum::extract::Json<Value>| async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|h| h.to_str().ok())
                            .map(|s| s.to_string());
                        s.received.lock().await.push((auth, body.0));
                        let code = s.status.load(Ordering::SeqCst);
                        StatusCode::from_u16(code).unwrap_or(StatusCode::OK)
                    },
                ),
            )
            .with_state(app_state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind hook stub");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            addr,
            received,
            status,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}/hook", self.addr)
    }

    pub fn set_status(&self, code: StatusCode) {
        self.status.store(code.as_u16(), Ordering::SeqCst);
    }

    pub async fn received(&self) -> Vec<RecordedEvent> {
        self.received.lock().await.clone()
    }

    pub async fn wait_for_events(&self, count: usize, timeout: Duration) -> Vec<RecordedEvent> {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            let events = self.received().await;
            if events.len() >= count {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.received().await
    }
}
