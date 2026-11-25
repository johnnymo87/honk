//! `honk launch`: start a goose session headlessly, the goose analogue of
//! `opencode-launch`.
//!
//! In order, each step a precondition of the next:
//!
//! 1. Preflight, before anything exists: pigeon reachable with our token, and
//!    the tag decided: an explicit `--tag` wins; else
//!    the launching session's (`$OPENCODE_SESSION_ID`, or `goose:` +
//!    `$AGENT_SESSION_ID`). A launcher that is a stall-watch coordinator (pigeon
//!    `bound_topic_key = 'stallwatch:<tag>'`) gives its program `<tag>`, never
//!    its own spend tag ([`COORDINATOR_OWN_TAG`]).
//! 2. `session/new` through honkd, with swarm on (honkd `--swarm-default`, or an
//!    explicit `_honk/mcp/enable` when honkd did not do it). With honkd
//!    `--project-mcp`, the project's own MCP servers are enabled there too, and
//!    one stderr line says which ([`project_mcp_line`]); that never fails the launch.
//! 3. `--title`: rename the session before its first prompt, so goose's
//!    auto-namer leaves it alone (the route pigeon#209 uses).
//! 4. `session-tags set -- <tag> goose:<id>`.
//! 5. Register it with pigeon (`POST /goose/sessions`: `notify` on, honkd as the
//!    push-capable `backend_endpoint`) and announce it, which creates its
//!    pigeon chat channel.
//! 6. `session/prompt`, wait until honkd shows the turn started, and disconnect.
//!    A viewer leaving does not cancel a turn in honkd.
//!
//! A failure stops the launch there, except a tag failure, which is reported
//! after the prompt (the caller can repair a tag, but not resume a launch).
//! Once the session exists a failure still reports its ids
//! ([`LaunchError::Partial`]); the line's `prompt=` field says whether it was
//! prompted.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

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
/// Exit code: the session exists but a step failed; the line is printed and
/// its `gse=`, `tag=` and `prompt=` fields say how far the launch got.
pub const EXIT_PARTIAL: i32 = 3;
/// Exit code: as [`EXIT_PARTIAL`], but the prompt was sent and its outcome is
/// unknown (`prompt=unknown`): the turn may be running. Do not relaunch.
pub const EXIT_PROMPT_UNKNOWN: i32 = 4;

/// Whether the session was prompted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptState {
    /// Not prompted (never sent, or honkd refused it).
    #[default]
    No,
    /// goose started the turn.
    Ok,
    /// Sent, but the turn was not seen to start.
    Unknown,
}

impl PromptState {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptState::No => "no",
            PromptState::Ok => "ok",
            PromptState::Unknown => "unknown",
        }
    }
}

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
    pub connect: Duration,
    pub session_new: Duration,
    pub swarm_enable: Duration,
    pub rename: Duration,
    pub model: Duration,
    pub default_model: Duration,
    pub effort: Duration,
    pub prompt_confirm: Duration,
    pub which: Duration,
    pub set: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            session_new: Duration::from_secs(30),
            swarm_enable: Duration::from_secs(30),
            rename: Duration::from_secs(10),
            model: Duration::from_secs(10),
            default_model: Duration::from_secs(3),
            effort: Duration::from_secs(10),
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
    pub model: Option<String>,
    pub default_model_cmd: Option<PathBuf>,
    pub effort: Option<String>,
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
    pub attach: Option<AttachSettings>,
    pub attach_skip_reason: Option<String>,
}

/// Default path where auto-attach output is logged.
pub const DEFAULT_AUTO_ATTACH_LOG: &str = "/tmp/oc-auto-attach.log";

/// Settings for auto-attaching to a launched goose session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachSettings {
    pub bin: String,
    pub tmux_session: Option<String>,
    pub log_file: PathBuf,
}

impl AttachSettings {
    pub fn new(bin: impl Into<String>, tmux_session: Option<String>) -> Self {
        Self {
            bin: bin.into(),
            tmux_session,
            log_file: PathBuf::from(DEFAULT_AUTO_ATTACH_LOG),
        }
    }
}

