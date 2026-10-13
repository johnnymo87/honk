use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Line(String),
    Interrupt,
    Eof,
    Tick(std::time::Instant),
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
    Transcript(Value),
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
    pub crid_seed: u64,
    pub idle_timeout: Duration,
}

impl Default for ChatOpts {
    fn default() -> Self {
        Self {
            crid_seed: 0,
            idle_timeout: Duration::from_secs(30 * 60),
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
pub(crate) struct InflightSteer {
    pub text: String,
    pub expected_run_id: String,
    pub conversion_count: u32,
}
