use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Line(String),
    Interrupt,
    Eof,
    Tick(Instant),
    Frame(Value),
    Connected,
    Disconnected,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Request {
        id: u64,
        method: String,
        params: Value,
    },
    Notify {
        method: String,
        params: Value,
    },
    Respond {
        id: Value,
        result: Value,
    },
    Print(String),
    Status(String),
    Detach,
    Reattach,
    Exit(i32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Unknown,
    Idle,
    Running(String),
}

impl RunState {
    pub fn is_running(&self) -> bool {
        matches!(self, RunState::Running(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    Unknown,
    Own,
    Foreign,
}

#[derive(Clone, Debug)]
pub struct ChatOpts {
    pub text_only: bool,
    pub thoughts: bool,
    pub idle_timeout: Duration,
    pub crid_prefix: Option<String>,
}

impl Default for ChatOpts {
    fn default() -> Self {
        Self {
            text_only: false,
            thoughts: false,
            idle_timeout: Duration::from_secs(30 * 60),
            crid_prefix: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PromptSent {
    pub crid: String,
    pub text: String,
    pub req_id: u64,
    pub run_seen: bool,
    pub conversion_count: u32,
}

#[derive(Clone, Debug)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Clone, Debug)]
pub struct PendingPermission {
    pub tag: String,
    pub goose_req_id: Value,
    pub tool_title: String,
    pub options: Vec<PermissionOption>,
}

#[derive(Clone, Debug)]
pub(crate) struct InflightSteer {
    pub text: String,
    #[allow(dead_code)]
    pub expected_run_id: String,
    pub conversion_count: u32,
}
