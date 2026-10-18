use crate::conn::is_response;
use honk_wire::Cursor;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use super::parse::{busy_run_id, is_busy_error, is_lost_track, is_no_active_run, is_run_mismatch};
use super::types::{
    ChatOpts, Effect, IdAlloc, InflightSteer, Input, Notice, Ownership, PendingPermission,
    PromptSent, RunState,
};

pub struct ChatCore {
    pub(crate) session_id: String,
    pub(crate) opts: ChatOpts,
    pub(crate) ids: IdAlloc,
    pub(crate) run_state: RunState,
    pub(crate) run_ownership: Ownership,
    pub(crate) last_echo_crid: Option<String>,
    pub(crate) replay_echo_matched_inflight: bool,
    pub(crate) crid_counter: u64,
    pub(crate) pending_load_req_id: Option<u64>,
    pub(crate) is_replaying: bool,
    pub(crate) is_detached: bool,
    pub(crate) is_reattaching: bool,
    pub(crate) detached_buffered_lines: Vec<String>,
    pub(crate) prompt_sent: Option<PromptSent>,
    pub(crate) held_steers: Vec<(String, u32)>,
    pub(crate) local_queue: VecDeque<(String, u32)>,
    pub(crate) pending_steer_reqs: HashMap<u64, InflightSteer>,
    pub(crate) pending_steers: HashMap<String, (String, String)>,
    pub(crate) consumed_steer_ids: HashSet<String>,
    pub(crate) unconsumed_steer_ids: HashSet<String>,
    pub(crate) answered_permissions: HashMap<String, Value>,
    pub(crate) pending_permissions: Vec<PendingPermission>,
    pub(crate) last_cursor: Option<Cursor>,
    pub(crate) current_time: Instant,
    pub(crate) last_activity: Instant,
    pub(crate) disconnected: bool,
}

impl ChatCore {
    /// Creates a new `ChatCore` with a fresh ID allocator.
    pub fn new(session_id: String, opts: ChatOpts, now: Instant) -> Self {
        Self::with_ids(session_id, opts, now, IdAlloc::new())
    }

    /// Creates a new `ChatCore` reusing an existing ID allocator.
    ///
    /// This is useful for `--new` workflows where the driver allocates request
    /// IDs for `session/new` before the session ID is known:
    /// ```text
    /// let mut ids = IdAlloc::new();
    /// let new_req_id = ids.allocate();
    /// // ... send session/new, receive session_id ...
    /// let mut core = ChatCore::with_ids(session_id, opts, now, ids);
    /// let (load_id, method, params) = core.attach_request(); // continues ID sequence
    /// ```
    pub fn with_ids(session_id: String, opts: ChatOpts, now: Instant, ids: IdAlloc) -> Self {
        Self {
            session_id,
            opts,
            ids,
            run_state: RunState::Unknown,
            run_ownership: Ownership::Unknown,
            last_echo_crid: None,
            replay_echo_matched_inflight: false,
            crid_counter: 0,
            pending_load_req_id: None,
            is_replaying: true,
            is_detached: false,
            is_reattaching: false,
            detached_buffered_lines: Vec::new(),
            prompt_sent: None,
            held_steers: Vec::new(),
            local_queue: VecDeque::new(),
            pending_steer_reqs: HashMap::new(),
            pending_steers: HashMap::new(),
            consumed_steer_ids: HashSet::new(),
            unconsumed_steer_ids: HashSet::new(),
            answered_permissions: HashMap::new(),
            pending_permissions: Vec::new(),
            last_cursor: None,
            current_time: now,
            last_activity: now,
            disconnected: false,
        }
    }

    pub fn allocate_id(&mut self) -> u64 {
        self.ids.allocate()
    }

    pub fn attach_request(&mut self) -> (u64, String, Value) {
        let id = self.allocate_id();
        self.pending_load_req_id = Some(id);
        self.is_replaying = true;
        (id, "session/load".to_string(), self.attach_params())
    }