impl std::fmt::Debug for LaunchOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchOpts")
            .field("dir", &self.dir)
            .field("prompt", &self.prompt)
            .field("title", &self.title)
            .field("model", &self.model)
            .field("default_model_cmd", &self.default_model_cmd)
            .field("effort", &self.effort)
            .field("tag", &self.tag)
            .field("launcher", &self.launcher)
            .field("tags_bin", &self.tags_bin)
            .field("honk_url", &self.honk_url)
            .field("secret", &"<redacted>")
            .field("pigeon_url", &self.pigeon.url)
            .field("timeouts", &self.timeouts)
            .field("attach", &self.attach)
            .field("attach_skip_reason", &self.attach_skip_reason)
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
    pub prompt: PromptState,
}

impl Launched {
    /// The machine-readable line:
    /// `native=<id> gse=<gse_…|-> tag=<tag|-> prompt=<ok|no|unknown>`.
    /// No field contains whitespace: goose ids are `YYYYMMDD_N`, pigeon ids are
    /// `gse_<uuid>`, and tags are validated by [`octags::parse_tag`].
    pub fn line(&self) -> String {
        format!(
            "native={} gse={} tag={} prompt={}",
            self.native,
            self.gse.as_deref().unwrap_or("-"),
            self.tag.as_deref().unwrap_or("-"),
            self.prompt.as_str()
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
            LaunchError::Partial { launched, .. } if launched.prompt == PromptState::Unknown => {
                EXIT_PROMPT_UNKNOWN
            }
            LaunchError::Partial { .. } => EXIT_PARTIAL,
        }
    }
}

/// Decides the tag before any session exists, so a tag that cannot be decided
/// creates nothing. `rows` is pigeon's `GET /sessions`.
pub async fn resolve_tag(opts: &LaunchOpts, rows: &[Value]) -> anyhow::Result<Option<String>> {
    let launcher = match (&opts.tag, &opts.launcher) {
        (TagRequest::Off, _) => return Ok(None),
        (TagRequest::Explicit(t), _) => return Ok(Some(t.clone())),
        (TagRequest::Inherit, None) => return Ok(None),
        (TagRequest::Inherit, Some(l)) => l,
    };

    if let Some(program) = pigeon::coordinator_program(rows, launcher) {
        return Ok(Some(program));
    }

    match octags::query_launcher_tag(&opts.tags_bin, launcher, opts.timeouts.which).await {
        WhichOutcome::Inherit { tag, .. } => {
            if tag.eq_ignore_ascii_case(COORDINATOR_OWN_TAG) {
                let why = if pigeon::reports_bound_topics(rows) {
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

/// Whether `bin` names an executable: a path to a file, or a name on `$PATH`.
fn bin_exists(bin: &str) -> bool {
    if bin.contains('/') {
        std::path::Path::new(bin).is_file()
    } else {
        octags::on_path(bin)
    }
}

pub const MAX_HOOK_OUTPUT_BYTES: u64 = 8 * 1024;

pub struct HookOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Reads up to `limit` bytes from `reader`, then drains any remaining bytes to `tokio::io::sink()`.
async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    limit: u64,
) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut take = (&mut reader).take(limit);
    take.read_to_end(&mut buf).await?;
    tokio::io::copy(&mut reader, &mut tokio::io::sink()).await?;
    Ok(buf)
}

/// Waits for `child` while boundedly reading `stdout` and `stderr` (up to `limit` bytes each).
pub async fn wait_bounded(
    mut child: tokio::process::Child,
    limit: u64,
) -> std::io::Result<HookOutput> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_fut = async {
        if let Some(stdout) = stdout {
            read_bounded(stdout, limit).await
        } else {
            Ok(Vec::new())
        }
    };
    let stderr_fut = async {
        if let Some(stderr) = stderr {
            read_bounded(stderr, limit).await
        } else {
            Ok(Vec::new())
        }
    };
    let (stdout_res, stderr_res, status_res) = tokio::join!(stdout_fut, stderr_fut, child.wait());
    let stdout = stdout_res?;
    let stderr = stderr_res?;
    let status = status_res?;
    Ok(HookOutput {
        status,
        stdout,
        stderr,
    })
}

fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &s[..boundary]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelSource {
    Explicit,
    Hook,
}

pub async fn resolve_default_model(
    cmd: &Path,
    tag: Option<&str>,
    timeout: Duration,
) -> Option<String> {
    let mut command = tokio::process::Command::new(cmd);
    if let Some(t) = tag {
        command.arg("--tag").arg(t.to_ascii_lowercase());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "honk launch: warning: default-model-cmd {} failed to spawn: {e}; using server default",
                cmd.display()
            );
            return None;
        }
    };

    let out = match tokio::time::timeout(timeout, wait_bounded(child, MAX_HOOK_OUTPUT_BYTES)).await
    {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            eprintln!(
                "honk launch: warning: default-model-cmd {} wait error: {e}; using server default",
                cmd.display()
            );
            return None;
        }
        Err(_) => {
            eprintln!(
                "honk launch: warning: default-model-cmd {} timed out after {timeout:?}; using server default",
                cmd.display()
            );
            return None;
        }
    };

    let stderr_str = String::from_utf8_lossy(&out.stderr);
    let stderr_trimmed = stderr_str.trim();
    let snippet = if stderr_trimmed.is_empty() {
        None
    } else {
        Some(truncate_str(stderr_trimmed, 512))
    };

    if !out.status.success() {
        if let Some(snip) = snippet {
            eprintln!(
                "honk launch: warning: default-model-cmd {} exited with {}: {snip}; using server default",
                cmd.display(),
                out.status
            );
        } else {
            eprintln!(
                "honk launch: warning: default-model-cmd {} exited with {}; using server default",
                cmd.display(),
                out.status
            );
        }
        return None;
    }

    if let Some(snip) = snippet {
        eprintln!("honk launch: default-model-cmd stderr: {snip}");
    }

    let stdout_str = String::from_utf8_lossy(&out.stdout);
    let first_line = stdout_str.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        None
    } else {
        Some(first_line.to_string())
    }
}

