use honk_upstream::{Event, EventRx, ReplayOutcome, StreamKey, SubscribeOpts, UpstreamHandle};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

#[derive(Clone, Debug)]
pub struct AttachSnapshot {
    pub reset: bool,
    pub entries: Vec<crate::log::Entry>,
    pub head: honk_wire::Cursor,
    pub init_result: Value,
    pub pending: Vec<crate::state::PendingRequest>,
}

pub enum SessionMsg {
    // from viewers / hub
    Attach {
        viewer_id: ViewerId,
        viewer: ViewerTx,
        after: Option<honk_wire::Cursor>,
        reply: tokio::sync::oneshot::Sender<AttachSnapshot>,
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
        frames: Vec<Value>,
        keys: std::collections::HashSet<honk_wire::BlockKey>,
        resume_from: Option<u64>,
    },
    ReplyText {
        owners: Vec<OwnerRef>,
        body: Reply,
        text: Option<String>,
    },
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
}

impl Session {
    pub async fn run(mut self) {
        let mut watchdog = tokio::time::interval(Duration::from_secs(5));
        let mut idle = tokio::time::interval(Duration::from_secs(60));

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

        loop {
            tokio::select! {
                biased;
                ev = self.events.recv(), if !matches!(self.state, ActorState::Gone) => match ev {
                    Some(ev) => {
                        self.on_upstream(ev).await;
                        tokio::task::yield_now().await;
                    }
                    None => break,
                },
                msg = self.rx.recv() => match msg {
                    Some(m) => self.on_msg(m).await,
                    None => break,
                },
                _ = watchdog.tick() => {
                    self.check_streams().await;
                }
                _ = idle.tick() => {
                    self.on_idle();
                }
            }
        }
        self.return_undelivered_to_hub().await;
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
        if self.state == ActorState::Gone || self.state == ActorState::Reattaching {
            return;
        }
        self.state = ActorState::Reattaching;

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
            self.check_restored_inflight_delivery().await;
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

                // Check whether goose is still running the turn live, or if the turn already ended in the gap.
                // In goose (crates/goose/src/acp/server.rs:2360-2374), activeRunId:null and the prompt response
                // are sent in-order to the session stream mailbox (vendor/agent-client-protocol-http/src/connection.rs:385-395).
                // Case (a): If the turn is still running live on goose, export has no turn text for the inflight prompt yet.
                //           We stay Live, send _honk/gap, and wait for activeRunId:null and real prompt reply to arrive live.
                // Case (b): If the turn finished inside the gap, export already has the committed assistant message.
                //           Goose is idle and will never send activeRunId:null live. We trigger rebuild immediately
                //           and synthesize end_turn upon rebuild completion.
                let is_turn_running = if let Some(inf) = self.turn.inflight.as_ref() {
                    let user_text = crate::rpc::extract_user_text(&inf.params);
                    match self.hub.export(&self.sid).await {
                        Ok(export_val) => {
                            let has_completed_turn =
                                !honk_wire::export::turn_text(&export_val, &user_text).is_empty();
                            !has_completed_turn
                        }
                        Err(_) => self.turn.run.is_some(),
                    }
                } else {
                    self.turn.run.is_some()
                };

                if is_turn_running {
                    self.state = ActorState::Live;
                    let gap_marker = json!({
                        "jsonrpc": "2.0",
                        "method": "_honk/gap",
                        "params": {
                            "sessionId": self.sid,
                        }
                    });
                    self.fan_out_stamped(&gap_marker, true);
                    self.rebuild_at_turn_end = true;
                    self.drain_queued_msgs().await;
                } else {
                    self.turn.run = None;
                    self.start_rebuild().await;
                }
            }
            ReplayOutcome::NotRequested => {
                self.state = ActorState::Live;
                self.drain_queued_msgs().await;
            }
        }
    }

    async fn on_reattach_failed(&mut self, gone: bool, error: String) {
        tracing::error!(
            "Session {} reattach failed: gone={gone}, err={error}",
            self.sid
        );
        if gone {
            self.state = ActorState::Gone;
            let actions = self.turn.interrupt("upstream_gone");
            self.execute_actions(actions).await;

            let finalizing = std::mem::take(&mut self.finalizing_waiters);
            for (_, waiters) in finalizing {
                for w in waiters {
                    let err_resp = crate::rpc::error(
                        w.downstream_id.clone(),
                        -32000,
                        "Session is gone (upstream 404)".to_string(),
                        None,
                    );
                    self.send_to_owner(&w, err_resp);
                }
            }

            let msgs = std::mem::take(&mut self.queued_msgs);
            for m in msgs {
                match m {
                    SessionMsg::Prompt { viewer_id, id, .. }
                    | SessionMsg::Steer { viewer_id, id, .. }
                    | SessionMsg::Forward { viewer_id, id, .. } => {
                        let err_resp = crate::rpc::error(
                            id,
                            -32000,
                            "Session is gone (upstream 404)".to_string(),
                            None,
                        );
                        self.send_to_viewer(viewer_id, err_resp);
                    }
                    _ => {}
                }
            }
        } else {
            self.state = ActorState::Reattaching;
            let self_tx = self.self_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = self_tx.send(SessionMsg::RetryReattach).await;
            });
        }
    }

    async fn check_restored_inflight_delivery(&mut self) {
        let Some(inf) = self.turn.inflight.as_ref() else {
            return;
        };
        let user_text = crate::rpc::extract_user_text(&inf.params);
        match self.hub.export(&self.sid).await {
            Ok(export_val) => {
                if !export_has_user_text(&export_val, &user_text) {
                    let actions = self.turn.interrupt("not_delivered");
                    self.execute_actions(actions).await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    "failed to export session {} for inflight check: {e:#}",
                    self.sid
                );
            }
        }
    }

    async fn start_rebuild(&mut self) {
        if self.state == ActorState::Gone || self.state == ActorState::Rebuilding {
            return;
        }
        self.state = ActorState::Rebuilding;
        self.rebuilding_buffered_frames.clear();

        let hub = self.hub.clone();
        let sid = self.sid.clone();
        let self_tx = self.self_tx.clone();
        let resume_from = self.last_processed_session_id;

        tokio::spawn(async move {
            match hub.export(&sid).await {
                Ok(export_val) => {
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
                            frames,
                            keys,
                            resume_from,
                        })
                        .await;
                }
                Err(e) => {
                    tracing::error!("rebuild export failed for {sid}: {e:#}");
                }
            }
        });
    }

    async fn on_rebuilt(
        &mut self,
        frames: Vec<Value>,
        keys: HashSet<honk_wire::BlockKey>,
        resume_from: Option<u64>,
    ) {
        let sid = self.sid.clone();
        let current_epoch = self.log.epoch();
        let mut next_epoch = current_epoch + 1;
        {
            let mut s = self.state_store.lock().await;
            let _ = s.mutate(|st| {
                if let Some(h) = st.sessions.get_mut(&sid) {
                    next_epoch = h.epoch.max(current_epoch) + 1;
                    h.epoch = next_epoch;
                } else if let Some(ret) = st.retired_epochs.get(&sid) {
                    next_epoch = ret.max(&current_epoch) + 1;
                }
            });
        }

        let log_path = self.state_dir.join("sessions").join(format!("{sid}.jsonl"));
        let mut combined: Vec<(Option<u64>, Value)> = Vec::new();
        for f in frames {
            combined.push((None, f));
        }
        for (id_opt, f) in std::mem::take(&mut self.rebuilding_buffered_frames) {
            let key = f.pointer("/params/update").and_then(honk_wire::block_key);
            if let Some(k) = key
                && keys.contains(&k)
            {
                continue;
            }
            combined.push((id_opt, f));
        }

        let max_resume = match (resume_from, self.last_processed_session_id) {
            (Some(r), Some(l)) => Some(r.max(l)),
            (None, l) => l,
            (r, None) => r,
        };

        match crate::log::SessionLog::create(&log_path, next_epoch, max_resume, combined) {
            Ok(new_log) => {
                self.log = new_log;
                let head = self.log.head();
                self.last_processed_session_id = self.log.last_upstream_id();

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

                // Drain already-buffered events from the session stream before checking synthesis
                while let Ok(ev) = self.events.try_recv() {
                    self.on_upstream(ev).await;
                }

                if self.gap_evidenced && self.turn.inflight.is_some() {
                    let upstream_id = self.turn.inflight.as_ref().unwrap().upstream_id.clone();
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
                self.gap_evidenced = false;
            }
            Err(e) => {
                tracing::error!("failed to create session log during rebuild of {sid}: {e:#}");
            }
        }

        self.state = ActorState::Live;
        self.drain_queued_msgs().await;
    }

    async fn drain_queued_msgs(&mut self) {
        let msgs = std::mem::take(&mut self.queued_msgs);
        for m in msgs {
            Box::pin(self.on_msg(m)).await;
        }
    }

    fn on_idle(&mut self) {
        // Stub for idle eviction in A16
    }

    async fn return_undelivered_to_hub(&mut self) {
        while let Ok(_msg) = self.rx.try_recv() {
            tracing::warn!("Session {} exiting; drained undelivered msg", self.sid);
        }
    }

    async fn sync_inflight(&self) {
        let target = self.turn.to_persisted();
        let sid = &self.sid;
        let mut s = self.state_store.lock().await;
        if let Some(h) = s.sessions.get(sid)
            && h.inflight == target
        {
            return;
        }
        let _ = s.mutate(|st| {
            if let Some(h) = st.sessions.get_mut(sid) {
                h.inflight = target;
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
                if self.state == ActorState::Live {
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
                        let actions = self.turn.on_steer_ok(msg_id.to_string(), pending.viewer_id);
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
                        let upstream_id = self.turn.inflight.as_ref().unwrap().upstream_id.clone();
                        let sidecar_path = self
                            .state_dir
                            .join("sessions")
                            .join(format!("{}.replies.jsonl", self.sid));
                        let _ = append_reply_sidecar(&sidecar_path, &upstream_id, &frame);

                        let actions =
                            self.turn
                                .on_prompt_reply(&upstream_id, &frame, &mut self.ids);
                        self.execute_actions(actions).await;
                    } else {
                        // Unmatched upstream response -> warn with upstream id only
                        let id_disp = frame
                            .get("id")
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "null".into());
                        tracing::warn!("unmatched upstream response for id: {id_disp}");
                    }
                }
            } else if kind == crate::rpc::Kind::Request {
                // Server request (e.g. request_permission) -> forward to viewers
                self.forward_raw(&frame);
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
                        let mut s = self.state_store.lock().await;
                        let _ = s.mutate(|st| {
                            if let Some(h) = st.sessions.get_mut(&sid) {
                                h.inflight = Some(persisted.clone());
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
                        });
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
                        if honk_upstream::is_gone(&e) {
                            self.state = ActorState::Gone;
                        }

                        // Prompt POST failed -> remove failed prompt's ledger entries AND clear inflight in one mutate
                        let failed_inf = self.turn.inflight.as_ref().map(|i| i.to_persisted());
                        {
                            let mut s = self.state_store.lock().await;
                            let _ = s.mutate(|st| {
                                if let Some(h) = st.sessions.get_mut(&sid) {
                                    h.inflight = None;
                                }
                                if let Some(inf) = &failed_inf {
                                    for crid in &inf.client_req_ids {
                                        st.ledger.remove(crid);
                                    }
                                }
                            });
                        }

                        let err_val = json!({
                            "code": -32603,
                            "message": format!("Prompt POST failed: {e:#}"),
                        });
                        let fail_actions =
                            self.turn
                                .on_prompt_send_failed(&upstream_id, err_val, &mut self.ids);
                        for a in fail_actions.into_iter().rev() {
                            queue.push_front(a);
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
                        for o in owners {
                            let resp = json!({
                                "jsonrpc": "2.0",
                                "id": o.downstream_id,
                                "error": err.clone(),
                            });
                            self.send_to_owner(&o, resp);
                        }
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
                    if let Some((_, is_honk_aware)) = self.viewers.get(&viewer)
                        && *is_honk_aware
                    {
                        let notify_frame = json!({
                            "jsonrpc": "2.0",
                            "method": "_honk/steer_unconsumed",
                            "params": {
                                "sessionId": self.sid,
                                "messageId": message_id,
                            }
                        });
                        self.send_to_viewer(viewer, notify_frame);
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
                reply,
            } => {
                let (reset, entries) = match self.log.resume(after.as_ref()) {
                    crate::log::Resume::Reset(e) => (true, e.to_vec()),
                    crate::log::Resume::Delta(e) => (false, e.to_vec()),
                };
                let head = self.log.head();

                self.viewers.insert(viewer_id, (viewer, true));

                let init_result = self.init_result.clone();
                let pending = Vec::new(); // Stub until A13

                let _ = reply.send(AttachSnapshot {
                    reset,
                    entries,
                    head,
                    init_result,
                    pending,
                });
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
                            Some(crate::state::LedgerEntry::Done { response, .. }) => {
                                let mut resp = response.clone();
                                resp["id"] = id.clone();
                                Some(Ok(resp))
                            }
                            Some(crate::state::LedgerEntry::InFlight {
                                session: led_sid, ..
                            }) => {
                                if led_sid != &self.sid {
                                    Some(Err(crate::rpc::error(
                                        id.clone(),
                                        -32602,
                                        format!(
                                            "client_req_id {crid} already in flight in another session"
                                        ),
                                        None,
                                    )))
                                } else {
                                    None
                                }
                            }
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
                    let err_resp =
                        crate::rpc::error(id, -32603, format!("Steer POST failed: {e:#}"), None);
                    self.send_to_viewer(viewer_id, err_resp);
                }
            }
            SessionMsg::Cancel => {
                if matches!(self.state, ActorState::Reattaching | ActorState::Rebuilding)
                    && self.queued_msgs.len() < 1024
                {
                    self.queued_msgs.push_back(SessionMsg::Cancel);
                    return;
                }
                let actions = self.turn.on_cancel();
                self.execute_actions(actions).await;
                // TODO(A13/A15): answer any pending server requests with "cancelled"
            }
            SessionMsg::Answer { viewer_id, frame } => {
                if matches!(self.state, ActorState::Reattaching | ActorState::Rebuilding)
                    && self.queued_msgs.len() < 1024
                {
                    self.queued_msgs
                        .push_back(SessionMsg::Answer { viewer_id, frame });
                }
                // Server request answer (A10/A13)
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
                    let err_resp =
                        crate::rpc::error(id, -32603, format!("Forward POST failed: {e:#}"), None);
                    self.send_to_viewer(viewer_id, err_resp);
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
                    self.trigger_reattach().await;
                }
            }
            SessionMsg::Rebuilt {
                frames,
                keys,
                resume_from,
            } => {
                self.on_rebuilt(frames, keys, resume_from).await;
            }
            SessionMsg::ReplyText { owners, body, text } => {
                self.handle_reply_text(owners, body, text).await;
            }
        }
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
                            st.ledger.insert(
                                crid.clone(),
                                crate::state::LedgerEntry::Done {
                                    session: sid,
                                    response: resp.clone(),
                                    done_unix: now,
                                },
                            );
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
                for o in &owners {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": o.downstream_id,
                        "error": err,
                    });
                    if let Some(crid) = &o.client_req_id
                        && let Some(waiters) = self.finalizing_waiters.remove(crid)
                    {
                        for w in waiters {
                            let mut w_resp = resp.clone();
                            w_resp["id"] = w.downstream_id.clone();
                            self.send_to_owner(&w, w_resp);
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

        if !was_present && !is_honk_aware {
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

    fn forward_raw(&mut self, frame: &Value) {
        let viewer_ids: Vec<ViewerId> = self.viewers.keys().copied().collect();
        for vid in viewer_ids {
            self.send_to_viewer(vid, frame.clone());
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

fn export_has_user_text(export: &Value, expected_text: &str) -> bool {
    let Some(conversation) = export.get("conversation").and_then(Value::as_array) else {
        return false;
    };
    // Only consider trailing user messages after the last completed assistant turn
    let last_assistant_idx = conversation
        .iter()
        .rposition(|msg| msg.get("role").and_then(Value::as_str) == Some("assistant"));

    let start_idx = match last_assistant_idx {
        Some(idx) => idx + 1,
        None => 0,
    };

    for msg in &conversation[start_idx..] {
        if msg.get("role").and_then(Value::as_str) == Some("user") {
            let mut text = String::new();
            if let Some(content) = msg.get("content").and_then(Value::as_array) {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(t) = block.get("text").and_then(Value::as_str)
                    {
                        text.push_str(t);
                    }
                }
            }
            if text == expected_text {
                return true;
            }
        }
    }
    false
}