    pub fn cursor(&self) -> Option<Cursor> {
        self.last_cursor.clone()
    }

    pub fn attach_params(&self) -> Value {
        if let Some(cursor) = &self.last_cursor {
            json!({
                "sessionId": self.session_id,
                "_meta": {
                    "honk": {
                        "after": {
                            "epoch": cursor.epoch,
                            "seq": cursor.seq,
                        }
                    }
                }
            })
        } else {
            json!({
                "sessionId": self.session_id
            })
        }
    }

    pub fn run_state(&self) -> &RunState {
        &self.run_state
    }

    pub fn ownership(&self) -> Ownership {
        self.run_ownership
    }

    pub fn is_replaying(&self) -> bool {
        self.is_replaying
    }

    pub fn is_detached(&self) -> bool {
        self.is_detached
    }

    pub fn prompt_in_flight(&self) -> bool {
        self.prompt_sent.is_some()
    }

    pub fn queued_len(&self) -> usize {
        self.local_queue.len() + self.held_steers.len() + self.detached_buffered_lines.len()
    }

    pub fn prompt_sent(&self) -> Option<&PromptSent> {
        self.prompt_sent.as_ref()
    }

    pub fn local_queue(&self) -> &VecDeque<(String, u32)> {
        &self.local_queue
    }

    pub fn max_turns(&self) -> Option<u32> {
        self.opts.max_turns
    }

    pub fn handle(&mut self, input: Input) -> Vec<Effect> {
        let mut effects = Vec::new();
        match input {
            Input::Line(line) => {
                self.last_activity = self.current_time;
                self.handle_line(line, &mut effects);
            }
            Input::Interrupt => {
                self.last_activity = self.current_time;
                self.handle_cancel(false, &mut effects);
            }
            Input::Eof => {
                effects.push(Effect::Notify {
                    method: "session/close".into(),
                    params: json!({"sessionId": self.session_id}),
                });
                effects.push(Effect::Exit(0));
            }
            Input::Tick(now) => {
                self.handle_tick(now, &mut effects);
            }
            Input::Frame(frame) => {
                self.handle_frame(frame, &mut effects);
            }
            Input::Connected => {
                self.last_activity = self.current_time;
                self.handle_connected(&mut effects);
            }
            Input::Disconnected => {
                self.handle_disconnected(&mut effects);
            }
        }
        effects
    }

    fn handle_line(&mut self, line: String, effects: &mut Vec<Effect>) {
        if line.starts_with('/') && !line.starts_with("//") && self.handle_command(&line, effects) {
            return;
        }

        if self.is_detached {
            self.is_detached = false;
            self.is_reattaching = true;
            effects.push(Effect::Reattach);
            effects.push(Effect::Notice(Notice::info("re-attaching...")));
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                self.detached_buffered_lines.push(line);
            }
            return;
        }

