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

pub enum SessionMsg {
    // from viewers / hub
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
}

pub fn extract_user_text(params: &Value) -> String {
    if let Some(s) = params.get("prompt").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(arr) = params.get("prompt").and_then(Value::as_array) {
        let mut text = String::new();
        for block in arr {
            if let Some(t) = block.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
        return text;
    }
    String::new()
}

impl Session {
    pub async fn run(mut self) {
        let mut watchdog = tokio::time::interval(Duration::from_secs(5));
        let mut idle = tokio::time::interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                ev = self.events.recv(), if !matches!(self.state, ActorState::Gone) => match ev {
                    Some(ev) => self.on_upstream(ev).await,
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

        // 2. Process frame
        let is_session_stream = matches!(&key, StreamKey::Session(_));
        let kind = crate::rpc::kind(&frame);

        if is_session_stream {
            if let Some(update) = frame.pointer("/params/update") {
                if let Some(run_opt) = honk_wire::run_state(update) {
                    let actions = self.turn.on_run_state(run_opt, &mut self.ids);
                    self.execute_actions(actions).await;
                }
                self.forward_to_viewers(&frame);
            } else if kind == crate::rpc::Kind::Response {
                // Check if this is a response to our inflight prompt
                let is_inflight_reply = self.turn.inflight.as_ref().is_some_and(|inf| {
                    honk_upstream::rpc_id_matches(
                        &json!(inf.upstream_id),
                        frame.get("id").unwrap_or(&Value::Null),
                    )
                });

                if is_inflight_reply {
                    let upstream_id = self.turn.inflight.as_ref().unwrap().upstream_id.clone();
                    let actions = self
                        .turn
                        .on_prompt_reply(&upstream_id, &frame, &mut self.ids);
                    self.execute_actions(actions).await;
                }
            } else if kind == crate::rpc::Kind::Request {
                // Server request (e.g. request_permission) -> forward to viewers
                self.forward_to_viewers(&frame);
            }
        }

        // 3. Record
        if is_session_stream {
            if kind == crate::rpc::Kind::Notification {
                let _ = self.log.append(vec![(id_opt, frame)]);
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
        for action in actions {
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
                    let up = self.up.clone();
                    let sid_clone = self.sid.clone();
                    tokio::spawn(async move {
                        let _ = up
                            .request_with_id(
                                json!(upstream_id),
                                "session/prompt",
                                clean_params,
                                Some(&sid_clone),
                            )
                            .await;
                    });
                }
                Action::SendCancel => {
                    let up = self.up.clone();
                    let sid_clone = self.sid.clone();
                    tokio::spawn(async move {
                        let _ = up
                            .notify(
                                "session/cancel",
                                json!({ "sessionId": sid_clone }),
                                Some(&sid_clone),
                            )
                            .await;
                    });
                }
                Action::SynthUser { params, owner } => {
                    let user_text = extract_user_text(&params);
                    let crid = owner.client_req_id.clone();
                    let mut synth_frame = json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": self.sid,
                            "update": {
                                "sessionUpdate": "user_message_chunk",
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

                    let _ = self.log.append(vec![(None, synth_frame.clone())]);
                    for (tx, is_honk_aware) in self.viewers.values() {
                        if *is_honk_aware {
                            tx.send_frame(axum::extract::ws::Message::Text(
                                synth_frame.to_string().into(),
                            ));
                        }
                    }
                }
                Action::Reply { owners, body } => match body {
                    Reply::Error(err) => {
                        let sid = self.sid.clone();
                        {
                            let mut s = self.state_store.lock().await;
                            let _ = s.mutate(|st| {
                                if let Some(h) = st.sessions.get_mut(&sid) {
                                    h.inflight = None;
                                }
                            });
                        }
                        for o in owners {
                            let resp = json!({
                                "jsonrpc": "2.0",
                                "id": o.downstream_id,
                                "error": err,
                            });
                            self.send_to_owner(&o, resp);
                        }
                    }
                    Reply::Result(result_val) => {
                        let hub = self.hub.clone();
                        let sid = self.sid.clone();
                        let user_text = extract_user_text(
                            &self
                                .turn
                                .inflight
                                .as_ref()
                                .map(|i| i.params.clone())
                                .unwrap_or(Value::Null),
                        );
                        let self_tx = self.self_tx.clone();
                        tokio::spawn(async move {
                            let export_res = hub.export(&sid).await;
                            let text = match export_res {
                                Ok(exp) => {
                                    let t = honk_wire::export::turn_text(&exp, &user_text);
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
                    if let Some((tx, is_honk_aware)) = self.viewers.get(&viewer)
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
                        tx.send_frame(axum::extract::ws::Message::Text(
                            notify_frame.to_string().into(),
                        ));
                    }
                }
            }
        }
    }

    async fn on_msg(&mut self, msg: SessionMsg) {
        match msg {
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
                viewer,
                id: _,
                params,
            } => {
                let up = self.up.clone();
                let sid = self.sid.clone();
                let sid_clone = sid.clone();
                let mut clean_params = params;
                crate::rpc::strip_honk_meta(&mut clean_params);
                let up_id = self.ids.next();
                tokio::spawn(async move {
                    let _ = up
                        .request_with_id(
                            json!(up_id),
                            "_goose/unstable/session/steer",
                            clean_params,
                            Some(&sid_clone),
                        )
                        .await;
                });
                self.turn.steers.push((sid, viewer_id));
                let _ = viewer;
            }
            SessionMsg::Cancel => {
                let up = self.up.clone();
                let sid = self.sid.clone();
                tokio::spawn(async move {
                    let _ = up
                        .notify("session/cancel", json!({ "sessionId": sid }), Some(&sid))
                        .await;
                });
            }
            SessionMsg::Answer {
                viewer_id: _,
                frame: _,
            } => {
                // Server request answer (A10/A13)
            }
            SessionMsg::Forward {
                viewer_id: _,
                viewer: _,
                id,
                method,
                params,
            } => {
                let up = self.up.clone();
                let sid = self.sid.clone();
                let mut clean_params = params;
                crate::rpc::strip_honk_meta(&mut clean_params);
                tokio::spawn(async move {
                    let _ = up
                        .request_with_id(id, &method, clean_params, Some(&sid))
                        .await;
                });
            }
            SessionMsg::ReplyText { owners, body, text } => {
                let sid = self.sid.clone();
                {
                    let mut s = self.state_store.lock().await;
                    let _ = s.mutate(|st| {
                        if let Some(h) = st.sessions.get_mut(&sid) {
                            h.inflight = None;
                        }
                    });
                }

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
                        for o in owners {
                            let resp = json!({
                                "jsonrpc": "2.0",
                                "id": o.downstream_id,
                                "result": res,
                            });
                            self.send_to_owner(&o, resp);
                        }
                    }
                    Reply::Error(err) => {
                        for o in owners {
                            let resp = json!({
                                "jsonrpc": "2.0",
                                "id": o.downstream_id,
                                "error": err,
                            });
                            self.send_to_owner(&o, resp);
                        }
                    }
                }
            }
        }
    }

    fn attach_live(&mut self, viewer_id: ViewerId, viewer: ViewerTx, is_honk_aware: bool) {
        self.viewers
            .insert(viewer_id, (viewer.clone(), is_honk_aware));
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
    }

    fn send_to_owner(&self, owner: &OwnerRef, frame: Value) {
        if let Some(vid) = owner.viewer
            && let Some((tx, _)) = self.viewers.get(&vid)
        {
            tx.send_frame(axum::extract::ws::Message::Text(frame.to_string().into()));
        }
    }

    fn forward_to_viewers(&self, frame: &Value) {
        for (tx, is_honk_aware) in self.viewers.values() {
            if *is_honk_aware {
                tx.send_frame(axum::extract::ws::Message::Text(frame.to_string().into()));
            } else {
                let mut legacy_frame = frame.clone();
                crate::rpc::strip_honk_meta(&mut legacy_frame);
                tx.send_frame(axum::extract::ws::Message::Text(
                    legacy_frame.to_string().into(),
                ));
            }
        }
    }
}
