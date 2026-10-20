use serde_json::Value;

pub type ViewerId = u64;

#[derive(Clone, Debug, PartialEq)]
pub struct OwnerRef {
    pub viewer: Option<ViewerId>,
    pub downstream_id: Value,
    pub client_req_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Inflight {
    pub upstream_id: String,
    pub owners: Vec<OwnerRef>,
    pub params: Value,
    pub announced: bool,
    pub prompt_index: usize,
    pub run_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnMeta {
    pub upstream_id: String,
    pub client_req_ids: Vec<String>,
    pub prompt_text: String,
    pub run_id: Option<String>,
    pub honk_reason: Option<String>,
}

impl Inflight {
    pub fn client_req_ids(&self) -> Vec<String> {
        let mut seen = std::collections::BTreeSet::new();
        let mut client_req_ids = Vec::new();
        for o in &self.owners {
            if let Some(id) = &o.client_req_id
                && seen.insert(id.as_str())
            {
                client_req_ids.push(id.clone());
            }
        }
        client_req_ids
    }

    pub fn to_turn_meta(&self, honk_reason: Option<String>) -> TurnMeta {
        TurnMeta {
            upstream_id: self.upstream_id.clone(),
            client_req_ids: self.client_req_ids(),
            prompt_text: crate::rpc::extract_user_text(&self.params),
            run_id: self.run_id.clone(),
            honk_reason,
        }
    }

    pub fn to_persisted(&self) -> crate::state::InflightPrompt {
        crate::state::InflightPrompt {
            upstream_id: self.upstream_id.clone(),
            client_req_ids: self.client_req_ids(),
            params: self.params.clone(),
            announced: self.announced,
            prompt_index: self.prompt_index,
            run_id: self.run_id.clone(),
        }
    }

    pub fn from_persisted(persisted: crate::state::InflightPrompt) -> Self {
        let owners = if persisted.client_req_ids.is_empty() {
            vec![OwnerRef {
                viewer: None,
                // Downstream client disconnected across restarts; downstream JSON-RPC id
                // is not preserved in state.json, so restored owners get Value::Null until/unless
                // re-attached via attach_waiter.
                downstream_id: Value::Null,
                client_req_id: None,
            }]
        } else {
            persisted
                .client_req_ids
                .into_iter()
                .map(|id| OwnerRef {
                    viewer: None,
                    // Downstream client disconnected across restarts; downstream JSON-RPC id
                    // is not preserved in state.json, so restored owners get Value::Null until/unless
                    // re-attached via attach_waiter.
                    downstream_id: Value::Null,
                    client_req_id: Some(id),
                })
                .collect()
        };
        Self {
            upstream_id: persisted.upstream_id,
            owners,
            params: persisted.params,
            announced: persisted.announced,
            prompt_index: persisted.prompt_index,
            run_id: persisted.run_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Result(Value),
    Error(Value),
}

#[derive(Debug, PartialEq)]
pub enum Action {
    SendPrompt {
        upstream_id: String,
        params: Value,
    },
    SendCancel,
    SynthUser {
        params: Value,
        owner: OwnerRef,
        upstream_id: Option<String>,
    },
    Reply {
        owners: Vec<OwnerRef>,
        body: Reply,
        user_text: Option<String>,
        prompt_index: Option<usize>,
        turn: Option<TurnMeta>,
    },
    NotifySteerUnconsumed {
        viewer: ViewerId,
        message_id: String,
    },
}

#[derive(Default, Debug)]
pub struct TurnCore {
    pub run: Option<String>,
    pub inflight: Option<Inflight>,
    pub parked: Vec<(Vec<OwnerRef>, Value)>,
    pub cancel_pending: bool,
    pub steers: Vec<(String, ViewerId)>,
    pub consumed_steers: std::collections::BTreeSet<String>,
    pub client_prompts: usize,
}

impl TurnCore {
    pub fn is_running(&self) -> bool {
        self.run.is_some() || self.inflight.is_some() || !self.parked.is_empty()
    }

    pub fn on_prompt(
        &mut self,
        owner: OwnerRef,
        params: Value,
        ids: &mut crate::rpc::UpstreamIds,
    ) -> Vec<Action> {
        if let Some(run) = &self.run {
            return vec![Action::Reply {
                owners: vec![owner],
                body: Reply::Error(crate::rpc::busy_error(run)),
                user_text: None,
                prompt_index: None,
                turn: None,
            }];
        }
        if self.inflight.is_some() {
            self.parked.push((vec![owner], params));
            return vec![];
        }
        let upstream_id = ids.next();
        self.inflight = Some(Inflight {
            upstream_id: upstream_id.clone(),
            owners: vec![owner],
            params: params.clone(),
            announced: false,
            prompt_index: self.client_prompts,
            run_id: None,
        });
        vec![Action::SendPrompt {
            upstream_id,
            params,
        }]
    }

    pub fn on_run_state(
        &mut self,
        run: Option<String>,
        ids: &mut crate::rpc::UpstreamIds,
    ) -> Vec<Action> {
        let mut out = vec![];
        match run {
            Some(r) => {
                self.run = Some(r.clone());
                self.consumed_steers.clear();
                if let Some(inf) = self.inflight.as_mut() {
                    inf.run_id = Some(r);
                    if !inf.announced {
                        inf.announced = true;
                        self.client_prompts += 1;
                        if let Some(owner) = inf.owners.first().cloned() {
                            out.push(Action::SynthUser {
                                params: inf.params.clone(),
                                owner,
                                upstream_id: Some(inf.upstream_id.clone()),
                            });
                        }
                    }
                }
                if std::mem::take(&mut self.cancel_pending) {
                    out.push(Action::SendCancel);
                }
            }
            None => {
                self.run = None;
                for (message_id, viewer) in self.steers.drain(..) {
                    out.push(Action::NotifySteerUnconsumed { viewer, message_id });
                }
                if self.inflight.is_none() {
                    out.extend(self.dispatch_parked(ids));
                }
            }
        }
        out
    }

    pub fn on_prompt_reply(
        &mut self,
        upstream_id: &str,
        frame: &Value,
        ids: &mut crate::rpc::UpstreamIds,
    ) -> Vec<Action> {
        let Some(inf) = self.inflight.take_if(|i| i.upstream_id == upstream_id) else {
            return vec![];
        };
        self.cancel_pending = false;
        let is_busy = crate::rpc::busy_run_id(frame).is_some();
        if !inf.announced && !is_busy && frame.get("error").is_none() {
            self.client_prompts += 1;
        }
        if let Some(run) = crate::rpc::busy_run_id(frame) {
            self.run = Some(run);
        } else {
            self.run = None;
        }
        let body = match frame.get("error") {
            Some(e) => Reply::Error(e.clone()),
            None => Reply::Result(frame.get("result").cloned().unwrap_or(Value::Null)),
        };
        let user_text = Some(crate::rpc::extract_user_text(&inf.params));
        let prompt_index = Some(inf.prompt_index);
        let turn = if is_busy {
            None
        } else {
            let honk_reason = if frame
                .pointer("/result/_meta/honk/synthesized")
                .and_then(Value::as_bool)
                == Some(true)
            {
                Some("synthesized".to_string())
            } else {
                frame
                    .pointer("/result/_meta/honk/reason")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            };
            Some(inf.to_turn_meta(honk_reason))
        };
        let mut out = vec![Action::Reply {
            owners: inf.owners,
            body,
            user_text,
            prompt_index,
            turn,
        }];
        if self.run.is_none() {
            out.extend(self.dispatch_parked(ids));
        }
        out
    }

    pub fn on_prompt_send_failed(
        &mut self,
        upstream_id: &str,
        error: Value,
        ids: &mut crate::rpc::UpstreamIds,
    ) -> Vec<Action> {
        let Some(inf) = self.inflight.take_if(|i| i.upstream_id == upstream_id) else {
            return vec![];
        };
        self.cancel_pending = false;
        let mut out = vec![Action::Reply {
            owners: inf.owners,
            body: Reply::Error(error),
            user_text: None,
            prompt_index: Some(inf.prompt_index),
            turn: None,
        }];
        if self.run.is_none() {
            out.extend(self.dispatch_parked(ids));
        }
        out
    }

    pub fn on_prompt_not_delivered(
        &mut self,
        upstream_id: &str,
        ids: &mut crate::rpc::UpstreamIds,
    ) -> Vec<Action> {
        let Some(inf) = self.inflight.take_if(|i| i.upstream_id == upstream_id) else {
            return vec![];
        };
        self.cancel_pending = false;
        let mut out = vec![Action::Reply {
            owners: inf.owners,
            body: Reply::Result(serde_json::json!({
                "stopReason": "cancelled",
                "_meta": {
                    "honk": {
                        "reason": "not_delivered"
                    }
                }
            })),
            user_text: None,
            prompt_index: Some(inf.prompt_index),
            turn: None,
        }];
        if self.run.is_none() {
            out.extend(self.dispatch_parked(ids));
        }
        out
    }

    fn dispatch_parked(&mut self, ids: &mut crate::rpc::UpstreamIds) -> Vec<Action> {
        if self.parked.is_empty() {
            return vec![];
        }
        let (owners, params) = self.parked.remove(0);
        let upstream_id = ids.next();
        self.inflight = Some(Inflight {
            upstream_id: upstream_id.clone(),
            owners,
            params: params.clone(),
            announced: false,
            prompt_index: self.client_prompts,
            run_id: None,
        });
        vec![Action::SendPrompt {
            upstream_id,
            params,
        }]
    }

    pub fn on_steer_ok(&mut self, message_id: String, viewer: ViewerId) -> Vec<Action> {
        if self.consumed_steers.contains(&message_id) {
            return vec![];
        }
        if self.run.is_none() {
            return vec![Action::NotifySteerUnconsumed { viewer, message_id }];
        }
        self.steers.push((message_id, viewer));
        vec![]
    }

    pub fn on_user_message(&mut self, message_id: &str) {
        self.steers.retain(|(m, _)| m != message_id);
        self.consumed_steers.insert(message_id.to_string());
    }

    pub fn on_cancel(&mut self) -> Vec<Action> {
        if self.inflight.as_ref().is_some_and(|i| !i.announced) {
            self.cancel_pending = true;
        }
        vec![Action::SendCancel]
    }

    pub fn interrupt(&mut self, reason: &str) -> Vec<Action> {
        self.run = None;
        self.cancel_pending = false;
        self.steers.clear();
        self.consumed_steers.clear();

        let mut out = Vec::new();
        let body = Reply::Result(serde_json::json!({
            "stopReason": "cancelled",
            "_meta": {
                "honk": {
                    "reason": reason
                }
            }
        }));

        if let Some(inf) = self.inflight.take() {
            let turn =
                if inf.announced || reason == "upstream_restart" || reason == "delivery_unknown" {
                    Some(inf.to_turn_meta(Some(reason.to_string())))
                } else {
                    None
                };
            if !inf.owners.is_empty() {
                out.push(Action::Reply {
                    owners: inf.owners,
                    body: body.clone(),
                    user_text: None,
                    prompt_index: Some(inf.prompt_index),
                    turn,
                });
            }
        }

        let parked_owners: Vec<OwnerRef> = self
            .parked
            .drain(..)
            .flat_map(|(owners, _)| owners)
            .collect();
        if !parked_owners.is_empty() {
            out.push(Action::Reply {
                owners: parked_owners,
                body,
                user_text: None,
                prompt_index: None,
                turn: None,
            });
        }

        out
    }

    pub fn attach_waiter(&mut self, client_req_id: &str, owner: OwnerRef) -> bool {
        if let Some(inf) = self.inflight.as_mut()
            && inf
                .owners
                .iter()
                .any(|o| o.client_req_id.as_deref() == Some(client_req_id))
        {
            inf.owners.push(owner);
            return true;
        }
        for (owners, _params) in &mut self.parked {
            if owners
                .iter()
                .any(|o| o.client_req_id.as_deref() == Some(client_req_id))
            {
                owners.push(owner);
                return true;
            }
        }
        false
    }

    pub fn to_persisted(&self) -> Option<crate::state::InflightPrompt> {
        self.inflight.as_ref().map(|i| i.to_persisted())
    }

    pub fn from_persisted(persisted: impl Into<Option<crate::state::InflightPrompt>>) -> Self {
        Self {
            inflight: persisted.into().map(Inflight::from_persisted),
            ..Default::default()
        }
    }
}

/// What a prompt reply says about how the session's last turn went, for the
/// turn-error state the pickers show (honk-9qf.15.11.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Clean,
    Failed { kind: String, message: String },
}

/// A turn error is written to state.json on every save and sent with every
/// `session/list`; an error reply can quote a whole provider response.
pub const TURN_ERROR_MAX_BYTES: usize = 2048;

fn capped(mut text: String) -> String {
    if text.len() <= TURN_ERROR_MAX_BYTES {
        return text;
    }
    let mut end = TURN_ERROR_MAX_BYTES - '…'.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push('…');
    text
}

/// Reads a turn's outcome from its `Action::Reply`. `None` means the reply
/// says nothing about the session's last turn: a busy rejection, or a prompt
/// that never reached goose and was answered `cancelled`.
pub fn turn_outcome(
    body: &Reply,
    turn: Option<&TurnMeta>,
    prompt_index: Option<usize>,
) -> Option<TurnOutcome> {
    match body {
        Reply::Error(err) => {
            if crate::rpc::busy_run_id(err).is_some() || (turn.is_none() && prompt_index.is_none())
            {
                return None;
            }
            let message = err.get("message").and_then(Value::as_str).unwrap_or("");
            let message = match err.get("data").and_then(Value::as_str) {
                Some(data) if !message.is_empty() => format!("{message}: {data}"),
                Some(data) => data.to_string(),
                None => message.to_string(),
            };
            Some(TurnOutcome::Failed {
                kind: "rpc".to_string(),
                message: capped(message),
            })
        }
        Reply::Result(result) => {
            let turn = turn?;
            match turn.honk_reason.as_deref() {
                Some("upstream_restart") => {
                    return Some(TurnOutcome::Failed {
                        kind: "interrupted".to_string(),
                        message: "goose restarted during the turn".to_string(),
                    });
                }
                Some("delivery_unknown") => {
                    return Some(TurnOutcome::Failed {
                        kind: "interrupted".to_string(),
                        message: "honkd could not tell whether goose got the prompt".to_string(),
                    });
                }
                _ => {}
            }
            let turn_error = result.pointer("/_meta/goose/turnError");
            match turn_error
                .and_then(|e| e.get("kind"))
                .and_then(Value::as_str)
            {
                Some(kind) => Some(TurnOutcome::Failed {
                    kind: capped(kind.to_string()),
                    message: capped(
                        turn_error
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                }),
                None => Some(TurnOutcome::Clean),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{UpstreamIds, busy_error};
    use serde_json::json;

    fn make_owner(viewer: Option<ViewerId>, downstream: i64, req_id: Option<&str>) -> OwnerRef {
        OwnerRef {
            viewer,
            downstream_id: json!(downstream),
            client_req_id: req_id.map(String::from),
        }
    }

    #[test]
    fn test_prompt_during_run_returns_busy_error_no_send_prompt() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore {
            run: Some("run_active".into()),
            ..Default::default()
        };
        let owner = make_owner(Some(1), 10, Some("req_1"));
        let actions = core.on_prompt(owner.clone(), json!({"prompt": "hi"}), &mut ids);

        assert_eq!(
            actions,
            vec![Action::Reply {
                owners: vec![owner],
                body: Reply::Error(busy_error("run_active")),
                user_text: None,
                prompt_index: None,
                turn: None,
            }]
        );
        assert!(core.inflight.is_none());
    }

    #[test]
    fn test_prompt_in_announce_window_parked_then_dispatched_at_turn_end() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let p1_actions = core.on_prompt(owner1.clone(), json!({"prompt": "first"}), &mut ids);
        assert_eq!(
            p1_actions,
            vec![Action::SendPrompt {
                upstream_id: "hk1-1".into(),
                params: json!({"prompt": "first"}),
            }]
        );

        let owner2 = make_owner(Some(2), 20, Some("req_2"));
        let p2_actions = core.on_prompt(owner2.clone(), json!({"prompt": "second"}), &mut ids);
        assert_eq!(p2_actions, vec![]);
        assert_eq!(core.parked.len(), 1);

        let run_actions = core.on_run_state(Some("run_1".into()), &mut ids);
        assert_eq!(core.run.as_deref(), Some("run_1"));
        assert_eq!(
            run_actions,
            vec![Action::SynthUser {
                params: json!({"prompt": "first"}),
                owner: owner1.clone(),
                upstream_id: Some("hk1-1".into()),
            }]
        );
        assert_eq!(core.parked.len(), 1);

        // Run ends
        let _ = core.on_run_state(None, &mut ids);

        // HTTP prompt reply arrives, ending turn and dispatching parked prompt
        let reply_frame = json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "result": {"text": "done"},
        });
        let reply_actions = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);
        assert_eq!(
            reply_actions,
            vec![
                Action::Reply {
                    owners: vec![owner1],
                    body: Reply::Result(json!({"text": "done"})),
                    user_text: Some("first".to_string()),
                    prompt_index: Some(0),
                    turn: Some(TurnMeta {
                        upstream_id: "hk1-1".into(),
                        client_req_ids: vec!["req_1".into()],
                        prompt_text: "first".into(),
                        run_id: Some("run_1".into()),
                        honk_reason: None,
                    }),
                },
                Action::SendPrompt {
                    upstream_id: "hk1-2".into(),
                    params: json!({"prompt": "second"}),
                },
            ]
        );
        assert!(core.parked.is_empty());
        assert_eq!(core.inflight.as_ref().unwrap().upstream_id, "hk1-2");
    }

    #[test]
    fn test_reply_ends_turn_and_dispatches_parked_prompt() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "first"}), &mut ids);

        let owner2 = make_owner(Some(2), 20, Some("req_2"));
        let _ = core.on_prompt(owner2.clone(), json!({"prompt": "second"}), &mut ids);
        assert_eq!(core.parked.len(), 1);

        // Turn ends without run (e.g. prompt reply without run announcement)
        let reply_frame = json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "result": {"text": "done"},
        });
        let actions = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);

        assert_eq!(
            actions,
            vec![
                Action::Reply {
                    owners: vec![owner1],
                    body: Reply::Result(json!({"text": "done"})),
                    user_text: Some("first".into()),
                    prompt_index: Some(0),
                    turn: Some(TurnMeta {
                        upstream_id: "hk1-1".into(),
                        client_req_ids: vec!["req_1".into()],
                        prompt_text: "first".into(),
                        run_id: None,
                        honk_reason: None,
                    }),
                },
                Action::SendPrompt {
                    upstream_id: "hk1-2".into(),
                    params: json!({"prompt": "second"}),
                },
            ]
        );
        assert!(core.parked.is_empty());
        assert!(core.run.is_none());
        assert_eq!(core.inflight.as_ref().unwrap().upstream_id, "hk1-2");
    }

    #[test]
    fn test_active_run_id_null_then_reply_one_reply() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner.clone(), json!({"prompt": "hi"}), &mut ids);
        let _ = core.on_run_state(Some("run_1".into()), &mut ids);

        // activeRunId: null arrives on session stream first
        let run_actions = core.on_run_state(None, &mut ids);
        assert_eq!(run_actions, vec![]);
        assert!(core.run.is_none());

        // Then HTTP prompt reply arrives
        let reply_frame = json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "result": {"status": "ok"},
        });
        let reply_actions = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);
        assert_eq!(
            reply_actions,
            vec![Action::Reply {
                owners: vec![owner],
                body: Reply::Result(json!({"status": "ok"})),
                user_text: Some("hi".into()),
                prompt_index: Some(0),
                turn: Some(TurnMeta {
                    upstream_id: "hk1-1".into(),
                    client_req_ids: vec!["req_1".into()],
                    prompt_text: "hi".into(),
                    run_id: Some("run_1".into()),
                    honk_reason: None,
                }),
            }]
        );
        assert!(core.inflight.is_none());
    }

    #[test]
    fn test_early_cancel_sends_cancel_now_and_again_at_announcement() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner.clone(), json!({"prompt": "hi"}), &mut ids);

        // Cancel before announcement
        let cancel_actions = core.on_cancel();
        assert_eq!(cancel_actions, vec![Action::SendCancel]);
        assert!(core.cancel_pending);

        // Run announcement arrives
        let run_actions = core.on_run_state(Some("run_1".into()), &mut ids);
        assert_eq!(
            run_actions,
            vec![
                Action::SynthUser {
                    params: json!({"prompt": "hi"}),
                    owner,
                    upstream_id: Some("hk1-1".into()),
                },
                Action::SendCancel,
            ]
        );
        assert!(!core.cancel_pending);
    }

    #[test]
    fn test_unconsumed_steer_notified_at_run_end_consumed_steer_nothing() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore {
            run: Some("run_1".into()),
            ..Default::default()
        };

        let _ = core.on_steer_ok("steer_consumed".into(), 10);
        let _ = core.on_steer_ok("steer_unconsumed".into(), 20);

        // Live user message chunk consumes steer_consumed
        core.on_user_message("steer_consumed");

        // Run ends
        let actions = core.on_run_state(None, &mut ids);
        assert_eq!(
            actions,
            vec![Action::NotifySteerUnconsumed {
                viewer: 20,
                message_id: "steer_unconsumed".into(),
            }]
        );
        assert!(core.steers.is_empty());
    }

    #[test]
    fn test_interrupt_returns_cancelled_reply_with_reason() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "first"}), &mut ids);

        let owner2 = make_owner(Some(2), 20, Some("req_2"));
        let _ = core.on_prompt(owner2.clone(), json!({"prompt": "second"}), &mut ids);

        core.run = Some("run_1".into());
        let _ = core.on_steer_ok("steer_1".into(), 30);

        let actions = core.interrupt("goose restart");
        assert_eq!(
            actions,
            vec![
                Action::Reply {
                    owners: vec![owner1],
                    body: Reply::Result(json!({
                        "stopReason": "cancelled",
                        "_meta": {
                            "honk": {
                                "reason": "goose restart"
                            }
                        }
                    })),
                    user_text: None,
                    prompt_index: Some(0),
                    turn: None,
                },
                Action::Reply {
                    owners: vec![owner2],
                    body: Reply::Result(json!({
                        "stopReason": "cancelled",
                        "_meta": {
                            "honk": {
                                "reason": "goose restart"
                            }
                        }
                    })),
                    user_text: None,
                    prompt_index: None,
                    turn: None,
                },
            ]
        );
        assert!(core.run.is_none());
        assert!(core.inflight.is_none());
        assert!(core.parked.is_empty());
        assert!(core.steers.is_empty());
    }

    #[test]
    fn test_attach_waiter_finds_inflight_by_client_req_id() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner1 = make_owner(Some(1), 10, Some("req_alpha"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "hi"}), &mut ids);

        let owner_other = make_owner(Some(2), 20, Some("req_beta"));
        assert!(!core.attach_waiter("req_nonexistent", owner_other));

        let owner2 = make_owner(Some(2), 20, Some("req_alpha"));
        assert!(core.attach_waiter("req_alpha", owner2.clone()));

        assert_eq!(core.inflight.as_ref().unwrap().owners, vec![owner1, owner2]);
    }

    #[test]
    fn test_replayed_reply_twice_second_returns_nothing() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner.clone(), json!({"prompt": "hi"}), &mut ids);

        let reply_frame = json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "result": {"status": "ok"},
        });

        let first = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);
        assert_eq!(first.len(), 1);

        let second = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);
        assert!(second.is_empty());
    }

    #[test]
    fn test_persisted_roundtrip() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "hi"}), &mut ids);

        // Attach a second waiter with duplicate req_1, and another with req_2
        let owner2 = make_owner(Some(2), 20, Some("req_1"));
        assert!(core.attach_waiter("req_1", owner2));
        let owner3 = make_owner(Some(3), 30, Some("req_2"));
        assert!(core.attach_waiter("req_1", owner3));

        let persisted = core.to_persisted();
        assert!(persisted.is_some());
        let p = persisted.clone().unwrap();
        assert_eq!(p.upstream_id, "hk1-1");
        // Deduplicated preserving order: req_1 was seen first, then req_2.
        assert_eq!(p.client_req_ids, vec!["req_1", "req_2"]);
        assert_eq!(p.params, json!({"prompt": "hi"}));
        assert!(!p.announced);
        assert_eq!(p.prompt_index, 0);

        let mut restored = TurnCore::from_persisted(persisted.clone());
        assert_eq!(restored.to_persisted(), persisted);

        let inf = restored.inflight.as_ref().unwrap();
        assert_eq!(inf.upstream_id, "hk1-1");
        assert_eq!(inf.prompt_index, 0);
        assert_eq!(inf.owners.len(), 2);
        assert_eq!(
            inf.owners[0],
            OwnerRef {
                viewer: None,
                downstream_id: Value::Null,
                client_req_id: Some("req_1".into()),
            }
        );
        assert_eq!(
            inf.owners[1],
            OwnerRef {
                viewer: None,
                downstream_id: Value::Null,
                client_req_id: Some("req_2".into()),
            }
        );

        // Attaching a reconnected waiter with client_req_id works on restored core
        let reconnected = make_owner(Some(42), 100, Some("req_1"));
        assert!(restored.attach_waiter("req_1", reconnected.clone()));
        assert_eq!(restored.inflight.as_ref().unwrap().owners.len(), 3);

        // None inflight roundtrips cleanly
        let empty_core = TurnCore::default();
        assert_eq!(empty_core.to_persisted(), None);
        let restored_empty = TurnCore::from_persisted(None);
        assert!(restored_empty.inflight.is_none());
    }

    #[test]
    fn test_early_cancel_cleared_on_prompt_reply_no_leak_to_next_turn() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1, json!({"prompt": "p1"}), &mut ids);

        // Cancel while p1 is unannounced
        let cancel_actions = core.on_cancel();
        assert_eq!(cancel_actions, vec![Action::SendCancel]);
        assert!(core.cancel_pending);

        // p1 completes without run announcement (e.g. error or simple result)
        let reply_frame = json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "result": {"status": "ok"},
        });
        let _ = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);
        assert!(!core.cancel_pending);

        // Next turn: p2 starts
        let owner2 = make_owner(Some(2), 20, Some("req_2"));
        let _ = core.on_prompt(owner2.clone(), json!({"prompt": "p2"}), &mut ids);

        // p2 is announced -> MUST NOT emit SendCancel
        let run_actions = core.on_run_state(Some("run_2".into()), &mut ids);
        assert_eq!(
            run_actions,
            vec![Action::SynthUser {
                params: json!({"prompt": "p2"}),
                owner: owner2,
                upstream_id: Some("hk1-2".into()),
            }]
        );
        assert!(!core.cancel_pending);
    }

    #[test]
    fn test_steer_ok_after_run_end_immediately_notifies_unconsumed() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        // Run is active then ends
        let _ = core.on_run_state(Some("run_1".into()), &mut ids);
        let _ = core.on_run_state(None, &mut ids);

        // Steer reply arrives after activeRunId: null
        let actions = core.on_steer_ok("steer_late".into(), 42);
        assert_eq!(
            actions,
            vec![Action::NotifySteerUnconsumed {
                viewer: 42,
                message_id: "steer_late".into(),
            }]
        );
        assert!(core.steers.is_empty());
    }

    #[test]
    fn test_steer_consumed_before_steer_ok_records_nothing() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        // Run starts
        let _ = core.on_run_state(Some("run_1".into()), &mut ids);

        // user_message_chunk arrives before steer reply
        core.on_user_message("steer_early");
        assert!(core.consumed_steers.contains("steer_early"));

        // steer reply arrives late
        let actions = core.on_steer_ok("steer_early".into(), 42);
        assert!(actions.is_empty());
        assert!(core.steers.is_empty());

        // Run ends -> nothing unconsumed notified
        let end_actions = core.on_run_state(None, &mut ids);
        assert!(end_actions.is_empty());
        // consumed_steers is kept until the next run starts
        assert!(core.consumed_steers.contains("steer_early"));
    }

    #[test]
    fn test_steer_consumed_then_run_ends_then_steer_ok_emits_nothing() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        // 1. Run 1 starts
        let _ = core.on_run_state(Some("run_1".into()), &mut ids);

        // 2. user_message_chunk arrives and consumes steer X before steer reply
        core.on_user_message("steer_x");
        assert!(core.consumed_steers.contains("steer_x"));

        // 3. activeRunId: null arrives (run ends) BEFORE steer reply
        let end_actions = core.on_run_state(None, &mut ids);
        assert!(end_actions.is_empty());
        // consumed_steers is preserved across run end until next run start
        assert!(core.consumed_steers.contains("steer_x"));

        // 4. Steer reply for X arrives AFTER activeRunId: null
        let steer_actions = core.on_steer_ok("steer_x".into(), 42);
        // Because X was consumed during the turn, it MUST NOT emit NotifySteerUnconsumed
        assert!(steer_actions.is_empty());
        assert!(core.steers.is_empty());

        // 5. When Run 2 starts, consumed_steers is cleared
        let _ = core.on_run_state(Some("run_2".into()), &mut ids);
        assert!(core.consumed_steers.is_empty());
    }

    #[test]
    fn test_prompt_reply_busy_error_records_run_does_not_dispatch_parked_until_run_null() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "p1"}), &mut ids);

        // Prompt 2 arrives in announce window -> parked
        let owner2 = make_owner(Some(2), 20, Some("req_2"));
        let p2_actions = core.on_prompt(owner2.clone(), json!({"prompt": "p2"}), &mut ids);
        assert_eq!(p2_actions, vec![]);
        assert_eq!(core.parked.len(), 1);

        // Upstream replies to prompt 1 with busy error (prompt 1 raced a run started elsewhere)
        let busy_err = busy_error("run_raced_elsewhere");
        let reply_frame = json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "error": busy_err,
        });

        let actions = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);

        // 1. Owner 1 gets busy error
        assert_eq!(
            actions,
            vec![Action::Reply {
                owners: vec![owner1],
                body: Reply::Error(busy_err.clone()),
                user_text: Some("p1".into()),
                prompt_index: Some(0),
                turn: None,
            }]
        );
        // 2. Run is recorded
        assert_eq!(core.run.as_deref(), Some("run_raced_elsewhere"));
        // 3. Parked prompt 2 is NOT dispatched (no SendPrompt in actions, still in parked)
        assert_eq!(core.parked.len(), 1);

        // 4. A new prompt while run is active gets immediate busy error (not parked, not sent upstream)
        let owner3 = make_owner(Some(3), 30, Some("req_3"));
        let p3_actions = core.on_prompt(owner3.clone(), json!({"prompt": "p3"}), &mut ids);
        assert_eq!(
            p3_actions,
            vec![Action::Reply {
                owners: vec![owner3],
                body: Reply::Error(busy_err),
                user_text: None,
                prompt_index: None,
                turn: None,
            }]
        );
        assert_eq!(core.parked.len(), 1);

        // 5. Later activeRunId: null arrives on session stream
        let end_actions = core.on_run_state(None, &mut ids);
        assert!(core.run.is_none());
        // Parked prompt 2 is now dispatched!
        assert_eq!(
            end_actions,
            vec![Action::SendPrompt {
                upstream_id: "hk1-2".into(),
                params: json!({"prompt": "p2"}),
            }]
        );
        assert!(core.parked.is_empty());
        assert_eq!(core.inflight.as_ref().unwrap().upstream_id, "hk1-2");
    }

    #[test]
    fn test_on_prompt_send_failed_takes_inflight_and_dispatches_parked() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "p1"}), &mut ids);

        let owner2 = make_owner(Some(2), 20, Some("req_2"));
        let _ = core.on_prompt(owner2.clone(), json!({"prompt": "p2"}), &mut ids);
        assert_eq!(core.parked.len(), 1);

        let err = json!({"code": -32603, "message": "network error"});
        let actions = core.on_prompt_send_failed("hk1-1", err.clone(), &mut ids);

        assert_eq!(
            actions,
            vec![
                Action::Reply {
                    owners: vec![owner1],
                    body: Reply::Error(err),
                    user_text: None,
                    prompt_index: Some(0),
                    turn: None,
                },
                Action::SendPrompt {
                    upstream_id: "hk1-2".into(),
                    params: json!({"prompt": "p2"}),
                },
            ]
        );
        assert_eq!(core.inflight.as_ref().unwrap().upstream_id, "hk1-2");
        assert!(core.parked.is_empty());
        assert!(!core.cancel_pending);
    }

    #[test]
    fn test_on_prompt_send_failed_mismatched_id_ignored() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();

        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "p1"}), &mut ids);

        let err = json!({"code": -32603, "message": "network error"});
        let actions = core.on_prompt_send_failed("hk1-999", err, &mut ids);

        assert_eq!(actions, vec![]);
        assert_eq!(core.inflight.as_ref().unwrap().upstream_id, "hk1-1");
    }

    #[test]
    fn test_attach_waiter_finds_parked_prompt_by_client_req_id() {
        let mut core = TurnCore::default();
        let mut ids = UpstreamIds::new(1);

        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1, json!({"prompt": "first"}), &mut ids);

        let owner2 = make_owner(Some(2), 20, Some("req_2"));
        let _ = core.on_prompt(owner2.clone(), json!({"prompt": "second"}), &mut ids);
        assert_eq!(core.parked.len(), 1);

        // Attach waiter with crid "req_2" matching the parked prompt
        let owner3 = make_owner(Some(3), 30, Some("req_2"));
        assert!(core.attach_waiter("req_2", owner3.clone()));

        // End turn 1
        let _ = core.on_run_state(Some("run_1".into()), &mut ids);
        let _ = core.on_run_state(None, &mut ids);
        let reply_frame = json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "result": {"text": "done1"},
        });
        let _ = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);

        // Now prompt 2 should be in-flight with BOTH owner2 and owner3
        let inf = core.inflight.as_ref().unwrap();
        assert_eq!(inf.owners, vec![owner2.clone(), owner3.clone()]);

        // When prompt 2 completes, both owners get the reply
        let reply_frame2 = json!({
            "jsonrpc": "2.0",
            "id": "hk1-2",
            "result": {"text": "done2"},
        });
        let reply2_actions = core.on_prompt_reply("hk1-2", &reply_frame2, &mut ids);
        assert_eq!(
            reply2_actions,
            vec![Action::Reply {
                owners: vec![owner2, owner3],
                body: Reply::Result(json!({"text": "done2"})),
                user_text: Some("second".to_string()),
                prompt_index: Some(1),
                turn: Some(TurnMeta {
                    upstream_id: "hk1-2".into(),
                    client_req_ids: vec!["req_2".into()],
                    prompt_text: "second".into(),
                    run_id: None,
                    honk_reason: None,
                }),
            }]
        );
    }

    #[test]
    fn test_inflight_run_id_recorded_and_persisted() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();
        let owner = make_owner(Some(1), 10, Some("req_1"));
        let p_actions = core.on_prompt(owner, json!({"prompt": "hello"}), &mut ids);
        assert_eq!(p_actions.len(), 1);
        assert_eq!(core.inflight.as_ref().unwrap().run_id, None);

        let run_actions = core.on_run_state(Some("run_123".into()), &mut ids);
        assert_eq!(
            core.inflight.as_ref().unwrap().run_id.as_deref(),
            Some("run_123")
        );
        assert_eq!(
            run_actions,
            vec![Action::SynthUser {
                params: json!({"prompt": "hello"}),
                owner: make_owner(Some(1), 10, Some("req_1")),
                upstream_id: Some("hk1-1".into()),
            }]
        );

        // Later run state update while in-flight still exists
        let _ = core.on_run_state(Some("run_456".into()), &mut ids);
        assert_eq!(
            core.inflight.as_ref().unwrap().run_id.as_deref(),
            Some("run_456")
        );

        // Persist and restore
        let persisted = core.to_persisted().unwrap();
        assert_eq!(persisted.run_id.as_deref(), Some("run_456"));
        let restored = TurnCore::from_persisted(persisted);
        assert_eq!(
            restored.inflight.as_ref().unwrap().run_id.as_deref(),
            Some("run_456")
        );
    }

    #[test]
    fn test_reply_carries_turn_meta() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();
        let owner1 = make_owner(Some(1), 10, Some("crid_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "hello"}), &mut ids);
        let owner2 = make_owner(Some(2), 20, Some("crid_2"));
        assert!(core.attach_waiter("crid_1", owner2.clone()));
        let owner3 = make_owner(Some(3), 30, Some("crid_1")); // duplicate crid
        assert!(core.attach_waiter("crid_1", owner3.clone()));

        let _ = core.on_run_state(Some("run_999".into()), &mut ids);

        let reply_frame = json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "result": {"stopReason": "end_turn"},
        });
        let reply_actions = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);
        assert_eq!(
            reply_actions,
            vec![Action::Reply {
                owners: vec![owner1, owner2, owner3],
                body: Reply::Result(json!({"stopReason": "end_turn"})),
                user_text: Some("hello".into()),
                prompt_index: Some(0),
                turn: Some(TurnMeta {
                    upstream_id: "hk1-1".into(),
                    client_req_ids: vec!["crid_1".into(), "crid_2".into()],
                    prompt_text: "hello".into(),
                    run_id: Some("run_999".into()),
                    honk_reason: None,
                }),
            }]
        );
    }

    #[test]
    fn test_synthesized_gap_reply_honk_reason_synthesized() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();
        let owner = make_owner(Some(1), 10, Some("crid_1"));
        let _ = core.on_prompt(owner.clone(), json!({"prompt": "hello"}), &mut ids);
        let _ = core.on_run_state(Some("run_1".into()), &mut ids);

        let reply_frame = json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "result": {
                "stopReason": "end_turn",
                "_meta": {
                    "honk": {
                        "synthesized": true
                    }
                }
            },
        });
        let reply_actions = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);
        match &reply_actions[0] {
            Action::Reply {
                turn: Some(turn), ..
            } => {
                assert_eq!(turn.honk_reason.as_deref(), Some("synthesized"));
            }
            other => panic!("unexpected action: {:?}", other),
        }

        // Also test _meta.honk.reason as string
        let mut core2 = TurnCore::default();
        let _ = core2.on_prompt(owner, json!({"prompt": "hello"}), &mut ids);
        let _ = core2.on_run_state(Some("run_2".into()), &mut ids);
        let reply_frame2 = json!({
            "jsonrpc": "2.0",
            "id": "hk1-2",
            "result": {
                "stopReason": "cancelled",
                "_meta": {
                    "honk": {
                        "reason": "custom_reason"
                    }
                }
            },
        });
        let reply_actions2 = core2.on_prompt_reply("hk1-2", &reply_frame2, &mut ids);
        match &reply_actions2[0] {
            Action::Reply {
                turn: Some(turn), ..
            } => {
                assert_eq!(turn.honk_reason.as_deref(), Some("custom_reason"));
            }
            other => panic!("unexpected action: {:?}", other),
        }
    }

    #[test]
    fn test_busy_reply_no_turn_meta() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();
        let owner1 = make_owner(Some(1), 10, Some("crid_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "p1"}), &mut ids);

        let busy_err = busy_error("run_active");
        let reply_frame = json!({
            "jsonrpc": "2.0",
            "id": "hk1-1",
            "error": busy_err,
        });
        let reply_actions = core.on_prompt_reply("hk1-1", &reply_frame, &mut ids);
        assert_eq!(
            reply_actions,
            vec![Action::Reply {
                owners: vec![owner1],
                body: Reply::Error(busy_err.clone()),
                user_text: Some("p1".into()),
                prompt_index: Some(0),
                turn: None,
            }]
        );

        // Also prompt while busy returns turn: None
        let owner2 = make_owner(Some(2), 20, Some("crid_2"));
        let prompt_actions = core.on_prompt(owner2.clone(), json!({"prompt": "p2"}), &mut ids);
        assert_eq!(
            prompt_actions,
            vec![Action::Reply {
                owners: vec![owner2],
                body: Reply::Error(busy_err),
                user_text: None,
                prompt_index: None,
                turn: None,
            }]
        );
    }

    #[test]
    fn test_prompt_send_failed_and_not_delivered_no_turn_meta() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();
        let owner1 = make_owner(Some(1), 10, Some("crid_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "p1"}), &mut ids);

        let err = json!({"code": -32603, "message": "fail"});
        let failed_actions = core.on_prompt_send_failed("hk1-1", err.clone(), &mut ids);
        assert_eq!(
            failed_actions,
            vec![Action::Reply {
                owners: vec![owner1.clone()],
                body: Reply::Error(err),
                user_text: None,
                prompt_index: Some(0),
                turn: None,
            }]
        );

        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "p2"}), &mut ids);
        let undelivered_actions = core.on_prompt_not_delivered("hk1-2", &mut ids);
        assert_eq!(
            undelivered_actions,
            vec![Action::Reply {
                owners: vec![owner1],
                body: Reply::Result(json!({
                    "stopReason": "cancelled",
                    "_meta": {
                        "honk": {
                            "reason": "not_delivered"
                        }
                    }
                })),
                user_text: None,
                prompt_index: Some(0),
                turn: None,
            }]
        );
    }

    #[test]
    fn test_interrupt_in_flight_announced_and_parked_two_replies() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();
        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "first"}), &mut ids);
        let owner2 = make_owner(Some(2), 20, Some("req_2"));
        let _ = core.on_prompt(owner2.clone(), json!({"prompt": "second"}), &mut ids);

        let _ = core.on_run_state(Some("run_1".into()), &mut ids);
        assert!(core.inflight.as_ref().unwrap().announced);

        let actions = core.interrupt("cancelled");
        assert_eq!(
            actions,
            vec![
                Action::Reply {
                    owners: vec![owner1],
                    body: Reply::Result(json!({
                        "stopReason": "cancelled",
                        "_meta": {
                            "honk": {
                                "reason": "cancelled"
                            }
                        }
                    })),
                    user_text: None,
                    prompt_index: Some(0),
                    turn: Some(TurnMeta {
                        upstream_id: "hk1-1".into(),
                        client_req_ids: vec!["req_1".into()],
                        prompt_text: "first".into(),
                        run_id: Some("run_1".into()),
                        honk_reason: Some("cancelled".into()),
                    }),
                },
                Action::Reply {
                    owners: vec![owner2],
                    body: Reply::Result(json!({
                        "stopReason": "cancelled",
                        "_meta": {
                            "honk": {
                                "reason": "cancelled"
                            }
                        }
                    })),
                    user_text: None,
                    prompt_index: None,
                    turn: None,
                },
            ]
        );
    }

    #[test]
    fn test_interrupt_unannounced_cancelled_no_turn_meta() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();
        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "first"}), &mut ids);

        let actions = core.interrupt("cancelled");
        assert_eq!(
            actions,
            vec![Action::Reply {
                owners: vec![owner1],
                body: Reply::Result(json!({
                    "stopReason": "cancelled",
                    "_meta": {
                        "honk": {
                            "reason": "cancelled"
                        }
                    }
                })),
                user_text: None,
                prompt_index: Some(0),
                turn: None,
            }]
        );
    }

    #[test]
    fn test_interrupt_unannounced_upstream_restart_has_turn_meta() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();
        let owner1 = make_owner(Some(1), 10, Some("req_1"));
        let _ = core.on_prompt(owner1.clone(), json!({"prompt": "first"}), &mut ids);

        let actions = core.interrupt("upstream_restart");
        assert_eq!(
            actions,
            vec![Action::Reply {
                owners: vec![owner1.clone()],
                body: Reply::Result(json!({
                    "stopReason": "cancelled",
                    "_meta": {
                        "honk": {
                            "reason": "upstream_restart"
                        }
                    }
                })),
                user_text: None,
                prompt_index: Some(0),
                turn: Some(TurnMeta {
                    upstream_id: "hk1-1".into(),
                    client_req_ids: vec!["req_1".into()],
                    prompt_text: "first".into(),
                    run_id: None,
                    honk_reason: Some("upstream_restart".into()),
                }),
            }]
        );

        // Also test delivery_unknown
        let mut core2 = TurnCore::default();
        let _ = core2.on_prompt(owner1.clone(), json!({"prompt": "first"}), &mut ids);
        let actions2 = core2.interrupt("delivery_unknown");
        assert_eq!(
            actions2,
            vec![Action::Reply {
                owners: vec![owner1],
                body: Reply::Result(json!({
                    "stopReason": "cancelled",
                    "_meta": {
                        "honk": {
                            "reason": "delivery_unknown"
                        }
                    }
                })),
                user_text: None,
                prompt_index: Some(0),
                turn: Some(TurnMeta {
                    upstream_id: "hk1-2".into(),
                    client_req_ids: vec!["req_1".into()],
                    prompt_text: "first".into(),
                    run_id: None,
                    honk_reason: Some("delivery_unknown".into()),
                }),
            }]
        );
    }

    fn turn_meta(honk_reason: Option<&str>) -> TurnMeta {
        TurnMeta {
            upstream_id: "hk1-1".into(),
            client_req_ids: vec![],
            prompt_text: "hi".into(),
            run_id: Some("run_1".into()),
            honk_reason: honk_reason.map(String::from),
        }
    }

    fn failed(kind: &str, message: &str) -> Option<TurnOutcome> {
        Some(TurnOutcome::Failed {
            kind: kind.into(),
            message: message.into(),
        })
    }

    #[test]
    fn turn_outcome_goose_turn_error_is_recorded() {
        let body = Reply::Result(json!({
            "stopReason": "end_turn",
            "_meta": { "goose": { "turnError": { "kind": "network", "message": "reset" } } }
        }));
        assert_eq!(
            turn_outcome(&body, Some(&turn_meta(None)), Some(0)),
            failed("network", "reset")
        );
    }

    #[test]
    fn turn_outcome_clean_result_clears_including_user_cancel() {
        let clean = Reply::Result(json!({ "stopReason": "end_turn" }));
        assert_eq!(
            turn_outcome(&clean, Some(&turn_meta(None)), Some(0)),
            Some(TurnOutcome::Clean)
        );
        let cancelled = Reply::Result(json!({
            "stopReason": "cancelled",
            "_meta": { "honk": { "reason": "cancelled" } }
        }));
        assert_eq!(
            turn_outcome(&cancelled, Some(&turn_meta(Some("cancelled"))), Some(0)),
            Some(TurnOutcome::Clean)
        );
        let malformed = Reply::Result(json!({
            "stopReason": "end_turn",
            "_meta": { "goose": { "turnError": { "kind": 7 } } }
        }));
        assert_eq!(
            turn_outcome(&malformed, Some(&turn_meta(None)), Some(0)),
            Some(TurnOutcome::Clean)
        );
    }

    #[test]
    fn turn_outcome_restart_and_unknown_delivery_interrupts_are_recorded() {
        let mut ids = UpstreamIds::new(1);
        for reason in ["upstream_restart", "delivery_unknown"] {
            let mut core = TurnCore::default();
            let _ = core.on_prompt(
                make_owner(Some(1), 10, None),
                json!({"prompt": "x"}),
                &mut ids,
            );
            let actions = core.interrupt(reason);
            let Some(Action::Reply {
                body,
                turn,
                prompt_index,
                ..
            }) = actions.first()
            else {
                panic!("interrupt replies to the inflight prompt: {actions:?}");
            };
            assert!(
                matches!(
                    turn_outcome(body, turn.as_ref(), *prompt_index),
                    Some(TurnOutcome::Failed { ref kind, .. }) if kind == "interrupted"
                ),
                "{reason} is an interrupted turn"
            );
        }
    }

    #[test]
    fn turn_outcome_rpc_errors_are_recorded_but_busy_is_ignored() {
        let err = Reply::Error(json!({
            "code": -32603,
            "message": "Internal error",
            "data": "Error in agent response stream: boom"
        }));
        assert_eq!(
            turn_outcome(&err, Some(&turn_meta(None)), Some(0)),
            failed(
                "rpc",
                "Internal error: Error in agent response stream: boom"
            )
        );
        // on_prompt_send_failed: the prompt never reached goose (no turn meta).
        assert_eq!(
            turn_outcome(&err, None, Some(0)),
            failed(
                "rpc",
                "Internal error: Error in agent response stream: boom"
            )
        );
        let busy = Reply::Error(busy_error("run_1"));
        assert_eq!(turn_outcome(&busy, None, None), None);
        assert_eq!(turn_outcome(&busy, None, Some(0)), None);
    }

    #[test]
    fn turn_outcome_caps_long_messages_on_a_char_boundary() {
        let body = Reply::Error(json!({ "message": "é".repeat(TURN_ERROR_MAX_BYTES) }));
        let Some(TurnOutcome::Failed { message, .. }) =
            turn_outcome(&body, Some(&turn_meta(None)), Some(0))
        else {
            panic!("an error reply is a failure");
        };
        assert!(message.len() <= TURN_ERROR_MAX_BYTES);
        assert!(message.ends_with('…'));
    }

    #[test]
    fn turn_outcome_prompts_that_never_ran_say_nothing() {
        let mut ids = UpstreamIds::new(1);
        let mut core = TurnCore::default();
        let _ = core.on_prompt(
            make_owner(Some(1), 10, None),
            json!({"prompt": "x"}),
            &mut ids,
        );
        let actions = core.on_prompt_not_delivered("hk1-1", &mut ids);
        let Some(Action::Reply {
            body,
            turn,
            prompt_index,
            ..
        }) = actions.first()
        else {
            panic!("not_delivered replies: {actions:?}");
        };
        assert_eq!(turn_outcome(body, turn.as_ref(), *prompt_index), None);
    }
}
