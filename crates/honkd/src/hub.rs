use anyhow::Context;
use honk_upstream::{Recorder, Redactor, SubscribeOpts, Upstream, UpstreamHandle, UpstreamOpts};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, oneshot};

use crate::config::Args;
use crate::rpc::UpstreamIds;
use crate::state::StateStore;

#[derive(Debug, Clone)]
pub struct ActivationError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for ActivationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ActivationError {}

#[derive(Clone, Debug)]
pub struct SessionHandle {
    pub sid: String,
    pub conn_id: String,
    pub tx: tokio::sync::mpsc::Sender<crate::session::SessionMsg>,
}
type ActivatingReceiver =
    tokio::sync::watch::Receiver<Option<Result<SessionHandle, ActivationError>>>;

pub struct Hub {
    pub args: Arc<Args>,
    pub state: Arc<Mutex<StateStore>>,
    pub state_dir: PathBuf,
    pub upstream_url: String,
    pub upstream_secret: String,
    pub control: Arc<RwLock<UpstreamHandle>>,
    pub control_init_result: Arc<std::sync::RwLock<Value>>,
    pub is_control_connected: Arc<std::sync::atomic::AtomicBool>,
    pub upstream_unpatched_refused: Arc<std::sync::atomic::AtomicBool>,
    pub control_connected_notify: Arc<tokio::sync::Notify>,
    pub control_reconnecting: Arc<std::sync::atomic::AtomicBool>,
    pub reactivation_slot: Arc<Mutex<tokio::time::Instant>>,
    pub upstream_ids: Arc<Mutex<UpstreamIds>>,
    pub pending_control: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    pub sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    pub self_weak: std::sync::RwLock<Option<std::sync::Weak<Hub>>>,
    pub activating: Arc<Mutex<HashMap<String, ActivatingReceiver>>>,
    pub evicting: Arc<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,
    pub in_flight_creations: Arc<std::sync::atomic::AtomicUsize>,
    pub next_actor_id: Arc<std::sync::atomic::AtomicU64>,
}

