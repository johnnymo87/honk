use anyhow::Context;
use reqwest::Url;

pub mod conn;
pub mod record;
pub mod redact;
pub mod sse;

pub use conn::{
    Event, EventRx, ReplayOutcome, StreamFeatures, StreamKey, SubscribeOpts, SubscribeOutcome,
    Upstream, UpstreamHandle, UpstreamOpts, rpc_id_matches,
};
pub use record::{Direction, RecordEntry, Recorder};
pub use redact::{Redactor, parse_redact_file, resolve_redact_file};
pub use sse::{SseFrame, sse_to_frames, sse_to_json_stream};

/// Production goose serve port; probes must never talk to it.
pub const PRODUCTION_GOOSE_PORT: u16 = 4080;

/// Validates that a target URL does not point to the production goose serve port
/// unless `allow_production` is true.
pub fn guard_url_with(url: &str, allow_production: bool) -> anyhow::Result<Url> {
    let parsed = Url::parse(url).with_context(|| format!("failed to parse URL '{url}'"))?;
    if parsed.port() == Some(PRODUCTION_GOOSE_PORT) && !allow_production {
        anyhow::bail!(
            "refusing to connect to production goose port {PRODUCTION_GOOSE_PORT} \
             (honkd: pass --allow-production-upstream)"
        );
    }
    Ok(parsed)
}

/// Validates that a target URL does not point to the production goose serve port.
///
/// Returns the parsed [`Url`] on success, or an error if the URL cannot be parsed
/// or targets the forbidden production port.
pub fn guard_url(url: &str) -> anyhow::Result<Url> {
    guard_url_with(url, false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpStatusError {
    pub op: &'static str,
    pub status: reqwest::StatusCode,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed with status {}", self.op, self.status)
    }
}

impl std::error::Error for HttpStatusError {}

pub fn status_of(err: &anyhow::Error) -> Option<reqwest::StatusCode> {
    err.chain()
        .find_map(|e| e.downcast_ref::<HttpStatusError>())
        .map(|e| e.status)
}

pub fn is_gone(err: &anyhow::Error) -> bool {
    status_of(err) == Some(reqwest::StatusCode::NOT_FOUND)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_driven_guard_url_rejections() {
        let rejected_cases = [
            "http://127.0.0.1:4080/acp",
            "http://localhost:4080/acp",
            "http://[::1]:4080/acp",
            "http://0.0.0.0:4080/acp",
            "ws://127.0.0.1:4080/acp",
            "http://127.0.0.1:4080",
        ];

        for url in rejected_cases {
            let err = match guard_url(url) {
                Ok(parsed) => panic!("expected '{url}' to be rejected, got ok: {parsed}"),
                Err(err) => err,
            };
            assert!(
                err.to_string().contains(&PRODUCTION_GOOSE_PORT.to_string()),
                "error for '{url}' should mention production port {PRODUCTION_GOOSE_PORT}, got: {err}"
            );
        }
    }

    #[test]
    fn table_driven_guard_url_acceptances() {
        let accepted_cases = [
            "http://127.0.0.1:4180/acp",
            "http://127.0.0.1/acp",
            "ws://127.0.0.1:4180/acp",
        ];

        for url in accepted_cases {
            let parsed = match guard_url(url) {
                Ok(parsed) => parsed,
                Err(err) => panic!("expected '{url}' to be accepted, got error: {err}"),
            };
            assert_eq!(parsed.as_str(), url);
        }
    }

    #[test]
    fn refuses_unparseable_url() {
        let err = match guard_url("not a valid url") {
            Ok(parsed) => panic!("expected unparseable URL to fail, got ok: {parsed}"),
            Err(err) => err,
        };
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("parse") || msg.contains("relative url"),
            "expected parse-related error message, got: {err}"
        );
    }

    #[test]
    fn production_port_allowed_only_with_opt_in() {
        assert!(guard_url_with("http://127.0.0.1:4080/acp", false).is_err());
        assert!(guard_url_with("http://127.0.0.1:4080/acp", true).is_ok());
        assert!(guard_url("http://127.0.0.1:4080/acp").is_err());
    }

    #[test]
    fn http_status_is_recoverable_from_error_chain() {
        let err = anyhow::Error::new(HttpStatusError {
            op: "GET stream",
            status: reqwest::StatusCode::NOT_FOUND,
        })
        .context("subscribe failed");
        assert_eq!(status_of(&err), Some(reqwest::StatusCode::NOT_FOUND));
        assert!(is_gone(&err));
        assert!(!is_gone(&anyhow::anyhow!("other")));
    }
}
