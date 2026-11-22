//! `/quote [full] [N|prev]`: the coordinator's replies, quoted for the composer.
//!
//! A turn is a user block plus every block after it up to the next user block. Its reply is
//! the text of its assistant blocks (thoughts, tools and notices are left out). Turns with no
//! reply text do not count, so silent wake runs are skipped. The user block only labels the
//! turn: stall-watch wakes, swarm messages and Jonathan's own prompts are never quoted.
//!
//! `/quote N` takes the last N such turns. Bare `/quote` takes every such turn from
//! Jonathan's own last prompt (`Trigger::You`) onward, the reply to that prompt included;
//! `/quote prev` does the same from the prompt before that one.
//! A `/compact` is not his prompt: stall-watch sends them too, and nothing tells the two apart.
//! See docs/plans/2026-11-19-quote-replies.md.

use crate::tui::transcript::{Block, BlockKind};
pub use honk_wire::{Trigger, classify};

/// Which turns to quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// The last N turns with reply text.
    Last(usize),
    /// Every turn with reply text from your last prompt on.
    SinceYou,
    /// `SinceYou`, but counted from your previous prompt (the one before your last).
    SincePrev,
}

impl Pick {
    /// Which replies this picks, for notices: "since your last prompt", "the last 5".
    pub fn describe(self) -> String {
        match self {
            Pick::Last(1) => "the last reply".into(),
            Pick::Last(n) => format!("the last {n} replies"),
            Pick::SinceYou => "since your last prompt".into(),
            Pick::SincePrev => "since your previous prompt".into(),
        }
    }
}

/// Why nothing was quoted.
#[derive(Debug, PartialEq, Eq)]
pub enum Empty {
    /// No turn has reply text (or N was 0).
    NoReplies,
    /// `SinceYou`, but the transcript has no prompt of yours.
    NeverSpoke,
    /// `SinceYou`, but no turn from your last prompt on has reply text yet.
    NothingSince,
    /// `SincePrev`, but the transcript has only one prompt of yours.
    OnePrompt,
}

/// The quoted replies and how many turns they cover.
#[derive(Debug, PartialEq, Eq)]
pub struct Quoted {
    pub text: String,
    pub count: usize,
    /// The picked replies, oldest first, unquoted (for `/quote`'s model trim, `tui::trim`).
    pub replies: Vec<Reply>,
    /// `SinceYou`: the text of the prompt counted from.
    pub your_prompt: Option<String>,
}

/// One picked reply: its `# <time> (<label>)` header line and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub header: String,
    pub text: String,
    /// The run is still writing it.
    pub in_progress: bool,
    /// Your prompt, when this is the reply to one (context for the trim, never quoted).
    pub your_prompt: Option<String>,
}

/// `text` as quote lines: `> line`, and a bare `>` for a blank line.
pub fn quote_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for (i, line) in lines.into_iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.is_empty() {
            out.push('>');
        } else {
            out.push_str("> ");
            out.push_str(line);
        }
    }
    out
}

struct Turn {
    trigger: Trigger,
    is_compact: bool,
    prompt: String,
    reply: Vec<String>,
    created: Option<i64>,
}

