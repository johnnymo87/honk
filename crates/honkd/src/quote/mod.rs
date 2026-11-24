//! `_honk/quote` (honk-9qf.31.7): `/quote`'s pick and trim, in honkd so that honk chat's
//! `/quote` and pigeon's Slack `/quote` run one implementation. honkd reads the session's
//! log on disk (an idle session is not loaded into goose), picks the replies ([`pick`]),
//! and has goose's default model trim them to what needs Jonathan ([`trim`], one
//! `_honk/oneshot` call per batch). Wire types: `honk_wire::quote`. See docs/chat.md.

pub mod pick;
pub mod trim;
pub mod turns;

use honk_wire::quote::{Empty, Pick, QuoteReply, QuoteReq, QuotedReply};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;

use crate::hub::Hub;

/// Replies a `Since` quote falls back to when the session has no prompt of yours.
pub const FALLBACK: usize = 3;
/// Trim answers remembered, one entry per distinct span (see [`AnswerCache`]).
const CACHE_ENTRIES: usize = 16;
/// Logs read at once by `_honk/quote` (each read holds the session's message text).
pub const MAX_CONCURRENT_READS: usize = 2;

/// The last good trim answers, by a hash of the prompts that produced them: quoting the same
/// replies again is free. Small and bounded.
#[derive(Default)]
pub struct AnswerCache {
    entries: VecDeque<(u64, Vec<trim::Answer>)>,
}

impl AnswerCache {
    pub fn get(&mut self, key: u64) -> Option<Vec<trim::Answer>> {
        let i = self.entries.iter().position(|(k, _)| *k == key)?;
        let e = self.entries.remove(i)?;
        let answers = e.1.clone();
        self.entries.push_back(e);
        Some(answers)
    }

