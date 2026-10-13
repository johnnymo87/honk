use crate::conn::is_response;
use crate::render::Renderer;
use honk_wire::Cursor;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use super::parse::{busy_run_id, is_busy_error, is_lost_track, is_no_active_run, is_run_mismatch};
use super::types::{
    ChatOpts, Effect, InflightSteer, Input, Ownership, PendingPermission, PermissionOption,
    PromptSent, RunState,
};

pub struct ChatCore {
    session_id: String,
    opts: ChatOpts,
    renderer: Renderer,
    run_state: RunState,
    run_ownership: Ownership,
    our_crids: HashSet<String>,
    crid_counter: u64,
    next_req_id: u64,
    prompt_sent: Option<PromptSent>,
    held_steers: Vec<(String, u32)>,
    local_queue: VecDeque<(String, u32)>,
    pending_steer_reqs: HashMap<u64, InflightSteer>,
    pending_steers: HashMap<String, String>,
    next_perm_tag: u64,
    pending_permissions: Vec<PendingPermission>,
    last_printed_perm_tag: Option<String>,
    last_cursor: Option<Cursor>,
    last_activity: Instant,
    is_detached: bool,
    detached_held_line: Option<String>,
}

impl ChatCore {
    pub fn new(session_id: String, opts: ChatOpts, now: Instant) -> Self {
        let renderer = Renderer::new(opts.text_only, opts.thoughts);
        Self {
            session_id,
            opts,
            renderer,
            run_state: RunState::Unknown,
            run_ownership: Ownership::Unknown,
            our_crids: HashSet::new(),
            crid_counter: 0,
            next_req_id: 1,
            prompt_sent: None,
            held_steers: Vec::new(),
            local_queue: VecDeque::new(),
            pending_steer_reqs: HashMap::new(),
            pending_steers: HashMap::new(),
            next_perm_tag: 0,
            pending_permissions: Vec::new(),
            last_printed_perm_tag: None,
            last_cursor: None,
            last_activity: now,
            is_detached: false,
            detached_held_line: None,
        }
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

    pub fn prompt_sent(&self) -> Option<&PromptSent> {
        self.prompt_sent.as_ref()
    }

    pub fn local_queue(&self) -> &VecDeque<(String, u32)> {
        &self.local_queue
    }

    pub fn pending_permissions(&self) -> &[PendingPermission] {
        &self.pending_permissions
    }

    pub fn is_detached(&self) -> bool {
        self.is_detached
    }

    pub fn handle(&mut self, input: Input) -> Vec<Effect> {
        let mut effects = Vec::new();
        match input {
            Input::Line(line) => {
                self.last_activity = Instant::now();
                self.handle_line(line, &mut effects);
            }
            Input::Interrupt => {
                self.last_activity = Instant::now();
                self.handle_interrupt(&mut effects);
            }
            Input::Eof => {
                self.handle_eof(&mut effects);
            }
            Input::Tick(now) => {
                self.handle_tick(now, &mut effects);
            }
            Input::Frame(frame) => {
                self.handle_frame(frame, &mut effects);
            }
            Input::Connected => {
                self.last_activity = Instant::now();
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
            effects.push(Effect::Reattach);
            effects.push(Effect::Status("[honk] re-attaching...".into()));
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                self.detached_held_line = Some(line);
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
            } else if let RunState::Running(ref run_id) = self.run_state {
                let run_id = run_id.clone();
                self.send_steer(text, run_id, conversion_count, effects);
            } else {
                self.held_steers.push((text, conversion_count));
            }
            return;
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
        let prefix = self.opts.crid_prefix.as_deref().unwrap_or("chat");
        let crid = format!("{prefix}-{}", self.crid_counter);
        let req_id = self.next_req_id;
        self.next_req_id += 1;

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
        let req_id = self.next_req_id;
        self.next_req_id += 1;

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
        if !self.run_state.is_running()
            && self.prompt_sent.is_none()
            && let Some((next_text, conv_count)) = self.local_queue.pop_front()
        {
            self.send_prompt(next_text, conv_count, effects);
        }
    }

    fn handle_command(&mut self, line: &str, effects: &mut Vec<Effect>) -> bool {
        let trimmed = line.trim();
        if trimmed == "/help" {
            let help = "[honk] commands:\n  /help       - show this help\n  /quit       - detach and exit (session continues)\n  /cancel     - interrupt current turn (if own)\n  /cancel!    - cancel current turn regardless of owner\n  /drop       - discard locally queued messages\n  /rK N       - answer request rK with option N\n  /rK deny    - deny request rK (selects reject option or cancels)\n  /N          - shorthand for /rK N when single request pending\n  //text      - send literal '/text' as a prompt";
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

        // Shorthand /N
        if let Some(num_str) = trimmed.strip_prefix('/')
            && let Ok(n) = num_str.parse::<usize>()
        {
            if self.pending_permissions.len() == 1
                && self.last_printed_perm_tag.as_deref() == Some(&self.pending_permissions[0].tag)
            {
                let tag = self.pending_permissions[0].tag.clone();
                self.answer_permission_option(&tag, n, effects);
            } else {
                let mut msg =
                    "[honk] shorthand /N ambiguous; specify /rK N. Pending requests:\n".to_string();
                for perm in &self.pending_permissions {
                    msg.push_str(&format!("  [{}] {}\n", perm.tag, perm.tool_title));
                    for (i, opt) in perm.options.iter().enumerate() {
                        let num = i + 1;
                        msg.push_str(&format!("    {num}. {} ({})\n", opt.name, opt.kind));
                    }
                }
                effects.push(Effect::Status(msg.trim_end().to_string()));
            }
            return true;
        }

        // /rK ...
        if let Some(rest) = trimmed.strip_prefix("/r") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if !parts.is_empty() {
                let tag_candidate = parts[0];
                if tag_candidate.chars().all(|c| c.is_ascii_digit()) {
                    let tag = format!("r{tag_candidate}");
                    let arg = parts.get(1).copied();
                    if arg == Some("deny") {
                        self.answer_permission_deny(&tag, effects);
                        return true;
                    }
                    if let Some(arg_str) = arg
                        && let Ok(n) = arg_str.parse::<usize>()
                    {
                        self.answer_permission_option(&tag, n, effects);
                        return true;
                    }
                    effects.push(Effect::Status(format!(
                        "[honk] invalid arguments for /{tag}; usage: /{tag} N or /{tag} deny"
                    )));
                    return true;
                }
            }
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
        } else if self.run_state.is_running() || self.prompt_sent.is_some() {
            if self.run_ownership == Ownership::Own || self.prompt_sent.is_some() {
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
        } else {
            effects.push(Effect::Status("[honk] no active turn to cancel".into()));
        }
    }

    fn answer_permission_option(
        &mut self,
        tag: &str,
        opt_idx_1based: usize,
        effects: &mut Vec<Effect>,
    ) {
        let Some(idx) = self.pending_permissions.iter().position(|p| p.tag == tag) else {
            effects.push(Effect::Status(format!("[honk] no pending request [{tag}]")));
            return;
        };
        let perm = &self.pending_permissions[idx];
        if opt_idx_1based < 1 || opt_idx_1based > perm.options.len() {
            effects.push(Effect::Status(format!(
                "[honk] invalid option {opt_idx_1based} for [{tag}] (must be 1..{})",
                perm.options.len()
            )));
            return;
        }
        let perm = self.pending_permissions.remove(idx);
        let opt = &perm.options[opt_idx_1based - 1];
        effects.push(Effect::Respond {
            id: perm.goose_req_id,
            result: json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": opt.option_id
                }
            }),
        });
        effects.push(Effect::Status(format!(
            "[honk] answered [{tag}] with option {opt_idx_1based} ({})",
            opt.name
        )));
    }

