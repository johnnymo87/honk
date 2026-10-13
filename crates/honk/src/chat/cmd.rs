use super::core::ChatCore;
use super::types::{Effect, Notice, Ownership};
use serde_json::json;

impl ChatCore {
    pub(crate) fn handle_command(&mut self, line: &str, effects: &mut Vec<Effect>) -> bool {
        let trimmed = line.trim();
        if trimmed == "/help" {
            let help = "[honk] commands:\n  /help       - show this help\n  /quit       - detach and exit (session continues)\n  /cancel     - interrupt current turn (if own)\n  /cancel!    - cancel current turn regardless of owner\n  /drop       - discard locally queued messages\n  /approve    - approve oldest pending permission request\n  //text      - send literal '/text' as a prompt";
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
            let count = self.local_queue.len();
            self.local_queue.clear();
            effects.push(Effect::Notice(Notice::info(format!(
                "[honk] dropped {count} queued message(s)"
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

        effects.push(Effect::Notice(Notice::warning(format!(
            "[honk] unknown command: '{trimmed}'; type /help for commands"
        ))));
        true
    }

    pub(crate) fn handle_approve_command(&mut self, effects: &mut Vec<Effect>) {
        if self.pending_permissions.is_empty() {
            effects.push(Effect::Notice(Notice::warning(
                "[honk] no pending permission requests to approve",
            )));
            return;
        }

        let perm = self.pending_permissions.remove(0);
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

        effects.push(Effect::Respond {
            id: perm.goose_req_id,
            result,
        });
        effects.push(Effect::Notice(Notice::info(format!(
            "[honk] approved: {}",
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
            if let Some(cached_result) = self.answered_permissions.get(&id_key) {
                effects.push(Effect::Respond {
                    id: goose_req_id,
                    result: cached_result.clone(),
                });
                effects.push(Effect::Notice(Notice::info(format!(
                    "[honk] auto-approved (re-offered): {tool_title}"
                ))));
                return;
            }

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

            self.answered_permissions.insert(id_key, result.clone());
            effects.push(Effect::Respond {
                id: goose_req_id,
                result,
            });
            effects.push(Effect::Notice(Notice::info(format!(
                "[honk] auto-approved: {tool_title}"
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
                "[honk] permission request from another client's turn: {tool_title}"
            ))));
        }
    }

    pub(crate) fn handle_cancel(&mut self, force: bool, effects: &mut Vec<Effect>) {
        if force {
            effects.push(Effect::Notify {
                method: "session/cancel".into(),
                params: json!({"sessionId": self.session_id}),
            });
            effects.push(Effect::Notice(Notice::info("[honk] cancelling turn...")));
            return;
        }

        if self.run_state.is_running() {
            if self.run_ownership == Ownership::Own {
                effects.push(Effect::Notify {
                    method: "session/cancel".into(),
                    params: json!({"sessionId": self.session_id}),
                });
                effects.push(Effect::Notice(Notice::info("[honk] cancelling turn...")));
            } else {
                effects.push(Effect::Notice(Notice::warning(
                    "[honk] this turn belongs to another client; /cancel! cancels it anyway",
                )));
            }
        } else if self.prompt_sent.is_some() {
            effects.push(Effect::Notify {
                method: "session/cancel".into(),
                params: json!({"sessionId": self.session_id}),
            });
            effects.push(Effect::Notice(Notice::info("[honk] cancelling turn...")));
        } else {
            effects.push(Effect::Notice(Notice::warning(
                "[honk] no active turn to cancel",
            )));
        }
    }
}
