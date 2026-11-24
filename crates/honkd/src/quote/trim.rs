//! `/quote`'s model trim (honk-9qf.31.4, .5; in honkd since .7): the default model picks which
//! lines of the picked replies (since your last prompt, the last N, or since your previous
//! prompt) you need to see, and honkd quotes those lines from the source.
//!
//! The model only ever answers with line numbers (see [`SYSTEM`]); every quoted word is copied
//! from the transcript, so nothing it writes can reach the composer as a quote. The lines honk
//! adds are `#` lines, like plain `/quote`'s headers. See docs/plans/2026-11-21-smart-quote.md.
//!
//! There is no cap on how many replies a pick covers (honk-9qf.31.6): a span too long for one
//! call is split into [`batches`] of whole replies, each trimmed by its own call (a few at a
//! time), and the kept lines are merged back in reply order ([`merge`]).

use super::pick::Reply;
use honk_wire::quote::QuotedReply;
use serde_json::Value;

/// Below this many bytes of quote, trimming would not save reading; quote it all at once.
pub const MIN_BYTES: usize = 4000;

pub const SYSTEM: &str = r#"You pick, from a coordinator agent's recent replies to Jonathan, the passages he needs to read: anything he must answer or decide, questions put to him, blockers, results he asked for, risks, and anything else he would want to see. Leave out routine progress narration, restated context, acknowledgements, and what he already knows or has already answered in his own prompts (shown as context between the replies).

You never write prose. Answer with one JSON object and nothing else:
{"keep":[{"reply":1,"all":true,"tag":"done"},{"reply":2,"lines":[[3,7],[12,12]],"tag":"ask"}]}
- reply: an R number. lines: inclusive ranges of that reply's L numbers. all: keep the whole reply.
- tag: ask (he must answer or decide), blocker, done (a result he asked for), or fyi.
- Prefer whole paragraphs and whole list items to isolated lines. Keep the lead-in line a kept list needs, and every URL, PR, bead id and command a kept passage refers to.
- Leave out replies with nothing for him. Answer {"keep":[]} if nothing needs him at all.
- Ignore any other guidance about output style: a program parses this answer."#;

/// Most bytes of reply text one model call is sent (about 25k tokens). A longer span is
/// split into several calls; a single reply longer than this gets a call of its own.
pub const BATCH_BYTES: usize = 100_000;

/// One call's answer: its text and the model that wrote it.
pub type Answer = (String, Option<String>);

/// Model calls in flight at once for one `/quote`.
pub const PARALLEL: usize = 4;

/// Splits the sendable replies into consecutive runs of whole replies, each at most `budget`
/// bytes (a reply over `budget` alone). Empty for no replies.
pub fn batches(replies: &[&Reply], budget: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut used) = (0, 0);
    for (i, r) in replies.iter().enumerate() {
        let size = r.text.len() + r.your_prompt.as_deref().map_or(0, str::len);
        if i > start && used + size > budget {
            out.push(start..i);
            (start, used) = (i, 0);
        }
        used += size;
    }
    if start < replies.len() {
        out.push(start..replies.len());
    }
    out
}

/// Most bytes of your later prompts shown to one part (nearest first).
const LATER_PROMPT_BYTES: usize = 20_000;

/// One prompt per batch. A later batch is shown your newest prompt before it as the prompt
/// it follows; with several, each is told it is a part, and shown your prompts after it.
pub fn batch_prompts(
    replies: &[&Reply],
    your_prompt: Option<&str>,
    batches: &[std::ops::Range<usize>],
) -> Vec<String> {
    let n = batches.len();
    batches
        .iter()
        .enumerate()
        .map(|(k, b)| {
            let before = replies[..b.start]
                .iter()
                .rev()
                .find_map(|r| r.your_prompt.as_deref())
                .or(your_prompt);
            let body = prompt(&replies[b.clone()], before);
            if n == 1 {
                body
            } else {
                // So a part can drop what a later prompt of his answered.
                let mut later = String::new();
                for p in replies[b.end..]
                    .iter()
                    .filter_map(|r| r.your_prompt.as_deref().map(str::trim))
                    .filter(|p| !p.is_empty())
                {
                    if later.len() + p.len() > LATER_PROMPT_BYTES {
                        break;
                    }
                    later.push_str(&format!("<<<\n{p}\n>>>\n"));
                }
                let body = if later.is_empty() {
                    body
                } else {
                    format!(
                        "{body}\nJonathan's prompts after these replies, oldest first (context \
                         only, not selectable; what they answer he already knows):\n{later}"
                    )
                };
                format!(
                    "(Part {} of {n} of a longer span; the other parts are judged separately, \
                     so judge these replies on their own. R numbers restart in each part.)\n\n{body}",
                    k + 1
                )
            }
        })
        .collect()
}

