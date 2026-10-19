use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use crate::chat::core::ChatCore;
use crate::chat::types::{ChatOpts, Effect, IdAlloc, Input, Notice, Ownership, RunState};
use crate::conn::Conn;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpLaunchConfig {
    pub names: Vec<String>,
    pub confirm: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Attach(String),
    New {
        cwd: PathBuf,
        extensions: Option<Vec<String>>,
        mcp: Option<McpLaunchConfig>,
    },
}

#[derive(Clone, Debug)]
pub struct DriverOpts {
    pub url: String,
    pub secret: String,
    pub target: Target,
    pub crid_seed: Option<u64>,
    pub idle_timeout: Option<Duration>,
    pub max_turns: Option<u32>,
    /// When set, a session created by `Target::New` is registered with this
    /// pigeon daemon (so it gets a Telegram topic). Runs in the background;
    /// its outcome arrives as a `UiEvent::Notice` and never blocks the chat.
    pub pigeon: Option<crate::pigeon::PigeonSettings>,
}

impl DriverOpts {
    pub fn new(url: impl Into<String>, secret: impl Into<String>, target: Target) -> Self {
        Self {
            url: url.into(),
            secret: secret.into(),
            target,
            crid_seed: None,
            idle_timeout: None,
            max_turns: None,
            pigeon: None,
        }
    }
}

/// Registers a just-created session with pigeon in the background and reports
/// the outcome as a notice. A failure is a warning, never fatal: the chat works
/// the same without a Telegram topic.
fn spawn_pigeon_registration(
    settings: crate::pigeon::PigeonSettings,
    honk_url: &str,
    honk_secret: &str,
    goose_session_id: String,
    cwd: String,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) -> tokio::task::JoinHandle<()> {
    let endpoint = crate::pigeon::acp_endpoint(honk_url);
    let auth_token = honk_secret.to_string();
    tokio::spawn(async move {
        let notice = match endpoint.and_then(|ep| Ok((ep, settings.load()?))) {
            Err(e) => Notice::warning(format!("Telegram: not registered with pigeon: {e:#}")),
            Ok((endpoint, cfg)) => {
                let reg = crate::pigeon::Registration {
                    goose_session_id,
                    endpoint,
                    auth_token,
                    cwd,
                    title: None,
                    reason: crate::pigeon::Reason::NewChat,
                };
                match crate::pigeon::register(&cfg, &reg).await {
                    Ok(r) if r.announced => Notice::info(r.summary()),
                    Ok(r) => Notice::warning(r.summary()),
                    Err(e) => {
                        Notice::warning(format!("Telegram: not registered with pigeon: {e:#}"))
                    }
                }
            }
        };
        let _ = ui_tx.send(UiEvent::Notice(notice));
    })
}

/// How long quitting waits for a registration still in flight, so that a quick
/// `:q` after `--new` does not leave pigeon with a session but no topic.
const PIGEON_EXIT_GRACE: Duration = Duration::from_secs(3);