impl Hub {
    pub async fn start(
        args: Arc<Args>,
        state_store: StateStore,
        upstream_secret: String,
    ) -> anyhow::Result<Arc<Self>> {
        let state_dir = state_store.dir().to_path_buf();
        let boot = state_store.boot;
        let state = Arc::new(Mutex::new(state_store));
        let upstream_url = args.upstream.clone();

        // 1. Collect orphan ids and old control connection from state
        let (orphans, old_control) = {
            let s = state.lock().await;
            (s.orphan_conns.clone(), s.control_conn.clone())
        };

        // 2. DELETE orphan ids and old control connection, best effort
        let upstream_opts = UpstreamOpts {
            allow_production: args.allow_production_upstream,
            ..Default::default()
        };

        let mut to_delete = orphans;
        if let Some(ctrl) = old_control {
            to_delete.push(ctrl);
        }

        for cid in to_delete {
            if let Ok(up) = Upstream::resume_with(
                &upstream_url,
                upstream_secret.clone(),
                cid,
                Recorder::disabled(),
                &upstream_opts,
            ) {
                let _ = up.delete().await;
            }
        }

        // Clear orphan conns and old control connection in state
        {
            let mut s = state.lock().await;
            s.mutate(|st| {
                st.orphan_conns.clear();
                st.control_conn = None;
            })?;
        }

        // Prune ledger at boot per documented policy
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        {
            let mut s = state.lock().await;
            s.prune_ledger(
                now_unix,
                crate::state::LEDGER_PRUNE_TTL_SECS,
                crate::state::LEDGER_PRUNE_MAX_ENTRIES,
            )?;
        }

        // 3. Initialize a new control connection, persisting its id first
        let recorder = if let Some(rec_path) = &args.record {
            let redactor = Redactor::from_env();
            Recorder::file(rec_path, redactor)?
        } else {
            Recorder::disabled()
        };

        let mut control_upstream = Upstream::initialize_with(
            &upstream_url,
            upstream_secret.clone(),
            recorder.clone(),
            &upstream_opts,
        )
        .await
        .context("failed to initialize upstream control connection")?;

        let conn_id = control_upstream.conn_id().to_string();

        // Persist its id first BEFORE subscribing!
        {
            let mut s = state.lock().await;
            s.mutate(|st| {
                st.control_conn = Some(conn_id.clone());
            })?;
        }

        // 4. Subscribe its conn stream with SubscribeOpts { takeover: true, resume_after: None }
        let _ = control_upstream
            .subscribe_with(
                None,
                SubscribeOpts {
                    takeover: true,
                    resume_after: None,
                },
            )
            .await
            .context("failed to subscribe control conn stream")?;

        // 5. Read stream_features()
        let features = control_upstream.stream_features();
        let is_patched = features.map(|f| f.is_patched()).unwrap_or(false);
        if !is_patched && !args.allow_unpatched_upstream {
            anyhow::bail!(
                "upstream goose lacks Acp-Stream-Features takeover+replay; run patched-1.54.0.3 or pass --allow-unpatched-upstream"
            );
        }

        let control_init_result = control_upstream
            .init_result()
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        // 6. Split control connection
        let (control_handle, mut control_rx) = control_upstream.split();
        let pending_control: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let control = Arc::new(RwLock::new(control_handle));
        let control_init_result = Arc::new(std::sync::RwLock::new(control_init_result));
        let is_control_connected = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let upstream_unpatched_refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let control_connected_notify = Arc::new(tokio::sync::Notify::new());
        let control_reconnecting = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reactivation_slot = Arc::new(Mutex::new(
            tokio::time::Instant::now() - Duration::from_millis(500),
        ));

        // 7. Register session handles from state (Task A12 fills this in)

        let hub = Arc::new(Self {
            args,
            state,
            state_dir,
            upstream_url,
            upstream_secret,
            control,
            control_init_result,
            is_control_connected,
            upstream_unpatched_refused,
            control_connected_notify,
            control_reconnecting,
            reactivation_slot,
            upstream_ids: Arc::new(Mutex::new(UpstreamIds::new(boot))),
            pending_control,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            self_weak: std::sync::RwLock::new(None),
            activating: Arc::new(Mutex::new(HashMap::new())),
            evicting: Arc::new(Mutex::new(HashMap::new())),
            in_flight_creations: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            next_actor_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        });

        *hub.self_weak.write().unwrap() = Some(Arc::downgrade(&hub));

        let pending_clone = hub.pending_control.clone();
        let hub_for_rx = hub.clone();
        tokio::spawn(async move {
            while let Some(ev) = control_rx.recv().await {
                match ev {
                    honk_upstream::Event::Frame(_key, frame, _id) => {
                        if let Some(id_val) = frame.get("id") {
                            let mut pending = pending_clone.lock().await;
                            let found_key = pending
                                .keys()
                                .find(|k| {
                                    honk_upstream::rpc_id_matches(&serde_json::json!(k), id_val)
                                })
                                .cloned();
                            if let Some(k) = found_key
                                && let Some(tx) = pending.remove(&k)
                            {
                                let _ = tx.send(frame.clone());
                            }
                        }
                    }
                    honk_upstream::Event::StreamEnded { .. } => {
                        break;
                    }
                }
            }
            hub_for_rx.trigger_control_reconnect();
        });

        let hub_watchdog = hub.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                if !hub_watchdog
                    .is_control_connected
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    continue;
                }
                let dead_secs = Duration::from_secs(hub_watchdog.args.stream_dead_secs);
                let handle = hub_watchdog.control.read().await.clone();
                let is_dead = handle
                    .last_byte_at(&honk_upstream::StreamKey::Conn)
                    .map(|t| t.elapsed() > dead_secs)
                    .unwrap_or(false);
                if is_dead {
                    tracing::warn!(
                        "Control connection watchdog detected dead/half-open stream; reconnecting"
                    );
                    hub_watchdog.trigger_control_reconnect();
                }
            }
        });

        let held_sessions: Vec<(String, crate::state::HeldSession)> = {
            let s = hub.state.lock().await;
            s.sessions
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        let mut session_handles = HashMap::new();

        for (sid, held) in held_sessions {
            let log_path = hub.state_dir.join("sessions").join(format!("{sid}.jsonl"));
            let (session_log, needs_rebuild) = if log_path.exists() {
                match crate::log::SessionLog::open(&log_path) {
                    Ok(l) => (l, false),
                    Err(e) => {
                        tracing::warn!("corrupt or invalid log for {sid}, rebuilding: {e:#}");
                        let l =
                            crate::log::SessionLog::create(&log_path, held.epoch, None, Vec::new())
                                .context("create new log for rebuild")?;
                        (l, true)
                    }
                }
            } else {
                let l = crate::log::SessionLog::create(&log_path, held.epoch, None, Vec::new())
                    .context("create missing log for rebuild")?;
                (l, true)
            };

            let session_upstream = match Upstream::resume_with(
                &hub.upstream_url,
                hub.upstream_secret.clone(),
                held.conn_id.clone(),
                recorder.clone(),
                &upstream_opts,
            ) {
                Ok(u) => u,
                Err(e) => {
                    tracing::error!("failed to resume upstream connection for {sid}: {e:#}");
                    continue;
                }
            };

            let (up_handle, event_rx) = session_upstream.split();
            let (session_tx, session_rx) = tokio::sync::mpsc::channel(1024);

            let last_processed_session = session_log.last_upstream_id();
            let last_processed_conn = held.last_conn_event_id;

            let mut turn = crate::turn::TurnCore {
                client_prompts: held.client_prompts,
                ..Default::default()
            };
            if let Some(inf) = &held.inflight {
                turn.inflight = Some(crate::turn::Inflight::from_persisted(inf.clone()));
            }

            let actor_id = hub
                .next_actor_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let session = crate::session::Session {
                sid: sid.clone(),
                state_dir: hub.state_dir.clone(),
                state_store: hub.state.clone(),
                hub: hub.arc(),
                up: up_handle,
                events: event_rx,
                rx: session_rx,
                self_tx: session_tx.clone(),
                log: session_log,
                turn,
                state: crate::session::ActorState::Live,
                ids: UpstreamIds::with_actor(boot, actor_id),
                last_processed_session_id: last_processed_session,
                last_processed_conn_id: last_processed_conn,
                viewers: HashMap::new(),
                pending_requests: HashMap::new(),
                pending_server_requests: held.pending.clone(),
                recently_resolved: std::collections::HashSet::new(),
                init_result: held
                    .init_result
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({})),
                queued_msgs: std::collections::VecDeque::new(),
                rebuilding_buffered_frames: Vec::new(),
                rebuild_at_turn_end: needs_rebuild,
                suppressed_keys: std::collections::HashSet::new(),
                gap_evidenced: false,
                start_reattach: true,
                needs_rebuild,
                inflight_needs_delivery_check: held.inflight.is_some(),
                finalizing_waiters: HashMap::new(),
                active_probe_id: None,
                gap_synthesis_grace: Duration::from_millis(hub.args.gap_synthesis_grace_ms),
                synthesized_replies: std::collections::HashSet::new(),
                initial_events: Vec::new(),
                gap_synthesis_pending: false,
                reactivating: false,
                reactivation_token: 0,
                rebuild_token: 0,
            };

            tokio::spawn(session.run());

            session_handles.insert(
                sid.clone(),
                SessionHandle {
                    sid: sid.clone(),
                    conn_id: held.conn_id.clone(),
                    tx: session_tx,
                },
            );
        }

        *hub.sessions.write().await = session_handles;

        Ok(hub)
    }

    pub fn arc(&self) -> Arc<Hub> {
        self.self_weak
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .expect("hub alive")
    }

    pub fn handle_initialize(&self, _params: &Value) -> Value {
        let init_val = self.control_init_result.read().unwrap().clone();
        let mut result_obj = match init_val.get("result") {
            Some(res) => res.clone(),
            None => init_val,
        };
        if !result_obj.is_object() {
            result_obj = serde_json::json!({});
        }

        if let Some(meta_val) = result_obj.get_mut("_meta").and_then(|v| v.as_object_mut()) {
            meta_val.insert(
                "honk".to_string(),
                serde_json::json!({
                    "protocol": honk_wire::PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                }),
            );
        } else {
            result_obj["_meta"] = serde_json::json!({
                "honk": {
                    "protocol": honk_wire::PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                }
            });
        }

        result_obj
    }

    pub fn trigger_control_reconnect(&self) {
        if self
            .control_reconnecting
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return;
        }
        self.is_control_connected
            .store(false, std::sync::atomic::Ordering::SeqCst);

        // Drain any pending control oneshots so callers don't wait for 30s timeout
        let pending = self.pending_control.clone();
        tokio::spawn(async move {
            let mut p = pending.lock().await;
            p.clear();
        });

        let hub = self.arc();
        tokio::spawn(async move {
            hub.run_control_reconnect().await;
        });
    }

    async fn run_control_reconnect(&self) {
        tracing::warn!("Upstream control connection lost; re-initializing with backoff");
        let mut backoff = Duration::from_millis(200);
        let max_backoff = Duration::from_secs(5);

        let upstream_opts = UpstreamOpts {
            allow_production: self.args.allow_production_upstream,
            ..Default::default()
        };

        loop {
            let recorder = if let Some(rec_path) = &self.args.record {
                let redactor = Redactor::from_env();
                Recorder::file(rec_path, redactor).unwrap_or_else(|_| Recorder::disabled())
            } else {
                Recorder::disabled()
            };

            let init_res = Upstream::initialize_with(
                &self.upstream_url,
                self.upstream_secret.clone(),
                recorder,
                &upstream_opts,
            )
            .await;

            let mut control_upstream = match init_res {
                Ok(u) => u,
                Err(e) => {
                    tracing::debug!(
                        "control initialize_with failed: {e:#}; retrying in {backoff:?}"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            };

            let new_conn_id = control_upstream.conn_id().to_string();

            // Persist new control conn id BEFORE subscribing
            {
                let mut s = self.state.lock().await;
                if let Err(e) = s.mutate(|st| {
                    st.control_conn = Some(new_conn_id.clone());
                }) {
                    tracing::error!("failed to persist reconnected control conn in state: {e:#}");
                }
            }

            // Subscribe conn stream
            let sub_res = control_upstream
                .subscribe_with(
                    None,
                    SubscribeOpts {
                        takeover: true,
                        resume_after: None,
                    },
                )
                .await;

            if let Err(e) = sub_res {
                tracing::debug!("control subscribe_with failed: {e:#}; retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }

            // Re-check Acp-Stream-Features
            let features = control_upstream.stream_features();
            let is_patched = features.map(|f| f.is_patched()).unwrap_or(false);
            if !is_patched && !self.args.allow_unpatched_upstream {
                self.upstream_unpatched_refused
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::error!("upstream goose is no longer patched");
            } else {
                self.upstream_unpatched_refused
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }

            let new_init_res = control_upstream
                .init_result()
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));

            let (control_handle, mut control_rx) = control_upstream.split();

            *self.control.write().await = control_handle;
            *self.control_init_result.write().unwrap() = new_init_res;
            self.is_control_connected
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.control_reconnecting
                .store(false, std::sync::atomic::Ordering::SeqCst);
            self.control_connected_notify.notify_waiters();
            tracing::info!("Upstream control connection re-initialized successfully");

            let pending_clone = self.pending_control.clone();
            let hub_clone = self.arc();
            tokio::spawn(async move {
                while let Some(ev) = control_rx.recv().await {
                    match ev {
                        honk_upstream::Event::Frame(_key, frame, _id) => {
                            if let Some(id_val) = frame.get("id") {
                                let mut pending = pending_clone.lock().await;
                                let found_key = pending
                                    .keys()
                                    .find(|k| {
                                        honk_upstream::rpc_id_matches(&serde_json::json!(k), id_val)
                                    })
                                    .cloned();
                                if let Some(k) = found_key
                                    && let Some(tx) = pending.remove(&k)
                                {
                                    let _ = tx.send(frame.clone());
                                }
                            }
                        }
                        honk_upstream::Event::StreamEnded { .. } => {
                            break;
                        }
                    }
                }
                hub_clone.trigger_control_reconnect();
            });

            break;
        }
    }

    pub async fn wait_for_control_connected(&self) {
        loop {
            if self
                .is_control_connected
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return;
            }
            let notified = self.control_connected_notify.notified();
            if self
                .is_control_connected
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return;
            }
            notified.await;
        }
    }

    pub async fn acquire_reactivation_slot(&self) {
        let mut last_slot = self.reactivation_slot.lock().await;
        let now = tokio::time::Instant::now();
        let target = (*last_slot + Duration::from_millis(500)).max(now);
        *last_slot = target;
        drop(last_slot);
        if target > now {
            tokio::time::sleep_until(target).await;
        }
    }

    pub async fn get_stored_cwd(&self, sid: &str) -> Result<String, ActivationError> {
        let list_res = self
            .control_request("session/list", json!({}), json!(1))
            .await
            .map_err(|e| ActivationError {
                code: -32603,
                message: format!("control session/list failed: {e:#}"),
            })?;

        let sessions = list_res
            .pointer("/result/sessions")
            .and_then(Value::as_array);
        let stored_cwd = sessions
            .and_then(|arr| arr.iter().find(|s| s["sessionId"] == sid))
            .and_then(|s| s["cwd"].as_str())
            .map(|s| s.to_string());

        stored_cwd.ok_or_else(|| ActivationError {
            code: -32602,
            message: format!("Session not found: {sid}"),
        })
    }

    pub async fn control_request(
        &self,
        method: &str,
        mut params: Value,
        downstream_id: Value,
    ) -> anyhow::Result<Value> {
        if !self
            .is_control_connected
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("control connection is down");
        }

        let up_id = self.upstream_ids.lock().await.next();
        let (tx, rx) = oneshot::channel();
        self.pending_control.lock().await.insert(up_id.clone(), tx);

        crate::rpc::strip_honk_meta(&mut params);

        let control_handle = self.control.read().await.clone();
        if let Err(e) = control_handle
            .request_with_id(serde_json::json!(up_id), method, params, None)
            .await
        {
            self.pending_control.lock().await.remove(&up_id);
            if honk_upstream::is_gone(&e) || honk_upstream::is_connect(&e) {
                self.trigger_control_reconnect();
            }
            return Err(e).context("failed to send control request");
        }

        let mut resp = match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(f)) => f,
            Ok(Err(_)) => {
                anyhow::bail!("control response channel closed");
            }
            Err(_) => {
                self.pending_control.lock().await.remove(&up_id);
                anyhow::bail!("control request timed out waiting for upstream reply");
            }
        };

        resp["id"] = downstream_id;

        if method == "session/list" {
            let held_sids: std::collections::HashSet<String> = {
                let s = self.state.lock().await;
                s.sessions.keys().cloned().collect()
            };

            if let Some(sessions_arr) = resp
                .pointer_mut("/result/sessions")
                .and_then(Value::as_array_mut)
            {
                for entry in sessions_arr {
                    let sid = entry.get("sessionId").and_then(Value::as_str).unwrap_or("");
                    let is_held = held_sids.contains(sid);
                    let is_running = self.is_session_running(sid);
                    if let Some(meta_obj) = entry.get_mut("_meta").and_then(Value::as_object_mut) {
                        meta_obj.insert(
                            "honk".to_string(),
                            serde_json::json!({ "held": is_held, "running": is_running }),
                        );
                    } else {
                        entry["_meta"] = serde_json::json!({
                            "honk": {
                                "held": is_held,
                                "running": is_running,
                            }
                        });
                    }
                }
            }
        }

        Ok(resp)
    }

    pub fn is_session_running(&self, _sid: &str) -> bool {
        // Wired for actor lookup in A9; returns false for now
        false
    }

    pub async fn export(&self, sid: &str) -> anyhow::Result<Value> {
        let init_opts = UpstreamOpts {
            allow_production: self.args.allow_production_upstream,
            client_capabilities: serde_json::json!({
                "fs": {
                    "readTextFile": false,
                    "writeTextFile": false
                },
                "terminal": false
            }),
        };

        let recorder = if let Some(rec_path) = &self.args.record {
            let redactor = Redactor::from_env();
            Recorder::file(rec_path, redactor)?
        } else {
            Recorder::disabled()
        };

        let mut side_up = Upstream::initialize_with(
            &self.upstream_url,
            self.upstream_secret.clone(),
            recorder,
            &init_opts,
        )
        .await
        .context("failed to initialize export side connection")?;

        let conn_id = side_up.conn_id().to_string();

        // 1. Persist orphan first
        {
            let mut s = self.state.lock().await;
            s.mutate(|st| {
                st.orphan_conns.push(conn_id.clone());
            })?;
        }

        // Guaranteed cleanup on every path after initialize
        let export_result: anyhow::Result<Value> = async {
            side_up
                .subscribe_with(
                    Some(sid),
                    SubscribeOpts {
                        takeover: false,
                        resume_after: None,
                    },
                )
                .await
                .context("failed to subscribe export session stream")?;

            let export_res = side_up
                .call(
                    "_goose/unstable/session/export",
                    serde_json::json!({
                        "sessionId": sid,
                        "format": "json"
                    }),
                    Some(sid),
                    Duration::from_secs(30),
                )
                .await
                .context("failed to call export on side connection")?;

            let data_str = export_res
                .get("data")
                .and_then(Value::as_str)
                .context("export missing data string")?;
            let session_val: Value =
                serde_json::from_str(data_str).context("failed to parse export data JSON")?;
            Ok(session_val)
        }
        .await;

        // Guaranteed cleanup: DELETE + orphan removal
        let _ = side_up.delete().await;
        {
            let mut s = self.state.lock().await;
            let _ = s.mutate(|st| {
                st.orphan_conns.retain(|id| id != &conn_id);
            });
        }

        export_result
    }

    pub async fn create_session(&self, params: Value) -> anyhow::Result<Value> {
        if !self
            .is_control_connected
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("upstream control connection is down");
        }

        struct CreatingGuard(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for CreatingGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let _creating_guard = {
            let activating = self.activating.lock().await;
            let held_count = self.sessions.read().await.len();
            let activating_count = activating.len();
            let creating_count = self
                .in_flight_creations
                .load(std::sync::atomic::Ordering::SeqCst);
            if held_count + activating_count + creating_count >= self.args.max_sessions {
                anyhow::bail!("honkd: held-session limit reached");
            }
            self.in_flight_creations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            CreatingGuard(self.in_flight_creations.clone())
        };

        // 1. Upstream::initialize_with
        let init_opts = UpstreamOpts {
            allow_production: self.args.allow_production_upstream,
            client_capabilities: serde_json::json!({
                "fs": {
                    "readTextFile": false,
                    "writeTextFile": false
                },
                "terminal": false
            }),
        };
        let recorder = if let Some(rec_path) = &self.args.record {
            let redactor = Redactor::from_env();
            Recorder::file(rec_path, redactor)?
        } else {
            Recorder::disabled()
        };
        let mut session_upstream = Upstream::initialize_with(
            &self.upstream_url,
            self.upstream_secret.clone(),
            recorder,
            &init_opts,
        )
        .await
        .context("failed to initialize session upstream")?;
        let conn_id = session_upstream.conn_id().to_string();

        // 2. Persist orphan BEFORE first subscribe (A15)
        {
            let mut s = self.state.lock().await;
            s.mutate(|st| {
                st.orphan_conns.push(conn_id.clone());
            })?;
        }

        // 3. Subscribe to the conn stream with takeover
        session_upstream
            .subscribe_with(
                None,
                SubscribeOpts {
                    takeover: true,
                    resume_after: None,
                },
            )
            .await
            .context("failed to subscribe session conn stream")?;

        // 4. call("session/new", strip_honk_meta(params))
        let mut clean_params = params.clone();
        crate::rpc::strip_honk_meta(&mut clean_params);
        let new_res = session_upstream
            .call("session/new", clean_params, None, Duration::from_secs(30))
            .await
            .context("failed to call session/new")?;

        let sid = new_res
            .get("sessionId")
            .and_then(Value::as_str)
            .context("session/new missing sessionId")?
            .to_string();
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // 5. Subscribe to session stream with takeover and resume_after: Some(0)
        // Note: In patched goose (vendor/agent-client-protocol-http/src/http_mailbox.rs),
        // sequence numbers start at 1 (next_seq = 1, trimmed_through = 0).
        // Calling resume_after(0) acknowledges nothing and replays all retained frames
        // from seq 1 onwards. For a brand-new session stream, this guarantees all initial
        // frames are replayed without creating a gap.
        session_upstream
            .subscribe_with(
                Some(&sid),
                SubscribeOpts {
                    takeover: true,
                    resume_after: Some(0),
                },
            )
            .await
            .context("failed to subscribe session stream")?;

        // 6. SessionLog::create(sessions/<sid>.jsonl, 1, None, []) BEFORE state mutate
        let log_path = self.state_dir.join("sessions").join(format!("{sid}.jsonl"));
        let session_log = crate::log::SessionLog::create(&log_path, 1, None, Vec::new())
            .context("failed to create session log")?;

        // 7. In one state.mutate: remove orphan; insert HeldSession
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        {
            let mut s = self.state.lock().await;
            s.mutate(|st| {
                st.orphan_conns.retain(|id| id != &conn_id);
                st.sessions.insert(
                    sid.clone(),
                    crate::state::HeldSession {
                        conn_id: conn_id.clone(),
                        cwd: cwd.clone(),
                        epoch: 1,
                        last_conn_event_id: None,
                        pending: Vec::new(),
                        inflight: None,
                        last_active_unix: now,
                        init_result: Some(new_res.clone()),
                        client_prompts: 0,
                    },
                );
            })?;
        }

        // Clean any old replies sidecar for this session ID
        let _ = std::fs::remove_file(
            self.state_dir
                .join("sessions")
                .join(format!("{sid}.replies.jsonl")),
        );

        // 8. Spawn actor
        let (up_handle, event_rx) = session_upstream.split();
        let (session_tx, session_rx) = tokio::sync::mpsc::channel(1024);

        let boot = { self.state.lock().await.boot };
        let actor_id = self
            .next_actor_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let last_processed_session = session_log.last_upstream_id();
        let last_processed_conn = {
            let s = self.state.lock().await;
            s.sessions.get(&sid).and_then(|h| h.last_conn_event_id)
        };

        let session = crate::session::Session {
            sid: sid.clone(),
            state_dir: self.state_dir.clone(),
            state_store: self.state.clone(),
            hub: self.arc(),
            up: up_handle,
            events: event_rx,
            rx: session_rx,
            self_tx: session_tx.clone(),
            log: session_log,
            turn: crate::turn::TurnCore::default(),
            state: crate::session::ActorState::Live,
            ids: UpstreamIds::with_actor(boot, actor_id),
            last_processed_session_id: last_processed_session,
            last_processed_conn_id: last_processed_conn,
            viewers: HashMap::new(),
            pending_requests: HashMap::new(),
            pending_server_requests: Vec::new(),
            recently_resolved: std::collections::HashSet::new(),
            init_result: new_res.clone(),
            queued_msgs: std::collections::VecDeque::new(),
            rebuilding_buffered_frames: Vec::new(),
            rebuild_at_turn_end: false,
            suppressed_keys: std::collections::HashSet::new(),
            gap_evidenced: false,
            start_reattach: false,
            needs_rebuild: false,
            inflight_needs_delivery_check: false,
            finalizing_waiters: HashMap::new(),
            active_probe_id: None,
            gap_synthesis_grace: Duration::from_millis(self.args.gap_synthesis_grace_ms),
            synthesized_replies: std::collections::HashSet::new(),
            initial_events: Vec::new(),
            gap_synthesis_pending: false,
            reactivating: false,
            reactivation_token: 0,
            rebuild_token: 0,
        };

        tokio::spawn(session.run());

        self.sessions.write().await.insert(
            sid.clone(),
            SessionHandle {
                sid: sid.clone(),
                conn_id: conn_id.clone(),
                tx: session_tx,
            },
        );

        Ok(new_res)
    }

    pub async fn begin_eviction(&self, sid: &str) -> Arc<tokio::sync::Notify> {
        let mut evicting = self.evicting.lock().await;
        let notify = Arc::new(tokio::sync::Notify::new());
        evicting.insert(sid.to_string(), notify.clone());
        notify
    }

    pub async fn finish_eviction(&self, sid: &str, notify: Arc<tokio::sync::Notify>) {
        {
            let mut evicting = self.evicting.lock().await;
            evicting.remove(sid);
        }
        notify.notify_waiters();
    }

    pub async fn get_or_activate_session(
        &self,
        sid: &str,
    ) -> Result<SessionHandle, ActivationError> {
        loop {
            // 1. If held, return handle
            if let Some(h) = self.sessions.read().await.get(sid).cloned() {
                return Ok(h);
            }

            // 2. If evicting, wait for eviction to complete
            let evict_notify = {
                let evicting = self.evicting.lock().await;
                evicting.get(sid).cloned()
            };
            if let Some(notify) = evict_notify {
                notify.notified().await;
                continue;
            }

            break;
        }

        // 3. Coalesce concurrent activations
        let mut rx = {
            let mut activating = self.activating.lock().await;
            if let Some(h) = self.sessions.read().await.get(sid).cloned() {
                return Ok(h);
            }

            if let Some(rx) = activating.get(sid) {
                rx.clone()
            } else {
                let held_count = self.sessions.read().await.len();
                let activating_count = activating.len();
                let creating_count = self
                    .in_flight_creations
                    .load(std::sync::atomic::Ordering::SeqCst);
                if held_count + activating_count + creating_count >= self.args.max_sessions {
                    return Err(ActivationError {
                        code: -32000,
                        message: "honkd: held-session limit reached".to_string(),
                    });
                }

                let (tx, rx) = tokio::sync::watch::channel(None);
                activating.insert(sid.to_string(), rx.clone());

                let hub = self.arc();
                let sid_clone = sid.to_string();

                struct ActivationGuard {
                    sid: String,
                    activating: Arc<Mutex<HashMap<String, ActivatingReceiver>>>,
                }
                impl Drop for ActivationGuard {
                    fn drop(&mut self) {
                        let sid = self.sid.clone();
                        let activating = self.activating.clone();
                        if let Ok(mut map) = activating.try_lock() {
                            map.remove(&sid);
                        } else {
                            tokio::spawn(async move {
                                activating.lock().await.remove(&sid);
                            });
                        }
                    }
                }

                tokio::spawn(async move {
                    let _guard = ActivationGuard {
                        sid: sid_clone.clone(),
                        activating: hub.activating.clone(),
                    };
                    let res = hub.activate_session_inner(&sid_clone).await;
                    let _ = tx.send(Some(res));
                });

                rx
            }
        };

        loop {
            if let Some(res) = rx.borrow().as_ref() {
                return res.clone();
            }
            if rx.changed().await.is_err() {
                return Err(ActivationError {
                    code: -32603,
                    message: "Activation task aborted".into(),
                });
            }
        }
    }

    async fn activate_session_inner(&self, sid: &str) -> Result<SessionHandle, ActivationError> {
        // Step 1: Find cwd in session/list on control connection
        let stored_cwd = self.get_stored_cwd(sid).await?;

        // Step 2: Create a new connection, persisting its id first
        let init_opts = UpstreamOpts {
            allow_production: self.args.allow_production_upstream,
            client_capabilities: serde_json::json!({
                "fs": {
                    "readTextFile": false,
                    "writeTextFile": false
                },
                "terminal": false
            }),
        };
        let recorder = if let Some(rec_path) = &self.args.record {
            let redactor = Redactor::from_env();
            Recorder::file(rec_path, redactor).map_err(|e| ActivationError {
                code: -32603,
                message: format!("recorder init failed: {e:#}"),
            })?
        } else {
            Recorder::disabled()
        };

        let mut session_upstream = Upstream::initialize_with(
            &self.upstream_url,
            self.upstream_secret.clone(),
            recorder,
            &init_opts,
        )
        .await
        .map_err(|e| ActivationError {
            code: -32603,
            message: format!("failed to initialize upstream connection: {e:#}"),
        })?;

        let conn_id = session_upstream.conn_id().to_string();

        {
            let mut s = self.state.lock().await;
            s.mutate(|st| {
                st.orphan_conns.push(conn_id.clone());
            })
            .map_err(|e| ActivationError {
                code: -32603,
                message: format!("failed to persist orphan conn: {e:#}"),
            })?;
        }

        let hub = self.arc();
        let sid_str = sid.to_string();
        let conn_id_clone = conn_id.clone();
        let stored_cwd_clone = stored_cwd.clone();

        let activation_res: Result<SessionHandle, ActivationError> = async {
            // Step 3: Subscribe both streams (takeover, resume_after: Some(0))
            session_upstream
                .subscribe_with(
                    None,
                    SubscribeOpts {
                        takeover: true,
                        resume_after: Some(0),
                    },
                )
                .await
                .map_err(|e| ActivationError {
                    code: -32603,
                    message: format!("failed to subscribe conn stream: {e:#}"),
                })?;

            session_upstream
                .subscribe_with(
                    Some(&sid_str),
                    SubscribeOpts {
                        takeover: true,
                        resume_after: Some(0),
                    },
                )
                .await
                .map_err(|e| ActivationError {
                    code: -32603,
                    message: format!("failed to subscribe session stream: {e:#}"),
                })?;

            // Step 4: session/load {sessionId, cwd: stored, mcpServers: []}; discard its replay frames (C1)
            let load_reply = session_upstream
                .call(
                    "session/load",
                    json!({
                        "sessionId": &sid_str,
                        "cwd": stored_cwd_clone,
                        "mcpServers": []
                    }),
                    Some(&sid_str),
                    Duration::from_secs(30),
                )
                .await
                .map_err(|e| ActivationError {
                    code: -32603,
                    message: format!("failed to call session/load: {e:#}"),
                })?;

            let (up_handle, mut event_rx) = session_upstream.split();

            // C1: Drain frames arrived before load reply.
            // Discard session stream notifications (load replay).
            // Retain server requests (Kind::Request) and conn stream events in initial_events.
            let mut initial_events = Vec::new();
            while let Ok(ev) = event_rx.try_recv() {
                if let honk_upstream::Event::Frame(key, frame, _) = &ev
                    && key == &honk_upstream::StreamKey::Session(sid_str.clone())
                    && crate::rpc::kind(frame) == crate::rpc::Kind::Notification
                {
                    continue;
                }
                initial_events.push(ev);
            }

            // Step 5: Export on a side connection -> new generation
            let export_val = hub.export(&sid_str).await.map_err(|e| ActivationError {
                code: -32603,
                message: format!("export failed during activation: {e:#}"),
            })?;

            let frames = honk_wire::export::to_updates(&export_val, &sid_str);
            let mut keys = std::collections::HashSet::new();
            for f in &frames {
                if let Some(k) = f.pointer("/params/update").and_then(honk_wire::block_key) {
                    keys.insert(k);
                }
            }

            let (new_log, next_epoch) = crate::session::create_new_generation_from_export(
                &sid_str,
                &hub.state_dir,
                &hub.state,
                frames,
                keys.clone(),
                Vec::new(),
                None,
                None,
            )
            .await
            .map_err(|e| ActivationError {
                code: -32603,
                message: format!("failed to create session log: {e:#}"),
            })?;

            let client_prompts = honk_wire::export::client_prompts(&export_val).len();

            // Step 6: The session is now held
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            {
                let mut s = hub.state.lock().await;
                s.mutate(|st| {
                    st.orphan_conns.retain(|id| id != &conn_id_clone);
                    st.retired_epochs.remove(&sid_str);
                    st.sessions.insert(
                        sid_str.clone(),
                        crate::state::HeldSession {
                            conn_id: conn_id_clone.clone(),
                            cwd: stored_cwd_clone,
                            epoch: next_epoch,
                            last_conn_event_id: None,
                            pending: Vec::new(),
                            inflight: None,
                            last_active_unix: now,
                            init_result: Some(load_reply.clone()),
                            client_prompts,
                        },
                    );
                })
                .map_err(|e| ActivationError {
                    code: -32603,
                    message: format!("state mutate failed during activation: {e:#}"),
                })?;
            }

            // Clean any old replies sidecar for this session ID
            let _ = std::fs::remove_file(
                hub.state_dir
                    .join("sessions")
                    .join(format!("{sid_str}.replies.jsonl")),
            );

            let (session_tx, session_rx) = tokio::sync::mpsc::channel(1024);
            let boot = { hub.state.lock().await.boot };
            let actor_id = hub
                .next_actor_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let last_processed_session = new_log.last_upstream_id();

            let session = crate::session::Session {
                sid: sid_str.clone(),
                state_dir: hub.state_dir.clone(),
                state_store: hub.state.clone(),
                hub: hub.clone(),
                up: up_handle,
                events: event_rx,
                rx: session_rx,
                self_tx: session_tx.clone(),
                log: new_log,
                turn: crate::turn::TurnCore {
                    client_prompts,
                    ..Default::default()
                },
                state: crate::session::ActorState::Live,
                ids: UpstreamIds::with_actor(boot, actor_id),
                last_processed_session_id: last_processed_session,
                last_processed_conn_id: None,
                viewers: HashMap::new(),
                pending_requests: HashMap::new(),
                pending_server_requests: Vec::new(),
                recently_resolved: std::collections::HashSet::new(),
                init_result: load_reply.clone(),
                queued_msgs: std::collections::VecDeque::new(),
                rebuilding_buffered_frames: Vec::new(),
                rebuild_at_turn_end: false,
                suppressed_keys: keys,
                gap_evidenced: false,
                start_reattach: false,
                needs_rebuild: false,
                inflight_needs_delivery_check: false,
                finalizing_waiters: HashMap::new(),
                active_probe_id: None,
                gap_synthesis_grace: Duration::from_millis(hub.args.gap_synthesis_grace_ms),
                synthesized_replies: std::collections::HashSet::new(),
                initial_events,
                gap_synthesis_pending: false,
                reactivating: false,
                reactivation_token: 0,
                rebuild_token: 0,
            };

            tokio::spawn(session.run());

            let handle = SessionHandle {
                sid: sid_str.clone(),
                conn_id: conn_id_clone.clone(),
                tx: session_tx,
            };

            hub.sessions
                .write()
                .await
                .insert(sid_str.clone(), handle.clone());
            Ok(handle)
        }
        .await;

        match activation_res {
            Ok(handle) => Ok(handle),
            Err(e) => {
                let upstream_opts = UpstreamOpts {
                    allow_production: self.args.allow_production_upstream,
                    ..Default::default()
                };
                if let Ok(up) = Upstream::resume_with(
                    &self.upstream_url,
                    self.upstream_secret.clone(),
                    conn_id.clone(),
                    Recorder::disabled(),
                    &upstream_opts,
                ) && up.delete().await.is_ok()
                {
                    let mut s = self.state.lock().await;
                    let _ = s.mutate(|st| {
                        st.orphan_conns.retain(|id| id != &conn_id);
                    });
                }
                Err(e)
            }
        }
    }
}
