//! Terminal renderer for honk tail sessions.
//!
//! Handles streaming updates, tool progress/completion, user prompt echoes,
//! and delta/rebuild reconciliation across resets.
//!
//! When `text_only` is true, only agent message text chunks (`agent_message_chunk` /
//! `BlockKey::Text`) are emitted. User messages (`> ...`), tool calls (`▸ ...`),
//! tool status marks (` ✓`/` ✗`), thoughts, and transcript rebuild markers
//! (`\n[honk: transcript rebuilt]\n`) are suppressed. This produces output that
//! exactly matches the turn text returned in export.

use honk_wire::BlockKey;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, Write};

pub struct Renderer {
    pub text_only: bool,
    pub thoughts: bool,
    printed: HashMap<BlockKey, String>,
    is_resetting: bool,
    resets: u32,
    shadow: HashMap<BlockKey, String>,
    shadow_order: Vec<BlockKey>,
    tool_titles: HashMap<String, String>,
    tool_statuses: HashMap<String, String>,
    shadow_tool_titles: HashMap<String, String>,
    shadow_tool_statuses: HashMap<String, String>,
    live_user_raw: HashMap<String, String>,
    shadow_user_raw: HashMap<String, String>,
    user_line_open: bool,
    open_user_id: Option<String>,
    rebuild_marker_printed: bool,
}

fn format_user_line(text: &str) -> String {
    if text.ends_with('\n') {
        format!("> {text}")
    } else {
        format!("> {text}\n")
    }
}

fn format_tool_line(title: &str, status: Option<&str>) -> String {
    match status {
        Some("completed" | "success") => format!("▸ {title} ✓\n"),
        Some("failed" | "error") => format!("▸ {title} ✗\n"),
        _ => format!("▸ {title}"),
    }
}

impl Renderer {
    pub fn new(text_only: bool, thoughts: bool) -> Self {
        Self {
            text_only,
            thoughts,
            printed: HashMap::new(),
            is_resetting: false,
            resets: 0,
            shadow: HashMap::new(),
            shadow_order: Vec::new(),
            tool_titles: HashMap::new(),
            tool_statuses: HashMap::new(),
            shadow_tool_titles: HashMap::new(),
            shadow_tool_statuses: HashMap::new(),
            live_user_raw: HashMap::new(),
            shadow_user_raw: HashMap::new(),
            user_line_open: false,
            open_user_id: None,
            rebuild_marker_printed: false,
        }
    }

