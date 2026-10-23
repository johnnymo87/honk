use axum::Json;
use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::convert::Infallible;
use std::sync::Arc;
use subtle::ConstantTimeEq;

use crate::hub::Hub;
use crate::viewer::viewer_loop;

pub struct DownstreamState {
    pub hub: Arc<Hub>,
    pub secret: String,
}

struct MaybeWs(Option<WebSocketUpgrade>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeWs {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let ws = WebSocketUpgrade::from_request_parts(parts, state)
            .await
            .ok();
        Ok(MaybeWs(ws))
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    if a_bytes.len() != b_bytes.len() {
        return false;
    }
    a_bytes.ct_eq(b_bytes).into()
}

pub fn check_secret_header(secret: &str, headers: &HeaderMap) -> bool {
    headers
        .get("x-secret-key")
        .and_then(|v| v.to_str().ok())
        .map(|h| !h.trim().is_empty() && !secret.is_empty() && constant_time_eq(secret, h.trim()))
        .unwrap_or(false)
}

pub fn check_secret_query(secret: &str, uri: &Uri) -> bool {
    uri.query()
        .and_then(|q| {
            for pair in q.split('&') {
                if let Some((k, v)) = pair.split_once('=')
                    && k == "token"
                {
                    return Some(v);
                }
            }
            None
        })
        .map(|t| !t.trim().is_empty() && !secret.is_empty() && constant_time_eq(secret, t.trim()))
        .unwrap_or(false)
}

pub fn router(hub: Arc<Hub>, secret: String) -> Router {
    let state = Arc::new(DownstreamState { hub, secret });
    Router::new()
        .route("/acp", get(acp_handler))
        .route("/mcp/{op}", post(mcp_self_handler))
        .with_state(state)
}

async fn acp_handler(
    State(state): State<Arc<DownstreamState>>,
    headers: HeaderMap,
    uri: Uri,
    MaybeWs(ws): MaybeWs,
) -> Response {
    let header_ok = check_secret_header(&state.secret, &headers);
    let token_ok = check_secret_query(&state.secret, &uri);

    if !header_ok && !token_ok {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match ws {
        Some(upgrade) => {
            let hub = state.hub.clone();
            upgrade
                .on_upgrade(move |socket| viewer_loop(socket, hub))
                .into_response()
        }
        None => StatusCode::NOT_ACCEPTABLE.into_response(),
    }
}

async fn mcp_self_handler(
    State(state): State<Arc<DownstreamState>>,
    Path(op): Path<String>,
    headers: HeaderMap,
    body_bytes: axum::body::Bytes,
) -> Response {
    if !state.hub.args.mcp_self_grant {
        return StatusCode::NOT_FOUND.into_response();
    }

    if op != "list" && op != "enable" && op != "disable" {
        return StatusCode::NOT_FOUND.into_response();
    }

    if !check_secret_header(&state.secret, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let body_json: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => {
            return Json(serde_json::json!({
                "error": {
                    "code": -32602,
                    "message": "Invalid JSON body",
                }
            }))
            .into_response();
        }
    };

    let Some(sid) = body_json
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
    else {
        return Json(serde_json::json!({
            "error": {
                "code": -32602,
                "message": format!("Missing sessionId in /mcp/{op}"),
            }
        }))
        .into_response();
    };
    let sid = sid.to_string();

    let (op_kind, reason, names_for_log) = match op.as_str() {
        "list" => (crate::session::McpOpKind::List, None, Vec::new()),
        "enable" => {
            let names_arr = body_json.get("names").and_then(serde_json::Value::as_array);
            let names = match names_arr {
                Some(arr) if arr.iter().all(serde_json::Value::is_string) => arr
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect::<Vec<_>>(),
                _ => {
                    return Json(serde_json::json!({
                        "error": {
                            "code": -32602,
                            "message": "Invalid or missing 'names' array in /mcp/enable",
                        }
                    }))
                    .into_response();
                }
            };

            let confirm = if let Some(conf_val) = body_json.get("confirm") {
                match conf_val.as_array() {
                    Some(arr) if arr.iter().all(serde_json::Value::is_string) => arr
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(String::from)
                        .collect::<Vec<_>>(),
                    _ => {
                        return Json(serde_json::json!({
                            "error": {
                                "code": -32602,
                                "message": "Invalid params: `confirm` in /mcp/enable must be an array of strings",
                            }
                        }))
                        .into_response();
                    }
                }
            } else {
                Vec::new()
            };

            let reason = if let Some(reason_val) = body_json.get("reason") {
                match reason_val.as_str() {
                    Some(r) => Some(r.to_string()),
                    None => {
                        return Json(serde_json::json!({
                            "error": {
                                "code": -32602,
                                "message": "Invalid params: `reason` in /mcp/enable must be a string",
                            }
                        }))
                        .into_response();
                    }
                }
            } else {
                None
            };

            let names_copy = names.clone();
            (
                crate::session::McpOpKind::Enable { names, confirm },
                reason,
                names_copy,
            )
        }
        "disable" => {
            let names_arr = body_json.get("names").and_then(serde_json::Value::as_array);
            let names = match names_arr {
                Some(arr) if arr.iter().all(serde_json::Value::is_string) => arr
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect::<Vec<_>>(),
                _ => {
                    return Json(serde_json::json!({
                        "error": {
                            "code": -32602,
                            "message": "Invalid or missing 'names' array in /mcp/disable",
                        }
                    }))
                    .into_response();
                }
            };
            let names_copy = names.clone();
            (
                crate::session::McpOpKind::Disable { names },
                None,
                names_copy,
            )
        }
        _ => unreachable!(),
    };

    tracing::info!(sessionId = %sid, op = %op, names = ?names_for_log, ?reason, "mcp self-op");

    let work = async {
        let session_tx = {
            let sessions = state.hub.sessions.read().await;
            sessions.get(&sid).map(|h| h.tx.clone())
        };

        let tx = match session_tx {
            Some(tx) => tx,
            None => match state.hub.get_or_activate_session(&sid).await {
                Ok(h) => h.tx,
                Err(e) => {
                    return serde_json::json!({
                        "error": {
                            "code": e.code,
                            "message": e.message,
                        }
                    });
                }
            },
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let msg = crate::session::SessionMsg::McpSelfOp {
            op: op_kind.clone(),
            reason: reason.clone(),
            reply: reply_tx,
        };

        let reply_rx = if tx.send(msg).await.is_err() {
            match state.hub.get_or_activate_session(&sid).await {
                Ok(new_h) => {
                    let (retry_tx, retry_rx) = tokio::sync::oneshot::channel();
                    let retry_msg = crate::session::SessionMsg::McpSelfOp {
                        op: op_kind,
                        reason,
                        reply: retry_tx,
                    };
                    if new_h.tx.send(retry_msg).await.is_err() {
                        return serde_json::json!({
                            "error": {
                                "code": -32000,
                                "message": "Failed to send to session actor",
                            }
                        });
                    }
                    retry_rx
                }
                Err(e) => {
                    return serde_json::json!({
                        "error": {
                            "code": e.code,
                            "message": e.message,
                        }
                    });
                }
            }
        } else {
            reply_rx
        };

        match reply_rx.await {
            Ok(Ok(crate::session::McpOpOutput::List(res))) => {
                serde_json::to_value(res).unwrap_or_else(|_| serde_json::json!({}))
            }
            Ok(Ok(crate::session::McpOpOutput::Op(res))) => {
                serde_json::to_value(res).unwrap_or_else(|_| serde_json::json!({}))
            }
            Ok(Err(err)) => {
                serde_json::json!({
                    "error": {
                        "code": err.code,
                        "message": err.message,
                    }
                })
            }
            Err(_) => {
                serde_json::json!({
                    "error": {
                        "code": -32000,
                        "message": "Session actor dropped reply",
                    }
                })
            }
        }
    };

    let res_body = match tokio::time::timeout(std::time::Duration::from_secs(60), work).await {
        Ok(val) => val,
        Err(_) => {
            serde_json::json!({
                "error": {
                    "code": -32603,
                    "message": "timed out; the change may still land; call mcp_list",
                }
            })
        }
    };

    Json(res_body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn test_check_secret_header() {
        let secret = "correct-secret-12345678901234567890";
        let mut headers = HeaderMap::new();

        // Missing header
        assert!(!check_secret_header(secret, &headers));

        // Wrong header
        headers.insert("x-secret-key", HeaderValue::from_static("wrong-secret"));
        assert!(!check_secret_header(secret, &headers));

        // Correct header
        headers.insert(
            "x-secret-key",
            HeaderValue::from_static("correct-secret-12345678901234567890"),
        );
        assert!(check_secret_header(secret, &headers));

        // Correct header with whitespace
        headers.insert(
            "x-secret-key",
            HeaderValue::from_static("  correct-secret-12345678901234567890 "),
        );
        assert!(check_secret_header(secret, &headers));

        // Empty secret
        assert!(!check_secret_header("", &headers));
    }

    #[test]
    fn test_check_secret_query() {
        let secret = "my-secret-key-12345678901234567890";

        let uri_no_query: Uri = "/acp".parse().unwrap();
        assert!(!check_secret_query(secret, &uri_no_query));

        let uri_other_query: Uri = "/acp?other=1".parse().unwrap();
        assert!(!check_secret_query(secret, &uri_other_query));

        let uri_wrong_token: Uri = "/acp?token=wrong".parse().unwrap();
        assert!(!check_secret_query(secret, &uri_wrong_token));

        let uri_correct_token: Uri = format!("/acp?token={secret}").parse().unwrap();
        assert!(check_secret_query(secret, &uri_correct_token));

        let uri_multi_query: Uri = format!("/acp?foo=bar&token={secret}&baz=1")
            .parse()
            .unwrap();
        assert!(check_secret_query(secret, &uri_multi_query));
    }
}
