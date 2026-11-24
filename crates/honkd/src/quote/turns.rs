//! Rebuilds a session's user and assistant blocks from its honkd log on disk, the way honk
//! chat's transcript does from the same frames (a chunk joins the block of its message id,
//! which keeps the position where that id first appeared), without loading the session into
//! goose. Thoughts and tool calls are not kept: no `/quote` form uses them.

use honk_wire::BlockKey;
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Assistant,
}

/// One user or assistant message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub kind: Kind,
    pub text: String,
    /// Unix seconds from goose's `_meta.goose.created` (first chunk that carries it).
    pub created: Option<i64>,
}

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(default, borrow)]
    frame: Option<&'a RawValue>,
}

/// The blocks of the log at `path`, read line by line (the file is never held whole).
/// The first line is the log header; a torn last line (a write in progress) is skipped.
pub fn read(path: &Path) -> std::io::Result<Vec<Block>> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut blocks = Builder::default();
    let mut line = String::new();
    let mut first = true;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if std::mem::take(&mut first) || !line.ends_with('\n') {
            continue;
        }
        // Most lines are tool calls, thoughts and run state: skip them without parsing.
        if !line.contains("_message_chunk\"") {
            continue;
        }
        let Ok(Line { frame: Some(raw) }) = serde_json::from_str::<Line>(&line) else {
            continue;
        };
        if let Ok(frame) = serde_json::from_str::<Value>(raw.get()) {
            blocks.apply(&frame);
        }
    }
    Ok(blocks.finish())
}

#[derive(Default)]
pub struct Builder {
    blocks: Vec<Block>,
    index: HashMap<BlockKey, usize>,
}

impl Builder {
    /// Applies one logged frame.
    pub fn apply(&mut self, frame: &Value) {
        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            return;
        }
        let Some(u) = frame.pointer("/params/update") else {
            return;
        };
        let kind = match honk_wire::block_key(u) {
            Some(BlockKey::User(_)) => Kind::User,
            Some(BlockKey::Text(_)) => Kind::Assistant,
            _ => return,
        };
        let key = honk_wire::block_key(u).expect("checked");
        let idx = *self.index.entry(key).or_insert_with(|| {
            self.blocks.push(Block {
                kind,
                text: String::new(),
                created: None,
            });
            self.blocks.len() - 1
        });
        let b = &mut self.blocks[idx];
        if b.created.is_none() {
            b.created = u.pointer("/_meta/goose/created").and_then(Value::as_i64);
        }
        if let Some(t) = honk_wire::text_of(u) {
            b.text.push_str(t);
        }
    }

    pub fn finish(self) -> Vec<Block> {
        self.blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn chunk(kind: &str, id: &str, text: &str, created: Option<i64>) -> Value {
        let mut u = json!({ "sessionUpdate": kind, "messageId": id,
            "content": { "type": "text", "text": text } });
        if let Some(c) = created {
            u["_meta"] = json!({ "goose": { "created": c } });
        }
        json!({ "jsonrpc": "2.0", "method": "session/update",
            "params": { "sessionId": "s", "update": u } })
    }

    #[test]
    fn rebuilds_blocks_by_message_id_in_first_appearance_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", json!({ "epoch": 1, "resume_from": null })).unwrap();
        let frames = [
            chunk("user_message_chunk", "u1", "hel", None),
            chunk("user_message_chunk", "u1", "lo", None),
            json!({ "method": "session/update", "params": { "sessionId": "s", "update": {
                "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "x" } } }),
            chunk("agent_message_chunk", "a1", "one ", Some(5)),
            chunk("agent_thought_chunk", "th", "hmm", Some(5)),
            chunk("agent_message_chunk", "a2", "two", Some(6)),
            chunk("agent_message_chunk", "a1", "more", Some(9)),
        ];
        for (i, fr) in frames.iter().enumerate() {
            writeln!(f, "{}", json!({ "seq": i + 1, "id": null, "frame": fr })).unwrap();
        }
        // A torn final line (an append in progress) is not read.
        write!(
            f,
            "{{\"seq\":9,\"frame\":{}",
            chunk("agent_message_chunk", "a3", "x", None)
        )
        .unwrap();
        drop(f);
        let blocks = read(&path).unwrap();
        assert_eq!(
            blocks,
            vec![
                Block {
                    kind: Kind::User,
                    text: "hello".into(),
                    created: None
                },
                Block {
                    kind: Kind::Assistant,
                    text: "one more".into(),
                    created: Some(5)
                },
                Block {
                    kind: Kind::Assistant,
                    text: "two".into(),
                    created: Some(6)
                },
            ]
        );
    }
}