    fn answer_permission_deny(&mut self, tag: &str, effects: &mut Vec<Effect>) {
        let Some(idx) = self.pending_permissions.iter().position(|p| p.tag == tag) else {
            effects.push(Effect::Status(format!("[honk] no pending request [{tag}]")));
            return;
        };
        let perm = self.pending_permissions.remove(idx);
        if let Some(opt) = perm.options.iter().find(|o| o.kind.starts_with("reject")) {
            effects.push(Effect::Respond {
                id: perm.goose_req_id,
                result: json!({
                    "outcome": {
                        "outcome": "selected",
                        "optionId": opt.option_id
                    }
                }),
            });
            effects.push(Effect::Status(format!(
                "[honk] denied [{tag}] ({})",
                opt.name
            )));
        } else {
            effects.push(Effect::Respond {
                id: perm.goose_req_id,
                result: json!({
                    "outcome": {
                        "outcome": "cancelled"
                    }
                }),
            });
            effects.push(Effect::Status(format!("[honk] cancelled [{tag}]")));
        }
    }

    fn handle_interrupt(&mut self, effects: &mut Vec<Effect>) {
        self.handle_cancel(false, effects);
    }

    fn handle_eof(&mut self, effects: &mut Vec<Effect>) {
        effects.push(Effect::Notify {
            method: "session/close".into(),
            params: json!({"sessionId": self.session_id}),
        });
        effects.push(Effect::Exit(0));
    }

