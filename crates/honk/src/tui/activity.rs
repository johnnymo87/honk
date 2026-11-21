//! The status bar's activity indicator: an animated spinner, what the session is
//! doing, and how long the current turn has run (`[⠹ thinking 12s]`).
//!
//! Everything here is derived in the TUI from the driver's [`StateSnapshot`] and
//! the transcript, so every attached view shows it and nothing goes on the wire.

use std::time::{Duration, Instant};

use ratatui::style::Color;

use crate::chat::driver::StateSnapshot;
use crate::chat::types::Ownership;
use crate::tui::transcript::{BlockKind, Transcript};

/// Braille spinner, as opencode's transcript spinner.
pub const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Time per spinner frame.
pub const FRAME_MS: u128 = 100;
/// Shown instead of the spinner when animation is off (`HONK_NO_ANIM`).
pub const STATIC_GLYPH: &str = "⋯";
/// Longest tool title shown, in columns, before it is cut with `…`.
const TOOL_LABEL_MAX: usize = 32;

/// What a busy session is doing, most specific first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Another client's turn is waiting on a permission answer.
    NeedsApproval,
    /// A local `/compact`.
    Compacting,
    /// Our prompt is sent and its turn has not visibly started.
    Waiting,
    Thinking,
    Replying,
    /// A tool call in progress, labelled with its title.
    Tool(String),
    /// Running, but nothing more specific is known (e.g. between tool calls).
    Working,
}

impl Phase {
    pub fn label(&self) -> String {
        match self {
            Phase::NeedsApproval => "needs approval".into(),
            Phase::Compacting => "compacting".into(),
            Phase::Waiting => "waiting".into(),
            Phase::Thinking => "thinking".into(),
            Phase::Replying => "replying".into(),
            Phase::Tool(title) => truncate(title, TOOL_LABEL_MAX),
            Phase::Working => "working".into(),
        }
    }

