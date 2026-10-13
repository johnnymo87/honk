use crate::conn::is_response;
use honk_wire::Cursor;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use super::parse::{busy_run_id, is_busy_error, is_lost_track, is_no_active_run, is_run_mismatch};
use super::types::{
    ChatOpts, Effect, InflightSteer, Input, Ownership, PermissionOption, PromptSent, RunState,
};

pub struct ChatCore {
    session_id: String,
    opts: ChatOpts,
    run_state: RunState,
    run_ownership: Ownership,
    our_crids: HashSet<String>,
    crid_counter: u64,
    next_req_id: u64,
    pending_load_req_id: Option<u64>,
    is_replaying: bool,
    is_detached: bool,
    is_reattaching: bool,
    detached_buffered_lines: Vec<String>,
    prompt_sent: Option<PromptSent>,
    held_steers: Vec<(String, u32)>,
    local_queue: VecDeque<(String, u32)>,
    pending_steer_reqs: HashMap<u64, InflightSteer>,
    pending_steers: HashMap<String, (String, String)>,
    consumed_steer_ids: HashSet<String>,
    unconsumed_steer_ids: HashSet<String>,
    answered_permissions: HashMap<String, Value>,
    seen_foreign_permissions: HashSet<String>,
    last_cursor: Option<Cursor>,
    current_time: Instant,
    last_activity: Instant,
}

impl ChatCore {
    pub fn new(session_id: String, opts: ChatOpts, now: Instant) -> Self {
        Self {
            session_id,
            opts,
            run_state: RunState::Unknown,
            run_ownership: Ownership::Unknown,
            our_crids: HashSet::new(),
            crid_counter: 0,
            next_req_id: 1,
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
            seen_foreign_permissions: HashSet::new(),
            last_cursor: None,
            current_time: now,
            last_activity: now,
        }
    }