/// Builds the quote of the turns `pick` selects, oldest first.
/// `running`: a run is active, so the last turn's reply may still be streaming.
/// `fmt_time` formats unix seconds for the header (injected so tests don't depend on the TZ).
pub fn build(
    blocks: &[Block],
    pick: Pick,
    running: bool,
    fmt_time: impl Fn(i64) -> Option<String>,
) -> Result<Quoted, Empty> {
    let mut turns: Vec<Turn> = Vec::new();
    for b in blocks {
        match b.kind {
            BlockKind::User => turns.push(Turn {
                trigger: classify(&b.text),
                is_compact: b.text.trim() == "/compact",
                prompt: b.text.clone(),
                reply: Vec::new(),
                created: None,
            }),
            BlockKind::Assistant => {
                if turns.is_empty() {
                    turns.push(Turn {
                        trigger: Trigger::Start,
                        is_compact: false,
                        prompt: String::new(),
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
    let mut your_prompt = None;
    let (start, n, empty) = match pick {
        Pick::Last(n) => (0, n, Empty::NoReplies),
        Pick::SinceYou | Pick::SincePrev => {
            let back = usize::from(matches!(pick, Pick::SincePrev));
            let yours: Vec<usize> = turns
                .iter()
                .enumerate()
                .filter(|(_, t)| t.trigger == Trigger::You && !t.is_compact)
                .map(|(i, _)| i)
                .collect();
            if yours.is_empty() {
                return Err(Empty::NeverSpoke);
            }
            let Some(&start) = yours.iter().rev().nth(back) else {
                return Err(Empty::OnePrompt);
            };
            your_prompt = Some(turns[start].prompt.clone());
            (start, usize::MAX, Empty::NothingSince)
        }
    };
    let eligible: Vec<(usize, &Turn)> = turns
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(_, t)| !t.reply.is_empty() && !t.is_compact)
        .collect();
    let picked = &eligible[eligible.len().saturating_sub(n)..];
    if picked.is_empty() {
        return Err(empty);
    }

    let replies: Vec<Reply> = picked
        .iter()
        .map(|(idx, turn)| {
            let in_progress = running && Some(*idx) == last_idx;
            let mut label = turn.trigger.label();
            if in_progress {
                label.push_str(", in progress");
            }
            let mut header = String::from("# ");
            if let Some(time) = turn.created.and_then(&fmt_time) {
                header.push_str(&time);
                header.push(' ');
            }
            header.push_str(&format!("({label})"));
            Reply {
                header,
                text: turn.reply.join("\n\n"),
                in_progress,
                your_prompt: (turn.trigger == Trigger::You && !turn.is_compact)
                    .then(|| turn.prompt.clone()),
            }
        })
        .collect();
    let text = replies
        .iter()
        .map(|r| format!("{}\n{}", r.header, quote_lines(r.text.lines())))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(Quoted {
        text,
        count: picked.len(),
        replies,
        your_prompt,
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
        let q = build(&blocks, Pick::Last(2), false, fmt).unwrap();
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
        let q = build(&blocks, Pick::Last(3), false, fmt).unwrap();
        assert_eq!(q.count, 1);
        assert_eq!(q.text, "# T5 (you)\n> part one\n>\n> part two");
    }

    #[test]
    fn missing_created_omits_time_and_start_turn_is_labelled() {
        let blocks = vec![block(BlockKind::Assistant, "hello", None)];
        let q = build(&blocks, Pick::Last(1), false, fmt).unwrap();
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
        let q = build(&blocks, Pick::Last(5), true, fmt).unwrap();
        assert_eq!(q.count, 2);
        assert_eq!(
            q.text,
            "# T1 (you)\n> answer\n\n# T3 (you, in progress)\n> partial"
        );
    }

    #[test]
    fn running_does_not_mark_an_earlier_turn() {
        let blocks = vec![user("hi"), ast("done", 1), user("next")];
        let q = build(&blocks, Pick::Last(1), true, fmt).unwrap();
        assert_eq!(q.text, "# T1 (you)\n> done");
    }

    #[test]
    fn nothing_to_quote() {
        assert_eq!(build(&[], Pick::Last(3), false, fmt), Err(Empty::NoReplies));
        assert_eq!(
            build(&[user("x"), user("y")], Pick::Last(3), false, fmt),
            Err(Empty::NoReplies)
        );
        assert_eq!(
            build(&[user("x"), ast("y", 1)], Pick::Last(0), false, fmt),
            Err(Empty::NoReplies)
        );
    }

    #[test]
    fn since_you_quotes_from_your_last_prompt_including_its_reply() {
        let blocks = vec![
            user("old prompt"),
            ast("old reply", 1),
            user("latest prompt"),
            ast("reply to you", 2),
            user("[sw wake run=r reason=mail] mail"),
            ast("wake reply", 3),
            user("<swarm_message v=\"1\" from=\"ses_w\">\nresult"),
            user("You are goose session x, resume"),
            ast("goose reply", 4),
        ];
        let q = since(&blocks, false).unwrap();
        assert_eq!(q.count, 3);
        assert_eq!(
            q.text,
            "# T2 (you)\n> reply to you\n\n# T3 (wake: mail)\n> wake reply\n\n# T4 (goose)\n> goose reply"
        );
    }

    #[test]
    fn since_you_has_no_cap() {
        let mut blocks = vec![user("me")];
        for i in 0..120 {
            blocks.push(user("[sw wake run=r reason=x] x"));
            blocks.push(ast(&format!("r{i}"), i));
        }
        let q = since(&blocks, false).unwrap();
        assert_eq!(q.count, 120);
        assert!(
            q.text.contains("> r0\n") && q.text.ends_with("> r119"),
            "{}",
            q.text
        );
        let q = build(&blocks, Pick::Last(500), false, fmt).unwrap();
        assert_eq!(q.count, 120);
    }

    #[test]
    fn since_you_compact_is_not_your_prompt() {
        let blocks = vec![
            user("me"),
            ast("a", 1),
            user("[sw wake run=r reason=x] x"),
            ast("w", 2),
            user("/compact"),
            ast("Compaction complete", 2),
            user("[sw wake run=r reason=x] x"),
            ast("b", 3),
        ];
        let q = since(&blocks, false).unwrap();
        assert_eq!(
            q.text,
            "# T1 (you)\n> a\n\n# T2 (wake: x)\n> w\n\n# T3 (wake: x)\n> b"
        );
    }

    #[test]
    fn since_you_edge_cases() {
        let wake_only = vec![user("[sw wake run=r reason=x] x"), ast("a", 1)];
        assert_eq!(since(&wake_only, false), Err(Empty::NeverSpoke));
        assert_eq!(since(&[ast("hello", 1)], false), Err(Empty::NeverSpoke));
        let waiting = vec![user("me"), ast("a", 1), user("again")];
        assert_eq!(since(&waiting, true), Err(Empty::NothingSince));
        let silent = vec![
            user("me"),
            user("[sw wake run=r reason=x] x"),
            block(BlockKind::Tool, "t", None),
        ];
        assert_eq!(since(&silent, false), Err(Empty::NothingSince));
    }

    #[test]
    fn since_you_marks_the_running_turn() {
        let blocks = vec![user("me"), ast("partial", 1)];
        let q = since(&blocks, true).unwrap();
        assert_eq!(q.text, "# T1 (you, in progress)\n> partial");
    }

    #[test]
    fn since_prev_counts_from_the_prompt_before_your_last() {
        let blocks = vec![
            user("first"),
            ast("a", 1),
            user("[sw wake run=r reason=x] x"),
            ast("w", 2),
            user("/compact"),
            ast("c", 3),
            user("second"),
            ast("b", 4),
        ];
        let q = build(&blocks, Pick::SincePrev, false, fmt).unwrap();
        assert_eq!(
            q.text,
            "# T1 (you)\n> a\n\n# T2 (wake: x)\n> w\n\n# T4 (you)\n> b"
        );
        assert_eq!(q.your_prompt.as_deref(), Some("first"));
        let one = vec![user("only"), ast("a", 1)];
        assert_eq!(
            build(&one, Pick::SincePrev, false, fmt),
            Err(Empty::OnePrompt)
        );
        assert_eq!(
            build(&[ast("a", 1)], Pick::SincePrev, false, fmt),
            Err(Empty::NeverSpoke)
        );
    }

    fn since(blocks: &[Block], running: bool) -> Result<Quoted, Empty> {
        build(blocks, Pick::SinceYou, running, fmt)
    }

    #[test]
    fn local_time_has_expected_shape() {
        let s = local_time(1_795_121_237).unwrap();
        assert_eq!(s.len(), "11-19 14:32".len());
        assert_eq!(&s[2..3], "-");
        assert_eq!(&s[8..9], ":");
    }
}