/// Everything checkable before a session exists: pigeon reachable and
/// accepting our token, the tag decided, session-tags present.
async fn preflight(opts: &LaunchOpts) -> anyhow::Result<(Option<String>, String)> {
    let rows = pigeon::fetch_sessions(&opts.pigeon).await.context(
        "pigeon preflight failed (is HONK_PIGEON_TOKEN_FILE / --pigeon-token-file set?)",
    )?;
    let tag = resolve_tag(opts, &rows).await?;
    if tag.is_some() && !bin_exists(&opts.tags_bin) {
        bail!(
            "'{}' not found, so the session could not be tagged; set {}",
            opts.tags_bin,
            octags::BIN_ENV
        );
    }
    let endpoint = pigeon::acp_endpoint(&opts.honk_url)?;
    Ok((tag, endpoint))
}

/// Runs the whole launch. On success the session is prompted and detached.
pub async fn run(opts: &LaunchOpts) -> Result<Launched, LaunchError> {
    let (tag, endpoint) = preflight(opts).await.map_err(LaunchError::NotCreated)?;

    let effective_model = if let Some(m) = opts.model.clone() {
        Some((m, ModelSource::Explicit))
    } else if let Some(cmd) = &opts.default_model_cmd {
        let resolved =
            resolve_default_model(cmd, tag.as_deref(), opts.timeouts.default_model).await;
        if let Some(m) = &resolved {
            eprintln!("honk launch: model {m} (default-model-cmd)");
        }
        resolved.map(|m| (m, ModelSource::Hook))
    } else {
        None
    };

    let mut client = match tokio::time::timeout(
        opts.timeouts.connect,
        Client::connect(&opts.honk_url, &opts.secret),
    )
    .await
    {
        Ok(r) => r.map_err(LaunchError::NotCreated)?,
        Err(_) => {
            return Err(LaunchError::NotCreated(anyhow!(
                "connecting to honkd at {} timed out after {:?}",
                opts.honk_url,
                opts.timeouts.connect
            )));
        }
    };
    let NewSession {
        id: native,
        swarm_meta,
        project_mcp,
    } = new_session(&mut client, opts)
        .await
        .map_err(LaunchError::NotCreated)?;

    if let Some(line) = project_mcp.as_ref().and_then(project_mcp_line) {
        eprintln!("{line}");
    }

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

    match effective_model {
        Some((model, ModelSource::Explicit)) => {
            step!(
                check_model_allowed(&mut client, &native, &model, opts.timeouts.model).await,
                "setting the model failed"
            );
            step!(
                set_model(&mut client, &native, &model, opts.timeouts.model).await,
                "setting the model failed"
            );
        }
        Some((model, ModelSource::Hook)) => {
            match check_model_allowed(&mut client, &native, &model, opts.timeouts.model).await {
                Ok(()) => {
                    if let Err(e) =
                        set_model(&mut client, &native, &model, opts.timeouts.model).await
                    {
                        eprintln!(
                            "honk launch: warning: setting default-model-cmd model '{model}' failed ({e:#}); continuing (if timed out, ambiguous: may still apply)"
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "honk launch: warning: default-model-cmd model '{model}' skipped ({e:#}); using server default"
                    );
                }
            }
        }
        None => {}
    }

    if let Some(effort) = &opts.effort {
        step!(
            set_effort(&mut client, &native, effort, opts.timeouts.effort).await,
            "setting the thinking effort failed"
        );
    }

    // A tag failure does not stop the launch: the worker is still worth
    // running, and the caller can repair the tag (sw launch verifies and
    // repairs, as for opencode). It still makes the exit non-zero.
    let mut tag_error = None;
    if let Some(t) = &tag {
        let settings = TagSettings {
            tag: t.clone(),
            bin: opts.tags_bin.clone(),
            timeout: opts.timeouts.set,
        };
        match octags::tag_session(&settings, &native).await {
            Ok(_) => launched.tag = Some(t.to_ascii_lowercase()),
            Err(e) => {
                tag_error = Some(e.context(format!(
                    "tagging failed; apply it with: {} set -- {t} {}",
                    opts.tags_bin,
                    octags::tagged_id(&native)
                )))
            }
        }
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
        "registered with pigeon, but announcing it (which creates its pigeon channel) failed"
    );
    if !announced.announced {
        eprintln!(
            "honk launch: pigeon registered {gse} but did not announce it ({}); its channel appears on its first turn notification",
            announced
                .not_announced_reason
                .as_deref()
                .unwrap_or("no reason given")
        );
    }

    let sent = prompt(
        &mut client,
        &native,
        &opts.prompt,
        opts.timeouts.prompt_confirm,
    )
    .await;
    // Dropping the socket detaches this viewer; honkd keeps the turn running.
    drop(client);
    match sent {
        Ok(()) => launched.prompt = PromptState::Ok,
        Err((state, _)) => launched.prompt = state,
    }

    maybe_auto_attach(opts, &launched);

    if let Err((_, error)) = sent {
        let error = match tag_error {
            Some(t) => error.context(format!("{t:#}; and prompting failed")),
            None => error.context("prompting failed"),
        };
        return Err(LaunchError::Partial { launched, error });
    }
    match tag_error {
        Some(error) => Err(LaunchError::Partial { launched, error }),
        None => Ok(launched),
    }
}

fn spawn_auto_attach(attach: &AttachSettings, native: &str) -> Result<(), std::io::Error> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let out_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&attach.log_file);

    let (stdout, stderr) = match out_file {
        Ok(file) => match file.try_clone() {
            Ok(clone) => (Stdio::from(file), Stdio::from(clone)),
            Err(_) => (Stdio::from(file), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    };

    let mut cmd = std::process::Command::new(&attach.bin);
    if let Some(session) = &attach.tmux_session {
        cmd.arg("--tmux-session").arg(session);
    }
    cmd.arg(format!("goose:{native}"));
    cmd.stdin(Stdio::null());
    cmd.stdout(stdout);
    cmd.stderr(stderr);
    cmd.process_group(0);
    cmd.env_remove(crate::octags::OPENCODE_SESSION_ID_ENV);
    cmd.env_remove(crate::octags::AGENT_SESSION_ID_ENV);

    cmd.spawn().map(|_| ())
}

fn auto_attach_step(
    opts: &LaunchOpts,
    launched: &Launched,
    spawn_fn: impl FnOnce(&AttachSettings, &str) -> Result<(), std::io::Error>,
) -> Option<String> {
    if let Some(attach) = &opts.attach {
        if launched.prompt != PromptState::No {
            match spawn_fn(attach, &launched.native) {
                Ok(()) => Some("honk launch: auto-attach: started".to_string()),
                Err(e) => Some(format!(
                    "honk launch: auto-attach: skipped (spawn failed: {e})"
                )),
            }
        } else {
            Some("honk launch: auto-attach: skipped (prompt refused)".to_string())
        }
    } else {
        opts.attach_skip_reason
            .as_ref()
            .map(|why| format!("honk launch: auto-attach: skipped ({why})"))
    }
}

fn maybe_auto_attach(opts: &LaunchOpts, launched: &Launched) {
    if let Some(line) = auto_attach_step(opts, launched, spawn_auto_attach) {
        eprintln!("{line}");
    }
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
        project_mcp: result.pointer("/_meta/honk/projectMcp").cloned(),
    })
}

