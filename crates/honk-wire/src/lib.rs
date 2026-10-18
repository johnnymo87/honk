//! Raw-JSON helpers shared by honkd and honk. No ACP types (design §4).
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
pub mod export;

pub const PROTOCOL: u32 = 1;
pub const FEATURE_RESUME_ONLY: &str = "resume_only";
pub const ERR_UNKNOWN_CLIENT_REQ_ID: i64 = -32004;

pub mod method {
    pub const RESET: &str = "_honk/reset";
    pub const REPLAY_DONE: &str = "_honk/replay_done";
    pub const REQUEST_RESOLVED: &str = "_honk/request_resolved";
    pub const STEER_UNCONSUMED: &str = "_honk/steer_unconsumed";
    pub const GAP: &str = "_honk/gap";
    pub const EVICTED: &str = "_honk/evicted";
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum BlockKey {
    User(String),
    Text(String),
    Thought(String),
    Tool(String),
    Notice(String),
}

pub fn block_key(update: &Value) -> Option<BlockKey> {
    let kind = update.get("sessionUpdate")?.as_str()?;
    let msg = || {
        update
            .get("messageId")
            .or_else(|| update.pointer("/_meta/goose/messageId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match kind {
        "user_message_chunk" => msg().map(BlockKey::User),
        "agent_message_chunk" => msg().map(BlockKey::Text),
        "agent_thought_chunk" => msg().map(BlockKey::Thought),
        "tool_call" | "tool_call_update" => update
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(|s| BlockKey::Tool(s.into())),
        _ => None,
    }
}

/// Some(Some(run)) = started, Some(None) = ended, None = not a run-state update
/// (e.g. `_meta.goose.queuedSteer` info updates carry no activeRunId key).
pub fn run_state(update: &Value) -> Option<Option<String>> {
    if update.get("sessionUpdate")?.as_str()? != "session_info_update" {
        return None;
    }
    match update
        .pointer("/_meta/goose")?
        .as_object()?
        .get("activeRunId")?
    {
        Value::Null => Some(None),
        Value::String(s) => Some(Some(s.clone())),
        _ => None,
    }
}
pub fn text_of(update: &Value) -> Option<&str> {
    update.pointer("/content/text").and_then(Value::as_str)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub used: u64,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cost: Option<Cost>,
}

pub fn usage(update: &Value) -> Option<Usage> {
    if update.get("sessionUpdate")?.as_str()? != "usage_update" {
        return None;
    }
    let used = update.get("used")?.as_u64()?;
    let size = update.get("size").and_then(Value::as_u64).unwrap_or(0);
    let cost = update.get("cost").and_then(|c| {
        let amount = c.get("amount")?.as_f64()?;
        if !amount.is_finite() {
            return None;
        }
        let currency = c.get("currency")?.as_str()?;
        if currency.is_empty() {
            return None;
        }
        Some(Cost {
            amount,
            currency: currency.to_string(),
        })
    });
    Some(Usage { used, size, cost })
}

/// epoch = upstream connection generation for the session (not honkd boot).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub epoch: u64,
    pub seq: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HonkMeta {
    #[serde(default)]
    pub client_req_id: Option<String>,
    #[serde(default)]
    pub after: Option<Cursor>,
    #[serde(default)]
    pub resume_only: bool,
}
pub fn honk_meta(params: &Value) -> HonkMeta {
    params
        .pointer("/_meta/honk")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}
/// Returns true if `/_meta/honk/resume_only` is present and not JSON `false`.
pub fn resume_only_requested(params: &Value) -> bool {
    match params.pointer("/_meta/honk/resume_only") {
        None => false,
        Some(Value::Bool(false)) => false,
        Some(_) => true,
    }
}
/// Returns the client request id if `/_meta/honk/client_req_id` is a string.
pub fn client_req_id(params: &Value) -> Option<&str> {
    params
        .pointer("/_meta/honk/client_req_id")
        .and_then(Value::as_str)
}
pub fn frame_cursor(frame: &Value) -> Option<Cursor> {
    frame
        .pointer("/params/_meta/honk")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Minimal transcript reducer (superseded by honk-transcript in M2).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Blocks {
    pub order: Vec<BlockKey>,
    pub text: HashMap<BlockKey, String>,
    pub tool_title: HashMap<String, String>,
    pub tool_status: HashMap<String, String>,
    pub cursor: Option<Cursor>,
    pub active_run: Option<String>,
    pub resets: u32,
}
impl Blocks {
    pub fn apply(&mut self, frame: &Value) {
        match frame.get("method").and_then(Value::as_str) {
            Some(method::RESET) => {
                let resets = self.resets + 1;
                *self = Blocks {
                    resets,
                    ..Default::default()
                };
            }
            Some("session/update") => {
                if let Some(c) = frame_cursor(frame) {
                    self.cursor = Some(c);
                }
                let Some(u) = frame.pointer("/params/update") else {
                    return;
                };
                if let Some(rs) = run_state(u) {
                    self.active_run = rs;
                }
                let Some(key) = block_key(u) else { return };
                if self.order.last() != Some(&key) && !self.order.contains(&key) {
                    self.order.push(key.clone());
                }
                match &key {
                    BlockKey::Tool(id) => {
                        if let Some(t) = u.get("title").and_then(Value::as_str) {
                            self.tool_title.insert(id.clone(), t.into());
                        }
                        if let Some(s) = u.get("status").and_then(Value::as_str) {
                            self.tool_status.insert(id.clone(), s.into());
                        }
                    }
                    _ => {
                        if let Some(t) = text_of(u) {
                            self.text.entry(key).or_default().push_str(t);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    pub fn agent_text(&self) -> String {
        self.order
            .iter()
            .filter(|k| matches!(k, BlockKey::Text(_)))
            .filter_map(|k| self.text.get(k))
            .cloned()
            .collect()
    }
    pub fn user_texts(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|k| matches!(k, BlockKey::User(_)))
            .filter_map(|k| self.text.get(k))
            .cloned()
            .collect()
    }
}