    pub fn allocate_id(&mut self) -> u64 {
        let id = self.next_req_id;
        self.next_req_id += 1;
        id
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

    pub fn prompt_sent(&self) -> Option<&PromptSent> {
        self.prompt_sent.as_ref()
    }

    pub fn local_queue(&self) -> &VecDeque<(String, u32)> {
        &self.local_queue
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
        if self.is_detached {
            self.is_detached = false;
            self.is_reattaching = true;
            effects.push(Effect::Reattach);
            effects.push(Effect::Status("[honk] re-attaching...".into()));
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

        if line.starts_with('/') && self.handle_command(&line, effects) {
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
            if !prompt.run_seen {
                self.held_steers.push((text, conversion_count));
                effects.push(Effect::Status(
                    "[honk] prompt sent, waiting for turn to start; line held for steer".into(),
                ));
                return;
            } else if let RunState::Running(ref run_id) = self.run_state {
                let run_id = run_id.clone();
                self.send_steer(text, run_id, conversion_count, effects);
                return;
            } else {
                self.local_queue.push_back((text, conversion_count));
                effects.push(Effect::Status(
                    "[honk] turn ended; message queued for next turn".into(),
                ));
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
                    effects.push(Effect::Status(
                        "[honk] turn belongs to another client; message queued for after it ends (use /drop to discard)".into(),
                    ));
                }
            }
        }
    }

    fn send_prompt(&mut self, text: String, conversion_count: u32, effects: &mut Vec<Effect>) {
        self.crid_counter += 1;
        let crid = format!(
            "honk-chat-{:016x}-{}",
            self.opts.crid_seed, self.crid_counter
        );
        let req_id = self.allocate_id();

        let params = json!({
            "sessionId": self.session_id,
            "prompt": [{"type": "text", "text": text}],
            "_meta": {
                "honk": {
                    "client_req_id": crid
                }
            }
        });

        self.our_crids.insert(crid.clone());
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
        effects.push(Effect::Status("[honk] prompt sent".into()));
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
        effects.push(Effect::Status("[honk] steer sent".into()));
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

    fn handle_command(&mut self, line: &str, effects: &mut Vec<Effect>) -> bool {
        let trimmed = line.trim();
        if trimmed == "/help" {
            let help = "[honk] commands:\n  /help       - show this help\n  /quit       - detach and exit (session continues)\n  /cancel     - interrupt current turn (if own)\n  /cancel!    - cancel current turn regardless of owner\n  /drop       - discard locally queued messages\n  //text      - send literal '/text' as a prompt";
            effects.push(Effect::Status(help.into()));
            return true;
        }

        if trimmed == "/quit" {
            effects.push(Effect::Notify {
                method: "session/close".into(),
                params: json!({"sessionId": self.session_id}),
            });
            effects.push(Effect::Exit(0));
            return true;
        }

        if trimmed == "/drop" {
            let count = self.local_queue.len();
            self.local_queue.clear();
            effects.push(Effect::Status(format!(
                "[honk] dropped {count} queued message(s)"
            )));
            return true;
        }

        if trimmed == "/cancel" {
            self.handle_cancel(false, effects);
            return true;
        }

        if trimmed == "/cancel!" {
            self.handle_cancel(true, effects);
            return true;
        }

        // Unknown command
        effects.push(Effect::Status(format!(
            "[honk] unknown command: '{trimmed}'; type /help for commands"
        )));
        true
    }

    fn handle_cancel(&mut self, force: bool, effects: &mut Vec<Effect>) {
        if force {
            effects.push(Effect::Notify {
                method: "session/cancel".into(),
                params: json!({"sessionId": self.session_id}),
            });
            effects.push(Effect::Status("[honk] cancelling turn...".into()));
            return;
        }

        if self.run_state.is_running() {
            if self.run_ownership == Ownership::Own {
                effects.push(Effect::Notify {
                    method: "session/cancel".into(),
                    params: json!({"sessionId": self.session_id}),
                });
                effects.push(Effect::Status("[honk] cancelling turn...".into()));
            } else {
                effects.push(Effect::Status(
                    "[honk] this turn belongs to another client; /cancel! cancels it anyway".into(),
                ));
            }
        } else if self.prompt_sent.is_some() {
            effects.push(Effect::Notify {
                method: "session/cancel".into(),
                params: json!({"sessionId": self.session_id}),
            });
            effects.push(Effect::Status("[honk] cancelling turn...".into()));
        } else {
            effects.push(Effect::Status("[honk] no active turn to cancel".into()));
        }
    }

    fn handle_tick(&mut self, now: Instant, effects: &mut Vec<Effect>) {
        self.current_time = now;
        if self.run_state.is_running() {
            self.last_activity = now;
        }

        if !self.is_detached
            && !self.run_state.is_running()
            && self.prompt_sent.is_none()
            && self.local_queue.is_empty()
            && self.pending_steer_reqs.is_empty()
            && now.saturating_duration_since(self.last_activity) >= self.opts.idle_timeout
        {
            self.is_detached = true;
            effects.push(Effect::Detach);
            effects.push(Effect::Status(
                "[honk] detached (idle); press Enter to re-attach".into(),
            ));
        }
    }

    fn handle_frame(&mut self, frame: Value, effects: &mut Vec<Effect>) {
        if is_response(&frame) {
            if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                if Some(id) == self.pending_load_req_id {
                    self.pending_load_req_id = None;
                    if let Some(err) = frame.get("error") {
                        effects.push(Effect::Status(format!("[honk] session load failed: {err}")));
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
            if method == "session/request_permission" || method == "request_permission" {
                self.handle_permission_request(&frame, effects);
            } else {
                effects.push(Effect::Status(format!(
                    "[honk] unsupported request {method}; answer it from another client"
                )));
            }
            return;
        }

        if let Some(method) = frame.get("method").and_then(Value::as_str) {
            let method = method.to_string();
            self.handle_notification(&method, &frame, effects);
        }
    }

    fn handle_permission_request(&mut self, frame: &Value, effects: &mut Vec<Effect>) {
        let goose_req_id = frame["id"].clone();
        let id_key = goose_req_id.to_string();

        let tool_title = frame
            .pointer("/params/toolCall/title")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string();

        let is_own_turn = self.run_ownership == Ownership::Own
            || (self.run_ownership != Ownership::Foreign && self.prompt_sent.is_some());

        if is_own_turn {
            if let Some(cached_result) = self.answered_permissions.get(&id_key) {
                effects.push(Effect::Respond {
                    id: goose_req_id,
                    result: cached_result.clone(),
                });
                effects.push(Effect::Status(format!(
                    "[honk] auto-approved: {tool_title}"
                )));
                return;
            }

            let mut options = Vec::new();
            if let Some(opts_arr) = frame.pointer("/params/options").and_then(Value::as_array) {
                for opt in opts_arr {
                    let option_id = opt
                        .get("optionId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = opt
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(&option_id)
                        .to_string();
                    let kind = opt
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    options.push(PermissionOption {
                        option_id,
                        name,
                        kind,
                    });
                }
            }

            let chosen = options
                .iter()
                .find(|o| o.kind == "allow_always")
                .or_else(|| options.iter().find(|o| o.kind == "allow_once"))
                .or_else(|| options.first());

            let result = if let Some(opt) = chosen {
                json!({
                    "outcome": {
                        "outcome": "selected",
                        "optionId": opt.option_id
                    }
                })
            } else {
                json!({
                    "outcome": {
                        "outcome": "cancelled"
                    }
                })
            };

            self.answered_permissions.insert(id_key, result.clone());
            effects.push(Effect::Respond {
                id: goose_req_id,
                result,
            });
            effects.push(Effect::Status(format!(
                "[honk] auto-approved: {tool_title}"
            )));
        } else if self.seen_foreign_permissions.insert(id_key) {
            effects.push(Effect::Status(format!(
                "[honk] permission request from another client's turn: {tool_title}"
            )));
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
                    effects.push(Effect::Status(
                        "[honk] turn belongs to another client; message queued for after it ends (use /drop to discard)".into(),
                    ));
                } else if is_lost_track(err) {
                    effects.push(Effect::Status(format!(
                        "[honk] lost track of prompt; your message did not run: {}",
                        prompt.text
                    )));
                } else {
                    let msg = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    effects.push(Effect::Status(format!(
                        "[honk] prompt error: {msg}; your message did not run: {}",
                        prompt.text
                    )));
                }
            } else {
                let res = frame.get("result").unwrap_or(&Value::Null);
                let stop_reason = res
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn");
                if stop_reason == "cancelled" {
                    let honk_reason = res.pointer("/_meta/honk/reason").and_then(Value::as_str);
                    if honk_reason == Some("not_delivered")
                        || honk_reason == Some("upstream_restart")
                    {
                        effects.push(Effect::Status(format!(
                            "[honk] your message did not run: {}",
                            prompt.text
                        )));
                    } else {
                        effects.push(Effect::Status(
                            "[honk] turn ended (stopReason: cancelled)".into(),
                        ));
                    }
                } else {
                    effects.push(Effect::Status(format!(
                        "[honk] turn ended (stopReason: {stop_reason})"
                    )));
                }
            }

            if !prompt.run_seen && !self.held_steers.is_empty() {
                let held = std::mem::take(&mut self.held_steers);
                unstarted_to_queue.extend(held);
                effects.push(Effect::Status("[honk] held lines moved to queue".into()));
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
                        effects.push(Effect::Status(
                            "[honk] no active run to steer; converting to prompt".into(),
                        ));
                        self.send_prompt(inflight.text, inflight.conversion_count + 1, effects);
                    } else {
                        effects.push(Effect::Status(format!(
                            "[honk] steer error: no active run; your message did not run: {}",
                            inflight.text
                        )));
                    }
                } else if is_run_mismatch(err) {
                    effects.push(Effect::Status(format!(
                        "[honk] the turn you were steering ended; re-send to send it to the current one: {}",
                        inflight.text
                    )));
                } else {
                    let msg = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    effects.push(Effect::Status(format!(
                        "[honk] steer error: {msg}; your message did not run: {}",
                        inflight.text
                    )));
                }
            } else if let Some(msg_id) = frame.pointer("/result/messageId").and_then(Value::as_str)
            {
                if self.consumed_steer_ids.contains(msg_id) {
                    effects.push(Effect::Status("[honk] steer accepted".into()));
                } else if self.unconsumed_steer_ids.remove(msg_id)
                    || self.run_state != RunState::Running(inflight.expected_run_id.clone())
                {
                    effects.push(Effect::Status(format!(
                        "[honk] steer not consumed: {}",
                        inflight.text
                    )));
                } else {
                    self.pending_steers.insert(
                        msg_id.to_string(),
                        (inflight.text, inflight.expected_run_id),
                    );
                    effects.push(Effect::Status("[honk] steer accepted".into()));
                }
            } else {
                effects.push(Effect::Status("[honk] steer accepted".into()));
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
            honk_wire::method::REPLAY_DONE => {
                self.is_replaying = false;
                self.is_reattaching = false;
                if let (Some(epoch), Some(seq)) = (
                    frame.pointer("/params/epoch").and_then(Value::as_u64),
                    frame.pointer("/params/seq").and_then(Value::as_u64),
                ) {
                    self.last_cursor = Some(Cursor { epoch, seq });
                }
                let buffered = std::mem::take(&mut self.detached_buffered_lines);
                for line in buffered {
                    self.handle_line(line, effects);
                }
                self.check_queued_prompts(effects);
            }
            "session/update" => {
                if let Some(u) = frame.pointer("/params/update") {
                    if let Some(run_opt) = honk_wire::run_state(u) {
                        match run_opt {
                            Some(run_id) => {
                                self.run_state = RunState::Running(run_id.clone());
                                if !self.is_replaying {
                                    if self.prompt_sent.is_some() {
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
                                    } else if self.run_ownership != Ownership::Own {
                                        self.run_ownership = Ownership::Foreign;
                                    }
                                }
                            }
                            None => {
                                let ended_run_id = match &self.run_state {
                                    RunState::Running(r) => Some(r.clone()),
                                    _ => None,
                                };
                                self.run_state = RunState::Idle;
                                self.run_ownership = Ownership::Unknown;
                                if !self.is_replaying {
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
                                        effects.push(Effect::Status(format!(
                                            "[honk] steer not consumed: {steer_text}"
                                        )));
                                    }
                                    self.check_queued_prompts(effects);
                                }
                            }
                        }
                    }

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
                            && self.our_crids.contains(crid)
                        {
                            self.run_ownership = Ownership::Own;
                        }
                    }
                }
            }
            honk_wire::method::STEER_UNCONSUMED => {
                if !self.is_replaying
                    && let Some(mid) = frame.pointer("/params/messageId").and_then(Value::as_str)
                {
                    if let Some((text, _)) = self.pending_steers.remove(mid) {
                        effects.push(Effect::Status(format!("[honk] steer not consumed: {text}")));
                    } else {
                        self.unconsumed_steer_ids.insert(mid.to_string());
                    }
                }
            }
            honk_wire::method::GAP => {
                if !self.is_replaying {
                    effects.push(Effect::Status(
                        "[honk] frame gap detected; transcript will reset at turn end".into(),
                    ));
                }
            }
            honk_wire::method::EVICTED => {
                self.handle_disconnected(effects);
                effects.push(Effect::Reattach);
                effects.push(Effect::Status(
                    "[honk] session evicted; re-attaching...".into(),
                ));
            }
            _ => {}
        }
    }

