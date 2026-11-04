//! Registering a goose session with a pigeon daemon so it gets a Telegram topic.
//!
//! pigeon learns about a goose session only when it launches one itself or when
//! something calls its `POST /goose/sessions`. Registering is not enough to make
//! the session visible, though: pigeon creates the forum topic lazily, on the
//! first notification it sends for the session, and it only sends notifications
//! for turns it prompted. So honk registers the session and then posts one
//! notification (`POST /stop`) announcing it, which is what creates the topic.
//!
//! Prompts and replies from honk chat appear in the topic when honkd runs with
//! its turn hook enabled (`--turn-hook-url`); without the hook, pigeon only
//! reports the turns it starts.
//!
//! honk hands pigeon the honkd secret it connected with, as the session's
//! `auth_token`. pigeon checks that honkd is reachable with the session's stored
//! token before it delivers each Telegram message, and refuses the message if no
//! token is stored, so a session registered without one gets a topic that
//! cannot be used. It is the same credential pigeon stores for the sessions it
//! launches through honkd itself.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};

/// How to reach the pigeon daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PigeonConfig {
    /// Base URL of the pigeon daemon, e.g. `http://127.0.0.1:4731`.
    pub url: String,
    /// Bearer token, when the daemon requires one.
    pub token: Option<String>,
}

/// Where pigeon is and where its token lives, as given on the command line.
/// Resolved into a [`PigeonConfig`] only when used, so a bad token file is
/// reported like any other registration failure instead of stopping honk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PigeonSettings {
    pub url: String,
    pub token_file: Option<PathBuf>,
}

impl PigeonSettings {
    /// Reads the token file (trimmed; an empty file means "no token").
    pub fn load(&self) -> anyhow::Result<PigeonConfig> {
        let url = self.url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            bail!("pigeon URL is empty");
        }
        let token = match &self.token_file {
            Some(p) => {
                let raw = std::fs::read_to_string(p)
                    .with_context(|| format!("reading pigeon token file {}", p.display()))?;
                let t = raw.trim().to_string();
                if t.is_empty() { None } else { Some(t) }
            }
            None => None,
        };
        Ok(PigeonConfig { url, token })
    }
}

/// What to tell pigeon about the session.
#[derive(Clone, PartialEq, Eq)]
pub struct Registration {
    /// goose's id for the session, e.g. `20261016_1`.
    pub goose_session_id: String,
    /// The honkd ACP endpoint pigeon should drive the session through (ws/wss).
    pub endpoint: String,
    /// The honkd secret pigeon must present at `endpoint`.
    pub auth_token: String,
    pub cwd: String,
    /// Session title, when known. pigeon names the topic after it.
    pub title: Option<String>,
    /// Why the announcement is being sent; shapes its wording.
    pub reason: Reason,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("goose_session_id", &self.goose_session_id)
            .field("endpoint", &self.endpoint)
            .field("auth_token", &"<redacted>")
            .field("cwd", &self.cwd)
            .field("title", &self.title)
            .field("reason", &self.reason)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// `honk chat --new` just created the session.
    NewChat,
    /// `honk telegram` for an existing session.
    Existing,
    /// `honk launch` just created the session and is about to prompt it.
    Launch,
}

/// The outcome of a successful registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registered {
    /// pigeon's own id for the session (`gse_…`).
    pub pigeon_session_id: String,
    /// Whether pigeon queued the announcement. `false` means pigeon accepted
    /// the session but declined to notify (e.g. a quiet policy), so no topic
    /// appears until pigeon next notifies about it.
    pub announced: bool,
    /// pigeon's reason for not announcing, when it gave one.
    pub not_announced_reason: Option<String>,
}

impl Registered {
    /// A one-line summary for the TUI or stdout.
    pub fn summary(&self) -> String {
        if self.announced {
            "Telegram: registered with pigeon; its topic appears shortly. Replies there prompt this session; turns typed here appear in the topic when honkd's turn hook is enabled.".to_string()
        } else {
            format!(
                "Telegram: registered with pigeon, but it did not announce the session ({}); no topic yet.",
                self.not_announced_reason
                    .as_deref()
                    .unwrap_or("no reason given")
            )
        }
    }
}

