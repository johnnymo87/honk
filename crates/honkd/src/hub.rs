use anyhow::Context;
use honk_upstream::{Recorder, Redactor, SubscribeOpts, Upstream, UpstreamHandle, UpstreamOpts};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, oneshot};

use crate::config::Args;
use crate::rpc::UpstreamIds;
use crate::state::StateStore;

#[derive(Clone, Debug)]
pub struct SessionHandle {
    pub sid: String,
    pub conn_id: String,
    pub tx: tokio::sync::mpsc::Sender<crate::session::SessionMsg>,
}

pub struct Hub {
    pub args: Arc<Args>,
    pub state: Arc<Mutex<StateStore>>,
    pub state_dir: PathBuf,
    pub upstream_url: String,
    pub upstream_secret: String,
    pub control: UpstreamHandle,
    pub control_init_result: Value,
    pub upstream_ids: Arc<Mutex<UpstreamIds>>,
    pub pending_control: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    pub sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    pub self_weak: std::sync::RwLock<Option<std::sync::Weak<Hub>>>,
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
        let pending_clone = pending_control.clone();

        tokio::spawn(async move {
            while let Some(ev) = control_rx.recv().await {
                if let Some(frame) = ev.frame()
                    && let Some(id_val) = frame.get("id")
                {
                    let mut pending = pending_clone.lock().await;
                    let found_key = pending
                        .keys()
                        .find(|k| honk_upstream::rpc_id_matches(&serde_json::json!(k), id_val))
                        .cloned();
                    if let Some(k) = found_key
                        && let Some(tx) = pending.remove(&k)
                    {
                        let _ = tx.send(frame.clone());
                    }
                }
            }
        });

        // 7. Register session handles from state (Task A12 fills this in)

        let hub = Arc::new(Self {
            args,
            state,
            state_dir,
            upstream_url,
            upstream_secret,
            control: control_handle,
            control_init_result,
            upstream_ids: Arc::new(Mutex::new(UpstreamIds::new(boot))),
            pending_control,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            self_weak: std::sync::RwLock::new(None),
        });

        *hub.self_weak.write().unwrap() = Some(Arc::downgrade(&hub));

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

            let mut turn = crate::turn::TurnCore::default();
            if let Some(inf) = &held.inflight {
                turn.inflight = Some(crate::turn::Inflight::from_persisted(inf.clone()));
            }

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
                ids: UpstreamIds::new(boot),
                last_processed_session_id: last_processed_session,
                last_processed_conn_id: last_processed_conn,
                viewers: HashMap::new(),
                pending_requests: HashMap::new(),
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
        let mut result_obj = match self.control_init_result.get("result") {
            Some(res) => res.clone(),
            None => self.control_init_result.clone(),
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

    pub async fn control_request(
        &self,
        method: &str,
        mut params: Value,
        downstream_id: Value,
    ) -> anyhow::Result<Value> {
        let up_id = self.upstream_ids.lock().await.next();
        let (tx, rx) = oneshot::channel();
        self.pending_control.lock().await.insert(up_id.clone(), tx);

        crate::rpc::strip_honk_meta(&mut params);

        if let Err(e) = self
            .control
            .request_with_id(serde_json::json!(up_id), method, params, None)
            .await
        {
            self.pending_control.lock().await.remove(&up_id);
            return Err(e).context("failed to send control request");
        }

        let mut resp = match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(f)) => f,
            Ok(Err(_)) => anyhow::bail!("control response channel closed"),
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
                    },
                );
            })?;
        }

        // 8. Spawn actor
        let (up_handle, event_rx) = session_upstream.split();
        let (session_tx, session_rx) = tokio::sync::mpsc::channel(1024);

        let boot = { self.state.lock().await.boot };
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
            ids: UpstreamIds::new(boot),
            last_processed_session_id: last_processed_session,
            last_processed_conn_id: last_processed_conn,
            viewers: HashMap::new(),
            pending_requests: HashMap::new(),
            init_result: new_res.clone(),
            queued_msgs: std::collections::VecDeque::new(),
            rebuilding_buffered_frames: Vec::new(),
            rebuild_at_turn_end: false,
            suppressed_keys: std::collections::HashSet::new(),
            gap_evidenced: false,
            start_reattach: false,
            needs_rebuild: false,
            inflight_needs_delivery_check: false,
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
}