struct NewSession {
    id: String,
    /// `_meta.honk.swarm` of the `session/new` reply, if honkd sent one.
    swarm_meta: Option<Value>,
    /// `_meta.honk.projectMcp` of the `session/new` reply, if honkd sent one.
    project_mcp: Option<Value>,
}

/// One line for the launcher's stderr about the project's MCP servers, from
/// honkd's `_meta.honk.projectMcp`: what is on, what failed and why, what was
/// left alone and why. `None` when there is nothing to say. Never fatal.
fn project_mcp_line(meta: &Value) -> Option<String> {
    let list = |key: &str| -> Vec<&Value> {
        meta.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    };
    let text = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };

    let mut on = Vec::new();
    let mut parts = Vec::new();
    for s in list("servers") {
        let name = text(s, "name");
        if s.get("ok").and_then(Value::as_bool) == Some(true) {
            on.push(name);
        } else {
            let pending = s.get("pending").and_then(Value::as_bool) == Some(true);
            let what = if pending { "pending" } else { "failed" };
            parts.push(format!("{what} {name}: {}", text(s, "error")));
        }
    }
    if !on.is_empty() {
        parts.insert(0, format!("enabled {}", on.join(", ")));
    }
    for s in list("skipped") {
        parts.push(format!(
            "skipped {}: {}",
            text(s, "name"),
            text(s, "reason")
        ));
    }
    for p in list("problems") {
        parts.push(format!("problem {}", p.as_str().unwrap_or("?")));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("honk launch: project MCP: {}", parts.join("; ")))
    }
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