/// Converts honk's honkd URL into the WebSocket form pigeon stores and dials.
pub fn acp_endpoint(honk_url: &str) -> anyhow::Result<String> {
    let mut u =
        url::Url::parse(honk_url).with_context(|| format!("invalid honkd URL {honk_url}"))?;
    let scheme = match u.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => bail!("unsupported honkd URL scheme `{other}`"),
    };
    u.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("cannot convert {honk_url} to a WebSocket URL"))?;
    // honk's own credentials never go to pigeon; it uses its own goose token.
    u.set_query(None);
    let _ = u.set_username("");
    let _ = u.set_password(None);
    Ok(u.to_string())
}

/// Kept short on purpose: a swipe-reply to it is quoted into the prompt.
fn announcement(reg: &Registration) -> String {
    let what = match reg.reason {
        Reason::NewChat => "New honk chat session",
        Reason::Existing => "honk session",
        Reason::Launch => "Launched goose session",
    };
    format!(
        "{what} {} in {}. Reply here to prompt it.",
        reg.goose_session_id, reg.cwd
    )
}

const TIMEOUT: Duration = Duration::from_secs(10);

/// Registers the session with pigeon and posts the announcement that creates
/// its topic. pigeon reuses its row for the same goose id but overwrites it
/// wholesale; see [`find_registered`].
pub async fn register(cfg: &PigeonConfig, reg: &Registration) -> anyhow::Result<Registered> {
    let pigeon_id = register_row(cfg, reg).await?;
    announce(cfg, &pigeon_id, reg)
        .await
        .context("session registered, but announcing it failed")
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .context("building HTTP client")
}

/// `POST /goose/sessions`: creates (or refreshes) pigeon's row for the session,
/// with `notify` on and `reg.endpoint` as its `backend_endpoint`, and returns
/// pigeon's id for it (`gse_…`). No topic exists until [`announce`].
pub async fn register_row(cfg: &PigeonConfig, reg: &Registration) -> anyhow::Result<String> {
    let http = http_client()?;
    let body = json!({
        "session_id": reg.goose_session_id,
        "endpoint": reg.endpoint,
        "cwd": reg.cwd,
        "label": reg.goose_session_id,
        "auth_token": reg.auth_token,
    });
    let res = post(&http, cfg, "/goose/sessions", &body).await?;
    let Some(pigeon_id) = res.get("session_id").and_then(Value::as_str) else {
        bail!("pigeon /goose/sessions response has no session_id: {res}");
    };
    Ok(pigeon_id.to_string())
}

/// Posts the one notification that makes pigeon create the session's topic.
pub async fn announce(
    cfg: &PigeonConfig,
    pigeon_id: &str,
    reg: &Registration,
) -> anyhow::Result<Registered> {
    let http = http_client()?;
    let mut stop = json!({
        "session_id": pigeon_id,
        "event": "Notification",
        "message": announcement(reg),
    });
    if let Some(t) = reg.title.as_deref().filter(|t| !t.trim().is_empty()) {
        stop["title"] = json!(t);
    }
    let res = post(&http, cfg, "/stop", &stop).await?;
    let announced = res.get("notified") != Some(&json!(false));
    let not_announced_reason = if announced {
        None
    } else {
        res.get("reason")
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    Ok(Registered {
        pigeon_session_id: pigeon_id.to_string(),
        announced,
        not_announced_reason,
    })
}

/// pigeon's id for the goose session, if pigeon already has it registered.
///
/// Registering again overwrites pigeon's row wholesale (token, label, cwd,
/// title), which can break a session pigeon launched itself, so `honk telegram`
/// checks first. Rows with `notify: false` (e.g. swarm mailbox-only rows) are
/// skipped so `honk telegram` proceeds to register and upgrade them.
pub async fn find_registered(
    cfg: &PigeonConfig,
    goose_session_id: &str,
) -> anyhow::Result<Option<String>> {
    let rows = fetch_sessions(cfg).await?;
    let found = rows
        .iter()
        .find(|s| {
            let id = |k: &str| s.get(k).and_then(Value::as_str);
            // Rows from before pigeon minted its own ids carry goose's id as
            // `session_id` and no `backend_session_id`. Swarm mailbox-only rows
            // have `notify: false` and are skipped so `honk telegram` can upgrade them.
            s.get("notify") != Some(&Value::Bool(false))
                && id("backend_kind") == Some("goose-acp")
                && (id("backend_session_id") == Some(goose_session_id)
                    || id("session_id") == Some(goose_session_id))
        })
        .and_then(|s| s.get("session_id").and_then(Value::as_str))
        .map(str::to_string);
    Ok(found)
}

/// Every row of pigeon's `GET /sessions`.
pub async fn fetch_sessions(cfg: &PigeonConfig) -> anyhow::Result<Vec<Value>> {
    let http = http_client()?;
    let mut req = http.get(format!("{}/sessions", cfg.url));
    if let Some(t) = &cfg.token {
        req = req.bearer_auth(t);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("pigeon unreachable at {}", cfg.url))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("pigeon /sessions returned HTTP {}", status.as_u16());
    }
    let v: Value = resp
        .json()
        .await
        .context("pigeon /sessions returned non-JSON")?;
    let Some(rows) = v.get("sessions").and_then(Value::as_array) else {
        bail!("pigeon /sessions response has no sessions array");
    };
    Ok(rows.clone())
}

