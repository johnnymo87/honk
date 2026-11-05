//! Reachability probe for remote (streamable-HTTP) MCP servers.
//!
//! goose answers a 401/403 from a streamable-HTTP server by starting a browser
//! OAuth flow that waits up to five minutes, and a headless serve has no
//! browser. So honkd sends one MCP `initialize` before every remote add and
//! only goes ahead when the server answers in a way that is not an
//! authorization challenge.

use std::future::Future;
use std::time::Duration;

/// Total time allowed for the probe request, connect included.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Checks that a remote MCP server can be added without stalling goose.
pub trait RemoteProbe: Send + Sync {
    /// `Ok(())` when goose may add the server; `Err(reason)` is a one-line,
    /// human-readable reason it should not.
    fn probe(&self, url: &str) -> impl Future<Output = Result<(), String>> + Send;
}

/// The real probe, over reqwest.
#[derive(Debug, Clone, Default)]
pub struct HttpProbe;

/// `url` without credentials or query, for messages.
fn display_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => "<invalid url>".to_string(),
    }
}

/// The innermost message of an error chain: `Connection refused (os error 111)`
/// rather than reqwest's `error sending request for url (...)`.
fn short_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return format!("timed out after {}s", PROBE_TIMEOUT.as_secs());
    }
    let mut msg = String::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        msg = e.to_string();
        cur = e.source();
    }
    // The innermost message of a builder or request error can still carry the URL.
    if let Some(url) = err.url() {
        msg = msg.replace(url.as_str(), "");
    }
    msg.trim().to_string()
}

impl RemoteProbe for HttpProbe {
    async fn probe(&self, url: &str) -> Result<(), String> {
        let shown = display_url(url);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(PROBE_TIMEOUT)
            .build()
            .map_err(|e| format!("not reachable at {shown}: {e}"))?;

        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "honkd-probe",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        });

        let resp = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(initialize.to_string())
            .send()
            .await
            .map_err(|e| format!("not reachable at {shown}: {}", short_error(&e)))?;

        // Do not read the body: it may be an SSE stream that never ends.
        // Dropping the response closes the connection.
        if let Some(session) = resp.headers().get("mcp-session-id").cloned() {
            let delete = client.delete(url).header("mcp-session-id", session);
            tokio::spawn(async move {
                let _ = delete.send().await;
            });
        }

        match resp.status().as_u16() {
            401 | 403 => {
                Err("needs authorization (OAuth), which honkd does not support".to_string())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::StatusCode, routing::post};
    use std::sync::{Arc, Mutex};

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}/mcp")
    }

    fn status_router(status: StatusCode) -> Router {
        Router::new().route("/mcp", post(move || async move { status }))
    }

    #[tokio::test]
    async fn status_200_is_ok() {
        let url = serve(status_router(StatusCode::OK)).await;
        assert_eq!(HttpProbe.probe(&url).await, Ok(()));
    }

    #[tokio::test]
    async fn other_statuses_are_ok() {
        for status in [
            StatusCode::NOT_FOUND,
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::NOT_ACCEPTABLE,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let url = serve(status_router(status)).await;
            assert_eq!(HttpProbe.probe(&url).await, Ok(()), "{status}");
        }
    }

    #[tokio::test]
    async fn status_401_and_403_need_authorization() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let url = serve(status_router(status)).await;
            let err = HttpProbe.probe(&url).await.unwrap_err();
            assert_eq!(
                err,
                "needs authorization (OAuth), which honkd does not support"
            );
        }
    }

    #[tokio::test]
    async fn closed_port_is_not_reachable() {
        // Bind, note the port, drop the listener: nothing listens there now.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}/mcp");
        let err = HttpProbe.probe(&url).await.unwrap_err();
        assert!(
            err.starts_with(&format!("not reachable at {url}: ")),
            "{err}"
        );
        assert!(
            !err.contains("error sending request"),
            "message should be the short cause: {err}"
        );
    }

    #[tokio::test]
    async fn slow_server_times_out() {
        let router = Router::new().route(
            "/mcp",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                StatusCode::OK
            }),
        );
        let url = serve(router).await;
        let started = std::time::Instant::now();
        let err = HttpProbe.probe(&url).await.unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn sends_initialize_with_the_mcp_headers() {
        #[derive(Default)]
        struct Seen {
            content_type: Option<String>,
            accept: Option<String>,
            body: Option<serde_json::Value>,
        }
        let seen = Arc::new(Mutex::new(Seen::default()));
        let s = seen.clone();
        let router = Router::new().route(
            "/mcp",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let s = s.clone();
                async move {
                    let mut g = s.lock().unwrap();
                    let h = |n: &str| {
                        headers
                            .get(n)
                            .and_then(|v| v.to_str().ok())
                            .map(String::from)
                    };
                    g.content_type = h("content-type");
                    g.accept = h("accept");
                    g.body = serde_json::from_str(&body).ok();
                    StatusCode::OK
                }
            }),
        );
        let url = serve(router).await;
        HttpProbe.probe(&url).await.unwrap();

        let g = seen.lock().unwrap();
        assert_eq!(g.content_type.as_deref(), Some("application/json"));
        assert_eq!(
            g.accept.as_deref(),
            Some("application/json, text/event-stream")
        );
        let body = g.body.as_ref().expect("json body");
        assert_eq!(body["method"], "initialize");
        assert_eq!(body["params"]["protocolVersion"], "2025-03-26");
        assert_eq!(body["params"]["clientInfo"]["name"], "honkd-probe");
    }

    #[tokio::test]
    async fn a_session_id_is_closed_best_effort() {
        let deleted = Arc::new(Mutex::new(None::<String>));
        let d = deleted.clone();
        let router = Router::new().route(
            "/mcp",
            post(|| async { ([("mcp-session-id", "sess-1")], StatusCode::OK) }).delete(
                move |headers: axum::http::HeaderMap| {
                    let d = d.clone();
                    async move {
                        *d.lock().unwrap() = headers
                            .get("mcp-session-id")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from);
                        StatusCode::OK
                    }
                },
            ),
        );
        let url = serve(router).await;
        HttpProbe.probe(&url).await.unwrap();
        for _ in 0..50 {
            if deleted.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(deleted.lock().unwrap().as_deref(), Some("sess-1"));
    }

    #[test]
    fn display_url_drops_credentials_and_query() {
        assert_eq!(
            display_url("http://user:pw@127.0.0.1:9/mcp?token=abc#frag"),
            "http://127.0.0.1:9/mcp"
        );
    }
}