    pub fn color(&self, foreign: bool) -> Color {
        match self {
            Phase::NeedsApproval => Color::LightRed,
            Phase::Compacting => Color::LightBlue,
            Phase::Waiting => Color::Cyan,
            _ if foreign => Color::Yellow,
            _ => Color::Green,
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let s = s.lines().next().unwrap_or("").trim();
    if unicode_width::UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Whether the snapshot describes a session that is doing something.
pub fn is_busy(s: &StateSnapshot) -> bool {
    s.run_state.is_running() || s.prompt_in_flight || s.compacting || s.awaiting_permission
}

/// What the session is doing, or `None` when idle.
pub fn phase(snapshot: Option<&StateSnapshot>, transcript: &Transcript) -> Option<Phase> {
    let s = snapshot?;
    if !is_busy(s) {
        return None;
    }
    if s.awaiting_permission {
        return Some(Phase::NeedsApproval);
    }
    if s.compacting {
        return Some(Phase::Compacting);
    }
    if s.ownership == Ownership::Pending || !s.run_state.is_running() {
        return Some(Phase::Waiting);
    }
    let last = transcript
        .blocks
        .iter()
        .rev()
        .find(|b| b.kind != BlockKind::Notice);
    Some(match last {
        Some(b) if b.kind == BlockKind::Thought => Phase::Thinking,
        Some(b) if b.kind == BlockKind::Assistant => Phase::Replying,
        Some(b)
            if b.kind == BlockKind::Tool
                && matches!(b.status.as_str(), "" | "pending" | "in_progress") =>
        {
            Phase::Tool(if b.title.is_empty() {
                "tool".into()
            } else {
                b.title.clone()
            })
        }
        _ => Phase::Working,
    })
}

/// `12s`, `3m05s`, `1h04m`.
pub fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// When the current busy stretch began.
#[derive(Clone, Debug, Default)]
pub struct Clock {
    started: Option<Instant>,
    /// The stretch was first seen during a replay, i.e. we attached mid-turn,
    /// so `started` is attach time and the transcript may know better.
    joined_mid_turn: bool,
}

impl Clock {
    /// Feed every new snapshot.
    pub fn observe(&mut self, snapshot: &StateSnapshot, now: Instant) {
        if is_busy(snapshot) {
            if self.started.is_none() {
                self.started = Some(now);
                self.joined_mid_turn = snapshot.replaying;
            }
        } else {
            *self = Self::default();
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Time since the busy stretch began. When we attached mid-turn, the
    /// newest user message's server timestamp (`now_unix`, Unix seconds) is
    /// used if it is older than the attach.
    pub fn elapsed(&self, now: Instant, transcript: &Transcript, now_unix: i64) -> Duration {
        let Some(started) = self.started else {
            return Duration::ZERO;
        };
        let local = now.saturating_duration_since(started);
        if !self.joined_mid_turn {
            return local;
        }
        let from_echo = transcript
            .blocks
            .iter()
            .rev()
            .find(|b| b.kind == BlockKind::User)
            .and_then(|b| b.created)
            .and_then(|c| u64::try_from(now_unix - c).ok())
            .map(Duration::from_secs);
        match from_echo {
            // A day-old echo is not this turn's.
            Some(d) if d > local && d < Duration::from_secs(86_400) => d,
            _ => local,
        }
    }
}

/// The spinner glyph for this instant.
pub fn glyph(animate: bool, elapsed: Duration) -> &'static str {
    if !animate {
        return STATIC_GLYPH;
    }
    FRAMES[(elapsed.as_millis() / FRAME_MS) as usize % FRAMES.len()]
}

/// The rendered indicator: `⠹ thinking 12s`, plus `· other client` for a turn
/// another client started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Indicator {
    pub text: String,
    pub color: Color,
}

pub fn indicator(
    snapshot: Option<&StateSnapshot>,
    transcript: &Transcript,
    clock: &Clock,
    animate: bool,
    now: Instant,
    now_unix: i64,
) -> Option<Indicator> {
    let phase = phase(snapshot, transcript)?;
    let foreign = snapshot.is_some_and(|s| s.ownership == Ownership::Foreign);
    let elapsed = clock.elapsed(now, transcript, now_unix);
    let local = clock
        .started
        .map(|s| now.saturating_duration_since(s))
        .unwrap_or_default();
    let mut text = format!("{} {}", glyph(animate, local), phase.label());
    if foreign {
        text.push_str(" · other client");
    }
    text.push(' ');
    text.push_str(&format_elapsed(elapsed));
    Some(Indicator {
        text,
        color: phase.color(foreign),
    })
}

/// Animation is on unless `HONK_NO_ANIM` is set to something other than `0` or empty.
pub fn animation_enabled() -> bool {
    !std::env::var("HONK_NO_ANIM").is_ok_and(|v| !v.is_empty() && v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::driver::ConnectionState;
    use crate::chat::types::RunState;
    use serde_json::json;

    fn snap(run: bool, ownership: Ownership) -> StateSnapshot {
        StateSnapshot {
            run_state: if run {
                RunState::Running("r1".into())
            } else {
                RunState::Idle
            },
            ownership,
            queued_len: 0,
            replaying: false,
            prompt_in_flight: false,
            connection: ConnectionState::Live,
            mcp_enabled: Vec::new(),
            model: None,
            known_models: Vec::new(),
            models_listed: false,
            compacting: false,
            awaiting_permission: false,
        }
    }

    fn update(t: &mut Transcript, u: serde_json::Value) {
        t.apply_frame(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "s", "update": u}
        }));
    }

    #[test]
    fn idle_has_no_phase() {
        let t = Transcript::new();
        assert_eq!(phase(None, &t), None);
        assert_eq!(phase(Some(&snap(false, Ownership::Unknown)), &t), None);
    }

    #[test]
    fn phase_follows_the_last_block() {
        let mut t = Transcript::new();
        let s = snap(true, Ownership::Own);
        assert_eq!(phase(Some(&s), &t), Some(Phase::Working));

        update(
            &mut t,
            json!({"sessionUpdate": "agent_thought_chunk", "messageId": "m1", "content": {"type": "text", "text": "hm"}}),
        );
        assert_eq!(phase(Some(&s), &t), Some(Phase::Thinking));

        update(
            &mut t,
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "shell · cargo test", "status": "pending"}),
        );
        assert_eq!(
            phase(Some(&s), &t),
            Some(Phase::Tool("shell · cargo test".into()))
        );