    fn close_user_line<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        if self.user_line_open {
            out.write_all(b"\n")?;
            out.flush()?;
            if let Some(id) = self.open_user_id.take() {
                self.printed
                    .entry(BlockKey::User(id))
                    .or_default()
                    .push('\n');
            }
            self.user_line_open = false;
        }
        Ok(())
    }

    pub fn flush_open_line<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.close_user_line(out)
    }

    pub fn reset_count(&self) -> u32 {
        self.resets
    }

    pub fn handle_frame<W: Write>(&mut self, frame: &Value, out: &mut W) -> io::Result<()> {
        let method = frame.get("method").and_then(Value::as_str);
        match method {
            Some(honk_wire::method::RESET) => {
                self.is_resetting = true;
                self.resets += 1;
                self.shadow.clear();
                self.shadow_order.clear();
                self.shadow_tool_titles.clear();
                self.shadow_tool_statuses.clear();
                self.shadow_user_raw.clear();
                self.live_user_raw.clear();
                self.user_line_open = false;
                self.open_user_id = None;
                self.rebuild_marker_printed = false;
                Ok(())
            }
            Some(honk_wire::method::REPLAY_DONE) => {
                if !self.is_resetting {
                    return Ok(());
                }
                for key in &self.shadow_order {
                    let shadow_str = self.shadow.get(key).map(|s| s.as_str()).unwrap_or("");
                    match self.printed.get(key) {
                        None => {
                            out.write_all(shadow_str.as_bytes())?;
                            self.printed.insert(key.clone(), shadow_str.to_string());
                        }
                        Some(printed_str) => {
                            let (shadow_cmp, printed_cmp, is_user) = match key {
                                BlockKey::User(_) => (
                                    shadow_str.trim_end_matches('\n'),
                                    printed_str.trim_end_matches('\n'),
                                    true,
                                ),
                                _ => (shadow_str, printed_str.as_str(), false),
                            };

                            if let Some(suffix) = shadow_cmp.strip_prefix(printed_cmp) {
                                if !suffix.is_empty() {
                                    out.write_all(suffix.as_bytes())?;
                                    if is_user {
                                        out.write_all(b"\n")?;
                                    }
                                }
                                self.printed.insert(key.clone(), shadow_str.to_string());
                            } else {
                                if !self.text_only && !self.rebuild_marker_printed {
                                    out.write_all(b"\n[honk: transcript rebuilt]\n")?;
                                    self.rebuild_marker_printed = true;
                                }
                                out.write_all(shadow_str.as_bytes())?;
                                self.printed.insert(key.clone(), shadow_str.to_string());
                            }
                        }
                    }
                }
                out.flush()?;
                self.is_resetting = false;
                self.shadow.clear();
                self.shadow_order.clear();
                self.shadow_tool_titles.clear();
                self.shadow_tool_statuses.clear();
                self.shadow_user_raw.clear();
                Ok(())
            }
            Some("session/update") => {
                let Some(u) = frame.pointer("/params/update") else {
                    return Ok(());
                };
                let Some(key) = honk_wire::block_key(u) else {
                    return Ok(());
                };

                // Close open user line if block is not the same user block
                if !self.is_resetting {
                    let is_same_user_block = match &key {
                        BlockKey::User(id) => self.open_user_id.as_deref() == Some(id),
                        _ => false,
                    };
                    if self.user_line_open && !is_same_user_block {
                        self.close_user_line(out)?;
                    }
                }

                // Filter blocks based on flags
                match &key {
                    BlockKey::Text(_) => {}
                    BlockKey::Thought(_) => {
                        if !self.thoughts || self.text_only {
                            return Ok(());
                        }
                    }
                    BlockKey::User(_) | BlockKey::Tool(_) => {
                        if self.text_only {
                            return Ok(());
                        }
                    }
                    BlockKey::Notice(_) => {}
                }

                if self.is_resetting {
                    self.apply_shadow(&key, u);
                } else {
                    self.apply_live(&key, u, out)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn apply_shadow(&mut self, key: &BlockKey, u: &Value) {
        if !self.shadow_order.contains(key) {
            self.shadow_order.push(key.clone());
        }
        match key {
            BlockKey::Text(_) | BlockKey::Thought(_) => {
                if let Some(t) = honk_wire::text_of(u) {
                    self.shadow.entry(key.clone()).or_default().push_str(t);
                }
            }
            BlockKey::User(id) => {
                if let Some(t) = honk_wire::text_of(u) {
                    let raw = self.shadow_user_raw.entry(id.clone()).or_default();
                    raw.push_str(t);
                    let user_str = format_user_line(raw);
                    self.shadow.insert(key.clone(), user_str);
                }
            }
            BlockKey::Tool(id) => {
                if let Some(title) = u.get("title").and_then(Value::as_str) {
                    self.shadow_tool_titles
                        .insert(id.clone(), title.to_string());
                }
                if let Some(status) = u.get("status").and_then(Value::as_str) {
                    self.shadow_tool_statuses
                        .insert(id.clone(), status.to_string());
                }
                let title = self
                    .shadow_tool_titles
                    .get(id)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let status = self.shadow_tool_statuses.get(id).map(|s| s.as_str());
                let tool_str = format_tool_line(title, status);
                self.shadow.insert(key.clone(), tool_str);
            }
            BlockKey::Notice(_) => {}
        }
    }

    fn apply_live<W: Write>(&mut self, key: &BlockKey, u: &Value, out: &mut W) -> io::Result<()> {
        match key {
            BlockKey::Text(_) | BlockKey::Thought(_) => {
                if let Some(t) = honk_wire::text_of(u) {
                    out.write_all(t.as_bytes())?;
                    out.flush()?;
                    self.printed.entry(key.clone()).or_default().push_str(t);
                }
            }
            BlockKey::User(id) => {
                if let Some(t) = honk_wire::text_of(u) {
                    let raw = self.live_user_raw.entry(id.clone()).or_default();
                    if raw.is_empty() {
                        out.write_all(b"> ")?;
                        self.printed.entry(key.clone()).or_default().push_str("> ");
                    }
                    out.write_all(t.as_bytes())?;
                    out.flush()?;
                    raw.push_str(t);
                    self.printed.entry(key.clone()).or_default().push_str(t);
                    self.user_line_open = !t.ends_with('\n');
                    self.open_user_id = if self.user_line_open {
                        Some(id.clone())
                    } else {
                        None
                    };
                }
            }
            BlockKey::Tool(id) => {
                if let Some(title) = u.get("title").and_then(Value::as_str) {
                    self.tool_titles.insert(id.clone(), title.to_string());
                }
                if let Some(status) = u.get("status").and_then(Value::as_str) {
                    self.tool_statuses.insert(id.clone(), status.to_string());
                }
                let title = self.tool_titles.get(id).map(|s| s.as_str()).unwrap_or("");
                let status = self.tool_statuses.get(id).map(|s| s.as_str());
                let target_str = format_tool_line(title, status);
                let printed_so_far = self.printed.entry(key.clone()).or_default();
                if target_str.starts_with(printed_so_far.as_str()) {
                    let suffix = &target_str[printed_so_far.len()..];
                    if !suffix.is_empty() {
                        out.write_all(suffix.as_bytes())?;
                        out.flush()?;
                        printed_so_far.push_str(suffix);
                    }
                } else {
                    out.write_all(target_str.as_bytes())?;
                    out.flush()?;
                    *printed_so_far = target_str;
                }
            }
            BlockKey::Notice(_) => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_update_frame(session_update: &str, content: Value, extra: Option<Value>) -> Value {
        let mut update = json!({
            "sessionUpdate": session_update,
        });
        if let Some(c) = content.as_object() {
            for (k, v) in c {
                update[k] = v.clone();
            }
        }
        if let Some(e) = extra.and_then(|v| v.as_object().cloned()) {
            for (k, v) in e {
                update[k] = v;
            }
        }
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": update,
            }
        })
    }

    fn make_text_frame(msg_id: &str, text: &str) -> Value {
        make_update_frame(
            "agent_message_chunk",
            json!({
                "messageId": msg_id,
                "content": { "type": "text", "text": text },
            }),
            None,
        )
    }

    fn make_user_frame(msg_id: &str, text: &str) -> Value {
        make_update_frame(
            "user_message_chunk",
            json!({
                "messageId": msg_id,
                "content": { "type": "text", "text": text },
            }),
            None,
        )
    }

    fn make_tool_call_frame(tool_id: &str, title: &str) -> Value {
        make_update_frame(
            "tool_call",
            json!({
                "toolCallId": tool_id,
                "title": title,
            }),
            None,
        )
    }

    fn make_tool_update_frame(tool_id: &str, status: &str) -> Value {
        make_update_frame(
            "tool_call_update",
            json!({
                "toolCallId": tool_id,
                "status": status,
            }),
            None,
        )
    }

    fn make_reset_frame() -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "_honk/reset",
            "params": {
                "sessionId": "s1",
                "epoch": 2,
            }
        })
    }

    fn make_replay_done_frame() -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "_honk/replay_done",
            "params": {
                "sessionId": "s1",
                "epoch": 2,
                "seq": 10,
            }
        })
    }

    #[test]
    fn test_streaming() {
        let mut r = Renderer::new(false, false);
        let mut out = Vec::new();

        r.handle_frame(&make_text_frame("m1", "Hello "), &mut out)
            .unwrap();
        r.handle_frame(&make_text_frame("m1", "world!"), &mut out)
            .unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "Hello world!");
    }

    #[test]
    fn test_suffix_after_reset() {
        let mut r = Renderer::new(false, false);
        let mut out = Vec::new();

        // 1. Live streaming
        r.handle_frame(&make_text_frame("m1", "Hello "), &mut out)
            .unwrap();
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "Hello ");

        // 2. Reset arrives
        r.handle_frame(&make_reset_frame(), &mut out).unwrap();

        // 3. Replay with longer text
        r.handle_frame(&make_text_frame("m1", "Hello world!"), &mut out)
            .unwrap();

        // While resetting, nothing new should be emitted yet
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "Hello ");

        // 4. Replay done
        r.handle_frame(&make_replay_done_frame(), &mut out).unwrap();

        // Only suffix "world!" should be emitted
        assert_eq!(String::from_utf8(out).unwrap(), "Hello world!");
    }

    #[test]
    fn test_identical_replay_prints_nothing() {
        let mut r = Renderer::new(false, false);
        let mut out = Vec::new();

        // 1. Live streaming
        r.handle_frame(&make_text_frame("m1", "Hello world!"), &mut out)
            .unwrap();
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "Hello world!");

        // 2. Reset and replay identical content
        r.handle_frame(&make_reset_frame(), &mut out).unwrap();
        r.handle_frame(&make_text_frame("m1", "Hello world!"), &mut out)
            .unwrap();
        r.handle_frame(&make_replay_done_frame(), &mut out).unwrap();

        // Total output is still identical - nothing printed on replay
        assert_eq!(String::from_utf8(out).unwrap(), "Hello world!");
    }

    #[test]
    fn test_divergent_replay_prints_marker_once() {
        let mut r = Renderer::new(false, false);
        let mut out = Vec::new();

        // 1. Live streaming two blocks
        r.handle_frame(&make_text_frame("m1", "Hello "), &mut out)
            .unwrap();
        r.handle_frame(&make_text_frame("m2", "there"), &mut out)
            .unwrap();
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "Hello there");

        // 2. Reset and replay completely different content
        r.handle_frame(&make_reset_frame(), &mut out).unwrap();
        r.handle_frame(&make_text_frame("m1", "Goodbye "), &mut out)
            .unwrap();
        r.handle_frame(&make_text_frame("m2", "friends"), &mut out)
            .unwrap();
        r.handle_frame(&make_replay_done_frame(), &mut out).unwrap();

        let s = String::from_utf8(out).unwrap();
        let marker = "\n[honk: transcript rebuilt]\n";
        let marker_count = s.matches(marker).count();
        assert_eq!(
            marker_count, 1,
            "rebuilt marker should be printed exactly once"
        );
        assert!(s.ends_with("Goodbye friends"));
    }

    #[test]
    fn test_tool_check_cross_and_user_lines() {
        let mut r = Renderer::new(false, false);
        let mut out = Vec::new();

        // User line
        r.handle_frame(&make_user_frame("u1", "list files"), &mut out)
            .unwrap();
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "> list files");

        // Tool call 1: completes (closes user line with newline)
        r.handle_frame(&make_tool_call_frame("t1", "shell: ls"), &mut out)
            .unwrap();
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            "> list files\n▸ shell: ls"
        );

        r.handle_frame(&make_tool_update_frame("t1", "completed"), &mut out)
            .unwrap();
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            "> list files\n▸ shell: ls ✓\n"
        );

        // Tool call 2: fails
        r.handle_frame(&make_tool_call_frame("t2", "shell: cat missing"), &mut out)
            .unwrap();
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            "> list files\n▸ shell: ls ✓\n▸ shell: cat missing"
        );

        r.handle_frame(&make_tool_update_frame("t2", "failed"), &mut out)
            .unwrap();
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            "> list files\n▸ shell: ls ✓\n▸ shell: cat missing ✗\n"
        );
    }

    #[test]
    fn test_text_only_mode() {
        let mut r = Renderer::new(true, false);
        let mut out = Vec::new();

        // User line, tool calls, and text
        r.handle_frame(&make_user_frame("u1", "list files"), &mut out)
            .unwrap();
        r.handle_frame(&make_tool_call_frame("t1", "shell: ls"), &mut out)
            .unwrap();
        r.handle_frame(&make_tool_update_frame("t1", "completed"), &mut out)
            .unwrap();
        r.handle_frame(&make_text_frame("m1", "Here is the result."), &mut out)
            .unwrap();

        // Divergent reset
        r.handle_frame(&make_reset_frame(), &mut out).unwrap();
        r.handle_frame(&make_text_frame("m1", "Rebuilt result."), &mut out)
            .unwrap();
        r.handle_frame(&make_replay_done_frame(), &mut out).unwrap();

        let s = String::from_utf8(out).unwrap();
        // In text_only mode, no user line (>), no tool line (▸), no checkmark (✓), no rebuild marker
        assert!(!s.contains('>'));
        assert!(!s.contains('▸'));
        assert!(!s.contains('✓'));
        assert!(!s.contains("[honk: transcript rebuilt]"));
        assert_eq!(s, "Here is the result.Rebuilt result.");
    }

    #[test]
    fn test_thoughts_flag() {
        let thought_frame = make_update_frame(
            "agent_thought_chunk",
            json!({
                "messageId": "th1",
                "content": { "type": "text", "text": "thinking deep" },
            }),
            None,
        );

        // thoughts disabled: should output nothing
        let mut r1 = Renderer::new(false, false);
        let mut out1 = Vec::new();
        r1.handle_frame(&thought_frame, &mut out1).unwrap();
        assert_eq!(String::from_utf8(out1).unwrap(), "");

        // thoughts enabled: should output thought text
        let mut r2 = Renderer::new(false, true);
        let mut out2 = Vec::new();
        r2.handle_frame(&thought_frame, &mut out2).unwrap();
        assert_eq!(String::from_utf8(out2).unwrap(), "thinking deep");
    }

    #[test]
    fn test_chunked_user_message_does_not_duplicate() {
        let mut r = Renderer::new(false, false);
        let mut out = Vec::new();

        r.handle_frame(&make_user_frame("u1", "hello "), &mut out)
            .unwrap();
        r.handle_frame(&make_user_frame("u1", "world"), &mut out)
            .unwrap();
        // Agent message closes the user line block
        r.handle_frame(&make_text_frame("m1", "reply"), &mut out)
            .unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "> hello world\nreply");
    }

    #[test]
    fn test_user_line_cleared_on_reset() {
        let mut r = Renderer::new(false, false);
        let mut out = Vec::new();

        r.handle_frame(&make_user_frame("u1", "hello"), &mut out)
            .unwrap();
        r.handle_frame(&make_reset_frame(), &mut out).unwrap();
        r.handle_frame(&make_user_frame("u1", "hello world"), &mut out)
            .unwrap();
        r.handle_frame(&make_replay_done_frame(), &mut out).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "> hello world\n");
    }
}
