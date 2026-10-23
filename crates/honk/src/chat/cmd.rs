use super::core::ChatCore;
use super::types::{Effect, Notice, Ownership, RunState};
use serde_json::json;

impl ChatCore {
    pub(crate) fn handle_command(&mut self, line: &str, effects: &mut Vec<Effect>) -> bool {
        let trimmed = line.trim();
        if trimmed == "/help" {
            let help = "commands:\n  /help       - show this help\n  /quit       - detach and exit (session continues)\n  /cancel     - interrupt current turn (if own)\n  /cancel!    - cancel current turn regardless of owner\n  /drop       - discard locally queued messages\n  /approve    - approve oldest pending permission request\n  /turns      - set or clear turn cap (/turns N or /turns off)\n  /compact    - compact session context\n  /mcp        - MCP servers: /mcp list, /mcp enable <names>, /mcp disable <names>; servers marked ! need a trailing ! to confirm (/mcp enable slack!); honkd decides which via --mcp-no-confirm\n  //text      - send literal '/text' as a prompt";
            effects.push(Effect::Notice(Notice::info(help)));
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
            let count = self.local_queue.len()
                + self.held_steers.len()
                + self.detached_buffered_lines.len();
            self.local_queue.clear();
            self.held_steers.clear();
            self.detached_buffered_lines.clear();
            effects.push(Effect::Notice(Notice::info(format!(
                "dropped {count} queued message(s)"
            ))));
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

        if trimmed == "/approve" {
            self.handle_approve_command(effects);
            return true;
        }

        if trimmed == "/turns" || trimmed.starts_with("/turns ") {
            let arg = trimmed.strip_prefix("/turns").unwrap().trim();
            if arg.is_empty() {
                let status = match self.opts.max_turns {
                    Some(n) => format!(
                        "max_turns is currently {n} (use /turns N to change, /turns off to clear)"
                    ),
                    None => "max_turns is currently disabled (use /turns N to set)".to_string(),
                };
                effects.push(Effect::Notice(Notice::info(status)));
                return true;
            }
            if arg == "off" {
                self.opts.max_turns = None;
                effects.push(Effect::Notice(Notice::info("max_turns disabled")));
                return true;
            }
            match arg.parse::<u32>() {
                Ok(n) if n >= 1 => {
                    self.opts.max_turns = Some(n);
                    effects.push(Effect::Notice(Notice::info(format!(
                        "max_turns set to {n}"
                    ))));
                }
                _ => {
                    effects.push(Effect::Notice(Notice::warning(
                        "invalid /turns argument: must be a positive integer (>= 1) or 'off'",
                    )));
                }
            }
            return true;
        }

        if trimmed == "/compact" || trimmed.starts_with("/compact ") {
            let arg = trimmed.strip_prefix("/compact").unwrap().trim();
            if !arg.is_empty() {
                effects.push(Effect::Notice(Notice::warning(
                    "/compact takes no arguments",
                )));
                return true;
            }

            if self.buffer_if_detached_or_replaying("/compact".to_string(), effects) {
                return true;
            }

            if self.prompt_sent.is_some()
                || self.run_state.is_running()
                || !self.local_queue.is_empty()
            {
                self.local_queue.push_back(("/compact".to_string(), 0));
                effects.push(Effect::Notice(Notice::info(
                    "compaction queued; it runs after the current turn ends",
                )));
            } else {
                self.send_prompt("/compact".to_string(), 0, effects);
            }
            return true;
        }

        if trimmed == "/mcp" || trimmed.starts_with("/mcp ") {
            self.handle_mcp_command(trimmed, effects);
            return true;
        }

        effects.push(Effect::Notice(Notice::warning(format!(
            "unknown command: '{trimmed}'; type /help for commands"
        ))));
        true
    }

    pub(crate) fn handle_mcp_command(&mut self, line: &str, effects: &mut Vec<Effect>) {
        self.handle_mcp_command_inner(line, None, effects);
    }

    pub(crate) fn handle_queued_mcp_command(
        &mut self,
        line: &str,
        requeues: u32,
        effects: &mut Vec<Effect>,
    ) {
        self.handle_mcp_command_inner(line, Some((line.to_string(), requeues)), effects);
    }

    fn handle_mcp_command_inner(
        &mut self,
        line: &str,
        queued: Option<(String, u32)>,
        effects: &mut Vec<Effect>,
    ) {
        let rest = line.strip_prefix("/mcp").unwrap().trim();
        if rest.is_empty() {
            self.dispatch_mcp_list(line, effects);
            return;
        }

        let (subcmd, args_str) = match rest.split_once(char::is_whitespace) {
            Some((cmd, args)) => (cmd, args.trim()),
            None => (rest, ""),
        };

        match subcmd {
            "list" => {
                if !args_str.is_empty() {
                    effects.push(Effect::Notice(Notice::warning("usage: /mcp list")));
                    return;
                }
                self.dispatch_mcp_list(line, effects);
            }
            "enable" | "on" | "on!" => {
                if args_str.is_empty() {
                    effects.push(Effect::Notice(Notice::warning(
                        "usage: /mcp enable <name>... (append ! to confirm a server marked !, e.g. /mcp enable slack!)",
                    )));
                    return;
                }
                let (names, confirm) = match crate::swarm_mcp::parse_enable_names(
                    args_str.split_whitespace(),
                ) {
                    Ok((n, mut c)) => {
                        if subcmd == "on!" {
                            c = n.clone();
                        }
                        (n, c)
                    }
                    Err(_) => {
                        effects.push(Effect::Notice(Notice::warning(
                                "usage: /mcp enable <name>... (append ! to confirm a server marked !, e.g. /mcp enable slack!)",
                            )));
                        return;
                    }
                };
                self.dispatch_mcp_enable(line, queued, names, confirm, effects);
            }
            "disable" | "off" => {
                if args_str.is_empty() {
                    effects.push(Effect::Notice(Notice::warning(
                        "usage: /mcp disable <name>...",
                    )));
                    return;
                }
                // `!` means nothing for disable; strip it so `/mcp disable slack!` works.
                let names = match crate::swarm_mcp::strip_disable_names(args_str.split_whitespace())
                {
                    Ok(n) => n,
                    Err(_) => {
                        effects.push(Effect::Notice(Notice::warning(
                            "usage: /mcp disable <name>...",
                        )));
                        return;
                    }
                };
                self.dispatch_mcp_disable(line, queued, names, effects);
            }
            _ => {
                effects.push(Effect::Notice(Notice::warning(format!(
                    "unknown /mcp subcommand '{subcmd}'; use /mcp list, /mcp enable, or /mcp disable"
                ))));
            }
        }
    }

    fn dispatch_mcp_list(&mut self, line: &str, effects: &mut Vec<Effect>) {
        if self.buffer_if_detached_or_replaying(line.to_string(), effects) {
            return;
        }
        let req_id = self.allocate_id();
        self.pending_mcp_reqs
            .insert(req_id, super::types::PendingMcpReq::List { explicit: true });
        effects.push(Effect::Request {
            id: req_id,
            method: "_honk/mcp/list".to_string(),
            params: json!({ "sessionId": self.session_id }),
        });
    }

    /// A fresh /mcp mutation waits behind a running turn or anything already queued. One
    /// popped off the queue (`queued_dispatch`) was already ordered; checking the queue again
    /// would push it behind the lines that followed it, and nothing re-runs the queue after
    /// that, so it would stall.
    fn must_queue_mcp(&self, queued_dispatch: bool) -> bool {
        self.prompt_sent.is_some()
            || self.run_state.is_running()
            || (!queued_dispatch && !self.local_queue.is_empty())
    }

    fn dispatch_mcp_enable(
        &mut self,
        line: &str,
        queued: Option<(String, u32)>,
        names: Vec<String>,
        confirm: Vec<String>,
        effects: &mut Vec<Effect>,
    ) {
        if self.buffer_if_detached_or_replaying(line.to_string(), effects) {
            return;
        }

        if self.must_queue_mcp(queued.is_some()) {
            self.local_queue.push_back((line.to_string(), 0));
            effects.push(Effect::Notice(Notice::info(format!("queued: {line}"))));
        } else {
            let req_id = self.allocate_id();
            self.pending_mcp_reqs.insert(
                req_id,
                super::types::PendingMcpReq::Enable {
                    names: names.clone(),
                    queued,
                },
            );
            effects.push(Effect::Request {
                id: req_id,
                method: "_honk/mcp/enable".to_string(),
                params: json!({
                    "sessionId": self.session_id,
                    "names": names,
                    "confirm": confirm,
                }),
            });
        }
    }

    fn dispatch_mcp_disable(
        &mut self,
        line: &str,
        queued: Option<(String, u32)>,
        names: Vec<String>,
        effects: &mut Vec<Effect>,
    ) {
        if self.buffer_if_detached_or_replaying(line.to_string(), effects) {
            return;
        }

        if self.must_queue_mcp(queued.is_some()) {
            self.local_queue.push_back((line.to_string(), 0));
            effects.push(Effect::Notice(Notice::info(format!("queued: {line}"))));
        } else {
            let req_id = self.allocate_id();
            self.pending_mcp_reqs.insert(
                req_id,
                super::types::PendingMcpReq::Disable {
                    names: names.clone(),
                    queued,
                },
            );
            effects.push(Effect::Request {
                id: req_id,
                method: "_honk/mcp/disable".to_string(),
                params: json!({
                    "sessionId": self.session_id,
                    "names": names,
                }),
            });
        }
    }

    pub(crate) fn handle_approve_command(&mut self, effects: &mut Vec<Effect>) {
        if self.pending_permissions.is_empty() {
            effects.push(Effect::Notice(Notice::warning(
                "no pending permission requests to approve",
            )));
            return;
        }

        let perm = self.pending_permissions.remove(0);
        let id_key = perm.goose_req_id.to_string();
        let chosen = perm
            .options
            .iter()
            .find(|o| o.kind == "allow_once")
            .or_else(|| perm.options.iter().find(|o| o.kind.starts_with("allow")))
            .or_else(|| perm.options.first());

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
            id: perm.goose_req_id,
            result,
        });
        effects.push(Effect::Notice(Notice::info(format!(
            "approved: {}",
            perm.tool_title
        ))));
    }

    pub(crate) fn handle_permission_request(
        &mut self,
        frame: &serde_json::Value,
        effects: &mut Vec<Effect>,
    ) {
        let goose_req_id = frame["id"].clone();
        let id_key = goose_req_id.to_string();

        let tool_title = frame
            .pointer("/params/toolCall/title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("tool")
            .to_string();

        if let Some(cached_result) = self.answered_permissions.get(&id_key) {
            self.pending_permissions
                .retain(|p| p.goose_req_id != goose_req_id);
            effects.push(Effect::Respond {
                id: goose_req_id,
                result: cached_result.clone(),
            });
            effects.push(Effect::Notice(Notice::info(format!(
                "auto-approved (re-offered): {tool_title}"
            ))));
            return;
        }

        let mut options = Vec::new();
        if let Some(opts_arr) = frame
            .pointer("/params/options")
            .and_then(serde_json::Value::as_array)
        {
            for opt in opts_arr {
                let option_id = opt
                    .get("optionId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = opt
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(&option_id)
                    .to_string();
                let kind = opt
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                options.push(super::types::PermissionOption {
                    option_id,
                    name,
                    kind,
                });
            }
        }

        let is_own_turn = self.run_ownership == Ownership::Own;

        if is_own_turn {
            let chosen = options
                .iter()
                .find(|o| o.kind == "allow_once")
                .or_else(|| options.iter().find(|o| o.kind.starts_with("allow")))
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

            self.pending_permissions
                .retain(|p| p.goose_req_id != goose_req_id);
            self.answered_permissions.insert(id_key, result.clone());
            effects.push(Effect::Respond {
                id: goose_req_id,
                result,
            });
            effects.push(Effect::Notice(Notice::info(format!(
                "auto-approved: {tool_title}"
            ))));
        } else if !self
            .pending_permissions
            .iter()
            .any(|p| p.goose_req_id == goose_req_id)
        {
            self.pending_permissions
                .push(super::types::PendingPermission {
                    goose_req_id,
                    tool_title: tool_title.clone(),
                    options,
                });
            effects.push(Effect::Notice(Notice::info(format!(
                "permission request from another client's turn: {tool_title}"
            ))));
        }
    }

    pub(crate) fn handle_cancel(&mut self, force: bool, effects: &mut Vec<Effect>) {
        if force {
            effects.push(Effect::Notify {
                method: "session/cancel".into(),
                params: json!({"sessionId": self.session_id}),
            });
            effects.push(Effect::Notice(Notice::info("cancelling turn...")));
            return;
        }

        if self.is_replaying {
            effects.push(Effect::Notice(Notice::warning(
                "this turn belongs to another client; /cancel! cancels it anyway",
            )));
            return;
        }

        if self.run_ownership == Ownership::Own {
            effects.push(Effect::Notify {
                method: "session/cancel".into(),
                params: json!({"sessionId": self.session_id}),
            });
            effects.push(Effect::Notice(Notice::info("cancelling turn...")));
        } else if self.run_state.is_running() || self.prompt_sent.is_some() {
            effects.push(Effect::Notice(Notice::warning(
                "this turn belongs to another client; /cancel! cancels it anyway",
            )));
        } else if matches!(self.run_state, RunState::Unknown) {
            // Run state is not known yet (e.g. right after a reset/attach): a run
            // may be active, so refuse rather than claim there is nothing to cancel.
            effects.push(Effect::Notice(Notice::warning(
                "turn state unknown; /cancel! cancels any active turn anyway",
            )));
        } else {
            effects.push(Effect::Notice(Notice::warning("no active turn to cancel")));
        }
    }
}
