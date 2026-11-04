//! `honk launch`: start a goose session headlessly, the goose analogue of
//! `opencode-launch`.
//!
//! In order, each step a precondition of the next:
//!
//! 1. Decide the tag, before anything exists: an explicit `--tag` wins; else
//!    the launching session's (`$OPENCODE_SESSION_ID`, or `goose:` +
//!    `$AGENT_SESSION_ID`). A launcher that is a stall-watch coordinator (pigeon
//!    `bound_topic_key = 'stallwatch:<tag>'`) gives its program `<tag>`, never
//!    its own spend tag ([`COORDINATOR_OWN_TAG`]).
//! 2. `session/new` through honkd, with swarm on (honkd `--swarm-default`, or an
//!    explicit `_honk/mcp/enable` when honkd did not do it).
//! 3. `--title`: rename the session before its first prompt, so goose's
//!    auto-namer leaves it alone (the route pigeon#209 uses).
//! 4. `session-tags set -- <tag> goose:<id>`.
//! 5. Register it with pigeon (`POST /goose/sessions`: `notify` on, honkd as the
//!    push-capable `backend_endpoint`) and announce it, which creates its
//!    Telegram topic.
//! 6. `session/prompt`, wait until honkd shows the turn started, and disconnect.
//!    A viewer leaving does not cancel a turn in honkd.
//!
//! Any failure stops the launch there. Once the session exists, a failure
//! still reports its ids ([`LaunchError::Partial`]) and the session is left
//! unprompted (unless the failure is in confirming the prompt itself).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};

use crate::client::Client;
use crate::octags::{self, TagSettings, WhichOutcome};
use crate::pigeon::{self, PigeonConfig, Reason, Registration};

/// The spend tag stall-watch coordinators carry themselves. A worker must never
/// inherit it: its spend belongs to the program it works on.
pub const COORDINATOR_OWN_TAG: &str = "stall-watcher-runs";

/// Exit code: launched, tagged as asked, registered, and prompted.
pub const EXIT_OK: i32 = 0;
/// Exit code: failed before any session was created.
pub const EXIT_NOT_CREATED: i32 = 1;
/// Exit code: the session exists but a later step failed (the ids line is
/// still printed).
pub const EXIT_PARTIAL: i32 = 3;

/// What `--tag` asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TagRequest {
    /// No `--tag`: inherit the launching session's tag, if there is one.
    Inherit,
    /// `--tag <tag>`.
    Explicit(String),
    /// `--tag auto`: no tag, no inheritance.
    Off,
}

#[derive(Clone, Debug)]
pub struct Timeouts {
    pub session_new: Duration,
    pub swarm_enable: Duration,
    pub rename: Duration,
    pub prompt_confirm: Duration,
    pub which: Duration,
    pub set: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            session_new: Duration::from_secs(30),
            swarm_enable: Duration::from_secs(30),
            rename: Duration::from_secs(10),
            prompt_confirm: Duration::from_secs(60),
            which: octags::DEFAULT_WHICH_TIMEOUT,
            set: octags::DEFAULT_TIMEOUT,
        }
    }
}

#[derive(Clone)]
pub struct LaunchOpts {
    /// Canonical working directory.
    pub dir: PathBuf,
    pub prompt: String,
    pub title: Option<String>,
    pub tag: TagRequest,
    /// The launching session as an oc-tags key (`ses_…` or `goose:<id>`).
    pub launcher: Option<String>,
    /// The session-tags executable.
    pub tags_bin: String,
    /// honkd URL.
    pub honk_url: String,
    pub secret: String,
    pub pigeon: PigeonConfig,
    pub timeouts: Timeouts,
}

impl std::fmt::Debug for LaunchOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchOpts")
            .field("dir", &self.dir)
            .field("prompt", &self.prompt)
            .field("title", &self.title)
            .field("tag", &self.tag)
            .field("launcher", &self.launcher)
            .field("tags_bin", &self.tags_bin)
            .field("honk_url", &self.honk_url)
            .field("secret", &"<redacted>")
            .field("pigeon_url", &self.pigeon.url)
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

/// What a launch produced, as far as it got.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Launched {
    /// goose's id for the session, e.g. `20261104_7`.
    pub native: String,
    /// pigeon's id for it, once registered.
    pub gse: Option<String>,
    /// The tag applied, as session-tags stores it (lowercased).
    pub tag: Option<String>,
}