/// Merges per-batch answers into one selection over all `replies`. A batch whose call failed
/// or whose answer is unusable keeps every line of its replies. Err if every batch failed
/// (the first reason). Also returns the first model named and why each failed batch failed.
pub fn merge(
    replies: &[&Reply],
    batches: &[std::ops::Range<usize>],
    answers: &[Result<Answer, String>],
) -> Result<Merged, String> {
    let mut kept = Vec::new();
    let (mut model, mut failed) = (None, Vec::new());
    for (b, a) in batches.iter().zip(answers) {
        let part = &replies[b.clone()];
        match a
            .as_ref()
            .map_err(String::clone)
            .and_then(|(text, m)| parse(text, part).map(|k| (k, m)))
        {
            Ok((ks, m)) => {
                model = model.or_else(|| m.clone());
                kept.extend(ks.into_iter().map(|mut k| {
                    k.reply += b.start;
                    k
                }));
            }
            Err(why) => {
                failed.push(why);
                for i in b.clone() {
                    let len = replies[i].text.lines().count();
                    if len > 0 {
                        kept.push(Kept {
                            reply: i,
                            ranges: vec![(1, len)],
                            tag: None,
                        });
                    }
                }
            }
        }
    }
    if failed.len() == batches.len() {
        return Err(failed
            .into_iter()
            .next()
            .unwrap_or_else(|| "nothing to trim".to_string()));
    }
    Ok(Merged {
        kept,
        model,
        failed,
    })
}

/// [`merge`]'s result: the kept lines over all replies, the first model named, and why each
/// failed call failed.
#[derive(Debug)]
pub struct Merged {
    pub kept: Vec<Kept>,
    pub model: Option<String>,
    pub failed: Vec<String>,
}

/// What one kept reply carries: merged, 1-based inclusive line ranges and an optional tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    /// Index into the replies sent to the model (0-based).
    pub reply: usize,
    pub ranges: Vec<(usize, usize)>,
    pub tag: Option<&'static str>,
}

/// The replies the model sees: all but one still being written.
pub fn sendable(replies: &[Reply]) -> Vec<&Reply> {
    replies.iter().filter(|r| !r.in_progress).collect()
}

/// The user message: your prompt the span starts from, then each reply with numbered lines,
/// each reply to a prompt of yours preceded by that prompt (context the model must not pick).
pub fn prompt(replies: &[&Reply], your_prompt: Option<&str>) -> String {
    fn nonempty(p: Option<&str>) -> Option<&str> {
        p.map(str::trim).filter(|p| !p.is_empty())
    }
    let mut out = String::new();
    // Shown with the first reply below when that is the reply to it.
    let first_has_it = replies
        .first()
        .is_some_and(|r| nonempty(r.your_prompt.as_deref()) == nonempty(your_prompt));
    if let Some(p) = nonempty(your_prompt).filter(|_| !first_has_it) {
        out.push_str(
            "Jonathan's prompt these replies follow (context only, not selectable):\n<<<\n",
        );
        out.push_str(p);
        out.push_str("\n>>>\n\nThe replies since then:\n");
    } else {
        out.push_str("The replies:\n");
    }
    for (i, r) in replies.iter().enumerate() {
        if let Some(p) = nonempty(r.your_prompt.as_deref()) {
            out.push_str(&format!(
                "\nJonathan's prompt that R{} answers (context only, not selectable):\n<<<\n{p}\n>>>\n",
                i + 1
            ));
        }
        let label = r.header.trim_start_matches('#').trim();
        out.push_str(&format!("\nR{} {label}\n", i + 1));
        for (j, line) in r.text.lines().enumerate() {
            out.push_str(&format!("L{}: {line}\n", j + 1));
        }
    }
    out
}

