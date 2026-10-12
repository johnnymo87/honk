use honk_upstream::{
    Event, EventRx, Recorder, Redactor, ReplayOutcome, StreamKey, SubscribeOpts, Upstream,
    UpstreamHandle, UpstreamOpts,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

use crate::hub::Hub;
use crate::log::SessionLog;
use crate::rpc::UpstreamIds;
use crate::turn::{Action, OwnerRef, Reply, TurnCore};
use crate::viewer::{ViewerId, ViewerTx};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorState {
    Live,
    Reattaching,
    Rebuilding,
    Gone,
}

pub struct PendingSessionRequest {
    pub viewer_id: ViewerId,
    pub downstream_id: Value,
    pub is_steer: bool,
}

pub struct ReactivationData {
    pub conn_id: String,
    pub up_handle: UpstreamHandle,
    pub event_rx: EventRx,
    pub new_log: SessionLog,
    pub next_epoch: u64,
    pub load_reply: Value,
    pub keys: HashSet<honk_wire::BlockKey>,
    pub initial_events: Vec<Event>,
    pub client_prompts: usize,
}

pub enum SessionMsg {
    // from viewers / hub
    Attach {
        viewer_id: ViewerId,
        viewer: ViewerTx,
        after: Option<honk_wire::Cursor>,
        load_id: Value,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    AttachLive {
        viewer_id: ViewerId,
        viewer: ViewerTx,
        is_honk_aware: bool,
    },
    Detach {
        viewer_id: ViewerId,
    },
    Prompt {
        viewer_id: ViewerId,
        viewer: ViewerTx,
        is_honk_aware: bool,
        id: Value,
        params: Value,
    },
    Steer {
        viewer_id: ViewerId,
        viewer: ViewerTx,
        id: Value,
        params: Value,
    },
    Cancel,
    Answer {
        viewer_id: ViewerId,
        frame: Value,
    },
    Forward {
        viewer_id: ViewerId,
        viewer: ViewerTx,
        id: Value,
        method: String,
        params: Value,
    },
    // from spawned recovery / helper tasks (I1)
    Reattached {
        outcome: honk_upstream::SubscribeOutcome,
    },
    ReattachFailed {
        gone: bool,
        error: String,
    },
    RetryReattach,
    Rebuilt {
        token: u64,
        frames: Vec<Value>,
        keys: std::collections::HashSet<honk_wire::BlockKey>,
        resume_from: Option<u64>,
        client_prompts: usize,
    },
    ReplyText {
        owners: Vec<OwnerRef>,
        body: Reply,
        text: Option<String>,
    },
    SynthesizeGapEndTurn {
        upstream_id: String,
    },
    AmbiguousCheckPoll {
        token: u64,
        upstream_id: String,
        is_last: bool,
    },
    AmbiguousExportResult {
        token: u64,
        upstream_id: String,
        is_last: bool,
        result: anyhow::Result<Value>,
    },
    ProbeTimeout {
        probe_id: String,
    },
    MarkPendingStale,
    ReactivationComplete {
        token: u64,
        data: Box<ReactivationData>,
    },
    ReactivationFailed {
        token: u64,
        reason: String,
        permanent: bool,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum PromptPostFailure {
    Gone,
    CertainlyNotStarted,
    Ambiguous,
}

fn classify_prompt_post_error(err: &anyhow::Error) -> PromptPostFailure {
    if honk_upstream::is_gone(err) {
        return PromptPostFailure::Gone;
    }

    // Goose answers prompt POST with 202 Accepted and the reply arrives on the session stream.
    // Therefore, any 4xx/5xx HTTP status means goose rejected the POST (prompt certainly not started).
    if honk_upstream::status_of(err).is_some() {
        return PromptPostFailure::CertainlyNotStarted;
    }

    // Connection errors before sending (connection refused, connect timeout, DNS failure).
    // The request was never sent over TCP.
    if honk_upstream::is_connect(err) {
        return PromptPostFailure::CertainlyNotStarted;
    }

    // Any transport error after sending (timeout waiting for 202, TCP reset/drop after body read).
    // The prompt may have reached goose and started executing.
    PromptPostFailure::Ambiguous
}

pub struct Session {
    pub sid: String,
    pub state_dir: PathBuf,
    pub state_store: Arc<Mutex<crate::state::StateStore>>,
    pub hub: Arc<Hub>,
    pub up: UpstreamHandle,
    pub events: EventRx,
    pub rx: mpsc::Receiver<SessionMsg>,
    pub self_tx: mpsc::Sender<SessionMsg>,
    pub log: SessionLog,
    pub turn: TurnCore,
    pub state: ActorState,
    pub ids: UpstreamIds,
    pub last_processed_session_id: Option<u64>,
    pub last_processed_conn_id: Option<u64>,
    pub viewers: HashMap<ViewerId, (ViewerTx, bool)>,
    pub pending_requests: HashMap<String, PendingSessionRequest>,
    pub pending_server_requests: Vec<crate::state::PendingRequest>,
    pub recently_resolved: HashSet<String>,
    pub init_result: Value,
    pub queued_msgs: VecDeque<SessionMsg>,
    pub rebuilding_buffered_frames: Vec<(Option<u64>, Value)>,
    pub rebuild_at_turn_end: bool,
    pub suppressed_keys: HashSet<honk_wire::BlockKey>,
    pub gap_evidenced: bool,
    pub start_reattach: bool,
    pub needs_rebuild: bool,
    pub inflight_needs_delivery_check: bool,
    pub finalizing_waiters: HashMap<String, Vec<OwnerRef>>,
    pub active_probe_id: Option<String>,
    pub gap_synthesis_grace: Duration,
    pub synthesized_replies: HashSet<String>,
    pub initial_events: Vec<honk_upstream::Event>,
    pub gap_synthesis_pending: bool,
    pub reactivating: bool,
    pub reactivation_token: u64,
    pub rebuild_token: u64,
    pub ambiguous_check_token: u64,
    pub ambiguous_window_expired: bool,
    pub ambiguous_export_in_flight: bool,
}

#[allow(clippy::too_many_arguments)]
pub async fn create_new_generation_from_export(
    sid: &str,
    state_dir: &Path,
    state_store: &Arc<Mutex<crate::state::StateStore>>,
    frames: Vec<Value>,
    keys: HashSet<honk_wire::BlockKey>,
    buffered_frames: Vec<(Option<u64>, Value)>,
    resume_from: Option<u64>,
    last_processed_session_id: Option<u64>,
) -> anyhow::Result<(SessionLog, u64)> {
    let log_path = state_dir.join("sessions").join(format!("{sid}.jsonl"));
    let log_header_epoch = if log_path.exists() {
        crate::log::read_header_epoch(&log_path).unwrap_or(0)
    } else {
        0
    };

    let mut next_epoch = 1;
    {
        let mut s = state_store.lock().await;
        s.mutate(|st| {
            if let Some(h) = st.sessions.get_mut(sid) {
                next_epoch = h.epoch.max(log_header_epoch) + 1;
                h.epoch = next_epoch;
            } else if let Some(ret) = st.retired_epochs.get_mut(sid) {
                next_epoch = (*ret).max(log_header_epoch) + 1;
                *ret = next_epoch;
            } else {
                next_epoch = log_header_epoch + 1;
                st.retired_epochs.insert(sid.to_string(), next_epoch);
            }
        })?;
    }

    let mut combined: Vec<(Option<u64>, Value)> =
        Vec::with_capacity(frames.len() + buffered_frames.len());
    for f in frames {
        combined.push((None, f));
    }
    for (id_opt, f) in buffered_frames {
        let key = f.pointer("/params/update").and_then(honk_wire::block_key);
        if let Some(k) = key
            && keys.contains(&k)
        {
            continue;
        }
        combined.push((id_opt, f));
    }

    let max_resume = match (resume_from, last_processed_session_id) {
        (Some(r), Some(l)) => Some(r.max(l)),
        (None, l) => l,
        (r, None) => r,
    };

    let new_log = crate::log::SessionLog::create(&log_path, next_epoch, max_resume, combined)?;
    Ok((new_log, next_epoch))
}

impl Session {
    pub async fn run(mut self) {
        let mut watchdog = tokio::time::interval(Duration::from_secs(5));
        let mut idle = tokio::time::interval(Duration::from_millis(200));
        let mut idle_since: Option<std::time::Instant> = None;

        if self.start_reattach {
            self.start_reattach = false;
            // Check if sidecar has reply for inflight
            if let Some(inf) = self.turn.inflight.as_ref() {
                let sidecar_path = self
                    .state_dir
                    .join("sessions")
                    .join(format!("{}.replies.jsonl", self.sid));
                let replies = load_reply_sidecar(&sidecar_path);
                if let Some(reply) = replies.get(&inf.upstream_id) {
                    let upstream_id = inf.upstream_id.clone();
                    let actions = self
                        .turn
                        .on_prompt_reply(&upstream_id, reply, &mut self.ids);
                    self.execute_actions(actions).await;
                }
            }
            self.trigger_reattach().await;
        }

        for ev in std::mem::take(&mut self.initial_events) {
            self.on_upstream(ev).await;
        }

        loop {
            if self.state == ActorState::Gone {
                break;
            }
            tokio::select! {
                biased;
                ev = self.events.recv(), if self.state == ActorState::Live => match ev {
                    Some(ev) => {
                        self.on_upstream(ev).await;
                        tokio::task::yield_now().await;
                    }
                    None => {
                        tracing::warn!(
                            "Session {} upstream event stream closed; triggering reattach",
                            self.sid
                        );
                        self.trigger_reattach().await;
                    }
                },
                msg = self.rx.recv() => match msg {
                    Some(m) => self.on_msg(m).await,
                    None => break,
                },
                _ = watchdog.tick() => {
                    self.check_streams().await;
                }
                _ = idle.tick() => {
                    if self.is_idle() {
                        let since = *idle_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= Duration::from_secs(self.hub.args.idle_ttl_secs)
                            && self.evict().await
                        {
                            break;
                        }
                    } else {
                        idle_since = None;
                    }
                }
            }
        }
        self.return_undelivered_to_hub();
    }

    async fn check_streams(&mut self) {
        if self.state != ActorState::Live {
            return;
        }
        let dead_secs = Duration::from_secs(self.hub.args.stream_dead_secs);
        let now = std::time::Instant::now();

        let session_key = StreamKey::Session(self.sid.clone());
        let conn_key = StreamKey::Conn;

        let session_dead = self
            .up
            .last_byte_at(&session_key)
            .map(|t| now.duration_since(t) > dead_secs)
            .unwrap_or(false);
        let conn_dead = self
            .up
            .last_byte_at(&conn_key)
            .map(|t| now.duration_since(t) > dead_secs)
            .unwrap_or(false);

        if session_dead || conn_dead {
            tracing::warn!(
                "Session {} watchdog detected dead/half-open stream (session_dead={}, conn_dead={}); reattaching",
                self.sid,
                session_dead,
                conn_dead
            );
            self.trigger_reattach().await;
        }
    }

    async fn trigger_reattach(&mut self) {
        if self.state == ActorState::Gone
            || self.state == ActorState::Reattaching
            || self.reactivating
        {
            return;
        }
        self.state = ActorState::Reattaching;
        self.do_reattach().await;
    }

    async fn do_reattach(&mut self) {
        let session_resume = Some(self.log.last_upstream_id().unwrap_or(0));
        let conn_resume = Some(self.last_processed_conn_id.unwrap_or(0));
        let is_patched = self
            .up
            .stream_features()
            .map(|f| f.is_patched())
            .unwrap_or(false);

        if is_patched {
            let session_res = self
                .up
                .subscribe_with(
                    Some(&self.sid),
                    SubscribeOpts {
                        takeover: true,
                        resume_after: session_resume,
                    },
                )
                .await;

            let outcome = match session_res {
                Ok(out) => out,
                Err(e) => {
                    let gone = honk_upstream::is_gone(&e);
                    self.on_reattach_failed(gone, e.to_string()).await;
                    return;
                }
            };

            let _ = self
                .up
                .subscribe_with(
                    None,
                    SubscribeOpts {
                        takeover: true,
                        resume_after: conn_resume,
                    },
                )
                .await;

            self.on_reattached(outcome).await;
        } else {
            let up = self.up.clone();
            let sid = self.sid.clone();
            let self_tx = self.self_tx.clone();
            tokio::spawn(async move {
                let session_res = up
                    .subscribe_with(
                        Some(&sid),
                        SubscribeOpts {
                            takeover: true,
                            resume_after: session_resume,
                        },
                    )
                    .await;

                let outcome = match session_res {
                    Ok(out) => out,
                    Err(e) => {
                        let gone = honk_upstream::is_gone(&e);
                        let _ = self_tx
                            .send(SessionMsg::ReattachFailed {
                                gone,
                                error: e.to_string(),
                            })
                            .await;
                        return;
                    }
                };

                let _ = up
                    .subscribe_with(
                        None,
                        SubscribeOpts {
                            takeover: true,
                            resume_after: conn_resume,
                        },
                    )
                    .await;

                let _ = self_tx.send(SessionMsg::Reattached { outcome }).await;
            });
        }
    }

    async fn on_reattached(&mut self, outcome: honk_upstream::SubscribeOutcome) {
        if std::mem::take(&mut self.inflight_needs_delivery_check) {
            let actions = self.check_restored_inflight_delivery().await;
            self.execute_actions(actions).await;
        }

        if self.needs_rebuild {
            self.needs_rebuild = false;
            self.start_rebuild().await;
            return;
        }

        match outcome.replay {
            ReplayOutcome::Complete => {
                self.state = ActorState::Live;
                self.drain_queued_msgs().await;
            }
            ReplayOutcome::Gap | ReplayOutcome::Unsupported => {
                self.gap_evidenced = true;

                // Non-destructive active-run probe after gap:
                // When a gap occurs during an active turn or inflight prompt, we issue a non-destructive
                // steer probe with a bogus expectedRunId (_goose/unstable/session/steer, require_active_run
                // in crates/goose/src/acp/server.rs:2391).
                //
                // Note on concurrent dispatch in goose:
                // ACP requests in goose are dispatched concurrently via cx.spawn (server/dispatch.rs:128,479).
                // The prompt task clears the active run (clear_active_run, server.rs:2360) BEFORE completing
                // usage updates and returning the prompt reply.
                //
                // Soundness of the branches:
                // Case (a): Probe answers actualRunId -> The run is still active at that instant, so its end
                //           and prompt reply will be emitted later on the session stream. Stay Live, set
                //           rebuild_at_turn_end, and await the real reply. Sound.
                // Case (b): Probe answers idle ("no active run to steer") -> The run has ended (clear_active_run
                //           has occurred). Rebuild export starts immediately. However, because goose dispatches
                //           concurrently, an idle probe response can overtake the real prompt reply (which is
                //           waiting on send_session_usage_updates).
                //           Rule: After rebuild export completes, do NOT synthesize end_turn immediately.
                //           Instead, wait for a grace window (gap_synthesis_grace) for the real reply to arrive.
                //           Any reply arriving in that window wins (no synthesis). Only if the grace window expires
                //           with no reply does honkd synthesize end_turn (the reply was truly lost in the gap).
                if self.turn.run.is_some() || self.turn.inflight.is_some() {
                    self.state = ActorState::Live;
                    let gap_marker = json!({
                        "jsonrpc": "2.0",
                        "method": "_honk/gap",
                        "params": {
                            "sessionId": self.sid,
                        }
                    });
                    self.fan_out_stamped(&gap_marker, true);

                    // Issue non-destructive steer probe
                    self.issue_steer_probe().await;
                    self.drain_queued_msgs().await;
                } else {
                    self.start_rebuild().await;
                }
            }
            ReplayOutcome::NotRequested => {
                self.state = ActorState::Live;
                self.drain_queued_msgs().await;
            }
        }
    }

    pub(crate) async fn issue_steer_probe(&mut self) {
        if self.active_probe_id.is_some() {
            return;
        }
        let probe_id = self.ids.next();
        self.active_probe_id = Some(probe_id.clone());
        let probe_params = json!({
            "sessionId": self.sid,
            "expectedRunId": format!("honk-probe-{}", self.ids.next()),
            "prompt": [{"type": "text", "text": "probe"}]
        });
        let up = self.up.clone();
        let sid = self.sid.clone();
        let self_tx = self.self_tx.clone();
        let send_probe_id = probe_id.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(50);
            let max_backoff = Duration::from_secs(1);
            let max_retries = 5;
            for attempt in 0..=max_retries {
                if self_tx.is_closed() {
                    return;
                }
                match up
                    .request_with_id(
                        json!(send_probe_id),
                        "_goose/unstable/session/steer",
                        probe_params.clone(),
                        Some(&sid),
                    )
                    .await
                {
                    Ok(()) => return,
                    Err(e) => {
                        let gone = honk_upstream::is_gone(&e);
                        if gone {
                            let _ = self_tx
                                .send(SessionMsg::ReattachFailed {
                                    gone: true,
                                    error: e.to_string(),
                                })
                                .await;
                            return;
                        }
                        if attempt == max_retries {
                            tracing::error!(
                                "steer probe failed after {max_retries} retries: {e:#}"
                            );
                            let _ = self_tx
                                .send(SessionMsg::ReattachFailed {
                                    gone: false,
                                    error: e.to_string(),
                                })
                                .await;
                            return;
                        }
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        });

        let timeout_secs = Duration::from_secs(self.hub.args.probe_timeout_secs);
        let timeout_probe_id = probe_id;
        let timeout_tx = self.self_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(timeout_secs).await;
            let _ = timeout_tx
                .send(SessionMsg::ProbeTimeout {
                    probe_id: timeout_probe_id,
                })
                .await;
        });
    }

    // In honkd, a 404 (Not Found) returned by goose for a session-scoped connection (POST, GET,
    // or reattach) indicates that the upstream goose process was restarted or its in-memory
    // connection table was cleared. Goose persists session conversation history in its SQLite
    // database, but connection state exists only in memory. When goose restarts:
    // 1. All prior connection IDs return 404 (Not Found).
    // 2. The session itself remains intact in SQLite.
    // 3. honkd treats connection-level 404s as an upstream restart ("upstream_restart"),
    //    cleans up the dead connection without calling DELETE, and re-activates the session
    //    via A16 using its stored cwd and export transcript.
    //
    // Other potential "Gone" / Not Found causes:
    // - A session that does not exist in SQLite (e.g. deleted or never created) fails at the
    //   session level during session/list or session/load with error -32602 ("Session not found"),
    //   not as a connection-level 404.
    // - Explicit upstream connection deletion by an external actor similarly invalidates the
    //   in-memory connection ID (404), which is handled identically by re-activating a fresh
    //   connection to the persistent session.
    async fn on_reattach_failed(&mut self, gone: bool, error: String) {
        tracing::error!(
            "Session {} reattach failed: gone={gone}, err={error}",
            self.sid
        );
        self.active_probe_id = None;
        if gone {
            Box::pin(self.handle_gone()).await;
        } else {
            self.state = ActorState::Reattaching;
            let self_tx = self.self_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = self_tx.send(SessionMsg::RetryReattach).await;
            });
        }
    }

    fn fail_pending_requests(&mut self, reason: &str) {
        let pending = std::mem::take(&mut self.pending_requests);
        for (_, req) in pending {
            let err_resp = crate::rpc::error(
                req.downstream_id,
                -32000,
                format!("Upstream connection was replaced; request cancelled ({reason})"),
                None,
            );
            self.send_to_viewer(req.viewer_id, err_resp);
        }
    }

    async fn handle_gone(&mut self) {
        if self.state == ActorState::Gone || self.reactivating {
            tracing::warn!(
                "Session {} ignoring Gone while already reactivating or Gone",
                self.sid
            );
            return;
        }
        tracing::warn!("Session {} handling upstream gone / restart", self.sid);
        self.active_probe_id = None;

        // Cancel any in-flight gap rebuild so a late Rebuilt msg will be ignored
        self.rebuild_token += 1;

        // Fail any in-flight pending steer / forward requests
        self.fail_pending_requests("upstream_gone");

        // Step 1: turn.interrupt("upstream_restart")
        let actions = self.turn.interrupt("upstream_restart");
        Box::pin(self.execute_actions(actions)).await;

        // Ensure any inflight ledger entries for this session in state become Done
        // with the cancelled/upstream_restart response (reconciliation with A14)
        self.resolve_inflight_on_restart().await;

        // Step 2: pending entries become stale
        self.mark_pending_stale().await;

        // Step 3: push a synthesized activeRunId:null into the log (stamped, fanned out to honk-aware)
        let synth_frame = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": self.sid,
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "goose": {
                            "activeRunId": Value::Null
                        }
                    }
                }
            }
        });
        let stamped_opt = match self.log.append(vec![(None, synth_frame)]) {
            Ok(entries) => entries.last().map(|e| e.frame.clone()),
            Err(_) => None,
        };
        if let Some(frame) = stamped_opt {
            // Synthesized activeRunId:null reaches both honk-aware and legacy viewers (pigeon tracks runs by it)
            self.fan_out_stamped(&frame, false);
        }

        // Step 4: never auto-resend

        // Step 5: The dead connection returned 404 (Not Found), meaning upstream goose has
        // already dropped it from memory (e.g. process restart). We do not send DELETE upstream,
        // and we do not record it in state.orphan_conns (orphans are cleaned up by sending DELETE
        // at boot, which would only fail with 404 again). We simply drop the dead handle in-memory.

        // Step 6: re-activate (A16)
        // Sessions with attached viewers re-activate 500 ms apart; others lazily on first use.
        let has_attached_viewers = !self.viewers.is_empty();
        if !has_attached_viewers {
            tracing::info!(
                "Session {} has no attached viewers; retiring session for lazy re-activation",
                self.sid
            );
            let sid = self.sid.clone();
            let epoch = self.log.head().epoch;
            {
                let mut s = self.state_store.lock().await;
                let _ = s.mutate(|st| {
                    if let Some(held) = st.sessions.remove(&sid) {
                        st.retired_epochs.insert(sid.clone(), held.epoch);
                    } else {
                        st.retired_epochs.insert(sid.clone(), epoch);
                    }
                });
            }
            self.hub.sessions.write().await.remove(&self.sid);
            self.state = ActorState::Gone;
            return;
        }

        self.state = ActorState::Rebuilding;
        self.reactivating = true;
        self.reactivation_token += 1;
        let token = self.reactivation_token;

        // Spawn reactivation task so the actor never awaits recovery inline (plan ~80-84)
        let sid = self.sid.clone();
        let hub = self.hub.clone();
        let state_dir = self.state_dir.clone();
        let state_store = self.state_store.clone();
        let self_tx = self.self_tx.clone();

        tokio::spawn(async move {
            Self::run_reactivation_task(token, sid, hub, state_dir, state_store, self_tx).await;
        });
    }

    async fn resolve_inflight_on_restart(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let sid = self.sid.clone();
        let mut s = self.state_store.lock().await;
        let _ = s.mutate(|st| {
            if let Some(h) = st.sessions.get_mut(&sid) {
                h.inflight = None;
            }
            for entry in st.ledger.values_mut() {
                if entry.session() == sid
                    && matches!(entry, crate::state::LedgerEntry::InFlight { .. })
                {
                    *entry = crate::state::LedgerEntry::Done {
                        session: sid.clone(),
                        response: json!({
                            "jsonrpc": "2.0",
                            "id": Value::Null,
                            "result": {
                                "stopReason": "cancelled",
                                "_meta": {
                                    "honk": {
                                        "reason": "upstream_restart"
                                    }
                                }
                            }
                        }),
                        done_unix: now,
                    };
                }
            }
            st.prune_ledger(
                now,
                crate::state::LEDGER_PRUNE_TTL_SECS,
                crate::state::LEDGER_PRUNE_MAX_ENTRIES,
                None,
            );
        });
    }

    async fn run_reactivation_task(
        token: u64,
        sid: String,
        hub: Arc<Hub>,
        state_dir: PathBuf,
        state_store: Arc<Mutex<crate::state::StateStore>>,
        self_tx: mpsc::Sender<SessionMsg>,
    ) {
        // 1. Wait for control connection to become available
        hub.wait_for_control_connected().await;

        // 2. Check if upstream unpatched is refused (permanent failure)
        if hub.upstream_unpatched_refused.load(Ordering::SeqCst) {
            let _ = self_tx
                .send(SessionMsg::ReactivationFailed {
                    token,
                    reason: "upstream goose is no longer patched".to_string(),
                    permanent: true,
                })
                .await;
            return;
        }

        // 3. Acquire staggered reactivation slot (500 ms apart)
        hub.acquire_reactivation_slot().await;

        // 4. Bounded retries on transient errors (up to 5 attempts with backoff)
        let mut attempts = 0;
        let max_attempts = 5;
        let mut backoff = Duration::from_millis(200);

        loop {
            attempts += 1;

            let stored_cwd = match hub.get_stored_cwd(&sid).await {
                Ok(cwd) => cwd,
                Err(e) => {
                    if e.code == -32602 {
                        // Session not found in session/list -> permanent failure
                        let _ = self_tx
                            .send(SessionMsg::ReactivationFailed {
                                token,
                                reason: format!("session not found upstream: {e:#}"),
                                permanent: true,
                            })
                            .await;
                        return;
                    }
                    if attempts >= max_attempts {
                        let _ = self_tx
                            .send(SessionMsg::ReactivationFailed {
                                token,
                                reason: format!(
                                    "failed to get stored cwd after {attempts} attempts: {e:#}"
                                ),
                                permanent: true,
                            })
                            .await;
                        return;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                    continue;
                }
            };

            let result: Result<ReactivationData, anyhow::Error> = async {
                let init_opts = UpstreamOpts {
                    allow_production: hub.args.allow_production_upstream,
                    client_capabilities: json!({
                        "fs": {
                            "readTextFile": false,
                            "writeTextFile": false
                        },
                        "terminal": false
                    }),
                };
                let recorder = if let Some(rec_path) = &hub.args.record {
                    let redactor = Redactor::from_env();
                    Recorder::file(rec_path, redactor).unwrap_or_else(|_| Recorder::disabled())
                } else {
                    Recorder::disabled()
                };

                let mut session_upstream = Upstream::initialize_with(
                    &hub.upstream_url,
                    hub.upstream_secret.clone(),
                    recorder,
                    &init_opts,
                )
                .await?;

                let conn_id = session_upstream.conn_id().to_string();

                {
                    let mut s = state_store.lock().await;
                    s.mutate(|st| {
                        st.orphan_conns.push(conn_id.clone());
                    })?;
                }

                session_upstream
                    .subscribe_with(
                        None,
                        SubscribeOpts {
                            takeover: true,
                            resume_after: Some(0),
                        },
                    )
                    .await?;

                session_upstream
                    .subscribe_with(
                        Some(&sid),
                        SubscribeOpts {
                            takeover: true,
                            resume_after: Some(0),
                        },
                    )
                    .await?;

                let load_reply = session_upstream
                    .call(
                        "session/load",
                        json!({
                            "sessionId": &sid,
                            "cwd": stored_cwd,
                            "mcpServers": []
                        }),
                        Some(&sid),
                        Duration::from_secs(30),
                    )
                    .await?;

                let (up_handle, mut event_rx) = session_upstream.split();

                let mut initial_events = Vec::new();
                while let Ok(ev) = event_rx.try_recv() {
                    if let honk_upstream::Event::Frame(key, frame, _) = &ev
                        && key == &honk_upstream::StreamKey::Session(sid.clone())
                        && crate::rpc::kind(frame) == crate::rpc::Kind::Notification
                    {
                        continue;
                    }
                    initial_events.push(ev);
                }

                let export_val = hub.export(&sid).await?;
                let client_prompts = honk_wire::export::client_prompts(&export_val).len();
                let frames = honk_wire::export::to_updates(&export_val, &sid);
                let mut keys = HashSet::new();
                for f in &frames {
                    if let Some(k) = f.pointer("/params/update").and_then(honk_wire::block_key) {
                        keys.insert(k);
                    }
                }

                let (new_log, next_epoch) = create_new_generation_from_export(
                    &sid,
                    &state_dir,
                    &state_store,
                    frames,
                    keys.clone(),
                    Vec::new(),
                    None,
                    None,
                )
                .await?;

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                {
                    let mut s = state_store.lock().await;
                    let sid_clone = sid.clone();
                    let conn_id_clone = conn_id.clone();
                    let stored_cwd_clone = stored_cwd.clone();
                    let load_reply_clone = load_reply.clone();
                    s.mutate(|st| {
                        st.orphan_conns.retain(|id| id != &conn_id_clone);
                        st.retired_epochs.remove(&sid_clone);
                        st.sessions.insert(
                            sid_clone,
                            crate::state::HeldSession {
                                conn_id: conn_id_clone,
                                cwd: stored_cwd_clone,
                                epoch: next_epoch,
                                last_conn_event_id: None,
                                pending: Vec::new(),
                                inflight: None,
                                last_active_unix: now,
                                init_result: Some(load_reply_clone),
                                client_prompts,
                            },
                        );
                    })?;
                }

                let _ = std::fs::remove_file(
                    state_dir
                        .join("sessions")
                        .join(format!("{sid}.replies.jsonl")),
                );

                Ok(ReactivationData {
                    conn_id,
                    up_handle,
                    event_rx,
                    new_log,
                    next_epoch,
                    load_reply,
                    keys,
                    initial_events,
                    client_prompts,
                })
            }
            .await;

            match result {
                Ok(data) => {
                    let _ = self_tx
                        .send(SessionMsg::ReactivationComplete {
                            token,
                            data: Box::new(data),
                        })
                        .await;
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        "run_reactivation_task for {sid} attempt {attempts} error: {e:#}"
                    );
                    if attempts >= max_attempts {
                        let _ = self_tx
                            .send(SessionMsg::ReactivationFailed {
                                token,
                                reason: format!(
                                    "reactivation failed after {attempts} attempts: {e:#}"
                                ),
                                permanent: true,
                            })
                            .await;
                        return;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
    }

    /// Restored inflight delivery check used only at restart/boot time.
    /// Note: After a crash, honkd restarts much later, so goose has usually committed the user message.
    /// However, there is a residual race if honkd restarts within milliseconds of the prompt POST.
    async fn check_restored_inflight_delivery(&mut self) -> Vec<Action> {
        let Some(inf) = self.turn.inflight.as_ref() else {
            return vec![];
        };
        let n = inf.prompt_index;
        let expected_text = crate::rpc::extract_user_text(&inf.params);

        match self.hub.export(&self.sid).await {
            Ok(export_val) => {
                let prompts = honk_wire::export::client_prompts(&export_val);
                if prompts.len() == n {
                    // Goose does not have prompt N; it was never received by goose before the crash.
                    return self.turn.interrupt("not_delivered");
                } else if prompts.len() > n && prompts[n] == expected_text {
                    // Prompt N is present in export at the expected index: delivered!
                    if self.turn.client_prompts <= n {
                        self.turn.client_prompts = n + 1;
                        self.sync_inflight().await;
                    }

                    // Bound the "delivered" wait:
                    // If goose finished the turn or is idle, issue the steer probe to discover it.
                    // If goose is idle, it triggers gap rebuild and grace window synthesis.
                    self.gap_evidenced = true;
                    self.issue_steer_probe().await;
                    return vec![];
                } else {
                    // Out-of-contract: prompts.len() > n but mismatch.
                    // Contract: honkd is the sole interactive ACP client for held upstream sessions.
                    // Prompts sent directly to goose by other clients bypassing honkd are out of contract.
                    // Safe side: we err toward NOT re-posting automatically to prevent duplicate or
                    // out-of-order execution. Finish the inflight with cancelled reason "delivery_unknown".
                    tracing::error!(
                        "Session {}: export prompt at index {n} ({:?}) does not match inflight expected text ({:?})",
                        self.sid,
                        prompts.get(n),
                        expected_text
                    );
                    return self.turn.interrupt("delivery_unknown");
                }
            }
            Err(e) => {
                tracing::warn!(
                    "failed to export session {} for inflight check: {e:#}",
                    self.sid
                );
            }
        }
        vec![]
    }

    async fn start_rebuild(&mut self) {
        if self.state == ActorState::Gone
            || self.state == ActorState::Rebuilding
            || self.reactivating
        {
            return;
        }
        self.state = ActorState::Rebuilding;
        self.rebuilding_buffered_frames.clear();
        self.rebuild_token += 1;
        let token = self.rebuild_token;

        let hub = self.hub.clone();
        let sid = self.sid.clone();
        let self_tx = self.self_tx.clone();
        let resume_from = self.last_processed_session_id;

        tokio::spawn(async move {
            match hub.export(&sid).await {
                Ok(export_val) => {
                    let client_prompts = honk_wire::export::client_prompts(&export_val).len();
                    let frames = honk_wire::export::to_updates(&export_val, &sid);
                    let mut keys = HashSet::new();
                    for f in &frames {
                        if let Some(k) = f.pointer("/params/update").and_then(honk_wire::block_key)
                        {
                            keys.insert(k);
                        }
                    }
                    let _ = self_tx
                        .send(SessionMsg::Rebuilt {
                            token,
                            frames,
                            keys,
                            resume_from,
                            client_prompts,
                        })
                        .await;
                }
                Err(e) => {
                    tracing::error!("rebuild export failed for {sid}: {e:#}");
                    let gone = honk_upstream::is_gone(&e);
                    let _ = self_tx
                        .send(SessionMsg::ReattachFailed {
                            gone,
                            error: e.to_string(),
                        })
                        .await;
                }
            }
        });
    }

    async fn on_rebuilt(
        &mut self,
        frames: Vec<Value>,
        keys: HashSet<honk_wire::BlockKey>,
        resume_from: Option<u64>,
        client_prompts: usize,
    ) {
        let buffered = std::mem::take(&mut self.rebuilding_buffered_frames);
        let (new_log, _next_epoch) = match create_new_generation_from_export(
            &self.sid,
            &self.state_dir,
            &self.state_store,
            frames,
            keys.clone(),
            buffered,
            resume_from,
            self.last_processed_session_id,
        )
        .await
        {
            Ok(res) => res,
            Err(e) => {
                tracing::error!("rebuild new generation failed for {}: {e:#}", self.sid);
                return;
            }
        };

        self.log = new_log;
        let head = self.log.head();
        let _ = std::fs::remove_file(
            self.state_dir
                .join("sessions")
                .join(format!("{}.replies.jsonl", self.sid)),
        );
        self.last_processed_session_id = self.log.last_upstream_id();
        self.turn.client_prompts = client_prompts;
        self.sync_inflight().await;

        let reset_frame = json!({
            "jsonrpc": "2.0",
            "method": "_honk/reset",
            "params": {
                "sessionId": self.sid,
                "epoch": head.epoch,
            }
        });
        let replay_done_frame = json!({
            "jsonrpc": "2.0",
            "method": "_honk/replay_done",
            "params": {
                "sessionId": self.sid,
                "epoch": head.epoch,
                "seq": head.seq,
            }
        });

        let mut msgs = Vec::with_capacity(self.log.entries().len() + 2);
        msgs.push(axum::extract::ws::Message::Text(
            reset_frame.to_string().into(),
        ));
        for entry in self.log.entries() {
            msgs.push(axum::extract::ws::Message::Text(
                entry.frame.to_string().into(),
            ));
        }
        msgs.push(axum::extract::ws::Message::Text(
            replay_done_frame.to_string().into(),
        ));

        for (tx, is_honk_aware) in self.viewers.values() {
            if *is_honk_aware {
                let batch = crate::viewer::ReplayBatch {
                    session_id: self.sid.clone(),
                    head: head.clone(),
                    msgs: msgs.clone(),
                    done_tx: None,
                };
                let _ = tx.send_replay(batch);
            }
        }

        self.suppressed_keys = keys;
        self.state = ActorState::Live;

        // Drain already-buffered events from the session stream before checking synthesis
        while let Ok(ev) = self.events.try_recv() {
            self.on_upstream(ev).await;
        }

        if self.gap_evidenced
            && let Some(inf) = self.turn.inflight.as_ref()
        {
            let upstream_id = inf.upstream_id.clone();
            let grace = self.gap_synthesis_grace;
            let self_tx = self.self_tx.clone();
            self.gap_synthesis_pending = true;
            tokio::spawn(async move {
                tokio::time::sleep(grace).await;
                let _ = self_tx
                    .send(SessionMsg::SynthesizeGapEndTurn { upstream_id })
                    .await;
            });
        }
        self.gap_evidenced = false;
        self.drain_queued_msgs().await;
    }

    async fn drain_queued_msgs(&mut self) {
        let msgs = std::mem::take(&mut self.queued_msgs);
        for m in msgs {
            Box::pin(self.on_msg(m)).await;
        }
    }

    pub fn is_idle(&self) -> bool {
        self.viewers.is_empty()
            && self.turn.run.is_none()
            && self.turn.inflight.is_none()
            && self.turn.parked.is_empty()
            && self.pending_server_requests.is_empty()
            && self.pending_requests.is_empty()
            && self.queued_msgs.is_empty()
            && self.active_probe_id.is_none()
            && !self.gap_synthesis_pending
            && self.state == ActorState::Live
            && !self.rebuild_at_turn_end
            && !self.needs_rebuild
            && self.finalizing_waiters.is_empty()
    }

    async fn evict(&mut self) -> bool {
        if !self.is_idle() {
            return false;
        }

        // Notify hub that eviction has begun for sid so concurrent activations wait
        let notify = self.hub.begin_eviction(&self.sid).await;

        // Remove from hub.sessions so no new triggers can acquire our tx
        self.hub.sessions.write().await.remove(&self.sid);

        // Check if a message arrived right before removal
        if let Ok(msg) = self.rx.try_recv() {
            self.hub.sessions.write().await.insert(
                self.sid.clone(),
                crate::hub::SessionHandle {
                    sid: self.sid.clone(),
                    conn_id: self.up.conn_id().to_string(),
                    tx: self.self_tx.clone(),
                },
            );
            self.hub.finish_eviction(&self.sid, notify).await;
            self.on_msg(msg).await;
            return false;
        }

        #[cfg(debug_assertions)]
        if let Ok(pause_str) = std::env::var("HONKD_TEST_PAUSE_EVICT_MS")
            && let Ok(ms) = pause_str.parse::<u64>()
        {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }

        let conn_id = self.up.conn_id().to_string();
        let sid = self.sid.clone();
        let epoch = self.log.epoch();

        // 1. Move connection to orphans
        {
            let mut s = self.state_store.lock().await;
            let _ = s.mutate(|st| {
                st.orphan_conns.push(conn_id.clone());
            });
        }

        // 2. DELETE
        let _ = self.up.delete().await;

        // 3. Remove orphan entry and session, record state.retired_epochs[sid]
        {
            let mut s = self.state_store.lock().await;
            let _ = s.mutate(|st| {
                st.orphan_conns.retain(|id| id != &conn_id);
                if let Some(held) = st.sessions.remove(&sid) {
                    st.retired_epochs.insert(sid.clone(), held.epoch);
                } else {
                    st.retired_epochs.insert(sid.clone(), epoch);
                }
            });
        }

        self.hub.finish_eviction(&self.sid, notify).await;
        self.state = ActorState::Gone;
        true
    }

    fn return_undelivered_to_hub(&mut self) {
        let mut undelivered = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            undelivered.push(msg);
        }
        if undelivered.is_empty() {
            return;
        }

        let sid = self.sid.clone();
        let hub = self.hub.clone();
        tokio::spawn(async move {
            tracing::warn!(
                "Session {sid} exiting; re-dispatching {} undelivered msg(s) in order",
                undelivered.len()
            );
            match hub.get_or_activate_session(&sid).await {
                Ok(handle) => {
                    for msg in undelivered {
                        let _ = handle.tx.send(msg).await;
                    }
                }
                Err(e) => {
                    for msg in undelivered {
                        match msg {
                            SessionMsg::Prompt { viewer, id, .. }
                            | SessionMsg::Steer { viewer, id, .. }
                            | SessionMsg::Forward { viewer, id, .. } => {
                                let err_frame =
                                    crate::rpc::error(id, e.code, e.message.clone(), None);
                                let _ = viewer.send_frame(axum::extract::ws::Message::Text(
                                    err_frame.to_string().into(),
                                ));
                            }
                            SessionMsg::Attach { viewer, .. } => {
                                let err_frame =
                                    crate::rpc::error(Value::Null, e.code, e.message.clone(), None);
                                let _ = viewer.send_frame(axum::extract::ws::Message::Text(
                                    err_frame.to_string().into(),
                                ));
                            }
                            _ => {}
                        }
                    }
                }
            }
        });
    }

    async fn sync_inflight(&self) {
        let target = self.turn.to_persisted();
        let client_prompts = self.turn.client_prompts;
        let sid = &self.sid;
        let mut s = self.state_store.lock().await;
        if let Some(h) = s.sessions.get(sid)
            && h.inflight == target
            && h.client_prompts == client_prompts
        {
            return;
        }
        let _ = s.mutate(|st| {
            if let Some(h) = st.sessions.get_mut(sid) {
                h.inflight = target;
                h.client_prompts = client_prompts;
            }
        });
    }

    fn send_to_viewer(&mut self, viewer_id: ViewerId, frame: Value) -> bool {
        if let Some((tx, _)) = self.viewers.get(&viewer_id) {
            if !tx.send_frame(axum::extract::ws::Message::Text(frame.to_string().into())) {
                self.viewers.remove(&viewer_id);
                return false;
            }
            true
        } else {
            false
        }
    }

    fn fan_out_legacy(&mut self, frame: &Value) {
        let legacy_vids: Vec<ViewerId> = self
            .viewers
            .iter()
            .filter_map(|(vid, (_, is_honk))| if !*is_honk { Some(*vid) } else { None })
            .collect();
        for vid in legacy_vids {
            self.send_to_viewer(vid, frame.clone());
        }
    }

    async fn on_upstream(&mut self, ev: Event) {
        let (key, frame, id_opt) = match ev {
            Event::Frame(key, frame, id_opt) => (key, frame, id_opt),
            Event::StreamEnded { key, error } => {
                tracing::warn!(
                    "Session {} stream ended: key={key:?}, error={error:?}",
                    self.sid
                );
                if self.state == ActorState::Live
                    || (self.state == ActorState::Rebuilding && !self.reactivating)
                {
                    self.trigger_reattach().await;
                }
                return;
            }
        };

        // 1. Dedupe
        if let Some(id) = id_opt {
            match &key {
                StreamKey::Session(_) => {
                    if let Some(last) = self.last_processed_session_id
                        && id <= last
                    {
                        return;
                    }
                }
                StreamKey::Conn => {
                    if let Some(last) = self.last_processed_conn_id
                        && id <= last
                    {
                        return;
                    }
                }
            }
        }

        // 2. Process
        let is_session_stream = matches!(&key, StreamKey::Session(_));
        let kind = crate::rpc::kind(&frame);

        let block_k = if is_session_stream {
            frame
                .pointer("/params/update")
                .and_then(honk_wire::block_key)
        } else {
            None
        };
        let is_suppressed = block_k
            .as_ref()
            .is_some_and(|k| self.suppressed_keys.contains(k));

        if self.state == ActorState::Rebuilding
            && is_session_stream
            && kind == crate::rpc::Kind::Notification
        {
            if let Some(update) = frame.pointer("/params/update") {
                if update.get("sessionUpdate").and_then(Value::as_str) == Some("user_message_chunk")
                    && let Some(msg_id) = update
                        .get("messageId")
                        .or_else(|| update.pointer("/_meta/goose/messageId"))
                        .and_then(Value::as_str)
                {
                    self.turn.on_user_message(msg_id);
                }

                if let Some(run_opt) = honk_wire::run_state(update) {
                    let actions = self.turn.on_run_state(run_opt, &mut self.ids);
                    self.execute_actions(actions).await;
                }
            }

            let mut legacy_frame = frame.clone();
            crate::rpc::strip_honk_meta(&mut legacy_frame);
            self.fan_out_legacy(&legacy_frame);

            self.rebuilding_buffered_frames.push((id_opt, frame));
            if let Some(id) = id_opt {
                self.last_processed_session_id = Some(id);
            }
            return;
        }

        if is_session_stream {
            if let Some(update) = frame.pointer("/params/update") {
                // If user_message_chunk, notify turn core of live user message
                if update.get("sessionUpdate").and_then(Value::as_str) == Some("user_message_chunk")
                    && let Some(msg_id) = update
                        .get("messageId")
                        .or_else(|| update.pointer("/_meta/goose/messageId"))
                        .and_then(Value::as_str)
                {
                    self.turn.on_user_message(msg_id);
                }

                if let Some(run_opt) = honk_wire::run_state(update) {
                    if run_opt.is_some() {
                        self.ambiguous_check_token += 1;
                    }
                    let actions = self.turn.on_run_state(run_opt.clone(), &mut self.ids);
                    self.execute_actions(actions).await;

                    if run_opt.is_none() {
                        self.suppressed_keys.clear();

                        if self.rebuild_at_turn_end {
                            self.rebuild_at_turn_end = false;
                            self.start_rebuild().await;
                        }
                    }
                }
            } else if kind == crate::rpc::Kind::Response {
                let is_probe_response = self.active_probe_id.as_ref().is_some_and(|pid| {
                    honk_upstream::rpc_id_matches(
                        &json!(pid),
                        frame.get("id").unwrap_or(&Value::Null),
                    )
                });

                if is_probe_response {
                    self.active_probe_id = None;

                    let is_idle = frame.pointer("/error/data").and_then(Value::as_str)
                        == Some("no active run to steer")
                        || frame
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .is_some_and(|m| m.contains("no active run to steer"));

                    let actual_run_id = frame
                        .pointer("/error/data/actualRunId")
                        .and_then(Value::as_str);

                    if let Some(run_id) = actual_run_id {
                        // Case (a): Turn is still running live on goose!
                        // If we were checking an ambiguous inflight prompt: DELIVERED!
                        let delivered_info = if let Some(inf) = self.turn.inflight.as_mut()
                            && !inf.announced
                        {
                            tracing::info!(
                                "Session {}: ambiguous prompt {} confirmed delivered via probe actualRunId `{run_id}`",
                                self.sid,
                                inf.upstream_id
                            );
                            inf.announced = true;
                            Some((
                                inf.prompt_index,
                                inf.params.clone(),
                                inf.owners.first().cloned(),
                            ))
                        } else {
                            None
                        };
                        if let Some((n, params, owner_opt)) = delivered_info {
                            self.ambiguous_check_token += 1;
                            if self.turn.client_prompts <= n {
                                self.turn.client_prompts = n + 1;
                                self.sync_inflight().await;
                            }
                            if let Some(owner) = owner_opt {
                                let synth_action = Action::SynthUser { params, owner };
                                self.execute_actions(vec![synth_action]).await;
                            }
                        }
                        self.turn.run = Some(run_id.to_string());
                        self.rebuild_at_turn_end = true;
                    } else if is_idle {
                        if self.gap_evidenced {
                            // Case (b): Turn ended inside the gap, or session is idle after evidenced delivery!
                            self.turn.run = None;
                            self.start_rebuild().await;
                        } else if let Some(inf) = self.turn.inflight.as_ref()
                            && !inf.announced
                        {
                            let up_id = inf.upstream_id.clone();
                            self.maybe_finish_ambiguous_not_delivered(&up_id).await;
                        }
                    }
                } else {
                    // Check if this is a response to pending steer or forward
                    let found_pending = self
                        .pending_requests
                        .keys()
                        .find(|k| {
                            honk_upstream::rpc_id_matches(
                                &json!(k),
                                frame.get("id").unwrap_or(&Value::Null),
                            )
                        })
                        .cloned();

                    if let Some(up_id) = found_pending {
                        let pending = self.pending_requests.remove(&up_id).unwrap();
                        let mut resp = frame.clone();
                        resp["id"] = pending.downstream_id;

                        if pending.is_steer
                            && let Some(msg_id) =
                                frame.pointer("/result/messageId").and_then(Value::as_str)
                        {
                            let actions =
                                self.turn.on_steer_ok(msg_id.to_string(), pending.viewer_id);
                            self.execute_actions(actions).await;
                        }

                        self.send_to_viewer(pending.viewer_id, resp);
                    } else {
                        // Check if response to inflight prompt
                        let is_inflight_reply = self.turn.inflight.as_ref().is_some_and(|inf| {
                            honk_upstream::rpc_id_matches(
                                &json!(inf.upstream_id),
                                frame.get("id").unwrap_or(&Value::Null),
                            )
                        });

                        if is_inflight_reply {
                            self.ambiguous_check_token += 1;
                            let upstream_id =
                                self.turn.inflight.as_ref().unwrap().upstream_id.clone();
                            let sidecar_path = self
                                .state_dir
                                .join("sessions")
                                .join(format!("{}.replies.jsonl", self.sid));
                            let _ = append_reply_sidecar(&sidecar_path, &upstream_id, &frame);

                            let actions =
                                self.turn
                                    .on_prompt_reply(&upstream_id, &frame, &mut self.ids);
                            self.gap_synthesis_pending = false;
                            self.execute_actions(actions).await;
                        } else {
                            let up_id_opt = frame
                                .get("id")
                                .and_then(Value::as_str)
                                .or_else(|| frame.pointer("/id").and_then(Value::as_str));

                            if let Some(id_str) = up_id_opt
                                && self.synthesized_replies.contains(id_str)
                            {
                                // The real prompt reply arrived AFTER synthesis grace window expired.
                                // Documented residual: requires >grace delay in goose's usage updates.
                                // Record real reply in replies sidecar and log a warning; do not double-reply.
                                tracing::warn!(
                                    "Session {}: real prompt reply arrived after synthesis for id: {id_str}",
                                    self.sid
                                );
                                let sidecar_path = self
                                    .state_dir
                                    .join("sessions")
                                    .join(format!("{}.replies.jsonl", self.sid));
                                let _ = append_reply_sidecar(&sidecar_path, id_str, &frame);
                            } else {
                                // Unmatched upstream response -> warn with upstream id only
                                let id_disp = frame
                                    .get("id")
                                    .map(|v| v.to_string())
                                    .unwrap_or_else(|| "null".into());
                                tracing::warn!("unmatched upstream response for id: {id_disp}");
                            }
                        }
                    }
                }
            } else if kind == crate::rpc::Kind::Request {
                // Server request (e.g. request_permission) -> forward to viewers
                self.on_server_request(frame.clone(), id_opt).await;
            }
        }

        // 3. Record & 4. Fan out stamped frame
        if is_session_stream {
            if kind == crate::rpc::Kind::Notification {
                if is_suppressed {
                    if let Some(id) = id_opt {
                        let _ = self.log.mark_processed(id);
                    }
                    let mut legacy_frame = frame.clone();
                    crate::rpc::strip_honk_meta(&mut legacy_frame);
                    self.fan_out_legacy(&legacy_frame);
                } else {
                    let stamped = match self.log.append(vec![(id_opt, frame)]) {
                        Ok(entries) => entries.last().map(|e| e.frame.clone()),
                        Err(e) => {
                            tracing::error!("failed to append frame to session log: {e:#}");
                            None
                        }
                    };
                    if let Some(frame) = stamped {
                        self.fan_out_stamped(&frame, false);
                    }
                }
            } else if let Some(id) = id_opt {
                let _ = self.log.mark_processed(id);
            }
            if let Some(id) = id_opt {
                self.last_processed_session_id = Some(id);
            }
        } else {
            // Conn stream
            if let Some(id) = id_opt {
                self.last_processed_conn_id = Some(id);
                let sid = self.sid.clone();
                let mut s = self.state_store.lock().await;
                let _ = s.mutate(|st| {
                    if let Some(h) = st.sessions.get_mut(&sid) {
                        h.last_conn_event_id = Some(id);
                    }
                });
            }
        }
    }

    async fn execute_actions(&mut self, actions: Vec<Action>) {
        let mut queue = std::collections::VecDeque::from(actions);
        while let Some(action) = queue.pop_front() {
            match action {
                Action::SendPrompt {
                    upstream_id,
                    params,
                } => {
                    let sid = self.sid.clone();
                    if let Some(inf) = self.turn.inflight.as_ref() {
                        let persisted = inf.to_persisted();
                        let client_prompts = self.turn.client_prompts;
                        let mutate_res = {
                            let mut s = self.state_store.lock().await;
                            s.mutate(|st| {
                                if let Some(h) = st.sessions.get_mut(&sid) {
                                    h.inflight = Some(persisted.clone());
                                    h.client_prompts = client_prompts;
                                }
                                for crid in &persisted.client_req_ids {
                                    st.ledger.insert(
                                        crid.clone(),
                                        crate::state::LedgerEntry::InFlight {
                                            session: sid.clone(),
                                            upstream_id: upstream_id.clone(),
                                        },
                                    );
                                }
                            })
                        };
                        if let Err(e) = mutate_res {
                            tracing::error!(
                                "Session {}: failed to persist inflight before POST: {e:#}",
                                sid
                            );
                        }
                    }

                    #[cfg(any(test, debug_assertions))]
                    if std::env::var("HONKD_TEST_CRASH_BEFORE_POST").as_deref() == Ok("1") {
                        std::process::abort();
                    }

                    let mut clean_params = params;
                    crate::rpc::strip_honk_meta(&mut clean_params);

                    // Awaited INLINE on handle (no tokio::spawn)
                    if let Err(e) = self
                        .up
                        .request_with_id(
                            json!(upstream_id),
                            "session/prompt",
                            clean_params,
                            Some(&self.sid),
                        )
                        .await
                    {
                        let failure = classify_prompt_post_error(&e);
                        match failure {
                            PromptPostFailure::Gone => {
                                Box::pin(self.handle_gone()).await;
                            }
                            PromptPostFailure::CertainlyNotStarted => {
                                // 4xx/5xx rejection from goose, or connect error before sending.
                                // Prompt was certainly not started -> remove ledger entry and clear inflight.
                                let failed_inf =
                                    self.turn.inflight.as_ref().map(|i| i.to_persisted());
                                let mutate_res = {
                                    let mut s = self.state_store.lock().await;
                                    s.mutate(|st| {
                                        if let Some(h) = st.sessions.get_mut(&sid) {
                                            h.inflight = None;
                                        }
                                        if let Some(inf) = &failed_inf {
                                            for crid in &inf.client_req_ids {
                                                st.ledger.remove(crid);
                                            }
                                        }
                                    })
                                };
                                if let Err(e) = mutate_res {
                                    tracing::error!(
                                        "Session {}: failed to clear inflight after prompt POST error: {e:#}",
                                        sid
                                    );
                                }

                                let err_val = json!({
                                    "code": -32603,
                                    "message": format!("Prompt POST failed: {e:#}"),
                                });
                                let fail_actions = self.turn.on_prompt_send_failed(
                                    &upstream_id,
                                    err_val,
                                    &mut self.ids,
                                );
                                for a in fail_actions.into_iter().rev() {
                                    queue.push_front(a);
                                }
                            }
                            PromptPostFailure::Ambiguous => {
                                tracing::warn!(
                                    "Session {}: ambiguous prompt POST failure for {upstream_id} ({e:#}); scheduling conservative bounded polling",
                                    sid
                                );
                                self.ambiguous_check_token += 1;
                                let token = self.ambiguous_check_token;
                                self.ambiguous_window_expired = false;
                                self.ambiguous_export_in_flight = false;

                                let total_secs =
                                    self.hub.args.ambiguous_inflight_timeout_secs.max(1);
                                let total_duration = Duration::from_secs(total_secs);
                                let ratios =
                                    [2.0 / 30.0, 5.0 / 30.0, 10.0 / 30.0, 20.0 / 30.0, 1.0];
                                let targets: Vec<Duration> =
                                    ratios.iter().map(|&r| total_duration.mul_f64(r)).collect();

                                let self_tx = self.self_tx.clone();
                                let up_id = upstream_id.clone();
                                tokio::spawn(async move {
                                    let mut prev = Duration::ZERO;
                                    let n = targets.len();
                                    for (i, &target) in targets.iter().enumerate() {
                                        let delay = target.saturating_sub(prev);
                                        prev = target;
                                        tokio::time::sleep(delay).await;
                                        let is_last = i + 1 == n;
                                        if self_tx
                                            .send(SessionMsg::AmbiguousCheckPoll {
                                                token,
                                                upstream_id: up_id.clone(),
                                                is_last,
                                            })
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                });
                            }
                        }
                    }
                }
                Action::SendCancel => {
                    // Awaited INLINE
                    let _ = self
                        .up
                        .notify(
                            "session/cancel",
                            json!({ "sessionId": self.sid }),
                            Some(&self.sid),
                        )
                        .await;
                }
                Action::SynthUser { params, owner } => {
                    let user_text = crate::rpc::extract_user_text(&params);
                    let crid = owner.client_req_id.clone();
                    let msg_id = crid
                        .as_ref()
                        .map(|c| format!("synth_{c}"))
                        .unwrap_or_else(|| format!("synth_{}", self.ids.next()));
                    let mut synth_frame = json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": self.sid,
                            "update": {
                                "sessionUpdate": "user_message_chunk",
                                "messageId": msg_id,
                                "content": {
                                    "type": "text",
                                    "text": user_text,
                                }
                            }
                        }
                    });
                    if let Some(cid) = crid {
                        synth_frame["params"]["update"]["_meta"] = json!({
                            "honk": {
                                "source": {
                                    "client_req_id": cid
                                }
                            }
                        });
                    }

                    // Record first -> fan out stamped entry
                    let stamped = match self.log.append(vec![(None, synth_frame.clone())]) {
                        Ok(entries) => entries.last().map(|e| e.frame.clone()),
                        Err(_) => None,
                    };
                    if let Some(frame) = stamped {
                        self.fan_out_stamped(&frame, true); // honk-aware only
                    }
                }
                Action::Reply {
                    owners,
                    body,
                    user_text,
                } => match body {
                    Reply::Error(err) => {
                        self.handle_reply_text(owners, Reply::Error(err), None)
                            .await;
                    }
                    Reply::Result(result_val) => {
                        if user_text.is_none() {
                            self.handle_reply_text(owners, Reply::Result(result_val), None)
                                .await;
                        } else {
                            for o in &owners {
                                if let Some(crid) = &o.client_req_id {
                                    self.finalizing_waiters.entry(crid.clone()).or_default();
                                }
                            }
                            let hub = self.hub.clone();
                            let sid = self.sid.clone();
                            let self_tx = self.self_tx.clone();
                            tokio::spawn(async move {
                                let export_res = hub.export(&sid).await;
                                let text = match export_res {
                                    Ok(exp) => {
                                        let u_text = user_text.as_deref().unwrap_or("");
                                        let t = honk_wire::export::turn_text(&exp, u_text);
                                        if t.is_empty() { None } else { Some(t) }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "failed to export session {sid} for turn_text: {e:#}"
                                        );
                                        None
                                    }
                                };
                                let _ = self_tx
                                    .send(SessionMsg::ReplyText {
                                        owners,
                                        body: Reply::Result(result_val),
                                        text,
                                    })
                                    .await;
                            });
                        }
                    }
                },
                Action::NotifySteerUnconsumed { viewer, message_id } => {
                    if let Some((_, is_honk_aware)) = self.viewers.get(&viewer) {
                        if *is_honk_aware {
                            let notify_frame = json!({
                                "jsonrpc": "2.0",
                                "method": "_honk/steer_unconsumed",
                                "params": {
                                    "sessionId": self.sid,
                                    "messageId": message_id,
                                }
                            });
                            self.send_to_viewer(viewer, notify_frame);
                        } else {
                            tracing::warn!(
                                "Session {}: unconsumed steer {message_id} for legacy viewer {viewer}",
                                self.sid
                            );
                        }
                    } else {
                        tracing::warn!(
                            "Session {}: unconsumed steer {message_id} for disconnected viewer {viewer}",
                            self.sid
                        );
                    }
                }
            }
        }

        self.sync_inflight().await;
    }

    fn queue_msg(&mut self, viewer_id: ViewerId, id: Value, msg: SessionMsg) {
        if self.queued_msgs.len() >= 1024 {
            let err = crate::rpc::error(id, -32603, "Session message queue full", None);
            self.send_to_viewer(viewer_id, err);
        } else {
            self.queued_msgs.push_back(msg);
        }
    }

    async fn on_msg(&mut self, msg: SessionMsg) {
        match msg {
            SessionMsg::Attach {
                viewer_id,
                viewer,
                after,
                load_id,
                reply,
            } => {
                let (reset, entries) = match self.log.resume(after.as_ref()) {
                    crate::log::Resume::Reset(e) => (true, e.to_vec()),
                    crate::log::Resume::Delta(e) => (false, e.to_vec()),
                };
                let head = self.log.head();

                let mut msgs = Vec::new();
                if reset {
                    let reset_frame = json!({
                        "jsonrpc": "2.0",
                        "method": "_honk/reset",
                        "params": {
                            "sessionId": self.sid,
                            "epoch": head.epoch,
                        }
                    });
                    msgs.push(axum::extract::ws::Message::Text(
                        reset_frame.to_string().into(),
                    ));
                }

                for entry in entries {
                    msgs.push(axum::extract::ws::Message::Text(
                        entry.frame.to_string().into(),
                    ));
                }

                let mut load_res = self.init_result.clone();
                if !load_res.is_object() {
                    load_res = json!({ "sessionId": self.sid });
                }
                if let Some(meta) = load_res.get_mut("_meta").and_then(Value::as_object_mut) {
                    meta.insert(
                        "honk".to_string(),
                        json!({
                            "epoch": head.epoch,
                            "seq": head.seq,
                        }),
                    );
                } else {
                    load_res["_meta"] = json!({
                        "honk": {
                            "epoch": head.epoch,
                            "seq": head.seq,
                        }
                    });
                }
                let load_reply_frame = crate::rpc::response(load_id, load_res);
                msgs.push(axum::extract::ws::Message::Text(
                    load_reply_frame.to_string().into(),
                ));

                let replay_done_frame = json!({
                    "jsonrpc": "2.0",
                    "method": "_honk/replay_done",
                    "params": {
                        "sessionId": self.sid,
                        "epoch": head.epoch,
                        "seq": head.seq,
                    }
                });
                msgs.push(axum::extract::ws::Message::Text(
                    replay_done_frame.to_string().into(),
                ));

                for p in &self.pending_server_requests {
                    let pending_frame = json!({
                        "jsonrpc": "2.0",
                        "id": p.id,
                        "method": p.method,
                        "params": p.params,
                    });
                    msgs.push(axum::extract::ws::Message::Text(
                        pending_frame.to_string().into(),
                    ));
                }

                let batch = crate::viewer::ReplayBatch {
                    session_id: self.sid.clone(),
                    head: head.clone(),
                    msgs,
                    done_tx: None,
                };
                let sent_ok = viewer.send_replay(batch);
                if sent_ok {
                    self.viewers.insert(viewer_id, (viewer, true));
                }
                let _ = reply.send(sent_ok);
            }
            SessionMsg::AttachLive {
                viewer_id,
                viewer,
                is_honk_aware,
            } => {
                self.attach_live(viewer_id, viewer, is_honk_aware);
            }
            SessionMsg::Detach { viewer_id } => {
                self.viewers.remove(&viewer_id);
            }
            SessionMsg::Prompt {
                viewer_id,
                viewer,
                is_honk_aware,
                id,
                params,
            } => {
                if self.hub.upstream_unpatched_refused.load(Ordering::SeqCst) {
                    tracing::error!("upstream goose is no longer patched");
                    let err_resp =
                        crate::rpc::error(id, -32000, "upstream goose is no longer patched", None);
                    self.send_to_viewer(viewer_id, err_resp);
                    return;
                }
                if !self.viewers.contains_key(&viewer_id) {
                    self.attach_live(viewer_id, viewer.clone(), is_honk_aware);
                }
                if matches!(self.state, ActorState::Reattaching | ActorState::Rebuilding) {
                    self.queue_msg(
                        viewer_id,
                        id.clone(),
                        SessionMsg::Prompt {
                            viewer_id,
                            viewer,
                            is_honk_aware,
                            id,
                            params,
                        },
                    );
                    return;
                }
                let client_req_id = honk_wire::honk_meta(&params).client_req_id;

                if let Some(crid) = &client_req_id {
                    let cached = {
                        let s = self.state_store.lock().await;
                        match s.ledger.get(crid) {
                            Some(entry) if entry.session() != self.sid => {
                                Some(Err(crate::rpc::error(
                                    id.clone(),
                                    -32602,
                                    "client_req_id belongs to another session",
                                    None,
                                )))
                            }
                            Some(crate::state::LedgerEntry::Done { response, .. }) => {
                                let mut resp = response.clone();
                                resp["id"] = id.clone();
                                Some(Ok(resp))
                            }
                            Some(crate::state::LedgerEntry::InFlight { .. }) => None,
                            None => None,
                        }
                    };
                    match cached {
                        Some(Ok(resp)) => {
                            self.send_to_viewer(viewer_id, resp);
                            return;
                        }
                        Some(Err(err_resp)) => {
                            self.send_to_viewer(viewer_id, err_resp);
                            return;
                        }
                        None => {}
                    }
                }

                let owner = OwnerRef {
                    viewer: Some(viewer_id),
                    downstream_id: id.clone(),
                    client_req_id: client_req_id.clone(),
                };

                if let Some(crid) = &client_req_id
                    && self.turn.attach_waiter(crid, owner.clone())
                {
                    return;
                }

                if let Some(crid) = &client_req_id
                    && let Some(waiters) = self.finalizing_waiters.get_mut(crid)
                {
                    waiters.push(owner);
                    return;
                }

                if let Some(crid) = &client_req_id {
                    let is_inconsistent_inflight = {
                        let s = self.state_store.lock().await;
                        matches!(
                            s.ledger.get(crid),
                            Some(crate::state::LedgerEntry::InFlight { session: led_sid, .. }) if led_sid == &self.sid
                        )
                    };
                    if is_inconsistent_inflight {
                        let err_resp = crate::rpc::error(
                            id,
                            -32000,
                            "honkd lost track of this client_req_id; not resending".to_string(),
                            None,
                        );
                        self.send_to_viewer(viewer_id, err_resp);
                        return;
                    }
                }

                let actions = self.turn.on_prompt(owner, params, &mut self.ids);
                self.execute_actions(actions).await;
            }
            SessionMsg::Steer {
                viewer_id,
                viewer,
                id,
                params,
            } => {
                if matches!(self.state, ActorState::Reattaching | ActorState::Rebuilding) {
                    self.queue_msg(
                        viewer_id,
                        id.clone(),
                        SessionMsg::Steer {
                            viewer_id,
                            viewer,
                            id,
                            params,
                        },
                    );
                    return;
                }
                let up_id = self.ids.next();
                self.pending_requests.insert(
                    up_id.clone(),
                    PendingSessionRequest {
                        viewer_id,
                        downstream_id: id.clone(),
                        is_steer: true,
                    },
                );

                let mut clean_params = params;
                crate::rpc::strip_honk_meta(&mut clean_params);

                // Awaited INLINE on handle
                if let Err(e) = self
                    .up
                    .request_with_id(
                        json!(up_id),
                        "_goose/unstable/session/steer",
                        clean_params,
                        Some(&self.sid),
                    )
                    .await
                {
                    self.pending_requests.remove(&up_id);
                    let is_gone = honk_upstream::is_gone(&e);
                    let err_code = if is_gone { -32000 } else { -32603 };
                    let err_resp =
                        crate::rpc::error(id, err_code, format!("Steer POST failed: {e:#}"), None);
                    self.send_to_viewer(viewer_id, err_resp);
                    if is_gone {
                        Box::pin(self.handle_gone()).await;
                    }
                }
            }
            SessionMsg::Cancel => {
                if matches!(self.state, ActorState::Reattaching | ActorState::Rebuilding)
                    && self.queued_msgs.len() < 1024
                {
                    self.queued_msgs.push_back(SessionMsg::Cancel);
                    return;
                }
                let mut actions = self.turn.on_cancel();

                // If inflight prompt was unannounced (e.g. prompt POST was ambiguous and never arrived at goose),
                // cancel resolves it immediately so owners get a cancelled reply and the ledger is updated to Done.
                if self.turn.inflight.as_ref().is_some_and(|i| !i.announced) {
                    self.ambiguous_check_token += 1;
                    actions.extend(self.turn.interrupt("cancelled"));
                }

                self.execute_actions(actions).await;

                if !self.pending_server_requests.is_empty() {
                    let pending_list = std::mem::take(&mut self.pending_server_requests);
                    let sid = self.sid.clone();
                    let mutate_res = {
                        let mut s = self.state_store.lock().await;
                        s.mutate(|st| {
                            if let Some(h) = st.sessions.get_mut(&sid) {
                                h.pending.clear();
                            }
                        })
                    };
                    if let Err(e) = mutate_res {
                        tracing::error!(
                            "Session {}: failed to clear pending requests on cancel: {e:#}",
                            sid
                        );
                    }
                    let honk_vids: Vec<ViewerId> = self
                        .viewers
                        .iter()
                        .filter_map(|(vid, (_, is_honk))| if *is_honk { Some(*vid) } else { None })
                        .collect();

                    for p in pending_list {
                        let _ = self
                            .up
                            .respond(
                                p.id.clone(),
                                json!({
                                    "outcome": {
                                        "outcome": "cancelled"
                                    }
                                }),
                                Some(&sid),
                            )
                            .await;
                        self.recently_resolved.insert(p.id.to_string());
                        let resolved_frame = json!({
                            "jsonrpc": "2.0",
                            "method": honk_wire::method::REQUEST_RESOLVED,
                            "params": {
                                "sessionId": sid,
                                "requestId": p.id,
                                "reason": "cancelled",
                            }
                        });
                        for vid in &honk_vids {
                            self.send_to_viewer(*vid, resolved_frame.clone());
                        }
                    }
                }
            }
            SessionMsg::Answer { viewer_id, frame } => {
                if matches!(self.state, ActorState::Reattaching | ActorState::Rebuilding)
                    && self.queued_msgs.len() < 1024
                {
                    self.queued_msgs
                        .push_back(SessionMsg::Answer { viewer_id, frame });
                    return;
                }
                self.handle_server_request_answer(viewer_id, frame).await;
            }
            SessionMsg::MarkPendingStale => {
                self.mark_pending_stale().await;
            }
            SessionMsg::Forward {
                viewer_id,
                viewer,
                id,
                method,
                params,
            } => {
                if matches!(self.state, ActorState::Reattaching | ActorState::Rebuilding) {
                    self.queue_msg(
                        viewer_id,
                        id.clone(),
                        SessionMsg::Forward {
                            viewer_id,
                            viewer,
                            id,
                            method,
                            params,
                        },
                    );
                    return;
                }
                let up_id = self.ids.next();
                self.pending_requests.insert(
                    up_id.clone(),
                    PendingSessionRequest {
                        viewer_id,
                        downstream_id: id.clone(),
                        is_steer: false,
                    },
                );

                let mut clean_params = params;
                crate::rpc::strip_honk_meta(&mut clean_params);

                // Awaited INLINE on handle
                if let Err(e) = self
                    .up
                    .request_with_id(json!(up_id), &method, clean_params, Some(&self.sid))
                    .await
                {
                    self.pending_requests.remove(&up_id);
                    let is_gone = honk_upstream::is_gone(&e);
                    let err_code = if is_gone { -32000 } else { -32603 };
                    let err_resp = crate::rpc::error(
                        id,
                        err_code,
                        format!("Forward POST failed: {e:#}"),
                        None,
                    );
                    self.send_to_viewer(viewer_id, err_resp);
                    if is_gone {
                        Box::pin(self.handle_gone()).await;
                    }
                }
            }
            SessionMsg::Reattached { outcome } => {
                self.on_reattached(outcome).await;
            }
            SessionMsg::ReattachFailed { gone, error } => {
                self.on_reattach_failed(gone, error).await;
            }
            SessionMsg::RetryReattach => {
                if self.state == ActorState::Reattaching {
                    self.do_reattach().await;
                }
            }
            SessionMsg::Rebuilt {
                token,
                frames,
                keys,
                resume_from,
                client_prompts,
            } => {
                if token != self.rebuild_token
                    || self.state != ActorState::Rebuilding
                    || self.reactivating
                {
                    tracing::warn!(
                        "Session {}: ignoring stale Rebuilt msg (token={token}, current={})",
                        self.sid,
                        self.rebuild_token
                    );
                    return;
                }
                self.on_rebuilt(frames, keys, resume_from, client_prompts)
                    .await;
            }
            SessionMsg::ReplyText { owners, body, text } => {
                self.handle_reply_text(owners, body, text).await;
            }
            SessionMsg::SynthesizeGapEndTurn { upstream_id } => {
                self.gap_synthesis_pending = false;
                if self
                    .turn
                    .inflight
                    .as_ref()
                    .is_some_and(|inf| inf.upstream_id == upstream_id)
                {
                    self.synthesized_replies.insert(upstream_id.clone());
                    let synth_reply = json!({
                        "jsonrpc": "2.0",
                        "id": upstream_id,
                        "result": {
                            "stopReason": "end_turn",
                            "_meta": {
                                "honk": {
                                    "synthesized": true
                                }
                            }
                        }
                    });
                    let reply_actions =
                        self.turn
                            .on_prompt_reply(&upstream_id, &synth_reply, &mut self.ids);
                    self.execute_actions(reply_actions).await;
                }
            }
            // Conservative Bounded Rule for Ambiguous Prompt POST Failures:
            //
            // When a prompt POST fails ambiguously (e.g. transport reset or timeout after bytes were sent),
            // honkd cannot immediately determine whether goose received the request.
            // Goose handles requests concurrently (dispatch.rs cx.spawn) and resolves the session agent
            // (which may involve loading extensions, taking seconds) BEFORE starting an active run
            // (start_active_run) and BEFORE committing the user message to the session export.
            // Therefore, a probe returning "no active run to steer" or an export missing the prompt in the
            // first few hundred milliseconds is NOT evidence of non-delivery. Declaring false-negative
            // not_delivered causes client retries to duplicate the prompt, and unparking next prompts while
            // goose is still running the first causes concurrent prompt runs.
            //
            // Rule:
            // 1. If a run is announced on the session stream or a prompt reply arrives, the prompt is
            //    delivered (existing paths). Pending ambiguous check timers become no-ops.
            // 2. Otherwise, poll at roughly 2s, 5s, 10s, 20s, 30s after the failure (proportionally scaled
            //    by --ambiguous-inflight-timeout-secs, default 30s). At each point, issue a non-destructive
            //    steer probe and export check.
            // 3. If ANY probe shows an active run (actualRunId) or the export contains the prompt at index N,
            //    the prompt is delivered: stop polling and transition to existing bounded-wait machinery
            //    (probe + grace synthesis).
            // 4. Only if the entire window expires with every probe idle AND the export never containing
            //    prompt N do we call turn.on_prompt_not_delivered, returning cancelled/not_delivered to
            //    the owner and dispatching any parked prompts.
            //
            // Residual Risk:
            // If a prompt POST actually reached goose, but goose's request handler stalls longer than
            // the entire window (--ambiguous-inflight-timeout-secs) before registering its run or committing
            // the prompt (e.g. due to severe system overload or stalled extension loading), honkd will
            // declare not_delivered after the window expires. If the client subsequently retries, goose
            // may eventually process both the stalled request and the retry, resulting in a duplicate prompt.
            SessionMsg::AmbiguousCheckPoll {
                token,
                upstream_id,
                is_last,
            } => {
                if token != self.ambiguous_check_token {
                    return;
                }
                let Some(inf) = self.turn.inflight.as_ref() else {
                    return;
                };
                if inf.upstream_id != upstream_id || inf.announced {
                    return;
                }

                if is_last {
                    self.ambiguous_window_expired = true;
                }

                // 1. Issue non-destructive steer probe
                self.issue_steer_probe().await;

                // 2. Issue export check via spawned task (actor never blocks)
                let hub = self.hub.clone();
                let sid = self.sid.clone();
                let self_tx = self.self_tx.clone();
                let up_id = upstream_id.clone();
                self.ambiguous_export_in_flight = true;
                tokio::spawn(async move {
                    let res = hub.export(&sid).await;
                    let _ = self_tx
                        .send(SessionMsg::AmbiguousExportResult {
                            token,
                            upstream_id: up_id,
                            is_last,
                            result: res,
                        })
                        .await;
                });
            }
            SessionMsg::AmbiguousExportResult {
                token,
                upstream_id,
                is_last: _,
                result,
            } => {
                if token != self.ambiguous_check_token {
                    return;
                }
                let Some(inf) = self.turn.inflight.as_ref() else {
                    return;
                };
                if inf.upstream_id != upstream_id || inf.announced {
                    return;
                }
                self.ambiguous_export_in_flight = false;
                match result {
                    Ok(export_val) => {
                        let prompts = honk_wire::export::client_prompts(&export_val);
                        let n = inf.prompt_index;
                        let expected_text = crate::rpc::extract_user_text(&inf.params);
                        if prompts.len() > n && prompts[n] == expected_text {
                            tracing::info!(
                                "Session {}: ambiguous prompt {upstream_id} confirmed delivered via export at index {n}",
                                self.sid
                            );
                            self.on_ambiguous_delivered_via_export(n).await;
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            "Session {}: export check for ambiguous prompt failed: {e:#}",
                            self.sid
                        );
                    }
                }

                self.maybe_finish_ambiguous_not_delivered(&upstream_id)
                    .await;
            }
            SessionMsg::ProbeTimeout { probe_id } => {
                if self.active_probe_id.as_deref() == Some(&probe_id) {
                    tracing::warn!(
                        "Session {}: steer probe {probe_id} timed out without response; resetting active probe",
                        self.sid
                    );
                    self.active_probe_id = None;
                    if self.turn.run.is_some() || self.gap_evidenced {
                        self.issue_steer_probe().await;
                    } else if let Some(inf) = self.turn.inflight.as_ref()
                        && !inf.announced
                    {
                        let up_id = inf.upstream_id.clone();
                        self.maybe_finish_ambiguous_not_delivered(&up_id).await;
                    }
                }
            }
            SessionMsg::ReactivationComplete { token, data } => {
                if token != self.reactivation_token || !self.reactivating {
                    tracing::warn!(
                        "Session {}: ignoring stale ReactivationComplete (token={token}, current={})",
                        self.sid,
                        self.reactivation_token
                    );
                    return;
                }
                self.reactivating = false;
                let ReactivationData {
                    conn_id,
                    up_handle,
                    event_rx,
                    new_log,
                    next_epoch: _,
                    load_reply,
                    keys,
                    initial_events,
                    client_prompts,
                } = *data;

                let handle = crate::hub::SessionHandle {
                    sid: self.sid.clone(),
                    conn_id,
                    tx: self.self_tx.clone(),
                };
                self.hub
                    .sessions
                    .write()
                    .await
                    .insert(self.sid.clone(), handle);

                self.up = up_handle;
                self.events = event_rx;
                self.log = new_log;
                self.last_processed_session_id = self.log.last_upstream_id();
                self.last_processed_conn_id = None;
                self.init_result = load_reply;
                self.suppressed_keys = keys;
                self.turn.client_prompts = client_prompts;
                self.sync_inflight().await;
                self.fail_pending_requests("connection_replaced");

                let head = self.log.head();
                let reset_frame = json!({
                    "jsonrpc": "2.0",
                    "method": "_honk/reset",
                    "params": {
                        "sessionId": self.sid,
                        "epoch": head.epoch,
                    }
                });
                let replay_done_frame = json!({
                    "jsonrpc": "2.0",
                    "method": "_honk/replay_done",
                    "params": {
                        "sessionId": self.sid,
                        "epoch": head.epoch,
                        "seq": head.seq,
                    }
                });

                let mut msgs = Vec::with_capacity(self.log.entries().len() + 2);
                msgs.push(axum::extract::ws::Message::Text(
                    reset_frame.to_string().into(),
                ));
                for entry in self.log.entries() {
                    msgs.push(axum::extract::ws::Message::Text(
                        entry.frame.to_string().into(),
                    ));
                }
                msgs.push(axum::extract::ws::Message::Text(
                    replay_done_frame.to_string().into(),
                ));

                for (tx, is_honk_aware) in self.viewers.values() {
                    if *is_honk_aware {
                        let batch = crate::viewer::ReplayBatch {
                            session_id: self.sid.clone(),
                            head: head.clone(),
                            msgs: msgs.clone(),
                            done_tx: None,
                        };
                        let _ = tx.send_replay(batch);
                    }
                }

                for ev in initial_events {
                    self.on_upstream(ev).await;
                }

                self.state = ActorState::Live;
                self.drain_queued_msgs().await;
            }
            SessionMsg::ReactivationFailed {
                token,
                reason,
                permanent: _,
            } => {
                if token != self.reactivation_token || !self.reactivating {
                    tracing::warn!(
                        "Session {}: ignoring stale ReactivationFailed (token={token}, current={})",
                        self.sid,
                        self.reactivation_token
                    );
                    return;
                }
                self.reactivating = false;
                tracing::error!("Session {} reactivation failed: {reason}", self.sid);
                self.state = ActorState::Gone;
                self.fail_pending_requests("reactivation_failed");

                // Remove from hub.sessions
                self.hub.sessions.write().await.remove(&self.sid);

                // Remove from state.sessions and record retired_epochs
                let sid = self.sid.clone();
                let epoch = self.log.head().epoch;
                {
                    let mut s = self.state_store.lock().await;
                    let _ = s.mutate(|st| {
                        if let Some(held) = st.sessions.remove(&sid) {
                            st.retired_epochs.insert(sid.clone(), held.epoch);
                        } else {
                            st.retired_epochs.insert(sid.clone(), epoch);
                        }
                    });
                }

                // Fail all finalizing waiters with -32000
                let finalizing = std::mem::take(&mut self.finalizing_waiters);
                for (_, waiters) in finalizing {
                    for w in waiters {
                        let err_resp = crate::rpc::error(
                            w.downstream_id.clone(),
                            -32000,
                            format!("Session is gone: {reason}"),
                            None,
                        );
                        self.send_to_owner(&w, err_resp);
                    }
                }

                // Fail all queued msgs with -32000
                let msgs = std::mem::take(&mut self.queued_msgs);
                for m in msgs {
                    match m {
                        SessionMsg::Prompt { viewer_id, id, .. }
                        | SessionMsg::Steer { viewer_id, id, .. }
                        | SessionMsg::Forward { viewer_id, id, .. } => {
                            let err_resp = crate::rpc::error(
                                id,
                                -32000,
                                format!("Session is gone: {reason}"),
                                None,
                            );
                            self.send_to_viewer(viewer_id, err_resp);
                        }
                        _ => {}
                    }
                }

                // Notify honk-aware viewers with _honk/evicted
                let evicted_frame = json!({
                    "jsonrpc": "2.0",
                    "method": honk_wire::method::EVICTED,
                    "params": {
                        "sessionId": self.sid,
                    }
                });
                for (tx, is_honk_aware) in self.viewers.values() {
                    if *is_honk_aware {
                        let _ = tx.send_frame(axum::extract::ws::Message::Text(
                            evicted_frame.to_string().into(),
                        ));
                    }
                }
            }
        }
    }

    async fn on_ambiguous_delivered_via_export(&mut self, n: usize) {
        self.ambiguous_check_token += 1;
        let delivered_info = if let Some(inf) = self.turn.inflight.as_mut()
            && !inf.announced
        {
            inf.announced = true;
            Some((inf.params.clone(), inf.owners.first().cloned()))
        } else {
            None
        };
        if let Some((params, Some(owner))) = delivered_info {
            let synth_action = Action::SynthUser { params, owner };
            self.execute_actions(vec![synth_action]).await;
        }
        if self.turn.client_prompts <= n {
            self.turn.client_prompts = n + 1;
            self.sync_inflight().await;
        }
        self.gap_evidenced = true;
        self.issue_steer_probe().await;
    }

    async fn maybe_finish_ambiguous_not_delivered(&mut self, upstream_id: &str) {
        if !self.ambiguous_window_expired {
            return;
        }
        if self.ambiguous_export_in_flight {
            return;
        }
        if self.active_probe_id.is_some() {
            return;
        }
        let Some(inf) = self.turn.inflight.as_ref() else {
            return;
        };
        if inf.upstream_id != upstream_id || inf.announced {
            return;
        }

        tracing::warn!(
            "Session {}: ambiguous prompt {upstream_id} window expired with all probes idle and not in export; marking not_delivered",
            self.sid
        );
        self.ambiguous_check_token += 1;
        let actions = self
            .turn
            .on_prompt_not_delivered(upstream_id, &mut self.ids);
        self.execute_actions(actions).await;
    }

    async fn handle_reply_text(
        &mut self,
        owners: Vec<OwnerRef>,
        body: Reply,
        text: Option<String>,
    ) {
        match body {
            Reply::Result(mut res) => {
                if let Some(t) = text {
                    if let Some(meta) = res.get_mut("_meta").and_then(Value::as_object_mut) {
                        if let Some(honk) = meta.get_mut("honk").and_then(Value::as_object_mut) {
                            honk.insert("text".to_string(), Value::String(t));
                        } else {
                            meta.insert("honk".to_string(), json!({ "text": t }));
                        }
                    } else {
                        res["_meta"] = json!({ "honk": { "text": t } });
                    }
                }

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                for o in &owners {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": o.downstream_id,
                        "result": res,
                    });

                    // Ledger: InFlight -> Done{response}
                    if let Some(crid) = &o.client_req_id {
                        let sid = self.sid.clone();
                        let mut s = self.state_store.lock().await;
                        let _ = s.mutate(|st| {
                            let mut cached_resp = resp.clone();
                            cached_resp["id"] = Value::Null;
                            st.insert_done_entry(crid.clone(), sid, cached_resp, now);
                        });
                        drop(s);

                        if let Some(waiters) = self.finalizing_waiters.remove(crid) {
                            for w in waiters {
                                let mut w_resp = resp.clone();
                                w_resp["id"] = w.downstream_id.clone();
                                self.send_to_owner(&w, w_resp);
                            }
                        }
                    }

                    self.send_to_owner(o, resp);
                }
            }
            Reply::Error(err) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                let is_busy = crate::rpc::busy_run_id(&err).is_some();
                if is_busy {
                    let sid = self.sid.clone();
                    let crids: Vec<String> = owners
                        .iter()
                        .filter_map(|o| o.client_req_id.clone())
                        .collect();
                    let mut s = self.state_store.lock().await;
                    let _ = s.mutate(|st| {
                        if let Some(h) = st.sessions.get_mut(&sid) {
                            h.inflight = None;
                        }
                        for crid in crids {
                            st.ledger.remove(&crid);
                        }
                    });
                }

                for o in &owners {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": o.downstream_id,
                        "error": err,
                    });
                    if let Some(crid) = &o.client_req_id {
                        if !is_busy {
                            let sid = self.sid.clone();
                            let mut s = self.state_store.lock().await;
                            let _ = s.mutate(|st| {
                                if matches!(
                                    st.ledger.get(crid),
                                    Some(crate::state::LedgerEntry::InFlight { .. })
                                ) {
                                    st.insert_done_entry(crid.clone(), sid, resp.clone(), now);
                                }
                            });
                            drop(s);
                        }

                        if let Some(waiters) = self.finalizing_waiters.remove(crid) {
                            for w in waiters {
                                let mut w_resp = resp.clone();
                                w_resp["id"] = w.downstream_id.clone();
                                self.send_to_owner(&w, w_resp);
                            }
                        }
                    }
                    self.send_to_owner(o, resp);
                }
            }
        }

        self.sync_inflight().await;
    }

    fn attach_live(&mut self, viewer_id: ViewerId, viewer: ViewerTx, is_honk_aware: bool) {
        let was_present = self
            .viewers
            .insert(viewer_id, (viewer.clone(), is_honk_aware))
            .is_some();

        if !was_present {
            if !is_honk_aware {
                let synth_info = json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": self.sid,
                        "update": {
                            "sessionUpdate": "session_info_update",
                            "_meta": {
                                "goose": {
                                    "activeRunId": self.turn.run
                                }
                            }
                        }
                    }
                });
                viewer.send_frame(axum::extract::ws::Message::Text(
                    synth_info.to_string().into(),
                ));
            }

            for p in &self.pending_server_requests {
                let pending_frame = json!({
                    "jsonrpc": "2.0",
                    "id": p.id,
                    "method": p.method,
                    "params": p.params,
                });
                viewer.send_frame(axum::extract::ws::Message::Text(
                    pending_frame.to_string().into(),
                ));
            }
        }
    }

    async fn on_server_request(&mut self, frame: Value, _id_opt: Option<u64>) {
        let req_id = match frame.get("id") {
            Some(id) if !id.is_null() => id.clone(),
            _ => return,
        };
        let method = match frame.get("method").and_then(Value::as_str) {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => return,
        };

        // If the id is already pending (a replayed frame), skip it
        if self
            .pending_server_requests
            .iter()
            .any(|p| honk_upstream::rpc_id_matches(&p.id, &req_id))
        {
            return;
        }

        // If this id was already resolved in this session, skip it
        if self.recently_resolved.contains(&req_id.to_string()) {
            return;
        }

        let params = frame.get("params").cloned().unwrap_or(json!({}));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let pending_req = crate::state::PendingRequest {
            id: req_id.clone(),
            method: method.clone(),
            params: params.clone(),
            received_unix: now,
        };

        // Persist via state.mutate BEFORE offering per ground rules.
        // If disk persistence fails, log and return early without adding to memory or
        // offering to viewers. This allows a subsequent replayed frame to retry persistence.
        let sid = self.sid.clone();
        let pending_clone = pending_req.clone();
        let mutate_res = {
            let mut s = self.state_store.lock().await;
            s.mutate(|st| {
                if let Some(h) = st.sessions.get_mut(&sid) {
                    h.pending.push(pending_clone);
                }
            })
        };
        if let Err(e) = mutate_res {
            tracing::error!(
                "Session {}: failed to persist pending request {} before offer: {e:#}",
                self.sid,
                req_id
            );
            return;
        }
        self.pending_server_requests.push(pending_req);

        // Offer verbatim (goose's id) to every attached viewer, honk-aware and legacy alike
        let viewer_ids: Vec<ViewerId> = self.viewers.keys().copied().collect();
        for vid in viewer_ids {
            self.send_to_viewer(vid, frame.clone());
        }
    }

    // Crash window safety note:
    // We execute up.respond BEFORE removing the pending request from persistent state (state.mutate).
    // If honkd crashes after up.respond succeeds but before state removal completes, the pending request
    // remains on disk and will be restored on restart and re-offered to viewers. A subsequent answer will
    // be sent upstream again with up.respond.
    //
    // As verified in goose source:
    // - vendor/agent-client-protocol-http/src/http_server.rs:183-186:
    //   POST /acp accepts any valid JSON-RPC response frame and returns HTTP 202 Accepted via
    //   connection.send_frame_to_agent.
    // - crates/goose/src/acp/server.rs:1606-1632:
    //   cx.send_request(permission_request).on_receiving_result(...) completes when the first result arrives.
    // - vendor/agent-client-protocol-http/src/connection.rs:373-383:
    //   The response route is removed once completed; subsequent response frames for an already-resolved
    //   request id are discarded without error.
    //
    // Therefore, a duplicate upstream answer is completely harmless. Conversely, if we removed from state
    // before up.respond succeeded and crashed, the pending request would be permanently lost from honkd while
    // goose remained parked forever waiting for a response. Thus, respond-then-remove is strictly safer.
    async fn handle_server_request_answer(&mut self, viewer_id: ViewerId, frame: Value) {
        let resp_id = match frame.get("id") {
            Some(id) if !id.is_null() => id.clone(),
            _ => return,
        };

        let pending_idx = self
            .pending_server_requests
            .iter()
            .position(|p| honk_upstream::rpc_id_matches(&p.id, &resp_id));

        let Some(idx) = pending_idx else {
            tracing::debug!(
                "Session {}: ignoring late or unknown server request answer for id {}",
                self.sid,
                resp_id
            );
            return;
        };

        // Note: Do not pop from pending_server_requests yet! If up.respond fails with 404/Gone,
        // on_reattach_failed -> mark_pending_stale needs this request in the list to broadcast "stale".
        // If up.respond fails with a non-gone error, we keep it pending and re-offer it.
        let pending = self.pending_server_requests[idx].clone();
        let sid = self.sid.clone();
        let up_id = pending.id.clone();
        let result = frame.get("result").cloned().unwrap_or(Value::Null);

        // 1. First viewer answer goes upstream with up.respond(id, result, Some(sid)) on this connection.
        // Include one bounded retry for transient errors.
        let mut respond_res = self
            .up
            .respond(up_id.clone(), result.clone(), Some(&sid))
            .await;
        if let Err(ref e) = respond_res
            && !honk_upstream::is_gone(e)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
            respond_res = self.up.respond(up_id.clone(), result, Some(&sid)).await;
        }

        if let Err(e) = respond_res {
            tracing::error!(
                "Session {}: failed to send upstream respond for id {}: {e:#}",
                self.sid,
                up_id
            );
            if honk_upstream::is_gone(&e) {
                // Request is still in pending_server_requests, so mark_pending_stale will find it
                // and broadcast "stale" to viewers.
                self.on_reattach_failed(true, e.to_string()).await;
                return;
            }

            // On non-gone failure: keep it pending in memory and state, do not broadcast request_resolved.
            // Re-offer the request verbatim to the answering viewer. A JSON-RPC response cannot receive
            // an error reply, so re-offering the request frame informs the viewer that the request is still pending.
            let pending_frame = json!({
                "jsonrpc": "2.0",
                "id": pending.id,
                "method": pending.method,
                "params": pending.params,
            });
            self.send_to_viewer(viewer_id, pending_frame);
            return;
        }

        // 2. Success: remove from in-memory pending_server_requests
        self.pending_server_requests
            .retain(|p| !honk_upstream::rpc_id_matches(&p.id, &up_id));

        // 3. Remove from persistent state
        let mutate_res = {
            let mut s = self.state_store.lock().await;
            s.mutate(|st| {
                if let Some(h) = st.sessions.get_mut(&sid) {
                    h.pending
                        .retain(|p| !honk_upstream::rpc_id_matches(&p.id, &up_id));
                }
            })
        };
        if let Err(e) = mutate_res {
            tracing::error!(
                "Session {}: failed to remove answered pending request {} from state: {e:#}",
                self.sid,
                up_id
            );
        }
        self.recently_resolved.insert(up_id.to_string());

        // 4. Send other honk-aware viewers _honk/request_resolved{reason:"answered"}
        let resolved_frame = json!({
            "jsonrpc": "2.0",
            "method": honk_wire::method::REQUEST_RESOLVED,
            "params": {
                "sessionId": sid,
                "requestId": up_id,
                "reason": "answered",
            }
        });

        let honk_vids: Vec<ViewerId> = self
            .viewers
            .iter()
            .filter_map(|(vid, (_, is_honk))| {
                if *is_honk && *vid != viewer_id {
                    Some(*vid)
                } else {
                    None
                }
            })
            .collect();

        for vid in honk_vids {
            self.send_to_viewer(vid, resolved_frame.clone());
        }
    }

    pub async fn mark_pending_stale(&mut self) {
        if self.pending_server_requests.is_empty() {
            return;
        }

        let pending_list = std::mem::take(&mut self.pending_server_requests);
        let sid = self.sid.clone();

        let mutate_res = {
            let mut s = self.state_store.lock().await;
            s.mutate(|st| {
                if let Some(h) = st.sessions.get_mut(&sid) {
                    h.pending.clear();
                }
            })
        };
        if let Err(e) = mutate_res {
            tracing::error!(
                "Session {}: failed to clear pending requests in state on stale: {e:#}",
                sid
            );
        }

        let honk_vids: Vec<ViewerId> = self
            .viewers
            .iter()
            .filter_map(|(vid, (_, is_honk))| if *is_honk { Some(*vid) } else { None })
            .collect();

        for p in pending_list {
            self.recently_resolved.insert(p.id.to_string());
            let resolved_frame = json!({
                "jsonrpc": "2.0",
                "method": honk_wire::method::REQUEST_RESOLVED,
                "params": {
                    "sessionId": sid,
                    "requestId": p.id,
                    "reason": "stale",
                }
            });
            for vid in &honk_vids {
                self.send_to_viewer(*vid, resolved_frame.clone());
            }
        }
    }

    fn send_to_owner(&mut self, owner: &OwnerRef, frame: Value) {
        if let Some(vid) = owner.viewer {
            self.send_to_viewer(vid, frame);
        }
    }

    fn fan_out_stamped(&mut self, stamped_frame: &Value, honk_aware_only: bool) {
        let mut to_send = Vec::new();
        let mut legacy_frame = None;

        for (vid, (_, is_honk_aware)) in &self.viewers {
            if *is_honk_aware {
                to_send.push((*vid, stamped_frame.clone()));
            } else if !honk_aware_only {
                let frame = legacy_frame.get_or_insert_with(|| {
                    let mut f = stamped_frame.clone();
                    crate::rpc::strip_honk_meta(&mut f);
                    f
                });
                to_send.push((*vid, frame.clone()));
            }
        }

        for (vid, frame) in to_send {
            self.send_to_viewer(vid, frame);
        }
    }
}

pub(crate) fn append_reply_sidecar(
    path: &Path,
    upstream_id: &str,
    reply: &Value,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    let line = json!({
        "upstream_id": upstream_id,
        "reply": reply,
    });
    let mut s = serde_json::to_string(&line)?;
    s.push('\n');
    file.write_all(s.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

pub(crate) fn load_reply_sidecar(path: &Path) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in content.lines() {
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && let Some(uid) = v.get("upstream_id").and_then(Value::as_str)
            && let Some(reply) = v.get("reply")
        {
            map.insert(uid.to_string(), reply.clone());
        }
    }
    map
}