type PigeonSlot = Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Live,
    Reconnecting { attempt: u32 },
    Stopped { reason: String },
    Detached,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateSnapshot {
    pub run_state: RunState,
    pub ownership: Ownership,
    pub queued_len: usize,
    pub replaying: bool,
    pub prompt_in_flight: bool,
    pub connection: ConnectionState,
    pub mcp_enabled: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiEvent {
    Transcript(Value),
    Notice(Notice),
    Attached {
        result: Value,
    },
    SessionNew {
        result: Value,
    },
    SessionInfo {
        title: Option<String>,
        cwd: Option<String>,
    },
    State(StateSnapshot),
    Fatal(String),
    Exit(i32),
}

impl UiEvent {
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            UiEvent::Exit(code) => Some(*code),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiCommand {
    Line(String),
    Interrupt,
    Eof,
    RetryAttach,
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct DriverHandle {
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    join_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl DriverHandle {
    pub fn send(&self, cmd: UiCommand) -> Result<(), mpsc::error::SendError<UiCommand>> {
        self.cmd_tx.send(cmd)
    }

    pub fn cmd_tx(&self) -> mpsc::UnboundedSender<UiCommand> {
        self.cmd_tx.clone()
    }

    pub async fn join(&self) -> Result<(), tokio::task::JoinError> {
        let mut guard = self.join_handle.lock().await;
        if let Some(h) = guard.take() {
            h.await
        } else {
            Ok(())
        }
    }
}

fn id_matches(frame_id: Option<&Value>, expected: u64) -> bool {
    let Some(id) = frame_id else {
        return false;
    };
    if let Some(n) = id.as_u64() {
        n == expected
    } else if let Some(s) = id.as_str() {
        s.parse::<u64>().ok() == Some(expected)
    } else {
        false
    }
}

#[derive(Default, Debug)]
struct ListPagingState {
    pending_id: Option<u64>,
    page_count: usize,
    seen_cursors: std::collections::HashSet<String>,
}

impl ListPagingState {
    fn start(&mut self, id: u64) {
        self.pending_id = Some(id);
        self.page_count = 1;
        self.seen_cursors.clear();
    }

    fn reset(&mut self) {
        self.pending_id = None;
        self.page_count = 0;
        self.seen_cursors.clear();
    }
}

fn random_seed() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let mut hasher = DefaultHasher::new();
    SystemTime::now().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    hasher.finish()
}

fn is_auth_error(error: &Value) -> bool {
    if let Some(code) = error.get("code").and_then(Value::as_i64)
        && (code == 401 || code == 403)
    {
        return true;
    }
    if let Some(msg) = error.get("message").and_then(Value::as_str) {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("auth") || lower.contains("401") || lower.contains("403") {
            return true;
        }
    }
    if let Some(data) = error.get("data").and_then(Value::as_str) {
        let lower = data.to_ascii_lowercase();
        if lower.contains("auth") || lower.contains("401") || lower.contains("403") {
            return true;
        }
    }
    if let Some(s) = error.as_str() {
        let lower = s.to_ascii_lowercase();
        if lower.contains("auth") || lower.contains("401") || lower.contains("403") {
            return true;
        }
    }
    false
}

fn maybe_emit_state(
    core: &ChatCore,
    connection: &ConnectionState,
    last_state: &mut Option<StateSnapshot>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) {
    let current = StateSnapshot {
        run_state: core.run_state().clone(),
        ownership: core.ownership(),
        queued_len: core.queued_len(),
        replaying: core.is_replaying(),
        prompt_in_flight: core.prompt_in_flight(),
        connection: connection.clone(),
        mcp_enabled: core.mcp_enabled().to_vec(),
    };
    if last_state.as_ref() != Some(&current) {
        *last_state = Some(current.clone());
        let _ = ui_tx.send(UiEvent::State(current));
    }
}

enum EffectAction {
    Continue,
    Reattach,
    Exit,
}

#[allow(clippy::too_many_arguments)]
async fn process_effects(
    effects: Vec<Effect>,
    core: &mut ChatCore,
    conn_opt: &mut Option<Conn>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    is_first_attach: &mut bool,
    attach_failed_attempts: &mut u32,
    attach_retry_timer: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    connection_state: &mut ConnectionState,
    list_paging: &mut ListPagingState,
    known_cwd: &mut Option<String>,
) -> EffectAction {
    for effect in effects {
        match effect {
            Effect::Request { id, method, params } => {
                let Some(conn) = conn_opt.as_mut() else {
                    return EffectAction::Reattach;
                };
                if conn.send_request(id, &method, params).await.is_err() {
                    return EffectAction::Reattach;
                }
            }
            Effect::Notify { method, params } => {
                let Some(conn) = conn_opt.as_mut() else {
                    return EffectAction::Reattach;
                };
                if conn.notify(&method, params).await.is_err() {
                    return EffectAction::Reattach;
                }
            }
            Effect::Respond { id, result } => {
                let Some(conn) = conn_opt.as_mut() else {
                    return EffectAction::Reattach;
                };
                if conn.respond(id, result).await.is_err() {
                    return EffectAction::Reattach;
                }
            }
            Effect::Transcript(val) => {
                let _ = ui_tx.send(UiEvent::Transcript(val));
            }
            Effect::Notice(notice) => {
                let _ = ui_tx.send(UiEvent::Notice(notice));
            }
            Effect::Attached { result } => {
                *is_first_attach = false;
                *attach_failed_attempts = 0;
                *attach_retry_timer = None;
                if let Some(wd) = result
                    .pointer("/_meta/workingDir")
                    .or_else(|| result.get("cwd"))
                    .and_then(Value::as_str)
                {
                    *known_cwd = Some(wd.to_string());
                }
                let _ = ui_tx.send(UiEvent::Attached { result });
                if let Some(conn) = conn_opt.as_mut() {
                    let list_id = core.allocate_id();
                    list_paging.start(list_id);
                    let mut params = serde_json::Map::new();
                    if let Some(cwd) = known_cwd.as_deref() {
                        params.insert("cwd".to_string(), json!(cwd));
                    }
                    let _ = conn
                        .send_request(list_id, "session/list", Value::Object(params))
                        .await;
                }
            }
            Effect::AttachFailed { error } => {
                if *is_first_attach || is_auth_error(&error) {
                    let msg = error
                        .get("message")
                        .and_then(Value::as_str)
                        .or_else(|| error.as_str())
                        .unwrap_or("attach failed");
                    let _ = ui_tx.send(UiEvent::Fatal(format!("session/load failed: {msg}")));
                    let _ = ui_tx.send(UiEvent::Exit(1));
                    return EffectAction::Exit;
                }
                *attach_failed_attempts += 1;
                if *attach_failed_attempts < 5 {
                    let delay = Duration::from_millis(
                        (250 * (1 << (*attach_failed_attempts - 1))).min(5000),
                    );
                    let _ = ui_tx.send(UiEvent::Notice(Notice::warning(format!(
                        "attach failed ({error}); retrying in {delay:?}..."
                    ))));
                    *attach_retry_timer = Some(Box::pin(tokio::time::sleep(delay)));
                } else {
                    *attach_retry_timer = None;
                    *connection_state = ConnectionState::Stopped {
                        reason: format!("attach failed after 5 attempts: {error}"),
                    };
                    let _ = ui_tx.send(UiEvent::Notice(Notice::warning(
                        "attach failed 5 times; press any key to retry",
                    )));
                }
            }
            Effect::Detach => {
                if let Some(conn) = conn_opt.as_mut() {
                    let _ = conn
                        .notify("session/close", json!({"sessionId": core.session_id}))
                        .await;
                    let _ = conn.close().await;
                }
                *conn_opt = None;
                *attach_retry_timer = None;
                *connection_state = ConnectionState::Detached;
            }
            Effect::Reattach => {
                return EffectAction::Reattach;
            }
            Effect::Exit(code) => {
                if let Some(conn) = conn_opt.as_mut() {
                    let _ = conn.close().await;
                }
                *conn_opt = None;
                let _ = ui_tx.send(UiEvent::Exit(code));
                return EffectAction::Exit;
            }
        }
    }
    EffectAction::Continue
}

fn reconnect_backoff(attempt: u32) -> Duration {
    if attempt <= 1 {
        Duration::ZERO
    } else {
        Duration::from_millis((250 * (1 << (attempt.saturating_sub(2)).min(5))).min(5000))
    }
}

#[allow(clippy::too_many_arguments)]
fn trigger_disconnect(
    core: &mut ChatCore,
    conn_opt: &mut Option<Conn>,
    frame_rx_opt: &mut Option<mpsc::UnboundedReceiver<Value>>,
    attach_retry_timer: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    reconnect_timer: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    reconnect_attempt: &mut u32,
    connection_state: &mut ConnectionState,
    last_state: &mut Option<StateSnapshot>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    live_since: &mut Option<Instant>,
    list_paging: &mut ListPagingState,
) {
    list_paging.reset();
    let effects = core.handle(Input::Disconnected);
    for effect in effects {
        match effect {
            Effect::Transcript(v) => {
                let _ = ui_tx.send(UiEvent::Transcript(v));
            }
            Effect::Notice(n) => {
                let _ = ui_tx.send(UiEvent::Notice(n));
            }
            _ => {}
        }
    }
    *conn_opt = None;
    *frame_rx_opt = None;
    *attach_retry_timer = None;
    let was_stable = live_since
        .take()
        .is_some_and(|t| t.elapsed() >= Duration::from_secs(10));
    if was_stable {
        *reconnect_attempt = 1;
    } else {
        *reconnect_attempt = reconnect_attempt.saturating_add(1);
    }
    *connection_state = ConnectionState::Reconnecting {
        attempt: *reconnect_attempt,
    };
    maybe_emit_state(core, connection_state, last_state, ui_tx);
    let delay = reconnect_backoff(*reconnect_attempt);
    *reconnect_timer = Some(Box::pin(tokio::time::sleep(delay)));
}

async fn run_driver(
    opts: DriverOpts,
    mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    pigeon_slot: PigeonSlot,
) {
    let mut connection_state = ConnectionState::Connecting;
    let mut last_state = Some(StateSnapshot {
        run_state: RunState::Unknown,
        ownership: Ownership::Unknown,
        queued_len: 0,
        replaying: true,
        prompt_in_flight: false,
        connection: connection_state.clone(),
        mcp_enabled: Vec::new(),
    });
    let _ = ui_tx.send(UiEvent::State(last_state.clone().unwrap()));

    let (conn, mut frame_rx, _init_res) = match Conn::connect(&opts.url, &opts.secret).await {
        Ok(c) => c,
        Err(e) => {
            let _ = ui_tx.send(UiEvent::Fatal(format!("connection failed: {e}")));
            let _ = ui_tx.send(UiEvent::Exit(1));
            return;
        }
    };

    let crid_seed = opts.crid_seed.unwrap_or_else(random_seed);
    let chat_opts = ChatOpts {
        crid_seed,
        idle_timeout: opts.idle_timeout,
        max_turns: opts.max_turns,
    };

    let mut core = match opts.target.clone() {
        Target::New {
            cwd,
            extensions,
            mcp,
        } => {
            let cwd_for_pigeon = cwd.to_string_lossy().to_string();
            let mut ids = IdAlloc::new();
            let new_req_id = ids.allocate();
            let mut params = json!({
                "cwd": cwd.to_string_lossy(),
                "mcpServers": [],
            });
            if let Some(exts) = extensions {
                let ext_arr: Vec<Value> = exts
                    .into_iter()
                    .map(|name| {
                        json!({
                            "type": "platform",
                            "name": name,
                        })
                    })
                    .collect();
                params["_meta"] = json!({
                    "enabledExtensions": ext_arr,
                });
            }
            if let Some(mcp_cfg) = mcp {
                if params.get("_meta").is_none() {
                    params["_meta"] = json!({});
                }
                if params["_meta"].get("honk").is_none() {
                    params["_meta"]["honk"] = json!({});
                }
                params["_meta"]["honk"]["mcp"] = json!({
                    "names": mcp_cfg.names,
                    "confirm": mcp_cfg.confirm,
                });
            }

            if let Err(e) = conn.send_request(new_req_id, "session/new", params).await {
                let _ = ui_tx.send(UiEvent::Fatal(format!(
                    "session/new failed: {e}; a session may have been created (see `honk list`)"
                )));
                let _ = ui_tx.send(UiEvent::Exit(1));
                return;
            }

            let new_res = tokio::time::timeout(Duration::from_secs(30), async {
                while let Some(frame) = frame_rx.recv().await {
                    if frame.get("id") == Some(&json!(new_req_id))
                        && crate::conn::is_response(&frame)
                    {
                        return Ok(frame);
                    }
                }
                Err(anyhow::anyhow!(
                    "stream closed while awaiting session/new response"
                ))
            })
            .await;

            let frame = match new_res {
                Ok(Ok(f)) => f,
                Ok(Err(e)) => {
                    let _ = ui_tx.send(UiEvent::Fatal(format!(
                        "session/new failed: {e}; a session may have been created (see `honk list`)"
                    )));
                    let _ = ui_tx.send(UiEvent::Exit(1));
                    return;
                }
                Err(_) => {
                    let _ = ui_tx.send(UiEvent::Fatal(
                        "session/new failed: timed out; a session may have been created (see `honk list`)".to_string(),
                    ));
                    let _ = ui_tx.send(UiEvent::Exit(1));
                    return;
                }
            };

            if let Some(err) = frame.get("error") {
                let _ = ui_tx.send(UiEvent::Fatal(format!(
                    "session/new failed: {err}; a session may have been created (see `honk list`)"
                )));
                let _ = ui_tx.send(UiEvent::Exit(1));
                return;
            }

            let result = frame.get("result").unwrap_or(&Value::Null);
            let Some(session_id) = result.get("sessionId").and_then(Value::as_str) else {
                let _ = ui_tx.send(UiEvent::Fatal(
                    "session/new failed: missing sessionId in response; a session may have been created (see `honk list`)".to_string(),
                ));
                let _ = ui_tx.send(UiEvent::Exit(1));
                return;
            };

            let _ = ui_tx.send(UiEvent::SessionNew {
                result: result.clone(),
            });

            if let Some(settings) = opts.pigeon.clone() {
                let task = spawn_pigeon_registration(
                    settings,
                    &opts.url,
                    &opts.secret,
                    session_id.to_string(),
                    cwd_for_pigeon.clone(),
                    ui_tx.clone(),
                );
                *pigeon_slot.lock().unwrap() = Some(task);
            }

            ChatCore::with_ids(session_id.to_string(), chat_opts, Instant::now(), ids)
        }
        Target::Attach(session_id) => ChatCore::new(session_id, chat_opts, Instant::now()),
    };

    let (load_id, method, params) = core.attach_request();
    if let Err(e) = conn.send_request(load_id, &method, params).await {
        let _ = ui_tx.send(UiEvent::Fatal(format!("failed to send session/load: {e}")));
        let _ = ui_tx.send(UiEvent::Exit(1));
        return;
    }

    connection_state = ConnectionState::Live;
    let mut is_first_attach = true;
    let mut attach_failed_attempts = 0u32;
    maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);

    let mut tick_interval = tokio::time::interval(Duration::from_secs(1));
    tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut conn_opt = Some(conn);
    let mut frame_rx_opt = Some(frame_rx);
    let mut attach_retry_timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut reconnect_timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut reconnect_attempt = 1u32;
    let mut list_paging = ListPagingState::default();
    let mut known_cwd = match &opts.target {
        Target::New { cwd, .. } => Some(cwd.to_string_lossy().to_string()),
        Target::Attach(_) => None,
    };
    let mut live_since: Option<Instant> = None;

    loop {
        tokio::select! {
            cmd_opt = cmd_rx.recv() => {
                let Some(cmd) = cmd_opt else {
                    if let Some(c) = conn_opt.as_mut() {
                        let _ = c.notify("session/close", json!({"sessionId": core.session_id})).await;
                        let _ = c.close().await;
                    }
                    let _ = ui_tx.send(UiEvent::Exit(0));
                    return;
                };

                match cmd {
                    UiCommand::Line(l) => {
                        let effects = core.handle(Input::Line(l));
                        let action = process_effects(
                            effects,
                            &mut core,
                            &mut conn_opt,
                            &ui_tx,
                            &mut is_first_attach,
                            &mut attach_failed_attempts,
                            &mut attach_retry_timer,
                            &mut connection_state,
                            &mut list_paging,
                            &mut known_cwd,
                        ).await;
                        if matches!(action, EffectAction::Exit) {
                            return;
                        }
                        if matches!(action, EffectAction::Reattach) {
                            trigger_disconnect(
                                &mut core,
                                &mut conn_opt,
                                &mut frame_rx_opt,
                                &mut attach_retry_timer,
                                &mut reconnect_timer,
                                &mut reconnect_attempt,
                                &mut connection_state,
                                &mut last_state,
                                &ui_tx,
                                &mut live_since,
                                &mut list_paging,
                            );
                        }
                        maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                    }
                    UiCommand::Interrupt => {
                        let effects = core.handle(Input::Interrupt);
                        let action = process_effects(
                            effects,
                            &mut core,
                            &mut conn_opt,
                            &ui_tx,
                            &mut is_first_attach,
                            &mut attach_failed_attempts,
                            &mut attach_retry_timer,
                            &mut connection_state,
                            &mut list_paging,
                            &mut known_cwd,
                        ).await;
                        if matches!(action, EffectAction::Exit) {
                            return;
                        }
                        if matches!(action, EffectAction::Reattach) {
                            trigger_disconnect(
                                &mut core,
                                &mut conn_opt,
                                &mut frame_rx_opt,
                                &mut attach_retry_timer,
                                &mut reconnect_timer,
                                &mut reconnect_attempt,
                                &mut connection_state,
                                &mut last_state,
                                &ui_tx,
                                &mut live_since,
                                &mut list_paging,
                            );
                        }
                        maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                    }
                    UiCommand::Eof => {
                        let effects = core.handle(Input::Eof);
                        let action = process_effects(
                            effects,
                            &mut core,
                            &mut conn_opt,
                            &ui_tx,
                            &mut is_first_attach,
                            &mut attach_failed_attempts,
                            &mut attach_retry_timer,
                            &mut connection_state,
                            &mut list_paging,
                            &mut known_cwd,
                        ).await;
                        if matches!(action, EffectAction::Exit) {
                            return;
                        }
                        return;
                    }
                    UiCommand::Shutdown => {
                        if let Some(c) = conn_opt.as_mut() {
                            let _ = c.notify("session/close", json!({"sessionId": core.session_id})).await;
                            let _ = c.close().await;
                        }
                        let _ = ui_tx.send(UiEvent::Exit(0));
                        return;
                    }
                    UiCommand::RetryAttach => {
                        if matches!(connection_state, ConnectionState::Stopped { .. } | ConnectionState::Detached) {
                            attach_failed_attempts = 0;
                            attach_retry_timer = None;
                            if let Some(c) = conn_opt.as_mut() {
                                let (load_id, method, params) = core.attach_request();
                                if c.send_request(load_id, &method, params).await.is_err() {
                                    trigger_disconnect(
                                        &mut core,
                                        &mut conn_opt,
                                        &mut frame_rx_opt,
                                        &mut attach_retry_timer,
                                        &mut reconnect_timer,
                                        &mut reconnect_attempt,
                                        &mut connection_state,
                                        &mut last_state,
                                        &ui_tx,
                                        &mut live_since,
                                        &mut list_paging,
                                    );
                                } else {
                                    connection_state = ConnectionState::Live;
                                    maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                                }
                            } else {
                                trigger_disconnect(
                                    &mut core,
                                    &mut conn_opt,
                                    &mut frame_rx_opt,
                                    &mut attach_retry_timer,
                                    &mut reconnect_timer,
                                    &mut reconnect_attempt,
                                    &mut connection_state,
                                    &mut last_state,
                                    &ui_tx,
                                    &mut live_since,
                                    &mut list_paging,
                                );
                            }
                        }
                    }
                }
            }
            frame_opt = async {
                match frame_rx_opt.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match frame_opt {
                    Some(frame) => {
                        if let Some(list_id) = list_paging.pending_id
                            && id_matches(frame.get("id"), list_id)
                            && frame.get("method").is_none()
                        {
                            let mut found = false;
                            let mut title = None;
                            let mut cwd = None;
                            let sessions = frame
                                .pointer("/result/sessions")
                                .or_else(|| frame.get("sessions"))
                                .or_else(|| frame.get("result"))
                                .and_then(Value::as_array);
                            if let Some(arr) = sessions {
                                for s in arr {
                                    let sid_matches = s
                                        .get("sessionId")
                                        .or_else(|| s.get("id"))
                                        .and_then(Value::as_str)
                                        == Some(&core.session_id);
                                    if sid_matches {
                                        found = true;
                                        title = s
                                            .get("title")
                                            .or_else(|| s.get("name"))
                                            .and_then(Value::as_str)
                                            .map(String::from);
                                        cwd = s
                                            .get("cwd")
                                            .or_else(|| s.pointer("/_meta/workingDir"))
                                            .and_then(Value::as_str)
                                            .map(String::from);
                                        break;
                                    }
                                }
                            }

                            if found {
                                if let Some(ref c) = cwd {
                                    known_cwd = Some(c.clone());
                                } else if cwd.is_none() {
                                    cwd = known_cwd.clone();
                                }
                                let _ = ui_tx.send(UiEvent::SessionInfo { title, cwd });
                                list_paging.reset();
                                continue;
                            }

                            // Session not found on this page: check nextCursor to follow pages
                            let next_cursor = frame
                                .pointer("/result/nextCursor")
                                .or_else(|| frame.get("nextCursor"))
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(String::from);

                            const MAX_PAGES: usize = 100;
                            if let Some(cursor) = next_cursor
                                && list_paging.page_count < MAX_PAGES
                                && list_paging.seen_cursors.insert(cursor.clone())
                                && let Some(conn) = conn_opt.as_mut()
                            {
                                list_paging.page_count += 1;
                                let next_list_id = core.allocate_id();
                                list_paging.pending_id = Some(next_list_id);
                                let mut params = serde_json::Map::new();
                                params.insert("cursor".to_string(), json!(cursor));
                                if let Some(k_cwd) = known_cwd.as_deref() {
                                    params.insert("cwd".to_string(), json!(k_cwd));
                                }
                                if conn
                                    .send_request(
                                        next_list_id,
                                        "session/list",
                                        Value::Object(params),
                                    )
                                    .await
                                    .is_ok()
                                {
                                    continue;
                                }
                            }

                            // Pages exhausted, cap reached, repeated cursor, or send failed
                            if cwd.is_none() {
                                cwd = known_cwd.clone();
                            }
                            let _ = ui_tx.send(UiEvent::SessionInfo { title, cwd });
                            list_paging.reset();
                            continue;
                        }

                        // Re-fetch session/list when a run ends (activeRunId -> null)
                        if let Some(u) = frame.pointer("/params/update")
                            && let Some(state) = honk_wire::run_state(u)
                            && state.is_none()
                            && connection_state == ConnectionState::Live
                            && !core.is_replaying()
                            && let Some(conn) = conn_opt.as_mut()
                        {
                            let list_id = core.allocate_id();
                            list_paging.start(list_id);
                            let mut params = serde_json::Map::new();
                            if let Some(cwd) = known_cwd.as_deref() {
                                params.insert("cwd".to_string(), json!(cwd));
                            }
                            let _ = conn
                                .send_request(list_id, "session/list", Value::Object(params))
                                .await;

                            if let Some(Effect::Request { id, method, params }) =
                                core.auto_refresh_mcp()
                            {
                                let _ = conn.send_request(id, &method, params).await;
                            }
                        }

                        if frame.get("method").and_then(Value::as_str)
                            == Some(honk_wire::method::REPLAY_DONE)
                            && connection_state == ConnectionState::Live
                        {
                            live_since = Some(Instant::now());
                            if let Some(conn) = conn_opt.as_mut() {
                                if let Some(Effect::Request { id, method, params }) =
                                    core.auto_refresh_mcp()
                                {
                                    let _ = conn.send_request(id, &method, params).await;
                                }
                            }
                        }
                        if frame.get("method").and_then(Value::as_str)
                            == Some(honk_wire::method::RESET)
                        {
                            live_since = None;
                        }

                        let effects = core.handle(Input::Frame(frame));
                        let action = process_effects(
                            effects,
                            &mut core,
                            &mut conn_opt,
                            &ui_tx,
                            &mut is_first_attach,
                            &mut attach_failed_attempts,
                            &mut attach_retry_timer,
                            &mut connection_state,
                            &mut list_paging,
                            &mut known_cwd,
                        ).await;
                        if matches!(action, EffectAction::Exit) {
                            return;
                        }
                        if matches!(action, EffectAction::Reattach) {
                            trigger_disconnect(
                                &mut core,
                                &mut conn_opt,
                                &mut frame_rx_opt,
                                &mut attach_retry_timer,
                                &mut reconnect_timer,
                                &mut reconnect_attempt,
                                &mut connection_state,
                                &mut last_state,
                                &ui_tx,
                                &mut live_since,
                                &mut list_paging,
                            );
                        }
                        maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                    }
                    None => {
                        trigger_disconnect(
                            &mut core,
                            &mut conn_opt,
                            &mut frame_rx_opt,
                            &mut attach_retry_timer,
                            &mut reconnect_timer,
                            &mut reconnect_attempt,
                            &mut connection_state,
                            &mut last_state,
                            &ui_tx,
                            &mut live_since,
                            &mut list_paging,
                        );
                    }
                }
            }
            _ = async {
                match attach_retry_timer.as_mut() {
                    Some(timer) => timer.as_mut().await,
                    None => std::future::pending().await,
                }
            } => {
                attach_retry_timer = None;
                if let Some(c) = conn_opt.as_mut() {
                    let (load_id, method, params) = core.attach_request();
                    if c.send_request(load_id, &method, params).await.is_err() {
                        trigger_disconnect(
                            &mut core,
                            &mut conn_opt,
                            &mut frame_rx_opt,
                            &mut attach_retry_timer,
                            &mut reconnect_timer,
                            &mut reconnect_attempt,
                            &mut connection_state,
                            &mut last_state,
                            &ui_tx,
                            &mut live_since,
                            &mut list_paging,
                        );
                    }
                }
            }
            _ = async {
                match reconnect_timer.as_mut() {
                    Some(timer) => timer.as_mut().await,
                    None => std::future::pending().await,
                }
            } => {
                reconnect_timer = None;
                match Conn::connect(&opts.url, &opts.secret).await {
                    Ok((new_conn, new_frame_rx, _init_res)) => {
                        let (load_id, method, params) = core.attach_request();
                        if new_conn.send_request(load_id, &method, params).await.is_err() {
                            live_since = None;
                            reconnect_attempt = reconnect_attempt.saturating_add(1);
                            connection_state = ConnectionState::Reconnecting { attempt: reconnect_attempt };
                            maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                            let backoff = reconnect_backoff(reconnect_attempt);
                            reconnect_timer = Some(Box::pin(tokio::time::sleep(backoff)));
                        } else {
                            let effects = core.handle(Input::Connected);
                            let mut temp_conn = Some(new_conn);
                            let action = process_effects(
                                effects,
                                &mut core,
                                &mut temp_conn,
                                &ui_tx,
                                &mut is_first_attach,
                                &mut attach_failed_attempts,
                                &mut attach_retry_timer,
                                &mut connection_state,
                                &mut list_paging,
                                &mut known_cwd,
                            ).await;
                            if matches!(action, EffectAction::Exit) {
                                return;
                            }
                            if matches!(action, EffectAction::Reattach) {
                                live_since = None;
                                reconnect_attempt = reconnect_attempt.saturating_add(1);
                                connection_state = ConnectionState::Reconnecting { attempt: reconnect_attempt };
                                maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                                let backoff = reconnect_backoff(reconnect_attempt);
                                reconnect_timer = Some(Box::pin(tokio::time::sleep(backoff)));
                            } else {
                                conn_opt = temp_conn;
                                frame_rx_opt = Some(new_frame_rx);
                                connection_state = ConnectionState::Live;
                                maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                            }
                        }
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if err_str.starts_with("fatal: ")
                            || err_str.contains("authentication failed")
                            || err_str.contains("401")
                            || err_str.contains("403")
                        {
                            let _ = ui_tx.send(UiEvent::Fatal(err_str));
                            let _ = ui_tx.send(UiEvent::Exit(1));
                            return;
                        }
                        live_since = None;
                        reconnect_attempt = reconnect_attempt.saturating_add(1);
                        connection_state = ConnectionState::Reconnecting { attempt: reconnect_attempt };
                        maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                        let backoff = reconnect_backoff(reconnect_attempt);
                        reconnect_timer = Some(Box::pin(tokio::time::sleep(backoff)));
                    }
                }
            }
            _ = tick_interval.tick() => {
                let effects = core.handle(Input::Tick(Instant::now()));
                let action = process_effects(
                    effects,
                    &mut core,
                    &mut conn_opt,
                    &ui_tx,
                    &mut is_first_attach,
                    &mut attach_failed_attempts,
                    &mut attach_retry_timer,
                    &mut connection_state,
                    &mut list_paging,
                    &mut known_cwd,
                ).await;
                if matches!(action, EffectAction::Exit) {
                    return;
                }
                if matches!(action, EffectAction::Reattach) {
                    trigger_disconnect(
                        &mut core,
                        &mut conn_opt,
                        &mut frame_rx_opt,
                        &mut attach_retry_timer,
                        &mut reconnect_timer,
                        &mut reconnect_attempt,
                        &mut connection_state,
                        &mut last_state,
                        &ui_tx,
                        &mut live_since,
                        &mut list_paging,
                    );
                }
                maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
            }
        }
    }
}

pub async fn start(
    opts: DriverOpts,
) -> anyhow::Result<(DriverHandle, mpsc::UnboundedReceiver<UiEvent>)> {
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let join_handle = Arc::new(Mutex::new(None));
    let handle = DriverHandle {
        cmd_tx,
        join_handle: join_handle.clone(),
    };

    let task = tokio::spawn(async move {
        let pigeon_slot = PigeonSlot::default();
        run_driver(opts, cmd_rx, ui_tx, pigeon_slot.clone()).await;
        let pending = pigeon_slot.lock().unwrap().take();
        if let Some(h) = pending {
            let _ = tokio::time::timeout(PIGEON_EXIT_GRACE, h).await;
        }
    });
    *join_handle.lock().await = Some(task);

    Ok((handle, ui_rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_seed_different() {
        let s1 = random_seed();
        let s2 = random_seed();
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_driver_opts_defaults() {
        let opts = DriverOpts::new(
            "ws://127.0.0.1:4280/acp",
            "secret",
            Target::Attach("s1".into()),
        );
        assert_eq!(opts.url, "ws://127.0.0.1:4280/acp");
        assert_eq!(opts.secret, "secret");
        assert_eq!(opts.target, Target::Attach("s1".into()));
        assert!(opts.crid_seed.is_none());
        assert!(opts.idle_timeout.is_none());
    }

    #[test]
    fn test_ui_event_exit_code() {
        let evt = UiEvent::Exit(42);
        assert_eq!(evt.exit_code(), Some(42));
        let fatal = UiEvent::Fatal("err".into());
        assert_eq!(fatal.exit_code(), None);
    }

    #[test]
    fn test_connection_state_equality() {
        assert_eq!(ConnectionState::Live, ConnectionState::Live);
        assert_eq!(
            ConnectionState::Reconnecting { attempt: 1 },
            ConnectionState::Reconnecting { attempt: 1 }
        );
        assert_ne!(
            ConnectionState::Reconnecting { attempt: 1 },
            ConnectionState::Reconnecting { attempt: 2 }
        );
        assert_eq!(
            ConnectionState::Stopped {
                reason: "err".into()
            },
            ConnectionState::Stopped {
                reason: "err".into()
            }
        );
        assert_eq!(ConnectionState::Detached, ConnectionState::Detached);
    }
}