impl Launched {
    /// The machine-readable line: `native=<id> gse=<gse_…|-> tag=<tag|->`.
    /// No field contains whitespace: goose ids are `YYYYMMDD_N`, pigeon ids are
    /// `gse_<uuid>`, and tags are validated by [`octags::parse_tag`].
    pub fn line(&self) -> String {
        format!(
            "native={} gse={} tag={}",
            self.native,
            self.gse.as_deref().unwrap_or("-"),
            self.tag.as_deref().unwrap_or("-")
        )
    }
}

#[derive(Debug)]
pub enum LaunchError {
    /// Nothing was created.
    NotCreated(anyhow::Error),
    /// The session exists; `launched` says how far the launch got.
    Partial {
        launched: Launched,
        error: anyhow::Error,
    },
}

impl LaunchError {
    pub fn exit_code(&self) -> i32 {
        match self {
            LaunchError::NotCreated(_) => EXIT_NOT_CREATED,
            LaunchError::Partial { .. } => EXIT_PARTIAL,
        }
    }
}

/// Decides the tag before any session exists, so a tag that cannot be decided
/// creates nothing.
pub async fn resolve_tag(opts: &LaunchOpts) -> anyhow::Result<Option<String>> {
    let launcher = match (&opts.tag, &opts.launcher) {
        (TagRequest::Off, _) => return Ok(None),
        (TagRequest::Explicit(t), _) => return Ok(Some(t.clone())),
        (TagRequest::Inherit, None) => return Ok(None),
        (TagRequest::Inherit, Some(l)) => l,
    };

    let rows = pigeon::fetch_sessions(&opts.pigeon)
        .await
        .with_context(|| {
            format!("cannot check whether the launching session {launcher} is a stall-watch coordinator; pass --tag")
        })?;
    if let Some(program) = pigeon::coordinator_program(&rows, launcher) {
        return Ok(Some(program));
    }

    match octags::query_launcher_tag(&opts.tags_bin, launcher, opts.timeouts.which).await {
        WhichOutcome::Inherit { tag, .. } => {
            if tag.eq_ignore_ascii_case(COORDINATOR_OWN_TAG) {
                let why = if pigeon::reports_bound_topics(&rows) {
                    "but pigeon has no program topic bound to it"
                } else {
                    "and this pigeon does not report bound topics, so its program is unknown"
                };
                bail!(
                    "the launching session {launcher} is tagged '{COORDINATOR_OWN_TAG}' (a stall-watch coordinator's own tag) {why}; pass --tag <program>"
                );
            }
            Ok(Some(tag))
        }
        WhichOutcome::Skip => Ok(None),
        WhichOutcome::NotFound(bin) => Err(anyhow!(
            "cannot inherit the tag of {launcher}: '{bin}' not found; pass --tag, or --tag auto for none"
        )),
        WhichOutcome::Failure(reason) => Err(anyhow!(
            "cannot inherit the tag of {launcher}: {reason}; pass --tag, or --tag auto for none"
        )),
    }
}

