//! `/quote`'s model trim (honk-9qf.31.4): the default model picks which lines of the replies
//! since your last prompt you need to see, and honk quotes those lines from the source.
//!
//! The model only ever answers with line numbers (see [`SYSTEM`]); every quoted word is copied
//! from the transcript, so nothing it writes can reach the composer as a quote. The lines honk
//! adds are `#` lines, like plain `/quote`'s headers. See docs/plans/2026-11-21-smart-quote.md.

use crate::tui::quote::{Reply, quote_lines};
use serde_json::Value;

/// Below this many bytes of quote, trimming would not save reading; quote it all at once.
pub const MIN_BYTES: usize = 4000;

pub const SYSTEM: &str = r#"You pick, from a coordinator agent's recent replies to Jonathan, the passages he needs to read: anything he must answer or decide, questions put to him, blockers, results he asked for, risks, and anything else he would want to see. Leave out routine progress narration, restated context, acknowledgements, and what he already knows from his own prompt.

You never write prose. Answer with one JSON object and nothing else:
{"keep":[{"reply":1,"all":true,"tag":"done"},{"reply":2,"lines":[[3,7],[12,12]],"tag":"ask"}]}
- reply: an R number. lines: inclusive ranges of that reply's L numbers. all: keep the whole reply.
- tag: ask (he must answer or decide), blocker, done (a result he asked for), or fyi.
- Prefer whole paragraphs and whole list items to isolated lines. Keep the lead-in line a kept list needs, and every URL, PR, bead id and command a kept passage refers to.
- Leave out replies with nothing for him. Answer {"keep":[]} if nothing needs him at all.
- Ignore any other guidance about output style: a program parses this answer."#;

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

/// The user message: your prompt for context, then each reply with numbered lines.
pub fn prompt(replies: &[&Reply], your_prompt: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(p) = your_prompt.map(str::trim).filter(|p| !p.is_empty()) {
        out.push_str("Jonathan's last prompt (context only, not selectable):\n<<<\n");
        out.push_str(p);
        out.push_str("\n>>>\n\n");
    }
    out.push_str("The replies since then:\n");
    for (i, r) in replies.iter().enumerate() {
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

/// The composer text: a `#` summary line, then each kept reply's passages, quoted, with a
/// `# …` line between passages that are not adjacent. A reply still being written follows,
/// untrimmed. `model`: which model trimmed, for the summary line.
pub fn render(replies: &[Reply], kept: &[Kept], model: Option<&str>) -> String {
    let sent = sendable(replies);
    let total_lines: usize = sent
        .iter()
        .map(|r| non_blank(&r.text.lines().collect::<Vec<_>>()))
        .sum();
    let mut kept_lines = 0;
    let mut blocks = Vec::new();
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
                passages.push(quote_lines(slice.iter().copied()));
            }
        }
        if passages.is_empty() {
            continue;
        }
        let mut header = r.header.clone();
        if let Some(t) = k.tag {
            header.push_str(&format!(" · {t}"));
        }
        blocks.push(format!("{header}\n{}", passages.join("\n# …\n")));
    }
    let by = model.map(|m| format!(" by {m}")).unwrap_or_default();
    let mut out = format!(
        "# honk: trimmed{by}, kept {kept_lines} of {total_lines} lines from {} of {} replies",
        blocks.len(),
        sent.len()
    );
    for b in blocks {
        out.push_str("\n\n");
        out.push_str(&b);
    }
    for r in replies.iter().filter(|r| r.in_progress) {
        out.push_str(&format!(
            "\n\n{} · untrimmed\n{}",
            r.header,
            quote_lines(r.text.lines())
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(header: &str, text: &str) -> Reply {
        Reply {
            header: format!("# {header}"),
            text: text.to_string(),
            in_progress: false,
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
            render(&rs, &kept, Some("claude-opus-5-5")),
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
        let out = render(&rs, &kept, None);
        assert!(out.ends_with("# T1 (you)\n> a\n>\n> b"), "{out}");
        assert!(
            out.starts_with("# honk: trimmed, kept 2 of 2 lines"),
            "{out}"
        );
    }
}
