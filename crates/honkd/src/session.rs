use honk_upstream::{Event, EventRx, StreamKey, UpstreamHandle};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
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
}

impl Session {
    pub async fn run(mut self) {
        let mut watchdog = tokio::time::interval(Duration::from_secs(5));
        let mut idle = tokio::time::interval(Duration::from_secs(60));

        loop {
            tokio::select! {
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
                    self.check_streams();
                }
                _ = idle.tick() => {
                    self.on_idle();
                }
            }
        }
        self.return_undelivered_to_hub().await;
    }

    fn check_streams(&mut self) {
        // Stub for I3 watchdog in A12
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

    async fn on_upstream(&mut self, ev: Event) {
        let (key, frame, id_opt) = match ev {
            Event::Frame(key, frame, id_opt) => (key, frame, id_opt),
            _ => return,
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
                    let actions = self.turn.on_run_state(run_opt, &mut self.ids);
                    self.execute_actions(actions).await;
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
                let client_req_id = honk_wire::honk_meta(&params).client_req_id;
                let owner = OwnerRef {
                    viewer: Some(viewer_id),
                    downstream_id: id,
                    client_req_id,
                };
                let actions = self.turn.on_prompt(owner, params, &mut self.ids);
                self.execute_actions(actions).await;
            }
            SessionMsg::Steer {
                viewer_id,
                viewer: _,
                id,
                params,
            } => {
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
                let actions = self.turn.on_cancel();
                self.execute_actions(actions).await;
                // TODO(A13/A15): answer any pending server requests with "cancelled"
            }
            SessionMsg::Answer {
                viewer_id: _,
                frame: _,
            } => {
                // Server request answer (A10/A13)
            }
            SessionMsg::Forward {
                viewer_id,
                viewer: _,
                id,
                method,
                params,
            } => {
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
            SessionMsg::ReplyText { owners, body, text } => {
                match body {
                    Reply::Result(mut res) => {
                        if let Some(t) = text {
                            if let Some(meta) = res.get_mut("_meta").and_then(Value::as_object_mut)
                            {
                                meta.insert(
                                    "honk".to_string(),
                                    json!({
                                        "text": t,
                                    }),
                                );
                            } else {
                                res["_meta"] = json!({
                                    "honk": {
                                        "text": t,
                                    }
                                });
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
                            self.send_to_owner(o, resp);
                        }
                    }
                }

                self.sync_inflight().await;
            }
        }
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