/// Runs the whole launch. On success the session is prompted and detached.
pub async fn run(opts: &LaunchOpts) -> Result<Launched, LaunchError> {
    let tag = resolve_tag(opts).await.map_err(LaunchError::NotCreated)?;
    let endpoint = pigeon::acp_endpoint(&opts.honk_url).map_err(LaunchError::NotCreated)?;

    let mut client = Client::connect(&opts.honk_url, &opts.secret)
        .await
        .map_err(LaunchError::NotCreated)?;
    let NewSession {
        id: native,
        swarm_meta,
    } = new_session(&mut client, opts)
        .await
        .map_err(LaunchError::NotCreated)?;

    let mut launched = Launched {
        native: native.clone(),
        ..Default::default()
    };
    macro_rules! step {
        ($e:expr, $what:expr) => {
            match $e {
                Ok(v) => v,
                Err(error) => {
                    return Err(LaunchError::Partial {
                        launched,
                        error: error.context($what),
                    });
                }
            }
        };
    }

    step!(
        ensure_swarm(&mut client, &native, swarm_meta, opts.timeouts.swarm_enable).await,
        "swarm tools are not on"
    );

    if let Some(title) = &opts.title {
        step!(
            rename(&mut client, &native, title, opts.timeouts.rename).await,
            "setting the title failed"
        );
    }

    if let Some(t) = &tag {
        let settings = TagSettings {
            tag: t.clone(),
            bin: opts.tags_bin.clone(),
            timeout: opts.timeouts.set,
        };
        step!(
            octags::tag_session(&settings, &native).await,
            format!(
                "tagging failed; apply it with: {} set -- {t} {}",
                opts.tags_bin,
                octags::tagged_id(&native)
            )
        );
        launched.tag = Some(t.to_ascii_lowercase());
    }

    let reg = Registration {
        goose_session_id: native.clone(),
        endpoint,
        auth_token: opts.secret.clone(),
        cwd: opts.dir.to_string_lossy().to_string(),
        title: opts.title.clone(),
        reason: Reason::Launch,
    };
    let gse = step!(
        pigeon::register_row(&opts.pigeon, &reg).await,
        "registering with pigeon failed"
    );
    launched.gse = Some(gse.clone());
    let announced = step!(
        pigeon::announce(&opts.pigeon, &gse, &reg).await,
        "registered with pigeon, but announcing it (which creates its Telegram topic) failed"
    );
    if !announced.announced {
        eprintln!(
            "honk launch: pigeon registered {gse} but did not announce it ({}); its topic appears on its first turn notification",
            announced
                .not_announced_reason
                .as_deref()
                .unwrap_or("no reason given")
        );
    }

    step!(
        prompt(
            &mut client,
            &native,
            &opts.prompt,
            opts.timeouts.prompt_confirm
        )
        .await,
        "prompting failed"
    );
    // Dropping the socket detaches this viewer; honkd keeps the turn running.
    drop(client);
    Ok(launched)
}

async fn call_with_timeout(
    client: &mut Client,
    method: &str,
    params: Value,
    timeout: Duration,
) -> anyhow::Result<Value> {
    match tokio::time::timeout(timeout, client.call(method, params)).await {
        Ok(r) => r,
        Err(_) => bail!("{method} timed out after {timeout:?}"),
    }
}

fn rpc_error(frame: &Value) -> Option<String> {
    let e = frame.get("error")?;
    Some(
        e.get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| e.to_string()),
    )
}

async fn new_session(client: &mut Client, opts: &LaunchOpts) -> anyhow::Result<NewSession> {
    let params = json!({
        "cwd": opts.dir.to_string_lossy(),
        "mcpServers": [],
    });
    let maybe = "; a session may have been created (see `honk list`)";
    let frame = call_with_timeout(client, "session/new", params, opts.timeouts.session_new)
        .await
        .map_err(|e| anyhow!("session/new failed: {e:#}{maybe}"))?;
    if let Some(e) = rpc_error(&frame) {
        bail!("session/new failed: {e}");
    }
    let result = frame.get("result").cloned().unwrap_or(Value::Null);
    let Some(sid) = result.get("sessionId").and_then(Value::as_str) else {
        bail!("session/new returned no sessionId{maybe}");
    };
    Ok(NewSession {
        id: sid.to_string(),
        swarm_meta: result.pointer("/_meta/honk/swarm").cloned(),
    })
}

struct NewSession {
    id: String,
    /// `_meta.honk.swarm` of the `session/new` reply, if honkd sent one.
    swarm_meta: Option<Value>,
}

/// honkd `--swarm-default` enables swarm before replying to `session/new` and
/// says so in `_meta.honk.swarm.enabled`. Anything else (honkd without the
/// flag, a pending or failed enable) gets one explicit `_honk/mcp/enable`.
async fn ensure_swarm(
    client: &mut Client,
    sid: &str,
    meta: Option<Value>,
    timeout: Duration,
) -> anyhow::Result<()> {
    if meta
        .as_ref()
        .and_then(|m| m.get("enabled"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Ok(());
    }
    let first = meta
        .as_ref()
        .and_then(|m| m.pointer("/error/message"))
        .and_then(Value::as_str)
        .map(|m| format!(" (honkd at session/new: {m})"))
        .unwrap_or_default();
    let frame = call_with_timeout(
        client,
        "_honk/mcp/enable",
        json!({"sessionId": sid, "names": ["swarm"]}),
        timeout,
    )
    .await?;
    if let Some(e) = rpc_error(&frame) {
        bail!("_honk/mcp/enable swarm: {e}{first}");
    }
    let result = frame.get("result").cloned().unwrap_or(Value::Null);
    let swarm = result
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|r| r.get("name").and_then(Value::as_str) == Some("swarm"));
    match swarm {
        Some(r) if r.get("ok").and_then(Value::as_bool) == Some(true) => Ok(()),
        Some(r) => bail!(
            "_honk/mcp/enable swarm: {}{first}",
            r.get("error")
                .and_then(Value::as_str)
                .unwrap_or("not enabled")
        ),
        None => bail!("_honk/mcp/enable swarm: no result for swarm: {result}{first}"),
    }
}