/// The prefix of the named topic stall-watch binds to a program's coordinator:
/// pigeon `bound_topic_key = 'stallwatch:<program tag>'`.
pub const PROGRAM_TOPIC_PREFIX: &str = "stallwatch:";

/// Whether pigeon's rows say anything about bound topics at all. A pigeon too
/// old to report `bound_topic_key` cannot tell a coordinator from a worker.
pub fn reports_bound_topics(rows: &[Value]) -> bool {
    rows.iter().any(|s| s.get("bound_topic_key").is_some())
}

/// The program tag of the stall-watch coordinator `launcher` is, if it is one.
///
/// `launcher` is an oc-tags key: `ses_…` for an opencode session (matched on
/// pigeon's `session_id`), or `goose:<goose id>` for a goose session (matched
/// on `backend_session_id`, or on `session_id` for a legacy row whose pigeon id
/// is goose's id). A row bound to `stallwatch:<tag>` makes it the coordinator
/// of program `<tag>`; `stallwatch:_notices` and other non-tag keys do not.
pub fn coordinator_program(rows: &[Value], launcher: &str) -> Option<String> {
    let goose = launcher.strip_prefix("goose:");
    rows.iter()
        .filter(|s| {
            let id = |k: &str| s.get(k).and_then(Value::as_str);
            match goose {
                Some(g) => {
                    id("backend_kind") == Some("goose-acp")
                        && (id("backend_session_id") == Some(g)
                            || (id("backend_session_id").is_none() && id("session_id") == Some(g)))
                }
                None => {
                    id("backend_kind") != Some("goose-acp") && id("session_id") == Some(launcher)
                }
            }
        })
        .filter_map(|s| s.get("bound_topic_key").and_then(Value::as_str))
        .filter_map(|k| k.strip_prefix(PROGRAM_TOPIC_PREFIX))
        .find_map(|t| crate::octags::parse_tag(t).ok())
}

