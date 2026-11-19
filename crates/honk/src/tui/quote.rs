//! `/quote N`: the coordinator's last N replies, quoted for the composer.
//!
//! A turn is a user block plus every block after it up to the next user block. Its reply is
//! the text of its assistant blocks (thoughts, tools and notices are left out). Turns with no
//! reply text do not count, so silent wake runs are skipped. The user block only labels the
//! turn: stall-watch wakes, swarm messages and Jonathan's own prompts are never quoted.
//! See docs/plans/2026-11-19-quote-replies.md.

use crate::tui::transcript::{Block, BlockKind};
pub use honk_wire::{Trigger, classify};

/// The quoted replies and how many turns they cover.
#[derive(Debug, PartialEq, Eq)]
pub struct Quoted {
    pub text: String,
    pub count: usize,
}

struct Turn {
    trigger: Trigger,
    is_compact: bool,
    reply: Vec<String>,
    created: Option<i64>,
}

/// Builds the quote of the last `n` turns that have reply text, oldest first.
/// `running`: a run is active, so the last turn's reply may still be streaming.
/// `fmt_time` formats unix seconds for the header (injected so tests don't depend on the TZ).
pub fn build(
    blocks: &[Block],
    n: usize,
    running: bool,
    fmt_time: impl Fn(i64) -> Option<String>,
) -> Option<Quoted> {
    if n == 0 {
        return None;
    }
    let mut turns: Vec<Turn> = Vec::new();
    for b in blocks {
        match b.kind {
            BlockKind::User => turns.push(Turn {
                trigger: classify(&b.text),
                is_compact: b.text.trim() == "/compact",
                reply: Vec::new(),
                created: None,
            }),
            BlockKind::Assistant => {
                if turns.is_empty() {
                    turns.push(Turn {
                        trigger: Trigger::Start,
                        is_compact: false,
                        reply: Vec::new(),
                        created: None,
                    });
                }
                let turn = turns.last_mut().expect("non-empty");
                let text = b.text.trim();
                if !text.is_empty() {
                    if turn.created.is_none() {
                        turn.created = b.created;
                    }
                    turn.reply.push(text.to_string());
                }
            }
            _ => {}
        }
    }
    let last_idx = turns.len().checked_sub(1);
    let mut picked: Vec<(usize, &Turn)> = turns
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, t)| !t.reply.is_empty() && !t.is_compact)
        .take(n)
        .collect();
    if picked.is_empty() {
        return None;
    }
    picked.reverse();

    let mut out = String::new();
    for (i, (idx, turn)) in picked.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        let mut label = turn.trigger.label();
        if running && Some(*idx) == last_idx {
            label.push_str(", in progress");
        }
        out.push_str("# ");
        if let Some(time) = turn.created.and_then(&fmt_time) {
            out.push_str(&time);
            out.push(' ');
        }
        out.push_str(&format!("({label})"));
        for line in turn.reply.join("\n\n").lines() {
            out.push('\n');
            if line.is_empty() {
                out.push('>');
            } else {
                out.push_str("> ");
                out.push_str(line);
            }
        }
    }
    Some(Quoted {
        text: out,
        count: picked.len(),
    })
}

/// `MM-DD HH:MM` in the host's local time zone.
pub fn local_time(secs: i64) -> Option<String> {
    let t: libc::time_t = secs;
    // SAFETY: localtime_r writes only into `tm`, which we own; honk never calls setenv.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::localtime_r(&t, &mut tm) };
    if r.is_null() {
        return None;
    }
    Some(format!(
        "{:02}-{:02} {:02}:{:02}",
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use honk_wire::BlockKey;

    fn block(kind: BlockKind, text: &str, created: Option<i64>) -> Block {
        let key = match kind {
            BlockKind::User => BlockKey::User(text.into()),
            BlockKind::Assistant => BlockKey::Text(text.into()),
            BlockKind::Thought => BlockKey::Thought(text.into()),
            BlockKind::Tool => BlockKey::Tool(text.into()),
            BlockKind::Notice => BlockKey::Notice(text.into()),
        };
        Block {
            key,
            kind,
            revision: 1,
            text: text.into(),
            title: String::new(),
            status: String::new(),
            tool_kind: None,
            input_text: String::new(),
            output_text: String::new(),
            notification_line: None,
            notice: None,
            created,
        }
    }
    fn user(t: &str) -> Block {
        block(BlockKind::User, t, None)
    }
    fn ast(t: &str, c: i64) -> Block {
        block(BlockKind::Assistant, t, Some(c))
    }
    fn fmt(secs: i64) -> Option<String> {
        Some(format!("T{secs}"))
    }

    #[test]
    fn picks_last_n_replies_oldest_first_and_skips_silent_turns() {
        let blocks = vec![
            user("first prompt"),
            ast("old reply", 10),
            user("[sw wake run=r reason=heartbeat] quiet"),
            block(BlockKind::Thought, "thinking", Some(20)),
            block(BlockKind::Tool, "tool", None),
            user("<swarm_message v=\"1\" from=\"ses_w\">\nresult"),
            ast("swarm reply", 30),
            user("[sw wake run=r reason=mail] mail"),
            ast("line 1\n\nline 3", 40),
        ];
        let q = build(&blocks, 2, false, fmt).unwrap();
        assert_eq!(q.count, 2);
        assert_eq!(
            q.text,
            "# T30 (swarm: ses_w)\n> swarm reply\n\n# T40 (wake: mail)\n> line 1\n>\n> line 3"
        );
    }

    #[test]
    fn joins_multiple_text_blocks_in_a_turn_and_excludes_user_text() {
        let blocks = vec![
            user("please do x"),
            ast("part one", 5),
            block(BlockKind::Tool, "tool", None),
            ast("part two", 6),
        ];
        let q = build(&blocks, 3, false, fmt).unwrap();
        assert_eq!(q.count, 1);
        assert_eq!(q.text, "# T5 (you)\n> part one\n>\n> part two");
    }

    #[test]
    fn missing_created_omits_time_and_start_turn_is_labelled() {
        let blocks = vec![block(BlockKind::Assistant, "hello", None)];
        let q = build(&blocks, 1, false, fmt).unwrap();
        assert_eq!(q.text, "# (start)\n> hello");
    }

    #[test]
    fn compact_turns_skipped_and_running_marks_last() {
        let blocks = vec![
            user("hi"),
            ast("answer", 1),
            user("/compact"),
            ast("Compaction complete", 2),
            user("next"),
            ast("partial", 3),
        ];
        let q = build(&blocks, 5, true, fmt).unwrap();
        assert_eq!(q.count, 2);
        assert_eq!(
            q.text,
            "# T1 (you)\n> answer\n\n# T3 (you, in progress)\n> partial"
        );
    }

    #[test]
    fn running_does_not_mark_an_earlier_turn() {
        let blocks = vec![user("hi"), ast("done", 1), user("next")];
        let q = build(&blocks, 1, true, fmt).unwrap();
        assert_eq!(q.text, "# T1 (you)\n> done");
    }

    #[test]
    fn nothing_to_quote() {
        assert_eq!(build(&[], 3, false, fmt), None);
        assert_eq!(build(&[user("x"), user("y")], 3, false, fmt), None);
        assert_eq!(build(&[user("x"), ast("y", 1)], 0, false, fmt), None);
    }

    #[test]
    fn local_time_has_expected_shape() {
        let s = local_time(1_795_121_237).unwrap();
        assert_eq!(s.len(), "11-19 14:32".len());
        assert_eq!(&s[2..3], "-");
        assert_eq!(&s[8..9], ":");
    }
}