async fn check_model_allowed(
    client: &mut Client,
    sid: &str,
    model: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let frame = match call_with_timeout(
        client,
        "_honk/model/list",
        json!({ "sessionId": sid }),
        timeout,
    )
    .await
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("honk launch: warning: could not verify model '{model}' against honkd ({e})");
            return Ok(());
        }
    };

    if let Some(err) = frame.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        if code == -32601 {
            return Ok(());
        }
        eprintln!("honk launch: warning: could not verify model '{model}' against honkd ({msg})");
        return Ok(());
    }

    if let Some(models) = frame.pointer("/result/models").and_then(Value::as_array)
        && !models.is_empty()
    {
        let found = models
            .iter()
            .any(|m| m.get("id").and_then(Value::as_str) == Some(model));
        if !found {
            bail!("{model} is not in honkd's model list (configured via honkd --models-file)");
        }
    }

    Ok(())
}

async fn set_model(
    client: &mut Client,
    sid: &str,
    model: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let frame = call_with_timeout(
        client,
        "session/set_config_option",
        json!({
            "sessionId": sid,
            "configId": "model",
            "value": model,
        }),
        timeout,
    )
    .await?;
    if let Some(e) = rpc_error(&frame) {
        bail!("session/set_config_option: {e}");
    }
    Ok(())
}