async fn post(
    http: &reqwest::Client,
    cfg: &PigeonConfig,
    path: &str,
    body: &Value,
) -> anyhow::Result<Value> {
    let url = format!("{}{}", cfg.url, path);
    let mut req = http.post(&url).json(body);
    if let Some(t) = &cfg.token {
        req = req.bearer_auth(t);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("pigeon unreachable at {}", cfg.url))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| text.chars().take(200).collect());
        bail!("pigeon {path} returned HTTP {}: {detail}", status.as_u16());
    }
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("pigeon {path} returned non-JSON"))?;
    if v.get("ok") == Some(&json!(false)) {
        bail!("pigeon {path} refused: {v}");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_endpoint_converts_http_to_ws() {
        assert_eq!(
            acp_endpoint("http://127.0.0.1:4280/acp").unwrap(),
            "ws://127.0.0.1:4280/acp"
        );
        assert_eq!(
            acp_endpoint("https://example.test/acp").unwrap(),
            "wss://example.test/acp"
        );
        assert_eq!(
            acp_endpoint("ws://127.0.0.1:4280/acp").unwrap(),
            "ws://127.0.0.1:4280/acp"
        );
    }

    #[test]
    fn acp_endpoint_drops_credentials_and_query() {
        assert_eq!(
            acp_endpoint("http://u:p@127.0.0.1:4280/acp?token=s3cret").unwrap(),
            "ws://127.0.0.1:4280/acp"
        );
    }

    #[test]
    fn acp_endpoint_rejects_other_schemes() {
        assert!(acp_endpoint("ftp://x/acp").is_err());
        assert!(acp_endpoint("not a url").is_err());
    }

    #[test]
    fn config_load_trims_url_and_token() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("tok");
        std::fs::write(&f, "  abc\n").unwrap();
        let settings = |u: &str| PigeonSettings {
            url: u.into(),
            token_file: Some(f.clone()),
        };
        let c = settings("http://127.0.0.1:4731/").load().unwrap();
        assert_eq!(c.url, "http://127.0.0.1:4731");
        assert_eq!(c.token.as_deref(), Some("abc"));

        std::fs::write(&f, "\n").unwrap();
        let c = settings("http://127.0.0.1:4731").load().unwrap();
        assert_eq!(c.token, None);
    }

    #[test]
    fn config_load_missing_token_file_errors() {
        let err = PigeonSettings {
            url: "http://x".into(),
            token_file: Some(PathBuf::from("/nonexistent/tok")),
        }
        .load()
        .unwrap_err();
        assert!(err.to_string().contains("pigeon token file"), "{err}");
    }

    #[test]
    fn coordinator_program_matches_the_launcher_row_only() {
        let rows = vec![
            json!({"session_id": "gse_a", "backend_kind": "goose-acp",
                   "backend_session_id": "20261021_40", "bound_topic_key": "stallwatch:honk"}),
            json!({"session_id": "20261001_3", "backend_kind": "goose-acp",
                   "backend_session_id": null, "bound_topic_key": "stallwatch:legacy"}),
            json!({"session_id": "ses_coord", "backend_kind": "opencode",
                   "bound_topic_key": "stallwatch:oc-prog"}),
            json!({"session_id": "gse_n", "backend_kind": "goose-acp",
                   "backend_session_id": "20261021_41", "bound_topic_key": "stallwatch:_notices"}),
            json!({"session_id": "gse_x", "backend_kind": "goose-acp",
                   "backend_session_id": "20261021_42", "bound_topic_key": "other:thing"}),
            json!({"session_id": "gse_w", "backend_kind": "goose-acp",
                   "backend_session_id": "20261021_43", "bound_topic_key": null}),
        ];
        let p = |l: &str| coordinator_program(&rows, l);
        assert_eq!(p("goose:20261021_40").as_deref(), Some("honk"));
        assert_eq!(p("goose:20261001_3").as_deref(), Some("legacy"));
        assert_eq!(p("ses_coord").as_deref(), Some("oc-prog"));
        // A pigeon id is not an oc-tags key for a goose session.
        assert_eq!(p("gse_a"), None);
        assert_eq!(p("20261021_40"), None);
        assert_eq!(p("goose:20261021_41"), None, "_notices is not a program");
        assert_eq!(p("goose:20261021_42"), None);
        assert_eq!(p("goose:20261021_43"), None);
        assert_eq!(p("goose:nope"), None);
        assert!(reports_bound_topics(&rows));
        assert!(!reports_bound_topics(&[json!({"session_id": "gse_a"})]));
    }

    #[test]
    fn announcement_names_session_and_never_the_token() {
        let reg = Registration {
            goose_session_id: "20261016_1".into(),
            endpoint: "ws://h/acp".into(),
            auth_token: "sekrit".into(),
            cwd: "/w".into(),
            title: None,
            reason: Reason::NewChat,
        };
        let a = announcement(&reg);
        assert!(a.contains("20261016_1") && a.contains("/w"), "{a}");
        assert!(!a.contains("sekrit"), "{a}");
        assert!(!format!("{reg:?}").contains("sekrit"));
    }
}
