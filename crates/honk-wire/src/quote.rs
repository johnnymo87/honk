//! `_honk/quote` (honk-9qf.31.7): wire types shared by honkd (which picks and trims) and its
//! clients (honk chat's `/quote`, pigeon's Slack `/quote`). See docs/chat.md, `_honk/quote`.

use serde::{Deserialize, Serialize};

pub const METHOD: &str = "_honk/quote";
pub const CANCEL: &str = "_honk/quote/cancel";

/// Which replies to quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pick {
    /// Every reply from your last prompt on, the reply to it included.
    Since,
    /// `Since`, counted from your previous prompt (the one before your last).
    Prev,
    /// The last `n` replies.
    Last { n: usize },
}

impl Pick {
    /// For notices: "since your last prompt", "the last 5 replies".
    pub fn describe(self) -> String {
        match self {
            Pick::Last { n: 1 } => "the last reply".into(),
            Pick::Last { n } => format!("the last {n} replies"),
            Pick::Since => "since your last prompt".into(),
            Pick::Prev => "since your previous prompt".into(),
        }
    }

    /// The untrimmed `/quote` command for the same replies.
    pub fn full_form(self) -> String {
        match self {
            Pick::Last { n } => format!("/quote full {n}"),
            Pick::Since => "/quote full".to_string(),
            Pick::Prev => "/quote full prev".to_string(),
        }
    }
}

/// `_honk/quote` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteReq {
    pub session_id: String,
    pub pick: Pick,
    /// Untrimmed: no model call.
    #[serde(default)]
    pub full: bool,
    /// session-tags tag for the model calls' spend; overrides the quoted session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// `_honk/quote/cancel {quoteId}` cancels the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_id: Option<String>,
}

/// Why nothing was quoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Empty {
    /// No turn has reply text.
    NoReplies,
    /// `Since`/`Prev` and no prompt of yours (and no replies to fall back to).
    NeverSpoke,
    /// No turn from the prompt counted from on has reply text yet.
    NothingSince,
    /// `Prev`, but only one prompt of yours (and nothing since it).
    OnePrompt,
    /// The model kept nothing.
    NothingKept,
}

/// One quoted reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotedReply {
    /// The composer's `#` header line: `# MM-DD HH:MM (trigger)`, plus ` · <tag>` when
    /// trimmed or ` · untrimmed` for a reply still streaming in a trimmed quote.
    pub header: String,
    /// Kept runs of the reply's own lines, verbatim and unquoted, oldest first. honk chat
    /// puts a `# …` line between two passages.
    pub passages: Vec<Vec<String>>,
    pub in_progress: bool,
}

/// `_honk/quote` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteReply {
    /// honk's first `#` line when trimmed (which model, how much it kept); null otherwise.
    pub summary: Option<String>,
    pub replies: Vec<QuotedReply>,
    pub notices: Vec<String>,
    pub warnings: Vec<String>,
    pub empty: Option<Empty>,
    /// The model that trimmed, if one did.
    pub model: Option<String>,
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

impl QuoteReply {
    /// The text honk chat loads into its composer: the summary line, then each reply's header
    /// and its passages quoted, with `# …` between passages. Empty when nothing was quoted.
    pub fn composer_text(&self) -> String {
        let blocks = self.replies.iter().map(|r| {
            let passages: Vec<String> = r
                .passages
                .iter()
                .map(|p| quote_lines(p.iter().map(String::as_str)))
                .collect();
            format!("{}\n{}", r.header, passages.join("\n# …\n"))
        });
        self.summary
            .iter()
            .cloned()
            .chain(blocks)
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_parses_the_contract_shapes() {
        let r: QuoteReq = serde_json::from_value(json!({
            "sessionId": "s", "pick": {"kind": "last", "n": 5}, "full": true,
            "tag": "pigeon", "quoteId": "q"
        }))
        .unwrap();
        assert_eq!(r.pick, Pick::Last { n: 5 });
        assert!(r.full);
        assert_eq!(r.tag.as_deref(), Some("pigeon"));
        let r: QuoteReq =
            serde_json::from_value(json!({"sessionId": "s", "pick": {"kind": "since"}})).unwrap();
        assert_eq!((r.pick, r.full, r.tag), (Pick::Since, false, None));
        let r: QuoteReq =
            serde_json::from_value(json!({"sessionId": "s", "pick": {"kind": "prev"}})).unwrap();
        assert_eq!(r.pick, Pick::Prev);
    }

    #[test]
    fn reply_serializes_camel_case_and_snake_case_empty() {
        let r = QuoteReply {
            empty: Some(Empty::NothingSince),
            replies: vec![QuotedReply {
                header: "# h".into(),
                passages: vec![],
                in_progress: true,
            }],
            ..Default::default()
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["empty"], "nothing_since");
        assert_eq!(v["replies"][0]["inProgress"], true);
        assert_eq!(v["summary"], serde_json::Value::Null);
    }

    #[test]
    fn composer_text_joins_summary_replies_and_passages() {
        let r = QuoteReply {
            summary: Some("# honk: trimmed".into()),
            replies: vec![
                QuotedReply {
                    header: "# T1 (you) · ask".into(),
                    passages: vec![vec!["a".into(), "".into(), "b".into()], vec!["c".into()]],
                    in_progress: false,
                },
                QuotedReply {
                    header: "# T2 (wake: x)".into(),
                    passages: vec![vec!["d".into()]],
                    in_progress: false,
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            r.composer_text(),
            "# honk: trimmed\n\n# T1 (you) · ask\n> a\n>\n> b\n# …\n> c\n\n# T2 (wake: x)\n> d"
        );
        assert_eq!(QuoteReply::default().composer_text(), "");
    }
}