        update(
            &mut t,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}),
        );
        assert_eq!(phase(Some(&s), &t), Some(Phase::Working));

        update(
            &mut t,
            json!({"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": {"type": "text", "text": "done"}}),
        );
        assert_eq!(phase(Some(&s), &t), Some(Phase::Replying));
    }

    #[test]
    fn specific_states_win() {
        let t = Transcript::new();
        let mut s = snap(true, Ownership::Pending);
        assert_eq!(phase(Some(&s), &t), Some(Phase::Waiting));

        let mut sent = snap(false, Ownership::Unknown);
        sent.prompt_in_flight = true;
        assert_eq!(phase(Some(&sent), &t), Some(Phase::Waiting));

        s.compacting = true;
        assert_eq!(phase(Some(&s), &t), Some(Phase::Compacting));

        s.awaiting_permission = true;
        assert_eq!(phase(Some(&s), &t), Some(Phase::NeedsApproval));
    }

    #[test]
    fn tool_titles_are_cut() {
        let p = Phase::Tool("shell · cargo test --workspace --all-features -- --nocapture".into());
        let label = p.label();
        assert!(label.ends_with('…'));
        assert_eq!(unicode_width::UnicodeWidthStr::width(label.as_str()), 32);
    }

    #[test]
    fn elapsed_formats() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(185)), "3m05s");
        assert_eq!(format_elapsed(Duration::from_secs(3840)), "1h04m");
    }

    #[test]
    fn clock_starts_on_busy_and_resets_on_idle() {
        let t = Transcript::new();
        let t0 = Instant::now();
        let mut c = Clock::default();
        c.observe(&snap(true, Ownership::Own), t0);
        // A second busy snapshot does not restart the clock.
        c.observe(&snap(true, Ownership::Own), t0 + Duration::from_secs(5));
        assert_eq!(
            c.elapsed(t0 + Duration::from_secs(12), &t, 0),
            Duration::from_secs(12)
        );
        c.observe(&snap(false, Ownership::Own), t0 + Duration::from_secs(13));
        assert_eq!(
            c.elapsed(t0 + Duration::from_secs(14), &t, 0),
            Duration::ZERO
        );
    }

    #[test]
    fn mid_turn_attach_backdates_from_the_user_echo() {
        let mut t = Transcript::new();
        update(
            &mut t,
            json!({"sessionUpdate": "user_message_chunk", "messageId": "m1", "content": {"type": "text", "text": "go"},
                   "_meta": {"goose": {"created": 1_000}}}),
        );
        let t0 = Instant::now();
        let mut s = snap(true, Ownership::Foreign);
        s.replaying = true;
        let mut c = Clock::default();
        c.observe(&s, t0);
        assert_eq!(c.elapsed(t0, &t, 1_090), Duration::from_secs(90));

        // Not when the turn started while we watched.
        let mut c = Clock::default();
        c.observe(&snap(true, Ownership::Own), t0);
        assert_eq!(c.elapsed(t0, &t, 1_090), Duration::ZERO);
    }

    #[test]
    fn indicator_text() {
        let mut t = Transcript::new();
        update(
            &mut t,
            json!({"sessionUpdate": "agent_thought_chunk", "messageId": "m1", "content": {"type": "text", "text": "hm"}}),
        );
        let t0 = Instant::now();
        let s = snap(true, Ownership::Own);
        let mut c = Clock::default();
        c.observe(&s, t0);
        let now = t0 + Duration::from_millis(12_250);
        let i = indicator(Some(&s), &t, &c, true, now, 0).unwrap();
        assert_eq!(i.text, "⠹ thinking 12s");
        assert_eq!(i.color, Color::Green);

        let f = snap(true, Ownership::Foreign);
        let i = indicator(Some(&f), &t, &c, false, now, 0).unwrap();
        assert_eq!(i.text, "⋯ thinking · other client 12s");
        assert_eq!(i.color, Color::Yellow);

        assert_eq!(indicator(None, &t, &c, true, now, 0), None);
    }
}