fn tag(v: Option<&Value>) -> Option<&'static str> {
    match v
        .and_then(Value::as_str)?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "ask" => Some("ask"),
        "blocker" => Some("blocker"),
        "done" => Some("done"),
        "fyi" => Some("fyi"),
        _ => None,
    }
}

fn json_object(answer: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(answer.trim()) {
        return Some(v);
    }
    let (a, b) = (answer.find('{')?, answer.rfind('}')?);
    (a < b)
        .then(|| serde_json::from_str(&answer[a..=b]).ok())
        .flatten()
}

fn range(v: &Value) -> Option<(i64, i64)> {
    if let Some(n) = v.as_i64() {
        return Some((n, n));
    }
    let a = v.as_array()?;
    let from = a.first()?.as_i64()?;
    let to = a.get(1).and_then(Value::as_i64).unwrap_or(from);
    Some((from.min(to), from.max(to)))
}

/// Line ranges of fenced code blocks (1-based, inclusive); an unclosed one runs to the end.
fn fences(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut open = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            match open.take() {
                None => open = Some(i + 1),
                Some(o) => out.push((o, i + 1)),
            }
        }
    }
    if let Some(o) = open {
        out.push((o, lines.len()));
    }
    out
}

/// Sorts and merges ranges, widening any that cut a code fence to the whole block.
fn normalize(mut ranges: Vec<(usize, usize)>, lines: &[&str]) -> Vec<(usize, usize)> {
    let fences = fences(lines);
    for r in ranges.iter_mut() {
        for &(a, b) in &fences {
            if r.0 <= b && a <= r.1 {
                r.0 = r.0.min(a);
                r.1 = r.1.max(b);
            }
        }
    }
    ranges.sort();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for r in ranges {
        match out.last_mut() {
            // Adjacent, or only blank lines between: one passage.
            Some(last)
                if r.0 <= last.1 + 1
                    || lines[last.1..r.0 - 1].iter().all(|l| l.trim().is_empty()) =>
            {
                last.1 = last.1.max(r.1);
            }
            _ => out.push(r),
        }
    }
    out
}

/// Parses the model's answer against the replies it was sent. Out-of-range numbers are
/// dropped or clamped; an answer with no usable selection (but a non-empty `keep`) is an error.
pub fn parse(answer: &str, replies: &[&Reply]) -> Result<Vec<Kept>, String> {
    let v = json_object(answer).ok_or("the answer was not JSON")?;
    let keep = v
        .get("keep")
        .and_then(Value::as_array)
        .ok_or("the answer had no keep list")?;
    let mut by_reply: Vec<Option<Kept>> = vec![None; replies.len()];
    for item in keep {
        let Some(n) = item.get("reply").and_then(Value::as_i64) else {
            continue;
        };
        let Some(idx) = usize::try_from(n)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .filter(|&i| i < replies.len())
        else {
            continue;
        };
        let lines: Vec<&str> = replies[idx].text.lines().collect();
        let len = lines.len();
        if len == 0 {
            continue;
        }
        let mut ranges = Vec::new();
        if item.get("all").and_then(Value::as_bool) == Some(true) {
            ranges.push((1, len));
        }
        for r in item
            .get("lines")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(range)
        {
            let (from, to) = (r.0.max(1) as usize, r.1.min(len as i64));
            if to >= 1 && from <= to as usize {
                ranges.push((from, to as usize));
            }
        }
        if ranges.is_empty() {
            continue;
        }
        let entry = by_reply[idx].get_or_insert(Kept {
            reply: idx,
            ranges: Vec::new(),
            tag: None,
        });
        entry.ranges.extend(ranges);
        entry.tag = entry.tag.or(tag(item.get("tag")));
    }
    let kept: Vec<Kept> = by_reply
        .into_iter()
        .flatten()
        .map(|mut k| {
            let lines: Vec<&str> = replies[k.reply].text.lines().collect();
            k.ranges = normalize(k.ranges, &lines);
            k
        })
        .collect();
    if kept.is_empty() && !keep.is_empty() {
        return Err("the answer selected no lines that exist".to_string());
    }
    Ok(kept)
}

fn non_blank(lines: &[&str]) -> usize {
    lines.iter().filter(|l| !l.trim().is_empty()).count()
}