    pub fn put(&mut self, key: u64, answers: Vec<trim::Answer>) {
        self.entries.retain(|(k, _)| *k != key);
        self.entries.push_back((key, answers));
        while self.entries.len() > CACHE_ENTRIES {
            self.entries.pop_front();
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum QuoteError {
    /// JSON-RPC -32602.
    BadRequest(String),
    /// JSON-RPC -32800.
    Cancelled,
    /// JSON-RPC -32603.
    Failed(String),
}

impl QuoteError {
    pub fn code(&self) -> i64 {
        match self {
            QuoteError::BadRequest(_) => -32602,
            QuoteError::Cancelled => -32800,
            QuoteError::Failed(_) => -32603,
        }
    }
    pub fn message(&self) -> String {
        match self {
            QuoteError::BadRequest(m) | QuoteError::Failed(m) => m.clone(),
            QuoteError::Cancelled => "cancelled".into(),
        }
    }
}

/// What a quote picked, before any model call.
#[derive(Debug)]
pub enum Step {
    /// The answer, no model needed.
    Done(QuoteReply),
    /// The picked replies need trimming.
    Trim(Pending),
}

/// A pick that needs the model: one prompt per batch.
#[derive(Debug)]
pub struct Pending {
    pub quoted: pick::Quoted,
    /// The pick actually used (a fallback may replace the one asked for).
    pub pick: Pick,
    /// `"<how they were picked>; "` or empty, for the notices.
    pub note: String,
    /// "3 replies since your last prompt", for the notices.
    pub what: String,
    pub batches: Vec<std::ops::Range<usize>>,
    pub prompts: Vec<String>,
    pub key: u64,
}

/// What a quote loaded, for notices: "3 replies since your last prompt", "the last 5 replies".
fn quoted_what(q: &pick::Quoted, pick: Pick) -> String {
    match pick {
        Pick::Last { .. } => Pick::Last { n: q.count }.describe(),
        _ => {
            let noun = if q.count == 1 { "reply" } else { "replies" };
            format!("{} {noun} {}", q.count, pick.describe())
        }
    }
}

fn hash_of(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Every line of every picked reply, as `/quote full` gives them.
fn untrimmed(q: &pick::Quoted) -> Vec<QuotedReply> {
    q.replies
        .iter()
        .map(|r| QuotedReply {
            header: r.header.clone(),
            passages: vec![r.text.lines().map(str::to_string).collect()],
            in_progress: r.in_progress,
        })
        .collect()
}

fn empty(why: Empty, notice: String) -> Step {
    Step::Done(QuoteReply {
        empty: Some(why),
        notices: vec![notice],
        ..Default::default()
    })
}

/// Picks the replies (with honk chat's fallbacks) and decides whether the model is needed.
/// `running`: a run is active, so the last turn may still be streaming.
pub fn prepare(
    blocks: &[turns::Block],
    asked: Pick,
    full: bool,
    running: bool,
    batch_bytes: usize,
    fmt_time: impl Fn(i64) -> Option<String>,
) -> Step {
    let build = |p| pick::build(blocks, p, running, &fmt_time);
    let (result, used, note) = match build(asked) {
        Err(pick::Empty::OnePrompt) => (
            build(Pick::Since),
            Pick::Since,
            Some("only one prompt of yours in this session, so counting from it".to_string()),
        ),
        Err(pick::Empty::NeverSpoke) => {
            let fallback = Pick::Last { n: FALLBACK };
            let r = build(fallback);
            let note = r.as_ref().ok().map(|q| {
                format!(
                    "no prompt of yours in this session, so the last {}",
                    q.count
                )
            });
            (r.map_err(|_| pick::Empty::NoReplies), fallback, note)
        }
        r => (r, asked, None),
    };
    let q = match result {
        Ok(q) => q,
        Err(pick::Empty::NothingSince) => {
            let yet = if running { " yet" } else { "" };
            return empty(
                Empty::NothingSince,
                format!(
                    "no replies {}{yet}; /quote N quotes the last N replies",
                    used.describe()
                ),
            );
        }
        Err(_) => return empty(Empty::NoReplies, "no replies to quote".into()),
    };
    let note = note.or_else(|| match used {
        Pick::Last { n } if q.count < n => Some(format!("only {} replies to quote", q.count)),
        _ => None,
    });
    let what = quoted_what(&q, used);
    let note = note.map(|n| format!("{n}; ")).unwrap_or_default();
    let done = |notice: String| {
        Step::Done(QuoteReply {
            replies: untrimmed(&q),
            notices: vec![notice],
            ..Default::default()
        })
    };
    if full {
        return done(format!("{note}quoted {what}, untrimmed"));
    }
    if q.text.len() <= trim::MIN_BYTES {
        let mut msg = format!("{note}quoted {what} in full: too short for trimming to help");
        if matches!(asked, Pick::Since) {
            msg.push_str(
                "; if you already replied, /quote N trims the last N replies and \
                 /quote prev counts from your previous prompt",
            );
        }
        return done(msg);
    }
    let sent = trim::sendable(&q.replies);
    if sent.is_empty() {
        return done(format!(
            "{note}quoted {what}: still being written, untrimmed"
        ));
    }
    let batches = trim::batches(&sent, batch_bytes);
    let prompts = trim::batch_prompts(&sent, q.your_prompt.as_deref(), &batches);
    let key = hash_of(&prompts.join("\u{0}"));
    drop(sent);
    Step::Trim(Pending {
        quoted: q,
        pick: used,
        note,
        what,
        batches,
        prompts,
        key,
    })
}

/// Whether every call answered with a usable selection (only then is the answer cached).
pub fn all_good(p: &Pending, answers: &[Result<trim::Answer, String>]) -> bool {
    let sent = trim::sendable(&p.quoted.replies);
    p.batches.iter().zip(answers).all(|(b, a)| {
        a.as_ref()
            .is_ok_and(|(text, _)| trim::parse(text, &sent[b.clone()]).is_ok())
    })
}

/// The reply once every call has answered (one answer per batch). `cached`: the answers are
/// the remembered ones for the same replies.
pub fn finish(p: &Pending, answers: &[Result<trim::Answer, String>], cached: bool) -> QuoteReply {
    let full = p.pick.full_form();
    let calls = match p.batches.len() {
        1 => String::new(),
        n => format!(" in {n} model calls"),
    };
    let how = if cached {
        " (the same replies as last time: reused that answer)".to_string()
    } else {
        calls
    };
    let mut reply = QuoteReply {
        notices: vec![format!(
            "{}trimmed {} to what needs you{how}",
            p.note, p.what
        )],
        ..Default::default()
    };
    let sent = trim::sendable(&p.quoted.replies);
    match trim::merge(&sent, &p.batches, answers) {
        Ok(m) if m.kept.is_empty() && !p.quoted.replies.iter().any(|r| r.in_progress) => {
            reply.empty = Some(Empty::NothingKept);
            reply.model = m.model;
            reply.notices.push(format!(
                "the model found nothing that needs you; {full} quotes them all"
            ));
        }
        Ok(trim::Merged {
            kept,
            model,
            failed,
        }) => {
            if let Some(why) = failed.first() {
                reply.warnings.push(format!(
                    "{} of {} trim calls failed ({why}); their replies are quoted in full",
                    failed.len(),
                    p.batches.len()
                ));
            }
            let (summary, replies) = trim::render(&p.quoted.replies, &kept, model.as_deref());
            reply.summary = Some(summary);
            reply.replies = replies;
            reply.model = model;
        }
        Err(why) => {
            reply.notices.clear();
            if !p.note.is_empty() {
                reply
                    .notices
                    .push(p.note.trim_end_matches("; ").to_string());
            }
            reply
                .warnings
                .push(format!("trim failed ({why}); quoted them all, untrimmed"));
            reply.replies = untrimmed(&p.quoted);
        }
    }
    reply
}

/// The program a stall-watch coordinator works for, from its working directory
/// (`…/stall-watch/coord-sessions/<tag>-<ms>`). Coordinators are session-tagged
/// `stall-watcher-runs`, so a quote of one would otherwise be charged there.
pub fn coordinator_program(cwd: &str) -> Option<String> {
    let mut parts = std::path::Path::new(cwd)
        .components()
        .rev()
        .map(|c| c.as_os_str().to_str());
    let leaf = parts.next()??;
    if parts.next()? != Some("coord-sessions") || parts.next()? != Some("stall-watch") {
        return None;
    }
    let (tag, ms) = leaf.rsplit_once('-')?;
    (!tag.is_empty() && !ms.is_empty() && ms.bytes().all(|b| b.is_ascii_digit()))
        .then(|| tag.to_string())
}

/// A goose session id honkd may name a log file after.
fn valid_sid(sid: &str) -> bool {
    !sid.is_empty()
        && sid.len() <= 128
        && sid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Hub {
    /// `_honk/quote`. `cancel` firing, or its sender being dropped (the viewer went away),
    /// cancels every model call; each still cleans up its throwaway session.
    pub async fn quote(
        &self,
        req: QuoteReq,
        mut cancel: oneshot::Receiver<()>,
    ) -> Result<QuoteReply, QuoteError> {
        let sid = req.session_id.clone();
        if !valid_sid(&sid) {
            return Err(QuoteError::BadRequest(format!("invalid sessionId {sid:?}")));
        }
        if matches!(req.pick, Pick::Last { n: 0 }) {
            return Err(QuoteError::BadRequest("pick.n must be at least 1".into()));
        }
        let path = self.state_dir.join("sessions").join(format!("{sid}.jsonl"));
        if !path.is_file() {
            return Err(QuoteError::BadRequest(format!(
                "unknown session {sid}: honkd has no log for it"
            )));
        }
        // Sampled on both sides of the read: a run that ends mid-read may have left its reply
        // cut off on disk, so any doubt marks the last reply in progress (never trimmed).
        let running_before = self.is_session_running(&sid);
        let blocks = {
            let permit = self.quote_reads.clone().acquire_owned().await;
            // The permit moves into the read, so a cancelled quote still holds it until the
            // read is done and the bound on concurrent reads holds.
            let read = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                turns::read(&path)
            });
            tokio::select! {
                r = read => r
                    .map_err(|e| QuoteError::Failed(format!("reading the log: {e}")))?
                    .map_err(|e| QuoteError::Failed(format!("reading the log: {e}")))?,
                _ = &mut cancel => return Err(QuoteError::Cancelled),
            }
        };
        let running = running_before || self.is_session_running(&sid);
        let step = prepare(
            &blocks,
            req.pick,
            req.full,
            running,
            self.quote_batch_bytes,
            pick::local_time,
        );
        drop(blocks);
        let p = match step {
            Step::Done(reply) => return Ok(reply),
            Step::Trim(p) => p,
        };
        let cached = self.quote_cache.lock().ok().and_then(|mut c| c.get(p.key));
        if let Some(answers) = cached.filter(|a| a.len() == p.batches.len()) {
            let answers: Vec<_> = answers.into_iter().map(Ok).collect();
            return Ok(finish(&p, &answers, true));
        }

        let tag = match &req.tag {
            Some(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
            _ => {
                let st = self.state.lock().await;
                st.sessions
                    .get(&sid)
                    .and_then(|h| coordinator_program(&h.cwd))
            }
        };
        let gate = tokio::sync::Semaphore::new(trim::PARALLEL);
        let stop = AtomicBool::new(false);
        let mut cancels = Vec::new();
        let calls = p.prompts.iter().map(|prompt| {
            let (tx, rx) = oneshot::channel();
            cancels.push(tx);
            let one = crate::oneshot::OneshotReq {
                system: trim::SYSTEM.to_string(),
                prompt: prompt.clone(),
                effort: Some("low".into()),
                timeout: crate::oneshot::DEFAULT_TIMEOUT,
                tag_from: Some(sid.clone()),
                tag: tag.clone(),
            };
            let (gate, stop) = (&gate, &stop);
            async move {
                let _permit = gate.acquire().await;
                if stop.load(Ordering::SeqCst) {
                    return Err("cancelled".to_string());
                }
                match self.oneshot(one, rx).await {
                    Ok(a) => Ok((a.text, a.model)),
                    Err(crate::oneshot::OneshotError::Cancelled) => Err("cancelled".to_string()),
                    Err(crate::oneshot::OneshotError::Failed(why)) => Err(why),
                }
            }
        });
        let all = futures::future::join_all(calls.collect::<Vec<_>>());
        tokio::pin!(all);
        let answers = tokio::select! {
            a = &mut all => a,
            _ = &mut cancel => {
                stop.store(true, Ordering::SeqCst);
                for tx in cancels.drain(..) {
                    let _ = tx.send(());
                }
                // Let each call archive its throwaway session.
                let _ = all.await;
                return Err(QuoteError::Cancelled);
            }
        };
        if all_good(&p, &answers)
            && let Ok(mut c) = self.quote_cache.lock()
        {
            c.put(p.key, answers.iter().cloned().map(Result::unwrap).collect());
        }
        Ok(finish(&p, &answers, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turns::{Block, Kind};

    fn user(t: &str) -> Block {
        Block {
            kind: Kind::User,
            text: t.into(),
            created: None,
        }
    }
    fn ast(t: &str, c: i64) -> Block {
        Block {
            kind: Kind::Assistant,
            text: t.into(),
            created: Some(c),
        }
    }
    fn fmt(secs: i64) -> Option<String> {
        Some(format!("T{secs}"))
    }
    fn done(s: Step) -> QuoteReply {
        match s {
            Step::Done(r) => r,
            Step::Trim(p) => panic!("needs the model: {p:?}"),
        }
    }
    fn pending(s: Step) -> Pending {
        match s {
            Step::Trim(p) => p,
            Step::Done(r) => panic!("done: {r:?}"),
        }
    }
    /// Your prompt, then three long wake replies (well over `trim::MIN_BYTES`).
    fn long() -> Vec<Block> {
        let mut b = vec![user("what is left?")];
        for i in 0..3 {
            b.push(user("[sw wake run=r reason=mail] mail"));
            let text: Vec<String> = (0..40)
                .map(|j| format!("reply {i} line {j} with enough words to make it long"))
                .collect();
            b.push(ast(&text.join("\n"), i));
        }
        b
    }

    #[test]
    fn full_quotes_every_line_with_a_notice() {
        let blocks = vec![user("me"), ast("a\n\nb", 1)];
        let r = done(prepare(&blocks, Pick::Since, true, false, 100, fmt));
        assert_eq!(r.composer_text(), "# T1 (you)\n> a\n>\n> b");
        assert_eq!(
            r.notices,
            vec!["quoted 1 reply since your last prompt, untrimmed"]
        );
        assert_eq!((r.summary, r.empty, r.model), (None, None, None));
    }

    #[test]
    fn short_quotes_are_not_trimmed_and_point_at_n_and_prev() {
        let blocks = vec![user("me"), ast("a", 1)];
        let r = done(prepare(&blocks, Pick::Since, false, false, 100, fmt));
        assert_eq!(r.composer_text(), "# T1 (you)\n> a");
        assert!(r.notices[0].contains("too short for trimming"), "{r:?}");
        assert!(r.notices[0].contains("/quote prev"), "{r:?}");
        let r = done(prepare(
            &blocks,
            Pick::Last { n: 4 },
            false,
            false,
            100,
            fmt,
        ));
        assert_eq!(
            r.notices,
            vec![
                "only 1 replies to quote; quoted the last reply in full: too short for trimming to help"
            ]
        );
    }

    #[test]
    fn empty_cases_and_fallbacks() {
        let r = done(prepare(&[], Pick::Since, false, false, 100, fmt));
        assert_eq!(r.empty, Some(Empty::NoReplies));
        assert_eq!(r.notices, vec!["no replies to quote"]);
        let waiting = vec![user("me"), ast("a", 1), user("again")];
        let r = done(prepare(&waiting, Pick::Since, false, true, 100, fmt));
        assert_eq!(r.empty, Some(Empty::NothingSince));
        assert_eq!(
            r.notices,
            vec!["no replies since your last prompt yet; /quote N quotes the last N replies"]
        );
        let wakes = vec![user("[sw wake run=r reason=x] x"), ast("a", 1)];
        let r = done(prepare(&wakes, Pick::Since, true, false, 100, fmt));
        assert!(
            r.notices[0].starts_with("no prompt of yours in this session, so the last 1; "),
            "{r:?}"
        );
        assert_eq!(r.composer_text(), "# T1 (wake: x)\n> a");
        let one = vec![user("only"), ast("a", 1)];
        let r = done(prepare(&one, Pick::Prev, true, false, 100, fmt));
        assert!(
            r.notices[0]
                .starts_with("only one prompt of yours in this session, so counting from it; "),
            "{r:?}"
        );
    }

    #[test]
    fn long_quotes_are_batched_and_finished_from_the_answers() {
        let p = pending(prepare(&long(), Pick::Since, false, false, 4500, fmt));
        assert_eq!(p.batches.len(), 2, "{:?}", p.batches);
        assert_eq!(p.prompts.len(), 2);
        assert!(p.prompts[0].starts_with("(Part 1 of 2"));
        let ok = |s: &str| Ok((s.to_string(), Some("m".to_string())));
        let answers = vec![
            ok(r#"{"keep":[{"reply":1,"lines":[[2,3]],"tag":"ask"}]}"#),
            Err("timeout".to_string()),
        ];
        assert!(!all_good(&p, &answers));
        let r = finish(&p, &answers, false);
        assert_eq!(r.model.as_deref(), Some("m"));
        assert_eq!(
            r.notices,
            vec!["trimmed 3 replies since your last prompt to what needs you in 2 model calls"]
        );
        assert_eq!(
            r.warnings,
            vec!["1 of 2 trim calls failed (timeout); their replies are quoted in full"]
        );
        let summary = r.summary.clone().unwrap();
        assert!(
            summary.starts_with("# honk: trimmed by m, kept 42 of 120 lines from 2 of 3 replies"),
            "{summary}"
        );
        assert_eq!(r.replies[0].header, "# T0 (wake: mail) · ask");
        assert_eq!(
            r.replies[0].passages,
            vec![vec![
                "reply 0 line 1 with enough words to make it long".to_string(),
                "reply 0 line 2 with enough words to make it long".to_string()
            ]]
        );
        assert_eq!(r.replies[1].passages[0].len(), 40);
    }

    #[test]
    fn finish_nothing_kept_and_total_failure() {
        let p = pending(prepare(
            &long(),
            Pick::Last { n: 2 },
            false,
            false,
            100_000,
            fmt,
        ));
        let r = finish(&p, &[Ok((r#"{"keep":[]}"#.into(), None))], false);
        assert_eq!(r.empty, Some(Empty::NothingKept));
        assert!(r.replies.is_empty());
        assert_eq!(
            r.notices[1],
            "the model found nothing that needs you; /quote full 2 quotes them all"
        );
        let r = finish(&p, &[Ok(("prose".into(), None))], false);
        assert_eq!(r.summary, None);
        assert_eq!(
            r.warnings,
            vec!["trim failed (the answer was not JSON); quoted them all, untrimmed"]
        );
        assert!(r.notices.is_empty(), "{r:?}");
        assert_eq!(r.replies.len(), 2);
        assert_eq!(r.replies[0].passages[0].len(), 40);
    }

    #[test]
    fn a_running_reply_is_appended_untrimmed() {
        let mut blocks = long();
        blocks.push(user("next?"));
        blocks.push(ast("partial", 9));
        let p = pending(prepare(
            &blocks,
            Pick::Last { n: 4 },
            false,
            true,
            100_000,
            fmt,
        ));
        let r = finish(
            &p,
            &[Ok((
                r#"{"keep":[{"reply":1,"lines":[[1,1]]}]}"#.into(),
                None,
            ))],
            false,
        );
        let last = r.replies.last().unwrap();
        assert_eq!(last.header, "# T9 (you, in progress) · untrimmed");
        assert!(last.in_progress);
        assert_eq!(last.passages, vec![vec!["partial".to_string()]]);
        // Kept nothing, but the running reply still comes back.
        let r = finish(&p, &[Ok((r#"{"keep":[]}"#.into(), None))], false);
        assert_eq!(r.empty, None);
        assert_eq!(r.replies.len(), 1);
    }

    #[test]
    fn cache_is_bounded_and_most_recent_wins() {
        let mut c = AnswerCache::default();
        for k in 0..20u64 {
            c.put(k, vec![(format!("{k}"), None)]);
        }
        assert!(c.get(0).is_none());
        assert_eq!(c.get(19).unwrap()[0].0, "19");
        assert_eq!(c.entries.len(), CACHE_ENTRIES);
    }

    #[test]
    fn coordinator_program_from_cwd() {
        assert_eq!(
            coordinator_program(
                "/home/dev/.local/state/stall-watch/coord-sessions/honk-1792619184012"
            ),
            Some("honk".into())
        );
        assert_eq!(
            coordinator_program(
                "/home/dev/.local/state/stall-watch/coord-sessions/baf-alerts-17916/"
            ),
            Some("baf-alerts".into())
        );
        assert_eq!(coordinator_program("/home/dev/projects/honk"), None);
        assert_eq!(
            coordinator_program("/x/stall-watch/coord-sessions/honk-12ab"),
            None
        );
        assert_eq!(coordinator_program("/x/other/coord-sessions/honk-12"), None);
    }

    #[test]
    fn sids_are_checked_before_naming_a_file() {
        assert!(valid_sid("20261124_56"));
        assert!(!valid_sid("../state"));
        assert!(!valid_sid(""));
        assert!(!valid_sid("a/b"));
    }
}