async fn rename(
    client: &mut Client,
    sid: &str,
    title: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let frame = call_with_timeout(
        client,
        "_goose/unstable/session/rename",
        json!({"sessionId": sid, "title": title}),
        timeout,
    )
    .await?;
    if let Some(e) = rpc_error(&frame) {
        bail!("_goose/unstable/session/rename: {e}");
    }
    Ok(())
}

fn client_req_id(sid: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("honk-launch-{sid}-{nanos:x}")
}

/// Sends the prompt and waits for proof that goose started the turn: honkd's
/// synthesized `user_message_chunk` carrying our `client_req_id`. The
/// `session/prompt` reply itself only comes when the turn ends, so it counts
/// too (a very short turn), and an error reply is a refusal.
async fn prompt(
    client: &mut Client,
    sid: &str,
    text: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let crid = client_req_id(sid);
    let id = client
        .request(
            "session/prompt",
            json!({
                "sessionId": sid,
                "prompt": [{"type": "text", "text": text}],
                "_meta": {"honk": {"client_req_id": crid}},
            }),
        )
        .await?;
    let wait = async {
        loop {
            let Some(frame) = client.next_frame().await? else {
                bail!("honkd closed the connection before the turn started");
            };
            if frame.get("id") == Some(&json!(id)) {
                if let Some(e) = rpc_error(&frame) {
                    bail!("session/prompt refused: {e}");
                }
                return Ok(());
            }
            if is_own_turn_start(&frame, sid, &crid) {
                return Ok(());
            }
        }
    };
    match tokio::time::timeout(timeout, wait).await {
        Ok(r) => r,
        Err(_) => bail!(
            "the prompt was sent but goose had not started the turn after {timeout:?}; it may still start (check with: honk tail {sid})"
        ),
    }
}

fn is_own_turn_start(frame: &Value, sid: &str, crid: &str) -> bool {
    frame.get("method").and_then(Value::as_str) == Some("session/update")
        && frame.pointer("/params/sessionId").and_then(Value::as_str) == Some(sid)
        && frame
            .pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("user_message_chunk")
        && frame
            .pointer("/params/update/_meta/honk/source/client_req_id")
            .and_then(Value::as_str)
            == Some(crid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_format() {
        let l = Launched {
            native: "20261104_7".into(),
            gse: Some("gse_abc".into()),
            tag: Some("honk".into()),
        };
        assert_eq!(l.line(), "native=20261104_7 gse=gse_abc tag=honk");
        let l = Launched {
            native: "20261104_7".into(),
            ..Default::default()
        };
        assert_eq!(l.line(), "native=20261104_7 gse=- tag=-");
    }

    #[test]
    fn turn_start_matches_only_our_echo() {
        let f = |sid: &str, kind: &str, crid: &str| {
            json!({"method": "session/update", "params": {"sessionId": sid, "update": {
                "sessionUpdate": kind, "_meta": {"honk": {"source": {"client_req_id": crid}}}}}})
        };
        assert!(is_own_turn_start(
            &f("s", "user_message_chunk", "c"),
            "s",
            "c"
        ));
        assert!(!is_own_turn_start(
            &f("s", "user_message_chunk", "other"),
            "s",
            "c"
        ));
        assert!(!is_own_turn_start(
            &f("t", "user_message_chunk", "c"),
            "s",
            "c"
        ));
        assert!(!is_own_turn_start(
            &f("s", "agent_message_chunk", "c"),
            "s",
            "c"
        ));
    }
}