    fn handle_tick(&mut self, now: Instant, effects: &mut Vec<Effect>) {
        if self.run_state.is_running() {
            self.last_activity = now;
        }

        if !self.is_detached
            && !self.run_state.is_running()
            && self.pending_permissions.is_empty()
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
                self.handle_response(id, &frame, effects);
            }
            return;
        }

        if frame.get("id").is_some() && frame.get("method").is_some() {
            let id = frame["id"].clone();
            let method = frame["method"].as_str().unwrap_or("").to_string();
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            self.handle_server_request(id, &method, &params, effects);
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
            if let Some(err) = frame.get("error") {
                if is_busy_error(err) {
                    if let Some(run_id) = busy_run_id(err) {
                        self.run_state = RunState::Running(run_id);
                    }
                    self.run_ownership = Ownership::Foreign;
                    self.local_queue
                        .push_front((prompt.text, prompt.conversion_count));
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
                self.check_queued_prompts(effects);
            }
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
            } else {
                if let Some(msg_id) = frame.pointer("/result/messageId").and_then(Value::as_str) {
                    self.pending_steers
                        .insert(msg_id.to_string(), inflight.text);
                }
                effects.push(Effect::Status("[honk] steer accepted".into()));
            }
        }
    }

    fn handle_server_request(
        &mut self,
        id: Value,
        method: &str,
        params: &Value,
        effects: &mut Vec<Effect>,
    ) {
        if method == "session/request_permission" || method == "request_permission" {
            self.next_perm_tag += 1;
            let tag = format!("r{}", self.next_perm_tag);
            let goose_req_id = id;

            let tool_title = params
                .pointer("/toolCall/title")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();

            let mut options = Vec::new();
            if let Some(opts_arr) = params.get("options").and_then(Value::as_array) {
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

            let mut msg = format!("[honk] permission request [{tag}]: {tool_title}\n");
            for (i, opt) in options.iter().enumerate() {
                let num = i + 1;
                msg.push_str(&format!("  {num}. {} ({})\n", opt.name, opt.kind));
            }
            let status_str = msg.trim_end().to_string();

            self.pending_permissions.push(PendingPermission {
                tag: tag.clone(),
                goose_req_id,
                tool_title,
                options,
            });
            self.last_printed_perm_tag = Some(tag);
            effects.push(Effect::Status(status_str));
        } else {
            effects.push(Effect::Status(format!(
                "[honk] unsupported request {method}; answer it from another client"
            )));
        }
    }

    fn handle_notification(&mut self, method: &str, frame: &Value, effects: &mut Vec<Effect>) {
        if let Some(cursor) = honk_wire::frame_cursor(frame) {
            self.last_cursor = Some(cursor);
        }

        match method {
            honk_wire::method::RESET => {
                let mut buf = Vec::new();
                let _ = self.renderer.handle_frame(frame, &mut buf);
                if !buf.is_empty() {
                    effects.push(Effect::Print(String::from_utf8_lossy(&buf).into_owned()));
                }
            }
            honk_wire::method::REPLAY_DONE => {
                if let (Some(epoch), Some(seq)) = (
                    frame.pointer("/params/epoch").and_then(Value::as_u64),
                    frame.pointer("/params/seq").and_then(Value::as_u64),
                ) {
                    self.last_cursor = Some(Cursor { epoch, seq });
                }
                let mut buf = Vec::new();
                let _ = self.renderer.handle_frame(frame, &mut buf);
                if !buf.is_empty() {
                    effects.push(Effect::Print(String::from_utf8_lossy(&buf).into_owned()));
                }
            }
            "session/update" => {
                if let Some(u) = frame.pointer("/params/update") {
                    if let Some(run_opt) = honk_wire::run_state(u) {
                        match run_opt {
                            Some(run_id) => {
                                self.run_state = RunState::Running(run_id.clone());
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
                            None => {
                                self.run_state = RunState::Idle;
                                self.run_ownership = Ownership::Unknown;
                                let mut unconsumed: Vec<(String, String)> =
                                    self.pending_steers.drain().collect();
                                unconsumed.sort_by(|a, b| a.0.cmp(&b.0));
                                for (_msg_id, steer_text) in unconsumed {
                                    effects.push(Effect::Status(format!(
                                        "[honk] steer not consumed: {steer_text}"
                                    )));
                                }
                                self.check_queued_prompts(effects);
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

                let mut buf = Vec::new();
                let _ = self.renderer.handle_frame(frame, &mut buf);
                if !buf.is_empty() {
                    effects.push(Effect::Print(String::from_utf8_lossy(&buf).into_owned()));
                }
            }
            honk_wire::method::REQUEST_RESOLVED => {
                let goose_req_id = frame.pointer("/params/requestId").unwrap_or(&Value::Null);
                if let Some(pos) = self
                    .pending_permissions
                    .iter()
                    .position(|p| p.goose_req_id == *goose_req_id)
                {
                    let perm = self.pending_permissions.remove(pos);
                    let reason = frame
                        .pointer("/params/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("resolved");
                    effects.push(Effect::Status(format!(
                        "[honk] permission request [{}] resolved: {reason}",
                        perm.tag
                    )));
                }
            }
            honk_wire::method::STEER_UNCONSUMED => {
                if let Some(mid) = frame.pointer("/params/messageId").and_then(Value::as_str)
                    && let Some(text) = self.pending_steers.remove(mid)
                {
                    effects.push(Effect::Status(format!("[honk] steer not consumed: {text}")));
                }
            }
            honk_wire::method::GAP => {
                effects.push(Effect::Status(
                    "[honk] frame gap detected; transcript will reset at turn end".into(),
                ));
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
        if let Some(p) = self.prompt_sent.as_mut() {
            let req_id = self.next_req_id;
            self.next_req_id += 1;
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
        if let Some(line) = self.detached_held_line.take() {
            self.handle_line(line, effects);
        }
    }

    fn handle_disconnected(&mut self, effects: &mut Vec<Effect>) {
        effects.push(Effect::Status(
            "[honk] disconnected; reconnecting...".into(),
        ));
        self.pending_permissions.clear();
        self.last_printed_perm_tag = None;
        for (_req_id, inflight) in self.pending_steer_reqs.drain() {
            effects.push(Effect::Status(format!(
                "[honk] steer may not have been delivered: {}",
                inflight.text
            )));
        }
    }
}
