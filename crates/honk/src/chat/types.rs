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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
    Returned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: String,
    pub returned_text: Option<String>,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Info,
            text: text.into(),
            returned_text: None,
        }
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Warning,
            text: text.into(),
            returned_text: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Error,
            text: text.into(),
            returned_text: None,
        }
    }

    pub fn returned(text: impl Into<String>, returned: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Returned,
            text: text.into(),
            returned_text: Some(returned.into()),
        }
    }
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
    Notice(Notice),
    Attached {
        result: Value,
    },
    AttachFailed {
        error: Value,
    },
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
    Pending,
    Foreign,
}

#[derive(Clone, Debug)]
pub struct ChatOpts {
    pub crid_seed: u64,
    pub idle_timeout: Option<Duration>,
    pub max_turns: Option<u32>,
}

impl Default for ChatOpts {
    fn default() -> Self {
        Self {
            crid_seed: 0,
            idle_timeout: Some(Duration::from_secs(30 * 60)),
            max_turns: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct IdAlloc {
    next_id: u64,
}

impl IdAlloc {
    pub fn new() -> Self {
        Self { next_id: 1 }
    }

    pub fn allocate(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

#[derive(Clone, Debug)]
pub struct PromptSent {
    pub crid: String,
    pub text: String,
    pub req_id: u64,
    pub run_seen: bool,
    pub conversion_count: u32,
    pub is_command: bool,
}

#[derive(Clone, Debug)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Clone, Debug)]
pub struct PendingPermission {
    pub goose_req_id: Value,
    pub tool_title: String,
    pub options: Vec<PermissionOption>,
}

#[derive(Clone, Debug)]
pub(crate) struct InflightSteer {
    pub text: String,
    pub expected_run_id: String,
    pub conversion_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingMcpReq {
    List { explicit: bool },
    Enable { names: Vec<String> },
    Disable { names: Vec<String> },
}
