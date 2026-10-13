use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::chat::core::ChatCore;
use crate::chat::types::{ChatOpts, Effect, IdAlloc, Input, Notice, Ownership, RunState};
use crate::conn::Conn;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Attach(String),
    New {
        cwd: PathBuf,
        extensions: Option<Vec<String>>,
    },
}

#[derive(Clone, Debug)]
pub struct DriverOpts {
    pub url: String,
    pub secret: String,
    pub target: Target,
    pub crid_seed: Option<u64>,
    pub idle_timeout: Option<Duration>,
}

impl DriverOpts {
    pub fn new(url: impl Into<String>, secret: impl Into<String>, target: Target) -> Self {
        Self {
            url: url.into(),
            secret: secret.into(),
            target,
            crid_seed: None,
            idle_timeout: None,
        }
    }
}

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
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiEvent {
    Transcript(Value),
    Notice(Notice),
    Attached { result: Value },
    SessionNew { result: Value },
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
}

impl DriverHandle {
    pub fn send(&self, cmd: UiCommand) -> Result<(), mpsc::error::SendError<UiCommand>> {
        self.cmd_tx.send(cmd)
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

async fn process_effects(
    effects: Vec<Effect>,
    core: &mut ChatCore,
    conn: &mut Conn,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    is_first_attach: &mut bool,
    attach_failed_attempts: &mut u32,
    connection_state: &mut ConnectionState,
) -> EffectAction {
    for effect in effects {
        match effect {
            Effect::Request { id, method, params } => {
                if conn.send_request(id, &method, params).await.is_err() {
                    return EffectAction::Reattach;
                }
            }
            Effect::Notify { method, params } => {
                if conn.notify(&method, params).await.is_err() {
                    return EffectAction::Reattach;
                }
            }
            Effect::Respond { id, result } => {
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
                let _ = ui_tx.send(UiEvent::Attached { result });
            }
            Effect::AttachFailed { error } => {
                if *is_first_attach {
                    let msg = error
                        .get("message")
                        .and_then(Value::as_str)
                        .or_else(|| error.as_str())
                        .unwrap_or("attach failed");
                    let _ = ui_tx.send(UiEvent::Fatal(format!("session/load failed: {msg}")));
                    let _ = ui_tx.send(UiEvent::Exit(1));
                    return EffectAction::Exit;
                } else {
                    *attach_failed_attempts += 1;
                    if *attach_failed_attempts < 5 {
                        let delay = Duration::from_millis(
                            (250 * (1 << (*attach_failed_attempts - 1))).min(5000),
                        );
                        let _ = ui_tx.send(UiEvent::Notice(Notice::warning(format!(
                            "[honk] attach failed ({error}); retrying in {delay:?}..."
                        ))));
                        tokio::time::sleep(delay).await;
                        let (load_id, method, params) = core.attach_request();
                        if conn.send_request(load_id, &method, params).await.is_err() {
                            return EffectAction::Reattach;
                        }
                    } else {
                        *connection_state = ConnectionState::Stopped {
                            reason: format!("attach failed after 5 attempts: {error}"),
                        };
                        let _ = ui_tx.send(UiEvent::Notice(Notice::warning(
                            "[honk] attach failed 5 times; press any key to retry",
                        )));
                    }
                }
            }
            Effect::Detach => {
                let _ = conn
                    .notify("session/close", json!({"sessionId": core.session_id}))
                    .await;
                let _ = conn.close().await;
                *connection_state = ConnectionState::Detached;
            }
            Effect::Reattach => {
                return EffectAction::Reattach;
            }
            Effect::Exit(code) => {
                let _ = conn.close().await;
                let _ = ui_tx.send(UiEvent::Exit(code));
                return EffectAction::Exit;
            }
        }
    }
    EffectAction::Continue
}

#[allow(clippy::too_many_arguments)]
async fn perform_reconnect(
    opts: &DriverOpts,
    core: &mut ChatCore,
    cmd_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    connection_state: &mut ConnectionState,
    last_state: &mut Option<StateSnapshot>,
    is_first_attach: &mut bool,
    attach_failed_attempts: &mut u32,
) -> Option<(Conn, mpsc::UnboundedReceiver<Value>)> {
    let mut attempt = 1u32;
    loop {
        *connection_state = ConnectionState::Reconnecting { attempt };
        maybe_emit_state(core, connection_state, last_state, ui_tx);

        match Conn::connect(&opts.url, &opts.secret).await {
            Ok((new_conn, new_frame_rx, _init_res)) => {
                // Order matters: send session/load first, then feed Connected
                let (load_id, method, params) = core.attach_request();
                let _ = new_conn.send_request(load_id, &method, params).await;
                let effects = core.handle(Input::Connected);
                let mut temp_conn = new_conn.clone();
                let action = process_effects(
                    effects,
                    core,
                    &mut temp_conn,
                    ui_tx,
                    is_first_attach,
                    attach_failed_attempts,
                    connection_state,
                )
                .await;
                if matches!(action, EffectAction::Exit) {
                    return None;
                }
                *connection_state = ConnectionState::Live;
                maybe_emit_state(core, connection_state, last_state, ui_tx);
                return Some((new_conn, new_frame_rx));
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.starts_with("fatal: ") {
                    let _ = ui_tx.send(UiEvent::Fatal(err_str));
                    let _ = ui_tx.send(UiEvent::Exit(1));
                    return None;
                }
                let backoff = Duration::from_millis(
                    (250 * (1 << (attempt.saturating_sub(1)).min(5))).min(5000),
                );
                tokio::select! {
                    cmd_opt = cmd_rx.recv() => {
                        match cmd_opt {
                            Some(UiCommand::Shutdown) | Some(UiCommand::Eof) | None => {
                                let _ = ui_tx.send(UiEvent::Exit(0));
                                return None;
                            }
                            Some(UiCommand::Line(l)) => {
                                let effects = core.handle(Input::Line(l));
                                for effect in effects {
                                    if let Effect::Notice(n) = effect {
                                        let _ = ui_tx.send(UiEvent::Notice(n));
                                    }
                                }
                                maybe_emit_state(core, connection_state, last_state, ui_tx);
                            }
                            Some(UiCommand::Interrupt) => {
                                let effects = core.handle(Input::Interrupt);
                                for effect in effects {
                                    if let Effect::Notice(n) = effect {
                                        let _ = ui_tx.send(UiEvent::Notice(n));
                                    }
                                }
                                maybe_emit_state(core, connection_state, last_state, ui_tx);
                            }
                            Some(UiCommand::RetryAttach) => {}
                        }
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

async fn run_driver(
    opts: DriverOpts,
    mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) {
    let mut connection_state = ConnectionState::Connecting;
    let mut last_state = Some(StateSnapshot {
        run_state: RunState::Unknown,
        ownership: Ownership::Unknown,
        queued_len: 0,
        replaying: true,
        prompt_in_flight: false,
        connection: connection_state.clone(),
    });
    let _ = ui_tx.send(UiEvent::State(last_state.clone().unwrap()));

    let (mut conn, mut frame_rx, _init_res) = match Conn::connect(&opts.url, &opts.secret).await {
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
    };

    let mut core = match opts.target.clone() {
        Target::New { cwd, extensions } => {
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

    loop {
        if matches!(connection_state, ConnectionState::Detached) {
            tokio::select! {
                cmd_opt = cmd_rx.recv() => {
                    match cmd_opt {
                        Some(UiCommand::Line(l)) => {
                            let effects = core.handle(Input::Line(l));
                            let action = process_effects(
                                effects, &mut core, &mut conn, &ui_tx,
                                &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                            ).await;
                            maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                            if matches!(action, EffectAction::Reattach) {
                                let Some((nc, nfr)) = perform_reconnect(
                                    &opts, &mut core, &mut cmd_rx, &ui_tx,
                                    &mut connection_state, &mut last_state,
                                    &mut is_first_attach, &mut attach_failed_attempts,
                                ).await else {
                                    return;
                                };
                                conn = nc;
                                frame_rx = nfr;
                            } else if matches!(action, EffectAction::Exit) {
                                return;
                            }
                        }
                        Some(UiCommand::RetryAttach) => {
                            let Some((nc, nfr)) = perform_reconnect(
                                &opts, &mut core, &mut cmd_rx, &ui_tx,
                                &mut connection_state, &mut last_state,
                                &mut is_first_attach, &mut attach_failed_attempts,
                            ).await else {
                                return;
                            };
                            conn = nc;
                            frame_rx = nfr;
                        }
                        Some(UiCommand::Shutdown) | Some(UiCommand::Eof) | None => {
                            let _ = ui_tx.send(UiEvent::Exit(0));
                            return;
                        }
                        Some(UiCommand::Interrupt) => {
                            let effects = core.handle(Input::Interrupt);
                            let _ = process_effects(
                                effects, &mut core, &mut conn, &ui_tx,
                                &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                            ).await;
                            maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                        }
                    }
                }
                _ = tick_interval.tick() => {
                    let effects = core.handle(Input::Tick(Instant::now()));
                    let _ = process_effects(
                        effects, &mut core, &mut conn, &ui_tx,
                        &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                    ).await;
                    maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                }
            }
            continue;
        }

        if matches!(connection_state, ConnectionState::Stopped { .. }) {
            tokio::select! {
                cmd_opt = cmd_rx.recv() => {
                    match cmd_opt {
                        Some(UiCommand::RetryAttach) => {
                            attach_failed_attempts = 0;
                            if conn.is_closed() {
                                let Some((nc, nfr)) = perform_reconnect(
                                    &opts, &mut core, &mut cmd_rx, &ui_tx,
                                    &mut connection_state, &mut last_state,
                                    &mut is_first_attach, &mut attach_failed_attempts,
                                ).await else {
                                    return;
                                };
                                conn = nc;
                                frame_rx = nfr;
                            } else {
                                connection_state = ConnectionState::Reconnecting { attempt: 1 };
                                maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                                let (load_id, method, params) = core.attach_request();
                                if conn.send_request(load_id, &method, params).await.is_err() {
                                    let Some((nc, nfr)) = perform_reconnect(
                                        &opts, &mut core, &mut cmd_rx, &ui_tx,
                                        &mut connection_state, &mut last_state,
                                        &mut is_first_attach, &mut attach_failed_attempts,
                                    ).await else {
                                        return;
                                    };
                                    conn = nc;
                                    frame_rx = nfr;
                                } else {
                                    connection_state = ConnectionState::Live;
                                    maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                                }
                            }
                        }
                        Some(UiCommand::Shutdown) | Some(UiCommand::Eof) | None => {
                            let _ = conn.close().await;
                            let _ = ui_tx.send(UiEvent::Exit(0));
                            return;
                        }
                        Some(UiCommand::Line(l)) => {
                            let effects = core.handle(Input::Line(l));
                            let _ = process_effects(
                                effects, &mut core, &mut conn, &ui_tx,
                                &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                            ).await;
                            maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                        }
                        Some(UiCommand::Interrupt) => {
                            let effects = core.handle(Input::Interrupt);
                            let _ = process_effects(
                                effects, &mut core, &mut conn, &ui_tx,
                                &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                            ).await;
                            maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                        }
                    }
                }
                frame_opt = frame_rx.recv() => {
                    match frame_opt {
                        Some(frame) => {
                            let effects = core.handle(Input::Frame(frame));
                            let action = process_effects(
                                effects, &mut core, &mut conn, &ui_tx,
                                &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                            ).await;
                            maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                            if matches!(action, EffectAction::Reattach) {
                                let Some((nc, nfr)) = perform_reconnect(
                                    &opts, &mut core, &mut cmd_rx, &ui_tx,
                                    &mut connection_state, &mut last_state,
                                    &mut is_first_attach, &mut attach_failed_attempts,
                                ).await else {
                                    return;
                                };
                                conn = nc;
                                frame_rx = nfr;
                            } else if matches!(action, EffectAction::Exit) {
                                return;
                            }
                        }
                        None => {
                            let effects = core.handle(Input::Disconnected);
                            let _ = process_effects(
                                effects, &mut core, &mut conn, &ui_tx,
                                &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                            ).await;
                            let Some((nc, nfr)) = perform_reconnect(
                                &opts, &mut core, &mut cmd_rx, &ui_tx,
                                &mut connection_state, &mut last_state,
                                &mut is_first_attach, &mut attach_failed_attempts,
                            ).await else {
                                return;
                            };
                            conn = nc;
                            frame_rx = nfr;
                        }
                    }
                }
                _ = tick_interval.tick() => {
                    let effects = core.handle(Input::Tick(Instant::now()));
                    let _ = process_effects(
                        effects, &mut core, &mut conn, &ui_tx,
                        &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                    ).await;
                    maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                }
            }
            continue;
        }

        tokio::select! {
            frame_opt = frame_rx.recv() => {
                match frame_opt {
                    Some(frame) => {
                        let effects = core.handle(Input::Frame(frame));
                        let action = process_effects(
                            effects, &mut core, &mut conn, &ui_tx,
                            &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                        ).await;
                        maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                        if matches!(action, EffectAction::Reattach) {
                            let Some((nc, nfr)) = perform_reconnect(
                                &opts, &mut core, &mut cmd_rx, &ui_tx,
                                &mut connection_state, &mut last_state,
                                &mut is_first_attach, &mut attach_failed_attempts,
                            ).await else {
                                return;
                            };
                            conn = nc;
                            frame_rx = nfr;
                        } else if matches!(action, EffectAction::Exit) {
                            return;
                        }
                    }
                    None => {
                        let effects = core.handle(Input::Disconnected);
                        let _ = process_effects(
                            effects, &mut core, &mut conn, &ui_tx,
                            &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                        ).await;
                        let Some((nc, nfr)) = perform_reconnect(
                            &opts, &mut core, &mut cmd_rx, &ui_tx,
                            &mut connection_state, &mut last_state,
                            &mut is_first_attach, &mut attach_failed_attempts,
                        ).await else {
                            return;
                        };
                        conn = nc;
                        frame_rx = nfr;
                    }
                }
            }
            cmd_opt = cmd_rx.recv() => {
                match cmd_opt {
                    Some(UiCommand::Line(l)) => {
                        let effects = core.handle(Input::Line(l));
                        let action = process_effects(
                            effects, &mut core, &mut conn, &ui_tx,
                            &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                        ).await;
                        maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                        if matches!(action, EffectAction::Reattach) {
                            let Some((nc, nfr)) = perform_reconnect(
                                &opts, &mut core, &mut cmd_rx, &ui_tx,
                                &mut connection_state, &mut last_state,
                                &mut is_first_attach, &mut attach_failed_attempts,
                            ).await else {
                                return;
                            };
                            conn = nc;
                            frame_rx = nfr;
                        } else if matches!(action, EffectAction::Exit) {
                            return;
                        }
                    }
                    Some(UiCommand::Interrupt) => {
                        let effects = core.handle(Input::Interrupt);
                        let action = process_effects(
                            effects, &mut core, &mut conn, &ui_tx,
                            &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                        ).await;
                        maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                        if matches!(action, EffectAction::Exit) {
                            return;
                        }
                    }
                    Some(UiCommand::Eof) => {
                        let effects = core.handle(Input::Eof);
                        let _ = process_effects(
                            effects, &mut core, &mut conn, &ui_tx,
                            &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                        ).await;
                        return;
                    }
                    Some(UiCommand::Shutdown) | None => {
                        let _ = conn
                            .notify("session/close", json!({"sessionId": core.session_id}))
                            .await;
                        let _ = conn.close().await;
                        let _ = ui_tx.send(UiEvent::Exit(0));
                        return;
                    }
                    Some(UiCommand::RetryAttach) => {}
                }
            }
            _ = tick_interval.tick() => {
                let effects = core.handle(Input::Tick(Instant::now()));
                let action = process_effects(
                    effects, &mut core, &mut conn, &ui_tx,
                    &mut is_first_attach, &mut attach_failed_attempts, &mut connection_state,
                ).await;
                maybe_emit_state(&core, &connection_state, &mut last_state, &ui_tx);
                if matches!(action, EffectAction::Exit) {
                    return;
                }
            }
        }
    }
}

pub async fn start(
    opts: DriverOpts,
) -> anyhow::Result<(DriverHandle, mpsc::UnboundedReceiver<UiEvent>)> {
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let handle = DriverHandle { cmd_tx };

    tokio::spawn(async move {
        run_driver(opts, cmd_rx, ui_tx).await;
    });

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
