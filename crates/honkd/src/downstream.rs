use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
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

pub fn router(hub: Arc<Hub>, secret: String) -> Router {
    let state = Arc::new(DownstreamState { hub, secret });
    Router::new()
        .route("/acp", get(acp_handler))
        .with_state(state)
}

async fn acp_handler(
    State(state): State<Arc<DownstreamState>>,
    headers: HeaderMap,
    uri: Uri,
    MaybeWs(ws): MaybeWs,
) -> Response {
    let auth_header = headers.get("x-secret-key").and_then(|v| v.to_str().ok());
    let token_query = uri.query().and_then(|q| {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=')
                && k == "token"
            {
                return Some(v);
            }
        }
        None
    });

    let header_ok = auth_header
        .map(|h| {
            !h.trim().is_empty()
                && !state.secret.is_empty()
                && constant_time_eq(&state.secret, h.trim())
        })
        .unwrap_or(false);
    let token_ok = token_query
        .map(|t| {
            !t.trim().is_empty()
                && !state.secret.is_empty()
                && constant_time_eq(&state.secret, t.trim())
        })
        .unwrap_or(false);

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