/// The trimmed quote: a `#` summary line, then each kept reply with its passages (the
/// composer puts a `# …` line between two). A reply still being written follows, untrimmed.
/// `model`: which model trimmed, for the summary line.
pub fn render(replies: &[Reply], kept: &[Kept], model: Option<&str>) -> (String, Vec<QuotedReply>) {
    let sent = sendable(replies);
    let total_lines: usize = sent
        .iter()
        .map(|r| non_blank(&r.text.lines().collect::<Vec<_>>()))
        .sum();
    let mut kept_lines = 0;
    let mut out = Vec::new();
    for k in kept {
        let r = sent[k.reply];
        let lines: Vec<&str> = r.text.lines().collect();
        let mut passages = Vec::new();
        for &(a, b) in &k.ranges {
            let mut slice = &lines[a - 1..b];
            while slice.first().is_some_and(|l| l.trim().is_empty()) {
                slice = &slice[1..];
            }
            while slice.last().is_some_and(|l| l.trim().is_empty()) {
                slice = &slice[..slice.len() - 1];
            }
            if !slice.is_empty() {
                kept_lines += non_blank(slice);
                passages.push(slice.iter().map(|l| l.to_string()).collect());
            }
        }
        if passages.is_empty() {
            continue;
        }
        let mut header = r.header.clone();
        if let Some(t) = k.tag {
            header.push_str(&format!(" · {t}"));
        }
        out.push(QuotedReply {
            header,
            passages,
            in_progress: false,
        });
    }
    let by = model.map(|m| format!(" by {m}")).unwrap_or_default();
    let summary = format!(
        "# honk: trimmed{by}, kept {kept_lines} of {total_lines} lines from {} of {} replies",
        out.len(),
        sent.len()
    );
    for r in replies.iter().filter(|r| r.in_progress) {
        out.push(QuotedReply {
            header: format!("{} · untrimmed", r.header),
            passages: vec![r.text.lines().map(str::to_string).collect()],
            in_progress: true,
        });
    }
    (summary, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_split_whole_replies_by_budget() {
        let rs = [
            reply("T1", &"a".repeat(40)),
            reply("T2", &"b".repeat(40)),
            reply("T3", &"c".repeat(150)),
            reply("T4", &"d".repeat(10)),
        ];
        let sent: Vec<&Reply> = rs.iter().collect();
        assert_eq!(batches(&sent, 100), vec![0..2, 2..3, 3..4]);
        assert_eq!(batches(&sent, 1000), vec![0..4]);
        assert!(batches(&[], 100).is_empty());
    }

    #[test]
    fn batch_prompts_carry_the_prompt_before_and_say_part() {
        let mut a = reply("T1 (you)", "a");
        a.your_prompt = Some("first?".into());
        let w = reply("T2 (wake: x)", "w");
        let w2 = reply("T3 (wake: x)", "w2");
        let rs = [a, w, w2];
        let sent: Vec<&Reply> = rs.iter().collect();
        let ps = batch_prompts(&sent, Some("first?"), &[0..1, 1..3]);
        assert!(ps[0].starts_with("(Part 1 of 2"), "{}", ps[0]);
        assert!(ps[1].starts_with("(Part 2 of 2"), "{}", ps[1]);
        assert!(
            ps[1].contains(
                "prompt these replies follow (context only, not selectable):\n<<<\nfirst?\n>>>"
            ),
            "{}",
            ps[1]
        );
        assert!(
            ps[1].contains("\nR1 T2") && ps[1].contains("\nR2 T3"),
            "{}",
            ps[1]
        );
        let all = 0..3;
        let one = batch_prompts(&sent, Some("first?"), std::slice::from_ref(&all));
        assert_eq!(one, vec![prompt(&sent, Some("first?"))]);
        assert!(!ps[1].contains("prompts after these"), "{}", ps[1]);
        let mut x = reply("T4 (you)", "x");
        x.your_prompt = Some("later answer".into());
        let rs2 = [rs[0].clone(), rs[1].clone(), x];
        let sent2: Vec<&Reply> = rs2.iter().collect();
        let ps = batch_prompts(&sent2, Some("first?"), &[0..2, 2..3]);
        assert!(
            ps[0].ends_with("prompts after these replies, oldest first (context only, not selectable; what they answer he already knows):\n<<<\nlater answer\n>>>\n"),
            "{}",
            ps[0]
        );
        // Your previous prompt stays as context when the first reply answers a later one.
        let mut y = reply("T5 (you)", "y");
        y.your_prompt = Some("last".into());
        let only = [y];
        let s3: Vec<&Reply> = only.iter().collect();
        let all = 0..1;
        let p = &batch_prompts(&s3, Some("prev"), std::slice::from_ref(&all))[0];
        assert!(
            p.contains("<<<\nprev\n>>>") && p.contains("<<<\nlast\n>>>"),
            "{p}"
        );
    }

    #[test]
    fn merge_offsets_parts_and_keeps_a_failed_part_whole() {
        let rs = [
            reply("T1", "a1\na2"),
            reply("T2", "b1\nb2"),
            reply("T3", "c1\nc2\nc3"),
        ];
        let sent: Vec<&Reply> = rs.iter().collect();
        let bs = [0..1, 1..2, 2..3];
        let ok = |s: &str| Ok((s.to_string(), Some("m".to_string())));
        let Merged {
            kept,
            model,
            failed,
        } = merge(
            &sent,
            &bs,
            &[
                ok(r#"{"keep":[{"reply":1,"lines":[[2,2]]}]}"#),
                Err("timeout".into()),
                ok(r#"{"keep":[{"reply":1,"lines":[[1,1]],"tag":"ask"}]}"#),
            ],
        )
        .unwrap();
        assert_eq!(
            (model.as_deref(), failed),
            (Some("m"), vec!["timeout".to_string()])
        );
        assert_eq!(
            kept,
            vec![
                Kept {
                    reply: 0,
                    ranges: vec![(2, 2)],
                    tag: None
                },
                Kept {
                    reply: 1,
                    ranges: vec![(1, 2)],
                    tag: None
                },
                Kept {
                    reply: 2,
                    ranges: vec![(1, 1)],
                    tag: Some("ask")
                },
            ]
        );
        assert_eq!(
            merge(&sent, &bs[..1], &[ok("not json")]).unwrap_err(),
            "the answer was not JSON"
        );
    }

    #[test]
    fn prompt_shows_your_prompts_between_the_replies_once() {
        let mut a = reply("T1 (you)", "a");
        a.your_prompt = Some("first?".into());
        let w = reply("T2 (wake: x)", "w");
        let mut b = reply("T3 (you)", "b");
        b.your_prompt = Some("second?".into());
        let rs = [a, w, b];
        let sent: Vec<&Reply> = rs.iter().collect();
        let p = prompt(&sent, Some("first?"));
        assert_eq!(p.matches("first?").count(), 1, "{p}");
        assert!(p.starts_with("The replies:\n"), "{p}");
        assert!(
            p.contains(
                "prompt that R1 answers (context only, not selectable):\n<<<\nfirst?\n>>>\n\nR1 T1"
            ),
            "{p}"
        );
        assert!(
            p.contains(
                "prompt that R3 answers (context only, not selectable):\n<<<\nsecond?\n>>>\n\nR3 T3"
            ),
            "{p}"
        );
    }

    fn text((summary, replies): (String, Vec<QuotedReply>)) -> String {
        honk_wire::quote::QuoteReply {
            summary: Some(summary),
            replies,
            ..Default::default()
        }
        .composer_text()
    }

    fn reply(header: &str, text: &str) -> Reply {
        Reply {
            header: format!("# {header}"),
            text: text.to_string(),
            in_progress: false,
            your_prompt: None,
        }
    }

    /// goose renders a system prompt override as a template; a render failure silently falls
    /// back to its own prompt and the JSON contract would be lost.
    #[test]
    fn system_prompt_has_no_template_syntax() {
        for d in ["{{", "}}", "{%", "%}", "{#", "#}"] {
            assert!(!SYSTEM.contains(d), "SYSTEM contains {d}");
        }
    }

    #[test]
    fn prompt_numbers_replies_and_lines_and_carries_your_prompt() {
        let rs = [reply("T1 (wake: x)", "a\n\nb"), reply("T2 (you)", "c")];
        let sent: Vec<&Reply> = rs.iter().collect();
        let p = prompt(&sent, Some("what now?"));
        assert!(p.contains("<<<\nwhat now?\n>>>"), "{p}");
        assert!(p.contains("R1 T1 (wake: x)\nL1: a\nL2: \nL3: b\n"), "{p}");
        assert!(p.contains("R2 T2 (you)\nL1: c\n"), "{p}");
        assert!(!prompt(&sent, None).contains("<<<"));
    }

    #[test]
    fn parse_takes_ranges_all_and_tags_and_tolerates_noise() {
        let rs = [reply("a", "1\n2\n3\n4\n5"), reply("b", "x\ny")];
        let sent: Vec<&Reply> = rs.iter().collect();
        let answer = "Here you go:\n```json\n{\"keep\":[{\"reply\":1,\"lines\":[[4,9],[1,1],2],\"tag\":\"ASK\"},{\"reply\":2,\"all\":true,\"tag\":\"weird\"},{\"reply\":7,\"all\":true}],\"note\":1}\n```";
        let k = parse(answer, &sent).unwrap();
        assert_eq!(
            k,
            vec![
                Kept {
                    reply: 0,
                    ranges: vec![(1, 2), (4, 5)],
                    tag: Some("ask")
                },
                Kept {
                    reply: 1,
                    ranges: vec![(1, 2)],
                    tag: None
                },
            ]
        );
    }

    #[test]
    fn parse_merges_a_reply_named_twice_and_ranges_split_by_blank_lines() {
        let rs = [reply("a", "1\n\n3\n4")];
        let sent: Vec<&Reply> = rs.iter().collect();
        let k = parse(
            r#"{"keep":[{"reply":1,"lines":[[1,1]]},{"reply":1,"lines":[[3,3]],"tag":"fyi"}]}"#,
            &sent,
        )
        .unwrap();
        assert_eq!(k[0].ranges, vec![(1, 3)]);
        assert_eq!(k[0].tag, Some("fyi"));
    }

    #[test]
    fn parse_widens_a_range_that_cuts_a_code_fence() {
        let rs = [reply("a", "intro\n```\ncode\nmore\n```\nafter")];
        let sent: Vec<&Reply> = rs.iter().collect();
        let k = parse(r#"{"keep":[{"reply":1,"lines":[[3,3]]}]}"#, &sent).unwrap();
        assert_eq!(k[0].ranges, vec![(2, 5)]);
    }

    #[test]
    fn parse_errors() {
        let rs = [reply("a", "1")];
        let sent: Vec<&Reply> = rs.iter().collect();
        assert!(parse("I think you should read R1", &sent).is_err());
        assert!(parse(r#"{"picks":[]}"#, &sent).is_err());
        assert!(parse(r#"{"keep":[{"reply":2,"all":true}]}"#, &sent).is_err());
        assert!(parse(r#"{"keep":[{"reply":1,"lines":[[5,9]]}]}"#, &sent).is_err());
        assert_eq!(parse(r#"{"keep":[]}"#, &sent), Ok(vec![]));
    }

    #[test]
    fn render_quotes_only_source_lines_with_marked_additions() {
        let mut rs = vec![
            reply("T1 (wake: x)", "skip\nkeep one\n\nskip\nkeep two"),
            reply("T2 (swarm: y)", "noise"),
            reply("T3 (wake: z, in progress)", "partial"),
        ];
        rs[2].in_progress = true;
        let kept = vec![Kept {
            reply: 0,
            ranges: vec![(2, 2), (5, 5)],
            tag: Some("ask"),
        }];
        assert_eq!(
            text(render(&rs, &kept, Some("claude-opus-5-5"))),
            "# honk: trimmed by claude-opus-5-5, kept 2 of 5 lines from 1 of 2 replies\n\n\
             # T1 (wake: x) · ask\n> keep one\n# …\n> keep two\n\n\
             # T3 (wake: z, in progress) · untrimmed\n> partial"
        );
    }

    #[test]
    fn render_trims_blank_edges_and_keeps_inner_blank_lines() {
        let rs = vec![reply("T1 (you)", "a\n\nb\n")];
        let kept = vec![Kept {
            reply: 0,
            ranges: vec![(1, 3)],
            tag: None,
        }];
        let out = text(render(&rs, &kept, None));
        assert!(out.ends_with("# T1 (you)\n> a\n>\n> b"), "{out}");
        assert!(
            out.starts_with("# honk: trimmed, kept 2 of 2 lines"),
            "{out}"
        );
    }
}