    fn handle_connected(&mut self, effects: &mut Vec<Effect>) {
        effects.push(Effect::Status("[honk] connected".into()));
        if self.prompt_sent.is_some() {
            let req_id = self.allocate_id();
            let p = self.prompt_sent.as_mut().unwrap();
            p.req_id = req_id;

            let params = json!({
                "sessionId": self.session_id,
                "prompt": [{"type": "text", "text": p.text}],
                "_meta": {
                    "honk": {
                        "client_req_id": p.crid
                    }
                }
            });
            effects.push(Effect::Request {
                id: req_id,
                method: "session/prompt".into(),
                params,
            });
            effects.push(Effect::Status(format!(
                "[honk] re-sent prompt (crid: {})",
                p.crid
            )));
        }
    }

    fn handle_disconnected(&mut self, effects: &mut Vec<Effect>) {
        effects.push(Effect::Status(
            "[honk] disconnected; reconnecting...".into(),
        ));
        self.run_state = RunState::Unknown;
        self.run_ownership = Ownership::Unknown;
        self.is_replaying = true;
        for (_req_id, inflight) in self.pending_steer_reqs.drain() {
            effects.push(Effect::Status(format!(
                "[honk] steer may not have been delivered: {}",
                inflight.text
            )));
        }
    }
}
