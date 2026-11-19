//! User prompt kind classification and trigger types.
use serde::{Deserialize, Serialize};

/// What started a turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    /// Blocks before the first user block.
    Start,
    /// A stall-watch wake: `[sw wake run=... reason=X] ...`.
    Wake(String),
    /// A swarm message: `<swarm_message ... from="X" ...>`.
    Swarm(String),
    /// A swarm message the session scheduled for itself (`scheduled_for=` attribute).
    Scheduled,
    /// A prompt goose wrote itself (self-compaction resume, final_output nudge).
    Goose,
    /// Anything else: the human.
    You,
}

impl Trigger {
    pub fn label(&self) -> String {
        match self {
            Trigger::Start => "start".into(),
            Trigger::Wake(r) => format!("wake: {r}"),
            Trigger::Swarm(f) => format!("swarm: {f}"),
            Trigger::Scheduled => "scheduled".into(),
            Trigger::Goose => "goose".into(),
            Trigger::You => "you".into(),
        }
    }
}

/// Classifies a user block's text.
pub fn classify(text: &str) -> Trigger {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix("[sw wake ") {
        let header = rest.split(']').next().unwrap_or("");
        let reason = header
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix("reason="))
            .filter(|r| !r.is_empty())
            .unwrap_or("wake");
        return Trigger::Wake(reason.to_string());
    }
    if t.starts_with("<swarm_message ") {
        let header = t.split('>').next().unwrap_or("");
        if header.contains(" scheduled_for=") {
            return Trigger::Scheduled;
        }
        let from = header
            .split(" from=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .filter(|s| !s.is_empty())
            .unwrap_or("?");
        return Trigger::Swarm(from.to_string());
    }
    if t.starts_with("You are goose session ") || t.starts_with("You MUST call the `final_output`")
    {
        return Trigger::Goose;
    }
    if t.starts_with("{\"program\"") {
        return Trigger::Wake("wake".into());
    }
    Trigger::You
}

/// Returns true iff `classify(text) == Trigger::You`.
pub fn is_human_prompt(text: &str) -> bool {
    classify(text) == Trigger::You
}