async fn set_effort(
    client: &mut Client,
    sid: &str,
    effort: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let frame = call_with_timeout(
        client,
        "session/set_config_option",
        json!({
            "sessionId": sid,
            "configId": "thinking_effort",
            "value": effort,
        }),
        timeout,
    )
    .await?;
    if let Some(e) = rpc_error(&frame) {
        bail!("session/set_config_option: {e}");
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
) -> Result<(), (PromptState, anyhow::Error)> {
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
        .await
        .map_err(|e| (PromptState::No, e))?;
    let unknown = |what: String| {
        (
            PromptState::Unknown,
            anyhow!(
                "{what}; the prompt was sent and the turn may still run (client_req_id {crid}; check with: honk tail {sid}). Do not relaunch the same task"
            ),
        )
    };
    let wait = async {
        loop {
            let frame = match client.next_frame().await {
                Ok(Some(f)) => f,
                Ok(None) => {
                    return Err(unknown(
                        "honkd closed the connection before the turn started".into(),
                    ));
                }
                Err(e) => {
                    return Err(unknown(format!(
                        "lost honkd before the turn started: {e:#}"
                    )));
                }
            };
            if frame.get("id") == Some(&json!(id)) && frame.get("method").is_none() {
                if let Some(e) = rpc_error(&frame) {
                    return Err((PromptState::No, anyhow!("session/prompt refused: {e}")));
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
        Err(_) => Err(unknown(format!(
            "goose had not started the turn after {timeout:?}"
        ))),
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

/// Whether an env var value is truthy according to the allowlist (`1`, `true`,
/// `TRUE`, `yes`, `YES`, `on`, `ON`). As in `opencode-launch`, `=0` does not
/// disable.
pub fn is_truthy(s: &str) -> bool {
    matches!(s, "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
}

/// Resolves auto-attach settings from CLI args and environment.
///
/// Returns `(Some(settings), None)` if auto-attach is enabled, or
/// `(None, Some(reason))` explaining why it was skipped.
pub fn resolve_attach_from(
    no_attach: bool,
    tmux_session: Option<String>,
    env_get: impl Fn(&str) -> Option<String>,
    on_path: impl Fn(&str) -> bool,
) -> (Option<AttachSettings>, Option<String>) {
    if no_attach {
        return (None, Some("--no-attach".to_string()));
    }
    if let Some(val) = env_get("HONK_LAUNCH_NO_ATTACH")
        && is_truthy(&val)
    {
        return (None, Some(format!("HONK_LAUNCH_NO_ATTACH={val}")));
    }
    let bin = if let Some(b) = env_get("HONK_AUTO_ATTACH_BIN").filter(|b| !b.is_empty()) {
        Some(b)
    } else if on_path("oc-auto-attach") {
        Some("oc-auto-attach".to_string())
    } else {
        None
    };

    match bin {
        Some(bin) => {
            let log_file = env_get("HONK_AUTO_ATTACH_LOG")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_AUTO_ATTACH_LOG));
            (
                Some(AttachSettings {
                    bin,
                    tmux_session,
                    log_file,
                }),
                None,
            )
        }
        None => (None, Some("oc-auto-attach not found".to_string())),
    }
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
            prompt: PromptState::Ok,
        };
        assert_eq!(l.line(), "native=20261104_7 gse=gse_abc tag=honk prompt=ok");
        let l = Launched {
            native: "20261104_7".into(),
            ..Default::default()
        };
        assert_eq!(l.line(), "native=20261104_7 gse=- tag=- prompt=no");
    }

    #[test]
    fn project_mcp_line_reports_enabled_failed_and_skipped() {
        let meta = json!({
            "servers": [
                {"name": "tec", "source": "project:opencode.json", "ok": true},
                {"name": "dev", "source": "project:opencode.json", "ok": false,
                 "error": "not reachable at http://127.0.0.1:9/mcp: Connection refused"},
                {"name": "slow", "source": "project:opencode.json", "ok": false,
                 "pending": true, "error": "pending; may still land; run /mcp"}
            ],
            "skipped": [
                {"name": "far", "reason": "not a loopback url; enable with /mcp enable far!"}
            ],
            "problems": ["opencode.json: expected value at line 1"]
        });
        assert_eq!(
            project_mcp_line(&meta).unwrap(),
            "honk launch: project MCP: enabled tec; \
             failed dev: not reachable at http://127.0.0.1:9/mcp: Connection refused; \
             pending slow: pending; may still land; run /mcp; \
             skipped far: not a loopback url; enable with /mcp enable far!; \
             problem opencode.json: expected value at line 1"
        );
    }

    #[test]
    fn project_mcp_line_is_silent_when_there_is_nothing_to_say() {
        assert_eq!(project_mcp_line(&json!({})), None);
        assert_eq!(
            project_mcp_line(&json!({"servers": [], "skipped": [], "problems": []})),
            None
        );
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

    #[test]
    fn truthy_allowlist() {
        for truthy in ["1", "true", "TRUE", "yes", "YES", "on", "ON"] {
            assert!(is_truthy(truthy), "{truthy} should be truthy");
        }
        for not_truthy in [
            "0", "false", "no", "off", "FALSE", "NO", "OFF", "random", "",
        ] {
            assert!(!is_truthy(not_truthy), "{not_truthy} should not be truthy");
        }
    }

    #[test]
    fn resolve_attach_prefers_cli_flag() {
        let env = |k: &str| match k {
            "HONK_LAUNCH_NO_ATTACH" => Some("1".into()),
            "HONK_AUTO_ATTACH_BIN" => Some("/bin/auto".into()),
            _ => None,
        };
        let (attach, skip) = resolve_attach_from(true, Some("tmux".into()), env, |_| true);
        assert_eq!(attach, None);
        assert_eq!(skip.as_deref(), Some("--no-attach"));
    }

    #[test]
    fn resolve_attach_honors_env_allowlist() {
        for v in ["1", "true", "TRUE", "yes", "YES", "on", "ON"] {
            let env = |k: &str| (k == "HONK_LAUNCH_NO_ATTACH").then(|| v.to_string());
            let (attach, skip) = resolve_attach_from(false, None, env, |_| true);
            assert_eq!(attach, None);
            assert_eq!(
                skip.as_deref(),
                Some(format!("HONK_LAUNCH_NO_ATTACH={v}").as_str())
            );
        }

        // =0 does not disable
        let env = |k: &str| (k == "HONK_LAUNCH_NO_ATTACH").then(|| "0".to_string());
        let (attach, skip) = resolve_attach_from(false, None, env, |b| b == "oc-auto-attach");
        assert_eq!(
            attach,
            Some(AttachSettings {
                bin: "oc-auto-attach".into(),
                tmux_session: None,
                log_file: PathBuf::from(DEFAULT_AUTO_ATTACH_LOG),
            })
        );
        assert_eq!(skip, None);
    }

    #[test]
    fn resolve_attach_bin_precedence_and_options() {
        // Custom bin wins over PATH
        let env = |k: &str| match k {
            "HONK_AUTO_ATTACH_BIN" => Some("/opt/my-attach".into()),
            "HONK_AUTO_ATTACH_LOG" => Some("/tmp/test.log".into()),
            _ => None,
        };
        let (attach, skip) = resolve_attach_from(false, Some("sess".into()), env, |_| true);
        assert_eq!(
            attach,
            Some(AttachSettings {
                bin: "/opt/my-attach".into(),
                tmux_session: Some("sess".into()),
                log_file: PathBuf::from("/tmp/test.log"),
            })
        );
        assert_eq!(skip, None);

        // Missing from PATH and no env bin -> skipped
        let (attach, skip) = resolve_attach_from(false, None, |_| None, |_| false);
        assert_eq!(attach, None);
        assert_eq!(skip.as_deref(), Some("oc-auto-attach not found"));
    }

    #[test]
    fn test_auto_attach_step_lines() {
        let dummy_opts = |attach, attach_skip_reason| LaunchOpts {
            dir: PathBuf::from("/tmp"),
            prompt: "p".into(),
            title: None,
            model: None,
            default_model_cmd: None,
            effort: None,
            tag: TagRequest::Off,
            launcher: None,
            tags_bin: "tags".into(),
            honk_url: "http://1".into(),
            secret: "s".into(),
            pigeon: PigeonConfig {
                url: "http://2".into(),
                token: None,
            },
            timeouts: Timeouts::default(),
            attach,
            attach_skip_reason,
        };
        let mut launched = Launched {
            native: "20261104_1".into(),
            prompt: PromptState::Ok,
            ..Default::default()
        };
        let attach = AttachSettings::new("my-attach", None);

        // 1. Success -> started
        let line = auto_attach_step(
            &dummy_opts(Some(attach.clone()), None),
            &launched,
            |_, _| Ok(()),
        );
        assert_eq!(line.as_deref(), Some("honk launch: auto-attach: started"));

        // 2. Spawn error -> skipped (spawn failed: ...)
        let line = auto_attach_step(
            &dummy_opts(Some(attach.clone()), None),
            &launched,
            |_, _| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no file")),
        );
        assert_eq!(
            line.as_deref(),
            Some("honk launch: auto-attach: skipped (spawn failed: no file)")
        );

        // 3. Prompt refused (PromptState::No) -> skipped (prompt refused)
        launched.prompt = PromptState::No;
        let line = auto_attach_step(
            &dummy_opts(Some(attach.clone()), None),
            &launched,
            |_, _| panic!("spawn_fn should not be called"),
        );
        assert_eq!(
            line.as_deref(),
            Some("honk launch: auto-attach: skipped (prompt refused)")
        );

        // 4. Attach None with skip reason -> skipped (<why>)
        let line = auto_attach_step(
            &dummy_opts(None, Some("--no-attach".into())),
            &launched,
            |_, _| panic!("spawn_fn should not be called"),
        );
        assert_eq!(
            line.as_deref(),
            Some("honk launch: auto-attach: skipped (--no-attach)")
        );

        // 5. Attach None without skip reason -> None
        let line = auto_attach_step(&dummy_opts(None, None), &launched, |_, _| {
            panic!("spawn_fn should not be called")
        });
        assert_eq!(line, None);
    }

    fn make_temp_script(dir: &std::path::Path, name: &str, content: &str) -> PathBuf {
        use std::io::Write;
        let path = dir.join(name);
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(&path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        path
    }

    #[test]
    fn test_truncate_str() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello");
        assert_eq!(truncate_str("🦀🦀🦀", 4), "🦀");
        assert_eq!(truncate_str("🦀🦀🦀", 5), "🦀");
        assert_eq!(truncate_str("🦀🦀🦀", 8), "🦀🦀");
    }

    #[tokio::test]
    async fn test_wait_bounded_large_output() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("head -c 65536 /dev/zero; head -c 65536 /dev/zero >&2")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().expect("spawn sh");
        let output = wait_bounded(child, MAX_HOOK_OUTPUT_BYTES)
            .await
            .expect("wait bounded");

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), MAX_HOOK_OUTPUT_BYTES as usize);
        assert_eq!(output.stderr.len(), MAX_HOOK_OUTPUT_BYTES as usize);
    }

    #[tokio::test]
    async fn test_resolve_default_model_stdout_trimming() {
        let tmp = tempfile::tempdir().unwrap();
        let script = make_temp_script(
            tmp.path(),
            "trim.sh",
            "#!/bin/sh\necho '  custom-model  '\n",
        );
        let res = resolve_default_model(&script, None, Duration::from_secs(3)).await;
        assert_eq!(res.as_deref(), Some("custom-model"));
    }

    #[tokio::test]
    async fn test_resolve_default_model_multiline_takes_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let script = make_temp_script(
            tmp.path(),
            "multi.sh",
            "#!/bin/sh\nprintf 'first-line\\nsecond-line\\n'\n",
        );
        let res = resolve_default_model(&script, None, Duration::from_secs(3)).await;
        assert_eq!(res.as_deref(), Some("first-line"));
    }

    #[tokio::test]
    async fn test_resolve_default_model_stderr_relay() {
        let tmp = tempfile::tempdir().unwrap();
        let script = make_temp_script(
            tmp.path(),
            "err.sh",
            "#!/bin/sh\necho 'warning message' >&2\necho 'good-model'\n",
        );
        let res = resolve_default_model(&script, None, Duration::from_secs(3)).await;
        assert_eq!(res.as_deref(), Some("good-model"));
    }

    #[tokio::test]
    async fn test_resolve_default_model_non_zero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let script = make_temp_script(
            tmp.path(),
            "fail.sh",
            "#!/bin/sh\necho 'something broke' >&2\nexit 1\n",
        );
        let res = resolve_default_model(&script, None, Duration::from_secs(3)).await;
        assert_eq!(res, None);
    }

    #[tokio::test]
    async fn test_resolve_default_model_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let script = make_temp_script(tmp.path(), "slow.sh", "#!/bin/sh\nsleep 10\necho 'model'\n");
        let res = resolve_default_model(&script, None, Duration::from_millis(50)).await;
        assert_eq!(res, None);
    }

    #[tokio::test]
    async fn test_resolve_default_model_missing_binary() {
        let res = resolve_default_model(
            Path::new("/nonexistent/binary/path"),
            None,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(res, None);
    }
}