        if self.is_reattaching || self.is_replaying {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                self.detached_buffered_lines.push(line);
            }
            return;
        }

        if line.starts_with("//") {
            let text = line[1..].to_string();
            self.process_text_input(text, 0, effects);
            return;
        }

        self.process_text_input(line, 0, effects);
    }

    fn process_text_input(
        &mut self,
        text: String,
        conversion_count: u32,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(prompt) = &mut self.prompt_sent {
            if self.run_ownership == Ownership::Own
                && let RunState::Running(ref run_id) = self.run_state
            {
                let run_id = run_id.clone();
                self.send_steer(text, run_id, conversion_count, effects);
                return;
            }
            // Run not yet started, or run is Pending/Foreign
            if !prompt.run_seen || self.run_ownership == Ownership::Pending {
                self.held_steers.push((text, conversion_count));
                effects.push(Effect::Notice(Notice::info(
                    "prompt sent, waiting for turn to start; line held for steer",
                )));
                return;
            } else if self.run_state.is_running() {
                // Foreign run active while our prompt is parked
                self.held_steers.push((text, conversion_count));
                effects.push(Effect::Notice(Notice::info(
                    "prompt sent, waiting for turn to start; line held for steer",
                )));
                return;
            } else {
                self.local_queue.push_back((text, conversion_count));
                effects.push(Effect::Notice(Notice::info(
                    "turn ended; message queued for next turn",
                )));
                return;
            }
        }

        match &self.run_state {
            RunState::Unknown | RunState::Idle => {
                self.send_prompt(text, conversion_count, effects);
            }
            RunState::Running(run_id) => {
                if self.run_ownership == Ownership::Own {
                    let run_id = run_id.clone();
                    self.send_steer(text, run_id, conversion_count, effects);
                } else {
                    self.local_queue.push_back((text, conversion_count));
                    effects.push(Effect::Notice(Notice::info(
                        "turn belongs to another client; message queued for after it ends (use /drop to discard)",
                    )));
                }
            }
        }
    }

    pub(crate) fn send_prompt(
        &mut self,
        text: String,
        conversion_count: u32,
        effects: &mut Vec<Effect>,
    ) {
        self.crid_counter += 1;
        let crid = format!(
            "honk-chat-{:016x}-{}",
            self.opts.crid_seed, self.crid_counter
        );
        let req_id = self.allocate_id();

        let mut meta = json!({
            "honk": {
                "client_req_id": crid
            }
        });
        if let Some(max_turns) = self.opts.max_turns {
            meta["goose"] = json!({ "maxTurns": max_turns });
        }

        let params = json!({
            "sessionId": self.session_id,
            "prompt": [{"type": "text", "text": text}],
            "_meta": meta
        });

        self.prompt_sent = Some(PromptSent {
            crid,
            text,
            req_id,
            run_seen: false,
            conversion_count,
        });

        effects.push(Effect::Request {
            id: req_id,
            method: "session/prompt".into(),
            params,
        });
    }

    fn send_steer(
        &mut self,
        text: String,
        run_id: String,
        conversion_count: u32,
        effects: &mut Vec<Effect>,
    ) {
        let req_id = self.allocate_id();

        let params = json!({
            "sessionId": self.session_id,
            "expectedRunId": run_id,
            "prompt": [{"type": "text", "text": text}]
        });

        self.pending_steer_reqs.insert(
            req_id,
            InflightSteer {
                text,
                expected_run_id: run_id,
                conversion_count,
            },
        );

        effects.push(Effect::Request {
            id: req_id,
            method: "_goose/unstable/session/steer".into(),
            params,
        });
    }

    fn check_queued_prompts(&mut self, effects: &mut Vec<Effect>) {
        if !self.is_replaying
            && !self.run_state.is_running()
            && self.prompt_sent.is_none()
            && let Some((next_text, conv_count)) = self.local_queue.pop_front()
        {
            self.send_prompt(next_text, conv_count, effects);
        }
    }

    fn handle_tick(&mut self, now: Instant, effects: &mut Vec<Effect>) {
        self.current_time = now;
        if self.run_state.is_running() {
            self.last_activity = now;
        }

        if let Some(idle_timeout) = self.opts.idle_timeout
            && !self.is_detached
            && !self.run_state.is_running()
            && self.prompt_sent.is_none()
            && self.local_queue.is_empty()
            && self.pending_steer_reqs.is_empty()
            && now.saturating_duration_since(self.last_activity) >= idle_timeout
        {
            self.is_detached = true;
            effects.push(Effect::Detach);
            effects.push(Effect::Notice(Notice::info(
                "detached (idle); press Enter to re-attach",
            )));
        }
    }

    fn handle_frame(&mut self, frame: Value, effects: &mut Vec<Effect>) {
        if is_response(&frame) {
            if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                if Some(id) == self.pending_load_req_id {
                    self.pending_load_req_id = None;
                    if let Some(err) = frame.get("error") {
                        self.is_replaying = false;
                        effects.push(Effect::AttachFailed { error: err.clone() });
                    } else {
                        let result = frame.get("result").cloned().unwrap_or(Value::Null);
                        effects.push(Effect::Attached { result });
                    }
                    return;
                }
                self.handle_response(id, &frame, effects);
            }
            return;
        }

        if frame.get("id").is_some() && frame.get("method").is_some() {
            if self.is_replaying {
                return;
            }
            let method = frame["method"].as_str().unwrap_or("");
            if method.contains("elicit") {
                if self.run_ownership == Ownership::Own {
                    effects.push(Effect::Respond {
                        id: frame["id"].clone(),
                        result: json!({"action": "decline"}),
                    });
                    effects.push(Effect::Notice(Notice::info(format!(
                        "declined elicitation request: {method}"
                    ))));
                } else {
                    effects.push(Effect::Notice(Notice::info(format!(
                        "elicitation request from another client's turn: {method}"
                    ))));
                }
                return;
            }
            if method == "session/request_permission" || method == "request_permission" {
                self.handle_permission_request(&frame, effects);
            } else {
                effects.push(Effect::Notice(Notice::warning(format!(
                    "unsupported request {method}; answer it from another client"
                ))));
            }
            return;
        }

        if let Some(method) = frame.get("method").and_then(Value::as_str) {
            let method = method.to_string();
            self.handle_notification(&method, &frame, effects);
        }
    }

    fn handle_response(&mut self, id: u64, frame: &Value, effects: &mut Vec<Effect>) {
        if let Some(prompt) = self.prompt_sent.as_ref()
            && prompt.req_id == id
        {
            let prompt = self.prompt_sent.take().unwrap();
            let mut unstarted_to_queue = Vec::new();
            if let Some(err) = frame.get("error") {
                if is_busy_error(err) {
                    if let Some(run_id) = busy_run_id(err) {
                        self.run_state = RunState::Running(run_id);
                        self.run_ownership = Ownership::Foreign;
                    }
                    unstarted_to_queue.push((prompt.text, prompt.conversion_count));
                    effects.push(Effect::Notice(Notice::info(
                        "turn belongs to another client; message queued for after it ends (use /drop to discard)",
                    )));
                } else if is_lost_track(err) {
                    effects.push(Effect::Notice(Notice::returned(
                        format!(
                            "lost track of prompt; your message did not run: {}",
                            prompt.text
                        ),
                        prompt.text.clone(),
                    )));
                } else {
                    let msg = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    effects.push(Effect::Notice(Notice::returned(
                        format!(
                            "prompt error: {msg}; your message did not run: {}",
                            prompt.text
                        ),
                        prompt.text.clone(),
                    )));
                }
            } else {
                let res = frame.get("result").unwrap_or(&Value::Null);
                let stop_reason = res
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn");
                self.run_state = RunState::Idle;
                self.run_ownership = Ownership::Unknown;
                if stop_reason == "cancelled" {
                    let honk_reason = res.pointer("/_meta/honk/reason").and_then(Value::as_str);
                    if honk_reason == Some("not_delivered")
                        || honk_reason == Some("upstream_restart")
                    {
                        effects.push(Effect::Notice(Notice::returned(
                            format!("your message did not run: {}", prompt.text),
                            prompt.text.clone(),
                        )));
                    } else {
                        effects.push(Effect::Notice(Notice::info(
                            "turn ended (stopReason: cancelled)",
                        )));
                    }
                } else if stop_reason != "end_turn" {
                    effects.push(Effect::Notice(Notice::info(format!(
                        "turn ended (stopReason: {stop_reason})"
                    ))));
                }
            }

            if !self.held_steers.is_empty() {
                let held = std::mem::take(&mut self.held_steers);
                unstarted_to_queue.extend(held);
                effects.push(Effect::Notice(Notice::info("held lines moved to queue")));
            }

            for (t, c) in unstarted_to_queue.into_iter().rev() {
                self.local_queue.push_front((t, c));
            }

            self.check_queued_prompts(effects);
            return;
        }

        if let Some(inflight) = self.pending_steer_reqs.remove(&id) {
            if let Some(err) = frame.get("error") {
                if is_no_active_run(err) {
                    if inflight.conversion_count < 1 {
                        effects.push(Effect::Notice(Notice::info(
                            "no active run to steer; converting to prompt",
                        )));
                        self.send_prompt(inflight.text, inflight.conversion_count + 1, effects);
                    } else {
                        effects.push(Effect::Notice(Notice::returned(
                            format!(
                                "steer error: no active run; your message did not run: {}",
                                inflight.text
                            ),
                            inflight.text,
                        )));
                    }
                } else if is_run_mismatch(err) {
                    effects.push(Effect::Notice(Notice::returned(
                        format!(
                            "the turn you were steering ended; re-send to send it to the current one: {}",
                            inflight.text
                        ),
                        inflight.text,
                    )));
                } else {
                    let msg = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    effects.push(Effect::Notice(Notice::returned(
                        format!(
                            "steer error: {msg}; your message did not run: {}",
                            inflight.text
                        ),
                        inflight.text,
                    )));
                }
            } else if let Some(msg_id) = frame.pointer("/result/messageId").and_then(Value::as_str)
            {
                if self.consumed_steer_ids.contains(msg_id) {
                    // steer accepted dropped
                } else if self.unconsumed_steer_ids.remove(msg_id)
                    || self.run_state != RunState::Running(inflight.expected_run_id.clone())
                {
                    effects.push(Effect::Notice(Notice::warning(format!(
                        "steer not consumed: {}",
                        inflight.text
                    ))));
                } else {
                    self.pending_steers.insert(
                        msg_id.to_string(),
                        (inflight.text, inflight.expected_run_id),
                    );
                    // steer accepted dropped
                }
            }
        }
    }

    fn handle_notification(&mut self, method: &str, frame: &Value, effects: &mut Vec<Effect>) {
        if method == "session/update"
            || method == honk_wire::method::RESET
            || method == honk_wire::method::REPLAY_DONE
        {
            effects.push(Effect::Transcript(frame.clone()));
        }

        if let Some(cursor) = honk_wire::frame_cursor(frame) {
            self.last_cursor = Some(cursor);
        }

        match method {
            honk_wire::method::RESET => {
                self.is_replaying = true;
                self.run_state = RunState::Unknown;
                self.run_ownership = Ownership::Unknown;
                self.last_echo_crid = None;
                self.replay_echo_matched_inflight = false;
                self.pending_permissions.clear();
                effects.push(Effect::Transcript(frame.clone()));
            }
            honk_wire::method::REPLAY_DONE => {
                self.is_replaying = false;
                self.is_reattaching = false;
                self.last_echo_crid = None;
                if let (Some(epoch), Some(seq)) = (
                    frame.pointer("/params/epoch").and_then(Value::as_u64),
                    frame.pointer("/params/seq").and_then(Value::as_u64),
                ) {
                    self.last_cursor = Some(Cursor { epoch, seq });
                }

                if let Some(p) = self.prompt_sent.as_mut() {
                    if self.replay_echo_matched_inflight {
                        if let RunState::Running(ref run_id) = self.run_state {
                            self.run_ownership = Ownership::Own;
                            p.run_seen = true;
                            let run_id = run_id.clone();
                            let held = std::mem::take(&mut self.held_steers);
                            for (held_text, conv_count) in held {
                                self.send_steer(held_text, run_id.clone(), conv_count, effects);
                            }
                        } else {
                            self.last_echo_crid = Some(p.crid.clone());
                        }
                    } else {
                        self.run_ownership = Ownership::Unknown;
                    }
                } else {
                    self.run_ownership = Ownership::Unknown;
                }
                self.replay_echo_matched_inflight = false;

                let buffered = std::mem::take(&mut self.detached_buffered_lines);
                for line in buffered {
                    self.handle_line(line, effects);
                }
                self.check_queued_prompts(effects);
            }
            "session/update" => {
                if let Some(u) = frame.pointer("/params/update") {
                    if u.get("sessionUpdate").and_then(Value::as_str) == Some("user_message_chunk")
                    {
                        let msg_id = u
                            .get("messageId")
                            .or_else(|| u.pointer("/_meta/goose/messageId"))
                            .and_then(Value::as_str);
                        if let Some(mid) = msg_id {
                            self.consumed_steer_ids.insert(mid.to_string());
                            self.pending_steers.remove(mid);
                        }
                        if let Some(crid) = u
                            .pointer("/_meta/honk/source/client_req_id")
                            .and_then(Value::as_str)
                        {
                            if self.is_replaying {
                                if self.prompt_sent.as_ref().is_some_and(|p| p.crid == crid) {
                                    self.replay_echo_matched_inflight = true;
                                }
                            } else if !self.run_state.is_running() {
                                self.last_echo_crid = Some(crid.to_string());
                            } else if self.run_ownership == Ownership::Pending {
                                if self.prompt_sent.as_ref().is_some_and(|p| p.crid == crid) {
                                    self.run_ownership = Ownership::Own;
                                    if let Some(p) = self.prompt_sent.as_mut() {
                                        p.run_seen = true;
                                    }
                                    if let RunState::Running(ref run_id) = self.run_state {
                                        let run_id = run_id.clone();
                                        let held = std::mem::take(&mut self.held_steers);
                                        for (held_text, conv_count) in held {
                                            self.send_steer(
                                                held_text,
                                                run_id.clone(),
                                                conv_count,
                                                effects,
                                            );
                                        }
                                    }
                                } else {
                                    self.run_ownership = Ownership::Foreign;
                                }
                            }
                        }
                    }

                    if let Some(run_opt) = honk_wire::run_state(u) {
                        match run_opt {
                            Some(run_id) => {
                                self.run_state = RunState::Running(run_id.clone());
                                if !self.is_replaying {
                                    let echo_crid = self.last_echo_crid.take();
                                    let is_own = self.prompt_sent.as_ref().is_some_and(|p| {
                                        echo_crid.as_deref() == Some(p.crid.as_str())
                                    });
                                    if is_own {
                                        self.run_ownership = Ownership::Own;
                                        if let Some(p) = self.prompt_sent.as_mut() {
                                            p.run_seen = true;
                                        }
                                        let held = std::mem::take(&mut self.held_steers);
                                        for (held_text, conv_count) in held {
                                            self.send_steer(
                                                held_text,
                                                run_id.clone(),
                                                conv_count,
                                                effects,
                                            );
                                        }
                                    } else if echo_crid.is_some() {
                                        self.run_ownership = Ownership::Foreign;
                                    } else if self.prompt_sent.is_some() {
                                        self.run_ownership = Ownership::Pending;
                                    } else {
                                        self.run_ownership = Ownership::Foreign;
                                    }
                                }
                            }
                            None => {
                                let ended_run_id = match &self.run_state {
                                    RunState::Running(r) => Some(r.clone()),
                                    _ => None,
                                };
                                let was_ownership = self.run_ownership;
                                self.run_state = RunState::Idle;
                                self.run_ownership = Ownership::Unknown;
                                self.last_echo_crid = None;
                                self.replay_echo_matched_inflight = false;
                                self.pending_permissions.clear();
                                if !self.is_replaying {
                                    if was_ownership != Ownership::Own
                                        && !self.held_steers.is_empty()
                                    {
                                        let held = std::mem::take(&mut self.held_steers);
                                        for (t, c) in held.into_iter().rev() {
                                            self.local_queue.push_front((t, c));
                                        }
                                        effects.push(Effect::Notice(Notice::info(
                                            "held lines moved to queue",
                                        )));
                                    }

                                    let mut unconsumed = Vec::new();
                                    self.pending_steers.retain(|_mid, (text, run_id)| {
                                        if ended_run_id.as_ref() == Some(run_id) {
                                            unconsumed.push(text.clone());
                                            false
                                        } else {
                                            true
                                        }
                                    });
                                    unconsumed.sort();
                                    for steer_text in unconsumed {
                                        effects.push(Effect::Notice(Notice::warning(format!(
                                            "steer not consumed: {steer_text}"
                                        ))));
                                    }
                                    self.check_queued_prompts(effects);
                                }
                            }
                        }
                    }
                }
            }
            honk_wire::method::REQUEST_RESOLVED => {
                let goose_req_id = frame.pointer("/params/requestId").unwrap_or(&Value::Null);
                self.pending_permissions
                    .retain(|p| p.goose_req_id != *goose_req_id);
                if !self.is_replaying {
                    let reason = frame
                        .pointer("/params/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("resolved");
                    effects.push(Effect::Notice(Notice::info(format!(
                        "permission request resolved: {reason}"
                    ))));
                }
            }
            honk_wire::method::STEER_UNCONSUMED => {
                if !self.is_replaying
                    && let Some(mid) = frame.pointer("/params/messageId").and_then(Value::as_str)
                {
                    if let Some((text, _)) = self.pending_steers.remove(mid) {
                        effects.push(Effect::Notice(Notice::warning(format!(
                            "steer not consumed: {text}"
                        ))));
                    } else {
                        self.unconsumed_steer_ids.insert(mid.to_string());
                    }
                }
            }
            honk_wire::method::GAP => {
                if !self.is_replaying {
                    effects.push(Effect::Notice(Notice::warning(
                        "frame gap detected; transcript will reset at turn end",
                    )));
                }
            }
            honk_wire::method::EVICTED => {
                self.handle_disconnected(effects);
                effects.push(Effect::Reattach);
                effects.push(Effect::Notice(Notice::info(
                    "session evicted; re-attaching...",
                )));
            }
            _ => {}
        }
    }

    fn handle_connected(&mut self, effects: &mut Vec<Effect>) {
        self.disconnected = false;
        if self.prompt_sent.is_some() {
            let req_id = self.allocate_id();
            let p = self.prompt_sent.as_mut().unwrap();
            p.req_id = req_id;

            let mut meta = json!({
                "honk": {
                    "client_req_id": p.crid
                }
            });
            if let Some(max_turns) = self.opts.max_turns {
                meta["goose"] = json!({ "maxTurns": max_turns });
            }

            let params = json!({
                "sessionId": self.session_id,
                "prompt": [{"type": "text", "text": p.text}],
                "_meta": meta
            });
            effects.push(Effect::Request {
                id: req_id,
                method: "session/prompt".into(),
                params,
            });
            effects.push(Effect::Notice(Notice::info(format!(
                "re-sent prompt (crid: {})",
                p.crid
            ))));
        }
    }

    fn handle_disconnected(&mut self, effects: &mut Vec<Effect>) {
        if !self.disconnected {
            self.disconnected = true;
            effects.push(Effect::Notice(Notice::info(
                "disconnected; reconnecting...",
            )));
        }
        self.run_state = RunState::Unknown;
        self.run_ownership = Ownership::Unknown;
        self.last_echo_crid = None;
        self.replay_echo_matched_inflight = false;
        self.is_replaying = true;
        self.pending_permissions.clear();
        for (_req_id, inflight) in self.pending_steer_reqs.drain() {
            effects.push(Effect::Notice(Notice::warning(format!(
                "steer may not have been delivered: {}",
                inflight.text
            ))));
        }
    }
}
