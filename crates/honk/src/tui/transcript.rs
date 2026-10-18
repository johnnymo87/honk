//! Transcript model and rendering for honk chat TUI.
//!
//! Pure, no terminal I/O.
//! Handles streaming updates, markdown rendering via tui-markdown, layout caching,
//! viewport virtualization, and reset/replay reconciliation.

use honk_wire::BlockKey;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::chat::driver::{ConnectionState, UiEvent};
use crate::chat::types::{Notice, NoticeKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockKind {
    User,
    Assistant,
    Thought,
    Tool,
    Notice,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub key: BlockKey,
    pub kind: BlockKind,
    pub revision: u64,
    pub text: String,
    pub title: String,
    pub status: String,
    pub tool_kind: Option<String>,
    pub input_text: String,
    pub output_text: String,
    pub notification_line: Option<String>,
    pub notice: Option<Notice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExpansionFlags {
    pub thoughts: bool,
    pub tools: bool,
    pub json: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LayoutKey {
    pub key: BlockKey,
    pub width: u16,
    pub flags: ExpansionFlags,
}

#[derive(Clone, Debug)]
pub struct CachedBlock {
    pub revision: u64,
    pub height: usize,
    pub lines: Arc<[Line<'static>]>,
}

#[derive(Clone, Debug)]
struct CachedPrefixSums {
    width: u16,
    flags: ExpansionFlags,
    sums: Vec<usize>,
    valid_up_to: usize,
}

pub struct Transcript {
    pub blocks: Vec<Block>,
    pub block_indices: HashMap<BlockKey, usize>,
    pub thoughts_expanded: bool,
    pub tools_expanded: bool,
    pub json_expanded: bool,
    pub active_run: Option<String>,
    pub usage: Option<honk_wire::Usage>,
    pub layout_cache: HashMap<LayoutKey, CachedBlock>,
    pub render_count: usize,
    last_width: Option<u16>,
    cached_prefix_sums: Option<CachedPrefixSums>,
    pending_rebuild: Option<Box<Transcript>>,
    is_rebuilding: bool,
    next_notice_id: u64,
    anchor_idx_before_reset: Option<usize>,
    anchor_key_before_reset: Option<BlockKey>,
    anchor_offset_before_reset: usize,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            block_indices: HashMap::new(),
            thoughts_expanded: false,
            tools_expanded: false,
            json_expanded: false,
            active_run: None,
            usage: None,
            layout_cache: HashMap::new(),
            render_count: 0,
            last_width: None,
            cached_prefix_sums: None,
            pending_rebuild: None,
            is_rebuilding: false,
            next_notice_id: 1,
            anchor_idx_before_reset: None,
            anchor_key_before_reset: None,
            anchor_offset_before_reset: 0,
        }
    }

    fn invalidate_prefix_sums_from(&mut self, idx: usize) {
        if let Some(ref mut c) = self.cached_prefix_sums {
            c.valid_up_to = c.valid_up_to.min(idx);
        }
    }

    pub fn apply_frame(&mut self, frame: &Value) -> bool {
        match frame.get("method").and_then(Value::as_str) {
            Some(honk_wire::method::RESET) => {
                self.is_rebuilding = true;
                let mut fresh = Transcript::new();
                fresh.thoughts_expanded = self.thoughts_expanded;
                fresh.tools_expanded = self.tools_expanded;
                fresh.json_expanded = self.json_expanded;
                fresh.active_run = self.active_run.clone();
                fresh.next_notice_id = self.next_notice_id;
                self.pending_rebuild = Some(Box::new(fresh));
                false
            }
            Some(honk_wire::method::REPLAY_DONE) => {
                if let Some(mut fresh) = self.pending_rebuild.take() {
                    for b in &self.blocks {
                        if b.kind == BlockKind::Notice {
                            let idx = fresh.blocks.len();
                            fresh.blocks.push(b.clone());
                            fresh.block_indices.insert(b.key.clone(), idx);
                        }
                    }
                    self.blocks = fresh.blocks;
                    self.block_indices = fresh.block_indices;
                    self.usage = fresh.usage.or(self.usage.take());
                    self.layout_cache.clear();
                    self.cached_prefix_sums = None;
                    self.is_rebuilding = false;
                    self.next_notice_id = self.next_notice_id.max(fresh.next_notice_id);
                    true
                } else {
                    false
                }
            }
            Some("session/update") => {
                if self.is_rebuilding {
                    if let Some(ref mut fresh) = self.pending_rebuild {
                        fresh.apply_session_update(frame);
                    }
                } else {
                    self.apply_session_update(frame);
                }
                false
            }
            _ => false,
        }
    }

    fn apply_session_update(&mut self, frame: &Value) {
        let Some(u) = frame.pointer("/params/update") else {
            return;
        };

        if let Some(usage) = honk_wire::usage(u) {
            self.usage = Some(usage);
        }

        if let Some(state) = honk_wire::run_state(u) {
            self.active_run = state;
            if self.active_run.is_none() {
                // Run ended (activeRunId null): mark any pending/in-progress tool blocks as interrupted
                let mut first_modified = None;
                for (idx, b) in self.blocks.iter_mut().enumerate() {
                    if b.kind == BlockKind::Tool
                        && (b.status == "pending"
                            || b.status == "in_progress"
                            || b.status.is_empty())
                    {
                        b.status = "interrupted".to_string();
                        b.revision += 1;
                        if first_modified.is_none() {
                            first_modified = Some(idx);
                        }
                    }
                }
                // Invalidate the last assistant block if any, so streaming placeholder flips to markdown
                if let Some(last_ast_idx) = self
                    .blocks
                    .iter()
                    .rposition(|b| b.kind == BlockKind::Assistant)
                {
                    self.blocks[last_ast_idx].revision += 1;
                    first_modified =
                        Some(first_modified.map_or(last_ast_idx, |m| m.min(last_ast_idx)));
                }
                if let Some(idx) = first_modified {
                    self.invalidate_prefix_sums_from(idx);
                }
            }
        }

        let Some(key) = honk_wire::block_key(u) else {
            return;
        };
        let kind = match &key {
            BlockKey::User(_) => BlockKind::User,
            BlockKey::Text(_) => BlockKind::Assistant,
            BlockKey::Thought(_) => BlockKind::Thought,
            BlockKey::Tool(_) => BlockKind::Tool,
            BlockKey::Notice(_) => BlockKind::Notice,
        };

        let idx = if let Some(&i) = self.block_indices.get(&key) {
            i
        } else {
            let i = self.blocks.len();
            self.blocks.push(Block {
                key: key.clone(),
                kind,
                revision: 0,
                text: String::new(),
                title: String::new(),
                status: String::new(),
                tool_kind: None,
                input_text: String::new(),
                output_text: String::new(),
                notification_line: None,
                notice: None,
            });
            self.block_indices.insert(key.clone(), i);
            i
        };

        self.invalidate_prefix_sums_from(idx);
        self.blocks[idx].revision += 1;

        match &key {
            BlockKey::Tool(_) => {
                let block = &mut self.blocks[idx];
                if let Some(t) = u.get("title").and_then(Value::as_str) {
                    block.title = t.to_string();
                }
                if let Some(s) = u.get("status").and_then(Value::as_str) {
                    block.status = s.to_string();
                }
                if let Some(raw_in) = u.get("rawInput") {
                    block.input_text = extract_input_text(raw_in);
                    if !block.title.contains(" · ") && !block.title.is_empty() {
                        block.title =
                            honk_wire::export::format_tool_title(&block.title, Some(raw_in));
                    }
                }
                if let Some(out) = extract_output_text(u) {
                    if !block.output_text.is_empty() && !out.is_empty() {
                        block.output_text.push('\n');
                    }
                    block.output_text.push_str(&out);
                }
                if let Some(notif) = extract_notification_line(u) {
                    block.notification_line = Some(notif);
                }
            }
            _ => {
                let is_user = self.blocks[idx].kind == BlockKind::User;
                let prev_run_start = if is_user
                    && idx > 0
                    && self.blocks[idx - 1].kind == BlockKind::User
                    && self.blocks[idx - 1].text == self.blocks[idx].text
                {
                    let mut f = idx - 1;
                    while f > 0
                        && self.blocks[f - 1].kind == BlockKind::User
                        && self.blocks[f - 1].text == self.blocks[idx].text
                    {
                        f -= 1;
                    }
                    Some(f)
                } else {
                    None
                };

                if let Some(t) = honk_wire::text_of(u) {
                    self.blocks[idx].text.push_str(t);
                }

                if is_user {
                    if let Some(prs) = prev_run_start
                        && self.blocks[prs].text != self.blocks[idx].text
                    {
                        self.blocks[prs].revision += 1;
                        self.invalidate_prefix_sums_from(prs);
                    }

                    let mut first_idx = idx;
                    while first_idx > 0
                        && self.blocks[first_idx - 1].kind == BlockKind::User
                        && self.blocks[first_idx - 1].text == self.blocks[idx].text
                    {
                        first_idx -= 1;
                    }
                    if first_idx != idx {
                        self.blocks[first_idx].revision += 1;
                        self.invalidate_prefix_sums_from(first_idx);
                    }
                }
            }
        }
    }

    pub fn apply_notice(&mut self, notice: Notice) {
        let key = BlockKey::Notice(format!("notice_{}", self.next_notice_id));
        self.next_notice_id += 1;
        if let Some(ref mut fresh) = self.pending_rebuild {
            fresh.next_notice_id = fresh.next_notice_id.max(self.next_notice_id);
        }

        let clean_text = notice
            .text
            .strip_prefix("[honk] ")
            .or_else(|| notice.text.strip_prefix("[honk]"))
            .unwrap_or(&notice.text)
            .to_string();

        let idx = self.blocks.len();
        self.invalidate_prefix_sums_from(idx);
        self.blocks.push(Block {
            key: key.clone(),
            kind: BlockKind::Notice,
            revision: 1,
            text: clean_text,
            title: String::new(),
            status: String::new(),
            tool_kind: None,
            input_text: String::new(),
            output_text: String::new(),
            notification_line: None,
            notice: Some(notice),
        });
        self.block_indices.insert(key, idx);
    }

    pub fn usage(&self) -> Option<&honk_wire::Usage> {
        self.usage.as_ref()
    }

    pub fn abort_rebuild(&mut self) {
        if self.is_rebuilding {
            self.pending_rebuild = None;
            self.is_rebuilding = false;
        }
    }

    pub fn apply_ui_event(&mut self, event: &UiEvent, viewport: &mut Viewport) {
        match event {
            UiEvent::Transcript(frame) => {
                let method = frame.get("method").and_then(Value::as_str);
                if method == Some(honk_wire::method::RESET) {
                    self.anchor_idx_before_reset =
                        self.block_indices.get(&viewport.anchor.0).copied();
                    self.anchor_key_before_reset = Some(viewport.anchor.0.clone());
                    self.anchor_offset_before_reset = viewport.anchor.1;
                }

                let was_follow = viewport.follow;
                let swapped = self.apply_frame(frame);
                if swapped {
                    if was_follow {
                        viewport.follow = true;
                    } else if let Some(ref old_key) = self.anchor_key_before_reset {
                        if self.block_indices.contains_key(old_key) {
                            viewport.anchor = (old_key.clone(), self.anchor_offset_before_reset);
                        } else if !self.blocks.is_empty() {
                            let fallback_idx = self
                                .anchor_idx_before_reset
                                .unwrap_or(0)
                                .min(self.blocks.len() - 1);
                            viewport.anchor = (self.blocks[fallback_idx].key.clone(), 0);
                        }
                    } else if !self.blocks.is_empty() {
                        let fallback_idx = self
                            .block_indices
                            .get(&viewport.anchor.0)
                            .copied()
                            .unwrap_or(0)
                            .min(self.blocks.len() - 1);
                        viewport.anchor = (self.blocks[fallback_idx].key.clone(), 0);
                    }
                }
            }
            UiEvent::Notice(notice) => {
                self.apply_notice(notice.clone());
            }
            UiEvent::SessionNew { result } => {
                if let Some((text, has_failures)) = format_extension_results(result) {
                    let notice = if has_failures {
                        Notice::warning(text)
                    } else {
                        Notice::info(text)
                    };
                    self.apply_notice(notice);
                }
            }
            UiEvent::Attached { result } => {
                if let Some(text) = format_failed_extensions(result) {
                    self.apply_notice(Notice::warning(text));
                }
            }
            UiEvent::State(snapshot) if snapshot.connection != ConnectionState::Live => {
                self.abort_rebuild();
            }
            _ => {}
        }
    }

    pub fn toggle_thoughts(&mut self) {
        self.thoughts_expanded = !self.thoughts_expanded;
        self.cached_prefix_sums = None;
    }

    pub fn toggle_tools(&mut self) {
        self.tools_expanded = !self.tools_expanded;
        self.cached_prefix_sums = None;
    }

    pub fn toggle_json(&mut self) {
        self.json_expanded = !self.json_expanded;
        self.cached_prefix_sums = None;
    }

    pub fn render_block(&mut self, block_idx: usize, width: u16) -> Arc<[Line<'static>]> {
        if block_idx >= self.blocks.len() {
            return Arc::new([]);
        }
        let block = &self.blocks[block_idx];
        let flags = ExpansionFlags {
            thoughts: self.thoughts_expanded,
            tools: self.tools_expanded,
            json: self.json_expanded,
        };
        let key = LayoutKey {
            key: block.key.clone(),
            width,
            flags,
        };

        if self.last_width != Some(width) {
            self.layout_cache.retain(|k, _| k.width == width);
            self.last_width = Some(width);
        }

        if let Some(entry) = self
            .layout_cache
            .get(&key)
            .filter(|e| e.revision == block.revision)
        {
            return entry.lines.clone();
        }

        self.render_count += 1;

        let is_streaming = self.active_run.is_some() && block_idx == self.blocks.len() - 1;

        let mut lines_vec = match block.kind {
            BlockKind::User => {
                if block_idx > 0
                    && self.blocks[block_idx - 1].kind == BlockKind::User
                    && self.blocks[block_idx - 1].text == block.text
                {
                    Vec::new()
                } else {
                    let mut count = 1;
                    while block_idx + count < self.blocks.len()
                        && self.blocks[block_idx + count].kind == BlockKind::User
                        && self.blocks[block_idx + count].text == block.text
                    {
                        count += 1;
                    }
                    render_user(&block.text, count, self.json_expanded, width)
                }
            }
            BlockKind::Assistant => render_assistant(&block.text, is_streaming, width),
            BlockKind::Thought => render_thought(&block.text, self.thoughts_expanded, width),
            BlockKind::Tool => render_tool(block, self.tools_expanded, width),
            BlockKind::Notice => render_notice(block, width),
        };

        if !lines_vec.is_empty()
            && block_idx > 0
            && needs_separator(self.blocks[block_idx - 1].kind, block.kind)
        {
            lines_vec.insert(0, Line::raw(""));
        }

        let height = lines_vec.len();
        let arc_lines: Arc<[Line<'static>]> = lines_vec.into();
        self.layout_cache.insert(
            key,
            CachedBlock {
                revision: block.revision,
                height,
                lines: arc_lines.clone(),
            },
        );
        arc_lines
    }

    pub fn block_height(&mut self, block_idx: usize, width: u16) -> usize {
        if block_idx >= self.blocks.len() {
            return 0;
        }
        let block = &self.blocks[block_idx];
        let key = LayoutKey {
            key: block.key.clone(),
            width,
            flags: ExpansionFlags {
                thoughts: self.thoughts_expanded,
                tools: self.tools_expanded,
                json: self.json_expanded,
            },
        };

        if let Some(entry) = self
            .layout_cache
            .get(&key)
            .filter(|e| e.revision == block.revision)
        {
            return entry.height;
        }

        self.render_block(block_idx, width).len()
    }

    fn ensure_prefix_sums(&mut self, width: u16) {
        let flags = ExpansionFlags {
            thoughts: self.thoughts_expanded,
            tools: self.tools_expanded,
            json: self.json_expanded,
        };
        let n_blocks = self.blocks.len();
        let (valid_up_to, start_sum) = match &self.cached_prefix_sums {
            Some(c) if c.width == width && c.flags == flags => {
                let v = c.valid_up_to.min(n_blocks);
                let sum = if v < c.sums.len() { c.sums[v] } else { 0 };
                (v, sum)
            }
            _ => (0, 0),
        };

        if valid_up_to == 0 {
            self.cached_prefix_sums = Some(CachedPrefixSums {
                width,
                flags,
                sums: vec![0],
                valid_up_to: 0,
            });
        } else if let Some(ref mut c) = self.cached_prefix_sums {
            c.sums.truncate(valid_up_to + 1);
        }

        let mut current = start_sum;
        for i in valid_up_to..n_blocks {
            let h = self.block_height(i, width);
            current += h;
            self.cached_prefix_sums.as_mut().unwrap().sums.push(current);
        }
        self.cached_prefix_sums.as_mut().unwrap().valid_up_to = n_blocks;
    }

    pub fn prefix_sums(&mut self, width: u16) -> Vec<usize> {
        self.ensure_prefix_sums(width);
        self.cached_prefix_sums.as_ref().unwrap().sums.clone()
    }

    pub fn total_height(&mut self, width: u16) -> usize {
        self.ensure_prefix_sums(width);
        *self
            .cached_prefix_sums
            .as_ref()
            .unwrap()
            .sums
            .last()
            .unwrap_or(&0)
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        for (i, b) in self.blocks.iter().enumerate() {
            match b.kind {
                BlockKind::User => {
                    let is_dup = i > 0
                        && self.blocks[i - 1].kind == BlockKind::User
                        && self.blocks[i - 1].text == b.text;
                    if is_dup {
                        continue;
                    }
                    let mut count = 1;
                    while i + count < self.blocks.len()
                        && self.blocks[i + count].kind == BlockKind::User
                        && self.blocks[i + count].text == b.text
                    {
                        count += 1;
                    }
                    if count > 1 {
                        out.push_str(&format!("## User (×{count})\n\n"));
                    } else {
                        out.push_str("## User\n\n");
                    }
                    out.push_str(&b.text);
                    if !b.text.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push('\n');
                }
                BlockKind::Assistant => {
                    out.push_str("## Assistant\n\n");
                    out.push_str(&b.text);
                    if !b.text.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push('\n');
                }
                BlockKind::Thought => {
                    out.push_str("> Thought:\n");
                    for l in b.text.lines() {
                        out.push_str(&format!("> {}\n", l));
                    }
                    out.push('\n');
                }
                BlockKind::Tool => {
                    out.push_str(&format!("### Tool: {} ({})\n\n", b.title, b.status));
                    if !b.input_text.is_empty() {
                        out.push_str("**Input:**\n```\n");
                        out.push_str(&b.input_text);
                        out.push_str("\n```\n\n");
                    }
                    if !b.output_text.is_empty() {
                        out.push_str("**Output:**\n```\n");
                        out.push_str(&b.output_text);
                        out.push_str("\n```\n\n");
                    }
                }
                BlockKind::Notice => {
                    out.push_str(&format!("> [honk] {}\n\n", b.text));
                }
            }
        }
        out
    }

    pub fn last_assistant_text(&self) -> Option<&str> {
        self.blocks
            .iter()
            .rev()
            .find(|b| b.kind == BlockKind::Assistant)
            .map(|b| b.text.as_str())
    }
}

fn extract_input_text(val: &Value) -> String {
    if let Some(s) = val.as_str() {
        s.to_string()
    } else if val.is_object() || val.is_array() {
        serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string())
    } else {
        val.to_string()
    }
}

fn extract_output_text(u: &Value) -> Option<String> {
    if let Some(content) = u.get("content") {
        if let Some(arr) = content.as_array() {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(t) = item.pointer("/content/text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                } else if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            if !parts.is_empty() {
                return Some(parts.join("\n"));
            }
        } else if let Some(s) = content.as_str() {
            return Some(s.to_string());
        }
    }

    if let Some(raw_out) = u.get("rawOutput") {
        if let Some(obj) = raw_out.as_object() {
            let stdout = obj.get("stdout").and_then(Value::as_str).unwrap_or("");
            let stderr = obj.get("stderr").and_then(Value::as_str).unwrap_or("");
            let mut s = String::new();
            if !stdout.is_empty() {
                s.push_str(stdout);
            }
            if !stderr.is_empty() {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str("stderr: ");
                s.push_str(stderr);
            }
            if !s.is_empty() {
                return Some(s);
            }
        } else if let Some(s) = raw_out.as_str() {
            return Some(s.to_string());
        } else {
            return Some(
                serde_json::to_string_pretty(raw_out).unwrap_or_else(|_| raw_out.to_string()),
            );
        }
    }

    None
}

fn extract_notification_line(u: &Value) -> Option<String> {
    let notif = u.pointer("/_meta/toolNotification")?;
    if let Some(chunks) = notif.pointer("/params/chunks").and_then(Value::as_array) {
        for chunk in chunks.iter().rev() {
            if let Some(out) = chunk.get("output").and_then(Value::as_str) {
                let trimmed = out.trim();
                if !trimmed.is_empty() {
                    let last_line = trimmed.lines().next_back().unwrap_or(trimmed);
                    return Some(last_line.trim().to_string());
                }
            }
        }
    }

    for key in &["summary", "message", "status", "text", "type"] {
        if let Some(s) = notif.get(*key).and_then(Value::as_str) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                let line = trimmed.lines().next().unwrap_or(trimmed);
                return Some(line.to_string());
            }
        }
    }

    None
}

// Line wrapping helpers
struct Token {
    text: String,
    style: Style,
    is_whitespace: bool,
    is_newline: bool,
    width: usize,
}

fn detect_hanging_indent(line: &Line) -> usize {
    if line.spans.is_empty() {
        return 0;
    }

    let mut span_idx = 0;
    let mut base = 0;

    // Check for user prompt prefix ("> " or "  ")
    if let Some(first) = line.spans.first()
        && (first.content == "> "
            || (first.content == "  "
                && line.spans.len() > 1
                && (line.spans[1].content.ends_with(": ")
                    || line.spans[1].content.starts_with("• "))))
    {
        base = 2;
        span_idx = 1;
    }

    if span_idx >= line.spans.len() {
        return base;
    }

    let first_text = line.spans[span_idx].content.as_ref();

    // Bullets
    if first_text.starts_with("  • ") {
        return base + 4;
    }
    if first_text.starts_with("    • ") {
        return base + 6;
    }
    if first_text.starts_with("      • ") {
        return base + 8;
    }
    if first_text.starts_with("• ") {
        return base + 2;
    }

    // Key: value lines (e.g. "key: ")
    if first_text.ends_with(": ") {
        return base + unicode_width::UnicodeWidthStr::width(first_text);
    }

    // Indented sub-block markers
    if first_text == "  - " || first_text == "    " || first_text == "  " || first_text == "      "
    {
        let lead_w = unicode_width::UnicodeWidthStr::width(first_text);
        if span_idx + 1 < line.spans.len() {
            let second_text = line.spans[span_idx + 1].content.as_ref();
            if second_text.ends_with(": ") {
                return base + lead_w + unicode_width::UnicodeWidthStr::width(second_text);
            }
        }
        return base + lead_w;
    }

    base
}

pub fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let hanging_indent = detect_hanging_indent(&line);
    wrap_line_with_indent(line, width, hanging_indent)
}

pub fn wrap_line_with_indent(
    line: Line<'static>,
    width: usize,
    hanging_indent: usize,
) -> Vec<Line<'static>> {
    let line_style = line.style;
    let max_width = width.max(1);
    let total_width: usize = line
        .spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();

    if total_width <= max_width && !line.spans.iter().any(|s| s.content.contains('\n')) {
        return vec![line];
    }

    let hanging_indent = hanging_indent.min(max_width.saturating_sub(10));
    let indent_span = if hanging_indent > 0 {
        Some(Span::raw(" ".repeat(hanging_indent)))
    } else {
        None
    };

    // Tokenize
    let mut tokens: Vec<Token> = Vec::new();
    for span in line.spans {
        let content = span.content;
        let style = span.style;
        let mut chars = content.chars().peekable();

        while let Some(&c) = chars.peek() {
            if c == '\n' {
                chars.next();
                tokens.push(Token {
                    text: "\n".to_string(),
                    style,
                    is_whitespace: true,
                    is_newline: true,
                    width: 0,
                });
            } else if c.is_whitespace() {
                let mut ws = String::new();
                let mut w = 0;
                while let Some(&next_c) = chars.peek() {
                    if next_c != '\n' && next_c.is_whitespace() {
                        ws.push(next_c);
                        w += unicode_width::UnicodeWidthChar::width(next_c).unwrap_or(1);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token {
                    text: ws,
                    style,
                    is_whitespace: true,
                    is_newline: false,
                    width: w,
                });
            } else {
                let char_width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
                if char_width >= 2 {
                    // CJK or emoji: individual token so it can wrap freely
                    chars.next();
                    tokens.push(Token {
                        text: c.to_string(),
                        style,
                        is_whitespace: false,
                        is_newline: false,
                        width: char_width,
                    });
                } else {
                    // ASCII word
                    let mut word = String::new();
                    let mut w = 0;
                    while let Some(&next_c) = chars.peek() {
                        if !next_c.is_whitespace() {
                            let nw = unicode_width::UnicodeWidthChar::width(next_c).unwrap_or(1);
                            if nw < 2 {
                                word.push(next_c);
                                w += nw;
                                chars.next();
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    tokens.push(Token {
                        text: word,
                        style,
                        is_whitespace: false,
                        is_newline: false,
                        width: w,
                    });
                }
            }
        }
    }

    let mut result: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;
    let mut is_start_of_original_line = true;

    for token in tokens {
        if token.is_newline {
            let mut l = Line::from(std::mem::take(&mut current_spans));
            l.style = line_style;
            result.push(l);
            current_width = 0;
            is_start_of_original_line = true;
            continue;
        }

        if token.is_whitespace {
            if current_width == 0 {
                if is_start_of_original_line {
                    current_spans.push(Span::styled(token.text, token.style));
                    current_width += token.width;
                }
            } else if current_width == hanging_indent && !is_start_of_original_line {
                // Ignore inter-word whitespace right at the wrap boundary of continuation line
            } else if current_width + token.width <= max_width {
                current_spans.push(Span::styled(token.text, token.style));
                current_width += token.width;
            } else {
                let mut l = Line::from(std::mem::take(&mut current_spans));
                l.style = line_style;
                result.push(l);
                if let Some(ref ind) = indent_span {
                    current_spans.push(ind.clone());
                    current_width = hanging_indent;
                } else {
                    current_width = 0;
                }
            }
            continue;
        }

        // Non-whitespace
        is_start_of_original_line = false;
        if current_width == 0
            && hanging_indent > 0
            && !result.is_empty()
            && let Some(ref ind) = indent_span
        {
            current_spans.push(ind.clone());
            current_width = hanging_indent;
        }

        if current_width + token.width <= max_width {
            current_spans.push(Span::styled(token.text, token.style));
            current_width += token.width;
        } else {
            if current_width > 0 && (current_width > hanging_indent || result.is_empty()) {
                let mut l = Line::from(std::mem::take(&mut current_spans));
                l.style = line_style;
                result.push(l);
                current_width = 0;
            }

            if current_width == 0
                && hanging_indent > 0
                && !result.is_empty()
                && let Some(ref ind) = indent_span
            {
                current_spans.push(ind.clone());
                current_width = hanging_indent;
            }

            if current_width + token.width <= max_width {
                current_spans.push(Span::styled(token.text, token.style));
                current_width += token.width;
            } else {
                // Token itself exceeds max_width (e.g. 100-char continuous string)
                // Coalesce: split an over-wide token into chunks of available width,
                // one span per wrapped line.
                let mut chars = token.text.chars().peekable();
                while chars.peek().is_some() {
                    let avail = max_width.saturating_sub(current_width);
                    if avail == 0 {
                        let mut l = Line::from(std::mem::take(&mut current_spans));
                        l.style = line_style;
                        result.push(l);
                        if let Some(ref ind) = indent_span {
                            current_spans.push(ind.clone());
                            current_width = hanging_indent;
                        } else {
                            current_width = 0;
                        }
                        continue;
                    }
                    let mut chunk = String::new();
                    let mut chunk_w = 0;
                    while let Some(&c) = chars.peek() {
                        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
                        if chunk_w + cw <= avail {
                            chunk.push(c);
                            chunk_w += cw;
                            chars.next();
                        } else {
                            if chunk_w == 0 && current_width == 0 {
                                chunk.push(c);
                                chunk_w += cw;
                                chars.next();
                            }
                            break;
                        }
                    }
                    if !chunk.is_empty() {
                        current_spans.push(Span::styled(chunk, token.style));
                        current_width += chunk_w;
                    }
                    if current_width >= max_width && chars.peek().is_some() {
                        let mut l = Line::from(std::mem::take(&mut current_spans));
                        l.style = line_style;
                        result.push(l);
                        if let Some(ref ind) = indent_span {
                            current_spans.push(ind.clone());
                            current_width = hanging_indent;
                        } else {
                            current_width = 0;
                        }
                    }
                }
            }
        }
    }

    if !current_spans.is_empty() {
        let is_only_hanging_indent =
            hanging_indent > 0 && current_width == hanging_indent && current_spans.len() == 1;
        if !is_only_hanging_indent || result.is_empty() {
            let mut l = Line::from(current_spans);
            l.style = line_style;
            result.push(l);
        }
    }

    result
}

fn needs_separator(prev_kind: BlockKind, curr_kind: BlockKind) -> bool {
    if prev_kind == curr_kind {
        return false;
    }
    // Consecutive tools form a group: no separator between tools
    // Thought before assistant: keep tight
    if prev_kind == BlockKind::Thought && curr_kind == BlockKind::Assistant {
        return false;
    }
    true
}

#[derive(Clone, Debug, PartialEq)]
pub enum OrderedValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<OrderedValue>),
    Object(indexmap::IndexMap<String, OrderedValue>),
}

impl OrderedValue {
    pub fn is_object(&self) -> bool {
        matches!(self, OrderedValue::Object(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self, OrderedValue::Array(_))
    }
    pub fn as_object(&self) -> Option<&indexmap::IndexMap<String, OrderedValue>> {
        match self {
            OrderedValue::Object(o) => Some(o),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[OrderedValue]> {
        match self {
            OrderedValue::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            OrderedValue::String(s) => Some(s),
            _ => None,
        }
    }
}

impl std::fmt::Display for OrderedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderedValue::Null => write!(f, "null"),
            OrderedValue::Bool(b) => write!(f, "{b}"),
            OrderedValue::Number(n) => write!(f, "{n}"),
            OrderedValue::String(s) => write!(f, "{s}"),
            OrderedValue::Array(a) => {
                if a.is_empty() {
                    write!(f, "[]")
                } else {
                    write!(f, "[...]")
                }
            }
            OrderedValue::Object(o) => {
                if o.is_empty() {
                    write!(f, "{{}}")
                } else {
                    write!(f, "{{...}}")
                }
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for OrderedValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct OrderedVisitor;

        impl<'de> serde::de::Visitor<'de> for OrderedVisitor {
            type Value = OrderedValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("any valid JSON value")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                Ok(OrderedValue::Bool(v))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(OrderedValue::Number(v.into()))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(OrderedValue::Number(v.into()))
            }

            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                Ok(OrderedValue::Number(
                    serde_json::Number::from_f64(v).unwrap_or_else(|| 0.into()),
                ))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(OrderedValue::String(v.to_string()))
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
                Ok(OrderedValue::String(v))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedValue::Null)
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                serde::Deserialize::deserialize(deserializer)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedValue::Null)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut vec = Vec::new();
                while let Some(elem) = seq.next_element()? {
                    vec.push(elem);
                }
                Ok(OrderedValue::Array(vec))
            }

            fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut map = indexmap::IndexMap::new();
                while let Some((key, val)) = access.next_entry()? {
                    map.insert(key, val);
                }
                Ok(OrderedValue::Object(map))
            }
        }

        deserializer.deserialize_any(OrderedVisitor)
    }
}

fn is_empty_value(v: &OrderedValue) -> bool {
    match v {
        OrderedValue::String(s) => s.trim().is_empty(),
        OrderedValue::Array(a) => a.is_empty(),
        OrderedValue::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn to_owned_spans(spans: Vec<Span<'_>>) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect()
}

fn extract_wake_summary(val: &OrderedValue, max_len: usize) -> String {
    let mut fields = Vec::new();
    let mut current_len = 0;

    let Some(map) = val.as_object() else {
        return String::new();
    };

    let preferred_keys = [
        "program",
        "wake_reason",
        "session",
        "action",
        "task",
        "status",
        "source",
        "type",
    ];

    let mut candidate_keys: Vec<&str> = Vec::new();
    for &pk in &preferred_keys {
        if map.contains_key(pk) {
            candidate_keys.push(pk);
        }
    }
    for k in map.keys() {
        if !candidate_keys.contains(&k.as_str()) {
            candidate_keys.push(k.as_str());
        }
    }

    for k in candidate_keys {
        if let Some(v) = map.get(k)
            && let Some(s) = v.as_str()
        {
            let s_trim = s.trim();
            if !s_trim.is_empty() && s_trim.len() <= 40 && !s_trim.contains('\n') && k.len() <= 20 {
                let field = format!("{k}={s_trim}");
                let added_len = field.len() + 3; // " · "
                if current_len + added_len <= max_len {
                    current_len += added_len;
                    fields.push(field);
                    if fields.len() == 3 {
                        break;
                    }
                }
            }
        }
    }

    if fields.is_empty() {
        String::new()
    } else {
        format!(" · {}", fields.join(" · "))
    }
}

fn render_structured_json(
    val: &OrderedValue,
    width: u16,
    is_user: bool,
    count: usize,
) -> Vec<Line<'static>> {
    let mut raw_lines: Vec<(Line<'static>, usize)> = Vec::new();

    if let Some(map) = val.as_object() {
        if map.is_empty() {
            // Empty {} reply: render raw text rather than zero lines
            let line = Line::from(val.to_string());
            return if is_user {
                let mut l = Line::from(vec![
                    Span::styled(
                        "> ".to_string(),
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .fg(Color::Yellow),
                    ),
                    Span::raw(val.to_string()),
                ]);
                if count > 1 {
                    l.spans.push(Span::styled(
                        format!(" ×{count}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
                wrap_line_with_indent(l, width as usize, 2)
            } else {
                wrap_line(line, width as usize)
            };
        }
        for (key, v) in map {
            if is_empty_value(v) {
                continue;
            }
            render_value_lines(&mut raw_lines, Some(key), v, 0, 0);
        }
    } else if let Some(arr) = val.as_array() {
        if arr.is_empty() {
            // Empty [] reply: render raw text rather than zero lines
            let line = Line::from(val.to_string());
            return if is_user {
                let mut l = Line::from(vec![
                    Span::styled(
                        "> ".to_string(),
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .fg(Color::Yellow),
                    ),
                    Span::raw(val.to_string()),
                ]);
                if count > 1 {
                    l.spans.push(Span::styled(
                        format!(" ×{count}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
                wrap_line_with_indent(l, width as usize, 2)
            } else {
                wrap_line(line, width as usize)
            };
        }
        for item in arr {
            if is_empty_value(item) {
                continue;
            }
            render_array_item(&mut raw_lines, item, 0, 0);
        }
    }

    let prefix_style = Style::default()
        .add_modifier(Modifier::BOLD)
        .fg(Color::Yellow);

    let mut out = Vec::new();
    for (i, (mut line, hanging_indent)) in raw_lines.into_iter().enumerate() {
        let total_indent = if is_user {
            let pfx = if i == 0 { "> " } else { "  " };
            line.spans
                .insert(0, Span::styled(pfx.to_string(), prefix_style));
            if i == 0 && count > 1 {
                line.spans.push(Span::styled(
                    format!(" ×{count}"),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            hanging_indent + 2
        } else {
            hanging_indent
        };
        out.extend(wrap_line_with_indent(line, width as usize, total_indent));
    }
    out
}

fn render_value_lines(
    raw_lines: &mut Vec<(Line<'static>, usize)>,
    key: Option<&str>,
    v: &OrderedValue,
    depth: usize,
    indent: usize,
) {
    let label_style = if depth == 0 {
        Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::DIM)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };

    if depth >= 6 {
        let s = v.to_string();
        let truncated = if s.chars().count() > 200 {
            let t: String = s.chars().take(200).collect();
            format!("{t}…")
        } else {
            s
        };
        let prefix = " ".repeat(indent);
        let line = if let Some(k) = key {
            let h_indent = indent + k.len() + 2;
            (
                Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(format!("{k}: "), label_style),
                    Span::raw(truncated),
                ]),
                h_indent,
            )
        } else {
            (Line::from(format!("{prefix}{truncated}")), indent)
        };
        raw_lines.push(line);
        return;
    }

    match v {
        OrderedValue::Null | OrderedValue::Bool(_) | OrderedValue::Number(_) => {
            let prefix = " ".repeat(indent);
            if let Some(k) = key {
                let h_indent = indent + k.len() + 2;
                raw_lines.push((
                    Line::from(vec![
                        Span::raw(prefix),
                        Span::styled(format!("{k}: "), label_style),
                        Span::raw(v.to_string()),
                    ]),
                    h_indent,
                ));
            } else {
                raw_lines.push((
                    Line::from(vec![Span::raw(prefix), Span::raw(v.to_string())]),
                    indent,
                ));
            }
        }
        OrderedValue::String(s) => {
            let prefix = " ".repeat(indent);
            let s_str = s.as_str();
            if !s_str.contains('\n') {
                let md = tui_markdown::from_str(s_str);
                if let Some(k) = key {
                    let h_indent = indent + k.len() + 2;
                    let mut spans = vec![
                        Span::raw(prefix),
                        Span::styled(format!("{k}: "), label_style),
                    ];
                    if let Some(first) = md.lines.into_iter().next() {
                        spans.extend(to_owned_spans(first.spans));
                    }
                    raw_lines.push((Line::from(spans), h_indent));
                } else {
                    let mut spans = vec![Span::raw(prefix)];
                    if let Some(first) = md.lines.into_iter().next() {
                        spans.extend(to_owned_spans(first.spans));
                    }
                    raw_lines.push((Line::from(spans), indent));
                }
            } else {
                if let Some(k) = key {
                    raw_lines.push((
                        Line::from(vec![
                            Span::raw(prefix),
                            Span::styled(format!("{k}:"), label_style),
                        ]),
                        indent,
                    ));
                }
                let md = tui_markdown::from_str(s_str);
                let sub_prefix = " ".repeat(indent + 2);
                let sub_indent = indent + 2;
                for line in md.lines {
                    let mut spans = vec![Span::raw(sub_prefix.clone())];
                    spans.extend(to_owned_spans(line.spans));
                    raw_lines.push((Line::from(spans), sub_indent));
                }
            }
        }
        OrderedValue::Array(arr) => {
            let prefix = " ".repeat(indent);
            if let Some(k) = key {
                raw_lines.push((
                    Line::from(vec![
                        Span::raw(prefix),
                        Span::styled(format!("{k}:"), label_style),
                    ]),
                    indent,
                ));
            }
            for item in arr {
                if is_empty_value(item) {
                    continue;
                }
                render_array_item(raw_lines, item, depth + 1, indent + 2);
            }
        }
        OrderedValue::Object(sub_map) => {
            let prefix = " ".repeat(indent);
            if let Some(k) = key {
                raw_lines.push((
                    Line::from(vec![
                        Span::raw(prefix),
                        Span::styled(format!("{k}:"), label_style),
                    ]),
                    indent,
                ));
            }
            let sub_indent = if key.is_some() { indent + 2 } else { indent };
            for (sub_k, sub_v) in sub_map {
                if is_empty_value(sub_v) {
                    continue;
                }
                render_value_lines(raw_lines, Some(sub_k), sub_v, depth + 1, sub_indent);
            }
        }
    }
}

fn render_array_item(
    raw_lines: &mut Vec<(Line<'static>, usize)>,
    item: &OrderedValue,
    depth: usize,
    indent: usize,
) {
    let sub_label_style = Style::default().add_modifier(Modifier::BOLD);
    let prefix = " ".repeat(indent);

    if let Some(s) = item.as_str() {
        let md = tui_markdown::from_str(s);
        let h_indent = indent + 2;
        for (idx, line) in md.lines.into_iter().enumerate() {
            let (bullet_prefix, cur_h_indent) = if idx == 0 {
                (format!("{prefix}• "), h_indent)
            } else {
                (format!("{prefix}  "), h_indent)
            };
            let mut spans = vec![Span::raw(bullet_prefix)];
            spans.extend(to_owned_spans(line.spans));
            raw_lines.push((Line::from(spans), cur_h_indent));
        }
    } else if let Some(obj) = item.as_object() {
        let mut is_first = true;
        for (sub_k, sub_v) in obj {
            if is_empty_value(sub_v) {
                continue;
            }
            let item_prefix = if is_first {
                format!("{prefix}- ")
            } else {
                format!("{prefix}  ")
            };
            is_first = false;
            let h_indent = indent + 2 + sub_k.len() + 2;

            match sub_v {
                OrderedValue::Null | OrderedValue::Bool(_) | OrderedValue::Number(_) => {
                    raw_lines.push((
                        Line::from(vec![
                            Span::raw(item_prefix),
                            Span::styled(format!("{sub_k}: "), sub_label_style),
                            Span::raw(sub_v.to_string()),
                        ]),
                        h_indent,
                    ));
                }
                OrderedValue::String(s_sub) => {
                    if !s_sub.contains('\n') {
                        let md = tui_markdown::from_str(s_sub);
                        let mut spans = vec![
                            Span::raw(item_prefix),
                            Span::styled(format!("{sub_k}: "), sub_label_style),
                        ];
                        if let Some(first) = md.lines.into_iter().next() {
                            spans.extend(to_owned_spans(first.spans));
                        }
                        raw_lines.push((Line::from(spans), h_indent));
                    } else {
                        raw_lines.push((
                            Line::from(vec![
                                Span::raw(item_prefix),
                                Span::styled(format!("{sub_k}:"), sub_label_style),
                            ]),
                            indent + 2,
                        ));
                        let md = tui_markdown::from_str(s_sub);
                        let deep_prefix = format!("{prefix}    ");
                        let deep_indent = indent + 4;
                        for line in md.lines {
                            let mut spans = vec![Span::raw(deep_prefix.clone())];
                            spans.extend(to_owned_spans(line.spans));
                            raw_lines.push((Line::from(spans), deep_indent));
                        }
                    }
                }
                OrderedValue::Array(sub_arr) => {
                    raw_lines.push((
                        Line::from(vec![
                            Span::raw(item_prefix),
                            Span::styled(format!("{sub_k}:"), sub_label_style),
                        ]),
                        indent + 2,
                    ));
                    for el in sub_arr {
                        if is_empty_value(el) {
                            continue;
                        }
                        render_array_item(raw_lines, el, depth + 1, indent + 4);
                    }
                }
                OrderedValue::Object(deep_obj) => {
                    raw_lines.push((
                        Line::from(vec![
                            Span::raw(item_prefix),
                            Span::styled(format!("{sub_k}:"), sub_label_style),
                        ]),
                        indent + 2,
                    ));
                    for (dk, dv) in deep_obj {
                        if is_empty_value(dv) {
                            continue;
                        }
                        render_value_lines(raw_lines, Some(dk), dv, depth + 1, indent + 4);
                    }
                }
            }
        }
    } else {
        let h_indent = indent + 2;
        raw_lines.push((
            Line::from(vec![
                Span::raw(format!("{prefix}• ")),
                Span::raw(item.to_string()),
            ]),
            h_indent,
        ));
    }
}

fn render_user(raw: &str, count: usize, json_expanded: bool, width: u16) -> Vec<Line<'static>> {
    let trimmed = raw.trim();
    if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']')))
        && let Ok(val) = serde_json::from_str::<OrderedValue>(trimmed)
        && (val.is_object() || val.is_array())
    {
        if json_expanded {
            return render_structured_json(&val, width, true, count);
        } else {
            let kb = raw.len().div_ceil(1024);
            let suffix = if count > 1 {
                format!(" ×{count}")
            } else {
                String::new()
            };
            let base_str = format!("▸ structured input · {kb} KB{suffix}");
            let avail = (width as usize).saturating_sub(base_str.len() + 3);
            let summary = extract_wake_summary(&val, avail);
            let mut text = format!("▸ structured input{summary} · {kb} KB{suffix}");
            if width > 4 && text.chars().count() > width as usize {
                let take_c = (width as usize).saturating_sub(1);
                let s: String = text.chars().take(take_c).collect();
                text = format!("{s}…");
            }
            let line = Line::from(Span::styled(
                text,
                Style::default().add_modifier(Modifier::DIM),
            ));
            return wrap_line(line, width as usize);
        }
    }

    let prefix_style = Style::default()
        .add_modifier(Modifier::BOLD)
        .fg(Color::Yellow);
    let mut out = Vec::new();
    let lines: Vec<&str> = raw.lines().collect();
    if lines.is_empty() {
        let mut spans = vec![Span::styled("> ", prefix_style)];
        if count > 1 {
            spans.push(Span::styled(
                format!("×{count}"),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        out.push(Line::from(spans));
        return out;
    }

    if count > 1 {
        let first_line = lines[0];
        let spans = vec![
            Span::styled("> ", prefix_style),
            Span::raw(first_line.to_string()),
            Span::styled(
                format!(" ×{count}"),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ];
        out.extend(wrap_line_with_indent(Line::from(spans), width as usize, 2));
        return out;
    }

    for (i, l) in lines.iter().enumerate() {
        let prefix = if i == 0 { "> " } else { "  " };
        let spans = vec![
            Span::styled(prefix.to_string(), prefix_style),
            Span::raw(l.to_string()),
        ];
        out.extend(wrap_line_with_indent(Line::from(spans), width as usize, 2));
    }
    out
}

fn ends_with_unclosed_code_block(s: &str) -> bool {
    let mut open_fence: Option<(char, usize)> = None;
    for line in s.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent > 3 {
            continue;
        }
        if let Some((fence_char, fence_len)) = open_fence {
            let fence_run = trimmed.chars().take_while(|&c| c == fence_char).count();
            if fence_run >= fence_len && trimmed[fence_run..].trim().is_empty() {
                open_fence = None;
            }
        } else {
            let (ch, run) = if trimmed.starts_with("```") {
                ('`', trimmed.chars().take_while(|&c| c == '`').count())
            } else if trimmed.starts_with("~~~") {
                ('~', trimmed.chars().take_while(|&c| c == '~').count())
            } else {
                (' ', 0)
            };
            if run >= 3 {
                let info = &trimmed[run..];
                if ch != '`' || !info.contains('`') {
                    open_fence = Some((ch, run));
                }
            }
        }
    }
    open_fence.is_some()
}

fn to_owned_line(line: Line<'_>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect();
    Line::from(spans).style(line.style)
}

fn looks_like_json_opening(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.starts_with("{\"") || trimmed.starts_with("[{") || trimmed.starts_with("[\"") {
        return true;
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let r = rest.trim_start();
        if r.starts_with('{') || r.starts_with('"') {
            return true;
        }
    }
    if let Some(rest) = trimmed.strip_prefix('{') {
        let r = rest.trim_start();
        if r.starts_with('"') {
            return true;
        }
    }
    false
}

fn render_assistant(raw: &str, is_streaming: bool, width: u16) -> Vec<Line<'static>> {
    let trimmed = raw.trim();
    if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']')))
        && let Ok(val) = serde_json::from_str::<OrderedValue>(trimmed)
        && (val.is_object() || val.is_array())
    {
        return render_structured_json(&val, width, false, 1);
    }

    if is_streaming && looks_like_json_opening(raw) {
        let kb = raw.len().div_ceil(1024);
        let line = Line::from(vec![
            Span::styled("◌ ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("receiving structured reply ({kb} KB)…"),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]);
        return wrap_line(line, width as usize);
    }

    let last_nl = raw.rfind('\n');
    let mut out_lines = Vec::new();
    let (committed, tail) = match last_nl {
        Some(idx) => (&raw[..=idx], &raw[idx + 1..]),
        None => ("", raw),
    };

    if !committed.is_empty() {
        if raw.len() > 200 * 1024 {
            for line in committed.lines() {
                out_lines.extend(wrap_line(Line::from(line.to_string()), width as usize));
            }
        } else {
            let md_text = tui_markdown::from_str(committed);
            let unclosed = ends_with_unclosed_code_block(committed);
            let mut lines = md_text.lines;
            if unclosed
                && !lines.is_empty()
                && let Some(last) = lines.last()
            {
                let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
                if text.trim() == "```" || text.trim() == "~~~" {
                    lines.pop();
                }
            }
            for line in lines {
                let owned = to_owned_line(line);
                out_lines.extend(wrap_line(owned, width as usize));
            }
        }
    }

    if !tail.is_empty() {
        let tail_line = Line::from(tail.to_string());
        out_lines.extend(wrap_line(tail_line, width as usize));
    }

    out_lines
}

fn render_thought(raw: &str, expanded: bool, width: u16) -> Vec<Line<'static>> {
    let dim_italic = Style::default()
        .add_modifier(Modifier::DIM)
        .add_modifier(Modifier::ITALIC);
    if !expanded {
        let n = raw.lines().count().max(1);
        let text = format!("thinking… {} lines", n);
        let line = Line::from(Span::styled(text, dim_italic));
        wrap_line(line, width as usize)
    } else {
        let mut out = Vec::new();
        for l in raw.lines() {
            let line = Line::from(Span::styled(l.to_string(), dim_italic));
            out.extend(wrap_line(line, width as usize));
        }
        if out.is_empty() {
            out.push(Line::from(Span::styled("thinking…", dim_italic)));
        }
        out
    }
}

fn truncate_tool_text(text: &str) -> (Vec<String>, usize) {
    let mut lines = Vec::new();
    let mut total_lines: usize = 0;
    for (i, raw_line) in text.lines().enumerate() {
        total_lines += 1;
        if i < 40 {
            let mut char_iter = raw_line.chars();
            let mut prefix: String = char_iter.by_ref().take(200).collect();
            if char_iter.next().is_some() {
                prefix.push('…');
            }
            lines.push(prefix);
        }
    }
    let more_count = total_lines.saturating_sub(40);
    (lines, more_count)
}

fn render_tool(block: &Block, expanded: bool, width: u16) -> Vec<Line<'static>> {
    let is_final_output = block.title.ends_with("final_output");

    let (glyph, glyph_style) = match block.status.as_str() {
        "completed" | "success" => ("✓ ", Style::default().fg(Color::Green)),
        "failed" | "error" => ("✗ ", Style::default().fg(Color::Red)),
        "interrupted" | "cancelled" => ("⊘ ", Style::default().add_modifier(Modifier::DIM)),
        _ => ("◌ ", Style::default().fg(Color::Yellow)),
    };
    let title_style = if is_final_output {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let title_text = if is_final_output {
        "final output".to_string()
    } else {
        block.title.clone()
    };

    let mut header_spans = vec![
        Span::styled(glyph.to_string(), glyph_style),
        Span::styled(title_text, title_style),
    ];
    if let Some(notif) = &block.notification_line {
        header_spans.push(Span::styled(
            format!("  {}", notif),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    let mut out = wrap_line(Line::from(header_spans), width as usize);

    if expanded {
        if !block.input_text.is_empty() {
            out.push(Line::from(Span::styled(
                "  Input:",
                Style::default().add_modifier(Modifier::DIM),
            )));
            let (in_lines, more_count) = truncate_tool_text(&block.input_text);
            for l in in_lines {
                out.extend(wrap_line(Line::from(format!("    {}", l)), width as usize));
            }
            if more_count > 0 {
                out.push(Line::from(Span::styled(
                    format!("    … {} more lines", more_count),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
        }
        if !block.output_text.is_empty() {
            out.push(Line::from(Span::styled(
                "  Output:",
                Style::default().add_modifier(Modifier::DIM),
            )));
            let (out_lines, more_count) = truncate_tool_text(&block.output_text);
            for l in out_lines {
                out.extend(wrap_line(Line::from(format!("    {}", l)), width as usize));
            }
            if more_count > 0 {
                out.push(Line::from(Span::styled(
                    format!("    … {} more lines", more_count),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
        }
    }

    out
}

fn format_extension_results(result: &Value) -> Option<(String, bool)> {
    let exts = result
        .pointer("/_meta/extensionResults")
        .or_else(|| result.get("extensionResults"))
        .and_then(Value::as_array)?;

    if exts.is_empty() {
        return None;
    }

    let mut successes = Vec::new();
    let mut failures = Vec::new();

    for ext in exts {
        let name = ext.get("name").and_then(Value::as_str).unwrap_or("unknown");
        let success = ext.get("success").and_then(Value::as_bool).unwrap_or(true);
        if success {
            successes.push(format!("{name} ✓"));
        } else {
            failures.push(format!("{name} ✗"));
        }
    }

    let has_failures = !failures.is_empty();
    let text = match (successes.is_empty(), failures.is_empty()) {
        (false, false) => format!(
            "extensions: {}; failed: {}",
            successes.join(", "),
            failures.join(", ")
        ),
        (false, true) => format!("extensions: {}", successes.join(", ")),
        (true, false) => format!("failed extensions: {}", failures.join(", ")),
        (true, true) => return None,
    };

    Some((text, has_failures))
}

fn format_failed_extensions(result: &Value) -> Option<String> {
    let exts = result
        .pointer("/_meta/extensionResults")
        .or_else(|| result.get("extensionResults"))
        .and_then(Value::as_array)?;

    let mut failures = Vec::new();
    for ext in exts {
        let name = ext.get("name").and_then(Value::as_str).unwrap_or("unknown");
        let success = ext.get("success").and_then(Value::as_bool).unwrap_or(true);
        if !success {
            failures.push(format!("{name} ✗"));
        }
    }

    if failures.is_empty() {
        None
    } else {
        Some(format!("failed extensions: {}", failures.join(", ")))
    }
}

fn render_notice(block: &Block, width: u16) -> Vec<Line<'static>> {
    let (prefix_style, text_style) = match block.notice.as_ref().map(|n| n.kind) {
        Some(NoticeKind::Error) => (
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Red),
        ),
        Some(NoticeKind::Warning) => (
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Yellow),
        ),
        _ => (
            Style::default().add_modifier(Modifier::DIM),
            Style::default().add_modifier(Modifier::DIM),
        ),
    };
    let spans = vec![
        Span::styled("[honk] ".to_string(), prefix_style),
        Span::styled(block.text.clone(), text_style),
    ];
    let mut out = wrap_line(Line::from(spans), width as usize);
    if let Some(Notice {
        returned_text: Some(ref returned),
        ..
    }) = block.notice
    {
        for l in returned.lines() {
            out.extend(wrap_line(
                Line::from(Span::styled(
                    format!("  {}", l),
                    Style::default().add_modifier(Modifier::DIM),
                )),
                width as usize,
            ));
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct Viewport {
    pub follow: bool,
    pub anchor: (BlockKey, usize),
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new()
    }
}

impl Viewport {
    pub fn new() -> Self {
        Self {
            follow: true,
            anchor: (BlockKey::User(String::new()), 0),
        }
    }

    pub fn visible_lines(
        &mut self,
        transcript: &mut Transcript,
        width: u16,
        height: usize,
    ) -> Vec<Line<'static>> {
        if transcript.blocks.is_empty() || height == 0 {
            return Vec::new();
        }

        transcript.ensure_prefix_sums(width);
        let (_total_h, v_start, v_end) = {
            let sums = &transcript.cached_prefix_sums.as_ref().unwrap().sums;
            let total_h = *sums.last().unwrap_or(&0);

            if total_h <= height {
                self.follow = true;
                self.anchor = (transcript.blocks[0].key.clone(), 0);
                (total_h, 0, total_h)
            } else if self.follow {
                let start = total_h - height;
                let anchor_idx = sums[1..].partition_point(|&end| end <= start);
                self.anchor = (
                    transcript.blocks[anchor_idx].key.clone(),
                    start - sums[anchor_idx],
                );
                (total_h, start, total_h)
            } else {
                let start = if let Some(&idx) = transcript.block_indices.get(&self.anchor.0) {
                    let block_h = sums[idx + 1] - sums[idx];
                    let offset = self.anchor.1.min(block_h.saturating_sub(1));
                    (sums[idx] + offset).min(total_h.saturating_sub(height))
                } else {
                    0
                };
                if start + height >= total_h {
                    self.follow = true;
                }
                (total_h, start, (start + height).min(total_h))
            }
        };

        let first_idx = {
            let sums = &transcript.cached_prefix_sums.as_ref().unwrap().sums;
            sums[1..].partition_point(|&end| end <= v_start)
        };

        let mut out = Vec::new();
        for i in first_idx..transcript.blocks.len() {
            let (b_start, b_end) = {
                let sums = &transcript.cached_prefix_sums.as_ref().unwrap().sums;
                (sums[i], sums[i + 1])
            };
            if b_start >= v_end {
                break;
            }
            let lines = transcript.render_block(i, width);
            let slice_start = v_start.saturating_sub(b_start);
            let slice_end = if v_end < b_end {
                v_end - b_start
            } else {
                lines.len()
            };
            if slice_start < lines.len() {
                let end = slice_end.min(lines.len());
                for l in &lines[slice_start..end] {
                    out.push(l.clone());
                }
            }
        }
        out
    }

    pub fn scroll_lines(
        &mut self,
        delta: i32,
        transcript: &mut Transcript,
        width: u16,
        height: usize,
    ) {
        if transcript.blocks.is_empty() || height == 0 {
            return;
        }
        transcript.ensure_prefix_sums(width);
        let sums = &transcript.cached_prefix_sums.as_ref().unwrap().sums;
        let total_h = *sums.last().unwrap_or(&0);
        if total_h <= height {
            self.follow = true;
            return;
        }

        let max_start = total_h - height;
        let current_start = if self.follow {
            max_start
        } else if let Some(&idx) = transcript.block_indices.get(&self.anchor.0) {
            let block_h = sums[idx + 1] - sums[idx];
            let offset = self.anchor.1.min(block_h.saturating_sub(1));
            sums[idx] + offset
        } else {
            0
        };

        let new_start = (current_start as i64 + delta as i64).clamp(0, max_start as i64) as usize;
        self.follow = new_start >= max_start;

        let anchor_idx = sums[1..].partition_point(|&end| end <= new_start);
        self.anchor = (
            transcript.blocks[anchor_idx].key.clone(),
            new_start - sums[anchor_idx],
        );
    }

    pub fn half_page(&mut self, dir: i32, transcript: &mut Transcript, width: u16, height: usize) {
        let step = (height / 2).max(1) as i32 * dir.signum();
        self.scroll_lines(step, transcript, width, height);
    }

    pub fn page(&mut self, dir: i32, transcript: &mut Transcript, width: u16, height: usize) {
        let step = height.max(1) as i32 * dir.signum();
        self.scroll_lines(step, transcript, width, height);
    }

    pub fn top(&mut self, transcript: &mut Transcript, width: u16, height: usize) {
        if transcript.blocks.is_empty() {
            return;
        }
        let total_h = transcript.total_height(width);
        self.follow = total_h <= height;
        self.anchor = (transcript.blocks[0].key.clone(), 0);
    }

    pub fn bottom(&mut self, _transcript: &mut Transcript, _width: u16, _height: usize) {
        self.follow = true;
    }

    pub fn reconcile(&mut self, transcript: &Transcript) {
        if self.follow {
            return;
        }
        if !transcript.block_indices.contains_key(&self.anchor.0) && !transcript.blocks.is_empty() {
            self.anchor = (transcript.blocks[0].key.clone(), 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_chunk_frame(session_update: &str, msg_id: &str, text: &str) -> Value {
        json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": session_update,
                    "messageId": msg_id,
                    "content": {
                        "type": "text",
                        "text": text,
                    }
                }
            }
        })
    }

    #[test]
    fn test_streaming_chunks_across_fence_no_flicker() {
        let mut transcript = Transcript::new();
        let chunk1 = make_chunk_frame("agent_message_chunk", "msg_1", "```rust\nfn foo() {\n");
        transcript.apply_frame(&chunk1);
        let render1 = transcript.render_block(0, 80);
        assert!(!render1.is_empty());

        let chunk2 = make_chunk_frame("agent_message_chunk", "msg_1", "    let x = 1;\n");
        transcript.apply_frame(&chunk2);
        let render2 = transcript.render_block(0, 80);
        assert!(render2.len() >= render1.len());
        // Committed lines must match exactly
        for i in 0..render1.len() {
            assert_eq!(render1[i], render2[i], "line {i} flickered between renders");
        }

        let chunk3 = make_chunk_frame("agent_message_chunk", "msg_1", "}\n```\n");
        transcript.apply_frame(&chunk3);
        let render3 = transcript.render_block(0, 80);
        assert!(render3.len() >= render2.len());
        for i in 0..render2.len() {
            assert_eq!(
                render2[i], render3[i],
                "line {i} flickered after close fence"
            );
        }
    }

    #[test]
    fn test_inline_code_split_across_chunks() {
        let mut transcript = Transcript::new();
        let chunk1 = make_chunk_frame("agent_message_chunk", "msg_1", "Use `foo");
        transcript.apply_frame(&chunk1);
        let lines1 = transcript.render_block(0, 80);
        assert_eq!(lines1.len(), 1);

        let chunk2 = make_chunk_frame("agent_message_chunk", "msg_1", "bar` to test\n");
        transcript.apply_frame(&chunk2);
        let lines2 = transcript.render_block(0, 80);
        assert!(!lines2.is_empty());
        // Rendered line contains foobar
        let plain: String = lines2[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(plain.contains("foobar"), "rendered: {plain}");
    }

    #[test]
    fn test_long_line_no_newline_wraps() {
        let mut transcript = Transcript::new();
        // 100 characters with no newline
        let long_text = "a".repeat(100);
        let chunk = make_chunk_frame("agent_message_chunk", "msg_1", &long_text);
        transcript.apply_frame(&chunk);
        let lines = transcript.render_block(0, 40);
        // At width 40, 100 characters must wrap into 3 lines (40, 40, 20)
        assert_eq!(lines.len(), 3);
        for line in lines.iter() {
            let width: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(width <= 40);
        }
    }

    #[test]
    fn test_wide_chars_wrap_by_display_width() {
        let mut transcript = Transcript::new();
        // 5 CJK characters = 10 display width. 8 chars = 16 width.
        let cjk = "你好世界测试文字";
        let chunk = make_chunk_frame("agent_message_chunk", "msg_1", cjk);
        transcript.apply_frame(&chunk);
        let lines = transcript.render_block(0, 10);
        assert_eq!(lines.len(), 2); // 10 cols, then 6 cols
        for line in lines.iter() {
            let width: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(width <= 10, "line width {width} exceeds 10");
        }

        // Emoji
        let mut transcript_emoji = Transcript::new();
        let emoji = "🦆🦆🦆"; // 3 ducks = 6 cols
        let chunk_e = make_chunk_frame("agent_message_chunk", "msg_e", emoji);
        transcript_emoji.apply_frame(&chunk_e);
        let lines_e = transcript_emoji.render_block(0, 4);
        assert_eq!(lines_e.len(), 2); // 2 ducks (4 cols), then 1 duck (2 cols)
    }

    #[test]
    fn test_tool_status_glyphs_and_notification() {
        let mut transcript = Transcript::new();
        let call = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t1",
                    "title": "shell · ls",
                    "status": "pending",
                }
            }
        });
        transcript.apply_frame(&call);
        let lines_pending = transcript.render_block(0, 80);
        let plain_p: String = lines_pending[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(plain_p.contains('◌'));

        let update_running = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "t1",
                    "status": "in_progress",
                    "_meta": {
                        "toolNotification": {
                            "type": "live_output",
                            "params": {
                                "chunks": [{"output": "running step 1\n", "stream": "stdout"}]
                            }
                        }
                    }
                }
            }
        });
        transcript.apply_frame(&update_running);
        let lines_running = transcript.render_block(0, 80);
        let plain_r: String = lines_running[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(plain_r.contains('◌'));
        assert!(plain_r.contains("running step 1"));

        let update_done = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "t1",
                    "status": "completed",
                }
            }
        });
        transcript.apply_frame(&update_done);
        let lines_done = transcript.render_block(0, 80);
        let plain_d: String = lines_done[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(plain_d.contains('✓'));

        let update_fail = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "t1",
                    "status": "failed",
                }
            }
        });
        transcript.apply_frame(&update_fail);
        let lines_fail = transcript.render_block(0, 80);
        let plain_f: String = lines_fail[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(plain_f.contains('✗'));
    }

    #[test]
    fn test_tools_expanded_truncation() {
        let mut transcript = Transcript::new();
        let long_output = (1..=60)
            .map(|i| format!("out line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let call = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t1",
                    "title": "shell · cat",
                    "status": "completed",
                    "content": [{"type": "text", "text": long_output}],
                }
            }
        });
        transcript.apply_frame(&call);

        // When collapsed: 1 line
        let lines_collapsed = transcript.render_block(0, 80);
        assert_eq!(lines_collapsed.len(), 1);

        // When expanded: header + output header + 40 lines + truncation marker
        transcript.toggle_tools();
        let lines_expanded = transcript.render_block(0, 80);
        let joined: String = lines_expanded
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("… 20 more lines"));
    }

    #[test]
    fn test_final_output_glyph_and_expanded() {
        let mut transcript = Transcript::new();
        let call = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t_out",
                    "title": "recipe__final_output",
                    "status": "completed",
                    "rawInput": {"response": "All tasks done"},
                    "content": [{"type": "content", "content": {"type": "text", "text": "All tasks done"}}],
                }
            }
        });
        transcript.apply_frame(&call);

        // Collapsed: "✓ final output"
        let lines = transcript.render_block(0, 80);
        assert_eq!(lines.len(), 1);
        let plain: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(plain, "✓ final output");

        // Expanded: shows header with "✓ final output" and Input/Output blocks
        transcript.toggle_tools();
        let lines_exp = transcript.render_block(0, 80);
        assert!(lines_exp.len() > 1);
        let plain_exp: String = lines_exp[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(plain_exp, "✓ final output");
        let joined: String = lines_exp
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Input:"));
        assert!(joined.contains("Output:"));

        // Failed final_output shows ✗
        let fail_call = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t_fail",
                    "title": "final_output",
                    "status": "failed",
                }
            }
        });
        transcript.apply_frame(&fail_call);
        let fail_lines = transcript.render_block(1, 80);
        let fail_plain: String = fail_lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(fail_plain, "✗ final output");
    }

    #[test]
    fn test_prose_containing_colon_does_not_get_hanging_indent() {
        let mut transcript = Transcript::new();
        let prose = "This is a paragraph of markdown prose ending at: `code` and continuing with more text that will wrap onto the next line";
        let frame = make_chunk_frame("agent_message_chunk", "msg_prose", prose);
        transcript.apply_frame(&frame);

        // Wrap at 40 cols
        let lines = transcript.render_block(0, 40);
        assert!(lines.len() > 1);
        for (i, l) in lines.iter().enumerate() {
            let plain: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            if i > 0 {
                // Continuation line must NOT have a hanging indent (must not start with space)
                assert!(
                    !plain.starts_with(' '),
                    "line {i} should not have a hanging indent, got: {plain:?}"
                );
            }
        }
    }

    #[test]
    fn test_streaming_vs_ended_structured_reply() {
        let mut transcript = Transcript::new();

        // 1. Text starting with '[' but not JSON opening: "[PR #42](https://...) is merged"
        let pr_text = "[PR #42](https://example.com) is merged";
        let frame_pr = make_chunk_frame("agent_message_chunk", "msg_pr", pr_text);
        transcript.apply_frame(&frame_pr);

        let lines_pr = transcript.render_block(0, 80);
        let plain_pr: String = lines_pr
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!plain_pr.contains("receiving structured reply"));
        assert!(plain_pr.contains("PR #42"));

        // 2. Text that has JSON opening but does not parse completely: `{"a":1}\n\nThat is the config.`
        let mut transcript2 = Transcript::new();
        // Start an active run
        let run_start = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "goose": {
                            "activeRunId": "run_123"
                        }
                    }
                }
            }
        });
        transcript2.apply_frame(&run_start);

        // Chunk while streaming
        let chunk_stream = "{\"a\": 1}\n\nThat is the config.";
        let frame_stream = make_chunk_frame("agent_message_chunk", "msg_stream", chunk_stream);
        transcript2.apply_frame(&frame_stream);

        // Now end the run
        let run_end = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "goose": {
                            "activeRunId": null
                        }
                    }
                }
            }
        });
        transcript2.apply_frame(&run_end);

        // Once turn ended, it must fall through to normal markdown, NOT the streaming placeholder!
        let lines_ended = transcript2.render_block(0, 80);
        let plain_ended: String = lines_ended
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!plain_ended.contains("receiving structured reply"));
        assert!(plain_ended.contains("That is the config."));
    }

    #[test]
    fn test_nested_json_deep_recursion_and_empty_collections() {
        let mut transcript = Transcript::new();
        let deep_json = json!({
            "level1": {
                "level2": {
                    "level3": {
                        "level4": "deep_value"
                    }
                }
            }
        });
        let frame_deep =
            make_chunk_frame("agent_message_chunk", "msg_deep", &deep_json.to_string());
        transcript.apply_frame(&frame_deep);

        let lines = transcript.render_block(0, 80);
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("level1:"));
        assert!(joined.contains("level2:"));
        assert!(joined.contains("level3:"));
        assert!(joined.contains("level4: deep_value"));

        // Empty {} assistant message renders raw text "{}" rather than zero lines
        let mut transcript_empty_obj = Transcript::new();
        let frame_empty_obj = make_chunk_frame("agent_message_chunk", "msg_empty_o", "{}");
        transcript_empty_obj.apply_frame(&frame_empty_obj);
        let lines_empty_obj = transcript_empty_obj.render_block(0, 80);
        assert_eq!(lines_empty_obj.len(), 1);
        let plain_empty_obj: String = lines_empty_obj[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(plain_empty_obj, "{}");

        // Empty [] assistant message renders raw text "[]" rather than zero lines
        let mut transcript_empty_arr = Transcript::new();
        let frame_empty_arr = make_chunk_frame("agent_message_chunk", "msg_empty_a", "[]");
        transcript_empty_arr.apply_frame(&frame_empty_arr);
        let lines_empty_arr = transcript_empty_arr.render_block(0, 80);
        assert_eq!(lines_empty_arr.len(), 1);
        let plain_empty_arr: String = lines_empty_arr[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(plain_empty_arr, "[]");
    }

    #[test]
    fn test_consecutive_user_message_stops_being_equal_updates_earlier_block() {
        let mut transcript = Transcript::new();
        let frame1 = make_chunk_frame("user_message_chunk", "msg_1", "nag");
        let frame2 = make_chunk_frame("user_message_chunk", "msg_2", "nag");
        transcript.apply_frame(&frame1);
        transcript.apply_frame(&frame2);

        // Before append: collapsed with ×2, total height 1
        assert_eq!(transcript.total_height(80), 1);
        let lines0 = transcript.render_block(0, 80);
        let plain0: String = lines0[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(plain0.ends_with("×2"));

        // Message 2 receives a chunk making it "nag different"
        let frame2_chunk2 = make_chunk_frame("user_message_chunk", "msg_2", " different");
        transcript.apply_frame(&frame2_chunk2);

        // Now block 0 must no longer have ×2, and block 1 renders "nag different", total height 2
        let lines0_after = transcript.render_block(0, 80);
        let plain0_after: String = lines0_after[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!plain0_after.contains("×"));

        let lines1_after = transcript.render_block(1, 80);
        assert!(!lines1_after.is_empty());
        let plain1_after: String = lines1_after[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(plain1_after.contains("nag different"));

        assert_eq!(transcript.total_height(80), 2);
    }

    #[test]
    fn test_tool_title_formats_from_raw_input() {
        let mut transcript = Transcript::new();
        let call = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t_sh",
                    "title": "shell",
                    "status": "completed",
                    "rawInput": {"command": "cargo test --workspace\necho done"},
                }
            }
        });
        transcript.apply_frame(&call);

        assert_eq!(transcript.blocks[0].title, "shell · cargo test --workspace");
        let lines = transcript.render_block(0, 80);
        let plain: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(plain.contains("shell · cargo test --workspace"));
    }

    #[test]
    fn test_structured_json_rendering_in_assistant() {
        let mut transcript = Transcript::new();
        let json_body = json!({
            "episode_status": "complete",
            "notes": "**Summary**\n\n- task completed",
            "count": 42,
            "active": true,
            "empty_str": "",
            "empty_arr": [],
            "empty_obj": {},
            "actions": ["checked logs", "verified fix"],
            "sessions": [
                {"id": "ses_1", "status": "running"},
                {"id": "ses_2", "status": "idle"}
            ]
        });
        let frame = make_chunk_frame("agent_message_chunk", "msg_ast", &json_body.to_string());
        transcript.apply_frame(&frame);

        let lines = transcript.render_block(0, 80);
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Top level labels and scalars inline
        assert!(joined.contains("episode_status: complete"));
        assert!(joined.contains("count: 42"));
        assert!(joined.contains("active: true"));

        // Markdown rendered and unescaped
        assert!(joined.contains("Summary"));
        assert!(joined.contains("task completed"));

        // Empty values omitted
        assert!(!joined.contains("empty_str"));
        assert!(!joined.contains("empty_arr"));
        assert!(!joined.contains("empty_obj"));

        // Array of strings as bullet list
        assert!(joined.contains("actions:"));
        assert!(joined.contains("• checked logs"));
        assert!(joined.contains("• verified fix"));

        // Array of objects as indented sub-blocks
        assert!(joined.contains("sessions:"));
        assert!(joined.contains("- id: ses_1"));
        assert!(joined.contains("status: running"));
        assert!(joined.contains("- id: ses_2"));
        assert!(joined.contains("status: idle"));
    }

    #[test]
    fn test_structured_json_preserves_key_order() {
        let mut transcript = Transcript::new();
        // b_key before a_key, and z before a in sub_obj
        let json_str = r#"{
            "b_key": "first",
            "a_key": "second",
            "sub_obj": {
                "z_field": 10,
                "a_field": 20
            }
        }"#;
        let frame = make_chunk_frame("agent_message_chunk", "msg_order", json_str);
        transcript.apply_frame(&frame);

        let lines = transcript.render_block(0, 80);
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        let b_pos = joined.find("b_key").expect("b_key present");
        let a_pos = joined.find("a_key").expect("a_key present");
        assert!(
            b_pos < a_pos,
            "b_key should appear before a_key in rendered output (order preserved)"
        );

        let z_pos = joined.find("z_field").expect("z_field present");
        let sub_a_pos = joined.find("a_field").expect("a_field present");
        assert!(
            z_pos < sub_a_pos,
            "z_field should appear before a_field in sub_obj (order preserved)"
        );
    }

    #[test]
    fn test_hanging_indent_on_wrapped_lines() {
        let mut transcript = Transcript::new();
        let json_str = r#"{
            "bullet_list": [
                "This is a long bullet item that will wrap onto multiple continuation lines in a narrow viewport"
            ],
            "notes": "A very long note that wraps onto multiple continuation lines in a narrow viewport",
            "sub_block": [
                {
                    "text": "A long text property in a sub block that wraps onto multiple lines"
                }
            ]
        }"#;
        let frame = make_chunk_frame("agent_message_chunk", "msg_hang", json_str);
        transcript.apply_frame(&frame);

        // Render at width 35 to force multiple wrapped lines
        let lines = transcript.render_block(0, 35);
        let plain_lines: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        // 1. Bullet list continuation line starts with 4 spaces ("    ")
        let bullet_start = plain_lines
            .iter()
            .position(|l| l.starts_with("  • "))
            .expect("bullet start line");
        let bullet_cont = &plain_lines[bullet_start + 1];
        assert!(
            bullet_cont.starts_with("    ") && !bullet_cont.starts_with("     "),
            "bullet continuation line should have hanging indent of 4 spaces, got: {bullet_cont:?}"
        );

        // 2. "notes: " continuation line starts with 7 spaces ("notes: ".len() == 7)
        let notes_start = plain_lines
            .iter()
            .position(|l| l.starts_with("notes: "))
            .expect("notes start line");
        let notes_cont = &plain_lines[notes_start + 1];
        assert!(
            notes_cont.starts_with("       ") && !notes_cont.starts_with("        "),
            "notes continuation line should have hanging indent of 7 spaces, got: {notes_cont:?}"
        );

        // 3. Sub-block "  - text: " continuation line starts with 10 spaces ("  - text: ".len() == 10)
        let sub_start = plain_lines
            .iter()
            .position(|l| l.starts_with("  - text: "))
            .expect("sub-block start line");
        let sub_cont = &plain_lines[sub_start + 1];
        assert!(
            sub_cont.starts_with("          ") && !sub_cont.starts_with("           "),
            "sub-block continuation line should have hanging indent of 10 spaces, got: {sub_cont:?}"
        );
    }

    #[test]
    fn test_user_wake_payload_collapsed_by_default_and_expanded_with_toggle() {
        let mut transcript = Transcript::new();
        let payload = json!({
            "program": "example-program",
            "wake_reason": "activity",
            "open_items": [{"id": "item_1"}],
            "field_notes": "very long notes..."
        });
        let frame = make_chunk_frame("user_message_chunk", "msg_user_json", &payload.to_string());
        transcript.apply_frame(&frame);

        // Collapsed by default: exactly 1 dim line
        let lines_collapsed = transcript.render_block(0, 80);
        assert_eq!(lines_collapsed.len(), 1);
        let plain_collapsed: String = lines_collapsed[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(plain_collapsed.starts_with("▸ structured input"));
        assert!(plain_collapsed.contains("program=example-program"));
        assert!(plain_collapsed.contains("wake_reason=activity"));
        assert!(plain_collapsed.contains("KB"));
        assert!(
            lines_collapsed[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );

        // Toggle JSON expanded
        transcript.toggle_json();
        let lines_expanded = transcript.render_block(0, 80);
        assert!(lines_expanded.len() > 1);
        let joined_expanded: String = lines_expanded
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined_expanded.contains("> "));
        assert!(joined_expanded.contains("program: example-program"));
        assert!(joined_expanded.contains("wake_reason: activity"));
    }

    #[test]
    fn test_streaming_incomplete_json_assistant() {
        let mut transcript = Transcript::new();
        // Set active run
        let run_start = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "goose": {
                            "activeRunId": "run_999"
                        }
                    }
                }
            }
        });
        transcript.apply_frame(&run_start);

        let incomplete = "{\"program\": \"example-program\", \"items\": [";
        let frame = make_chunk_frame("agent_message_chunk", "msg_stream", incomplete);
        transcript.apply_frame(&frame);

        let lines = transcript.render_block(0, 80);
        assert_eq!(lines.len(), 1);
        let plain: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(plain.contains("◌ receiving structured reply"));
        assert!(plain.contains("KB)…"));
    }

    #[test]
    #[ignore]
    fn render_dump_file_visual_check() {
        let dump_path = std::env::var("HONK_DUMP_FILE").unwrap_or_default();
        if dump_path.is_empty() {
            return;
        }
        let content = std::fs::read_to_string(&dump_path).expect("read dump");
        let mut transcript = Transcript::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(frame) = serde_json::from_str::<Value>(line) {
                transcript.apply_frame(&frame);
            }
        }

        let mut viewport = Viewport::new();
        let height = transcript.total_height(140);
        let visible = viewport.visible_lines(&mut transcript, 140, height);
        for l in visible {
            let plain: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("{plain}");
        }
    }

    #[test]
    fn test_consecutive_identical_user_messages_collapse_with_count() {
        let mut transcript = Transcript::new();
        let nag = "You MUST call the `final_output` tool NOW";
        let frame1 = make_chunk_frame("user_message_chunk", "msg_1", nag);
        let frame2 = make_chunk_frame("user_message_chunk", "msg_2", nag);
        let frame3 = make_chunk_frame("user_message_chunk", "msg_3", nag);

        transcript.apply_frame(&frame1);
        transcript.apply_frame(&frame2);
        transcript.apply_frame(&frame3);

        assert_eq!(transcript.blocks.len(), 3);

        // Block 0 renders with ×3 suffix
        let lines0 = transcript.render_block(0, 80);
        assert_eq!(lines0.len(), 1);
        let plain0: String = lines0[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(plain0.contains(nag));
        assert!(plain0.ends_with("×3"), "expected ×3 suffix, got: {plain0}");

        // Blocks 1 and 2 render 0 lines
        let lines1 = transcript.render_block(1, 80);
        assert_eq!(lines1.len(), 0);
        let lines2 = transcript.render_block(2, 80);
        assert_eq!(lines2.len(), 0);

        assert_eq!(transcript.total_height(80), 1);

        // Applying a 4th identical message updates block 0 to ×4
        let frame4 = make_chunk_frame("user_message_chunk", "msg_4", nag);
        transcript.apply_frame(&frame4);
        let lines0_updated = transcript.render_block(0, 80);
        assert_eq!(lines0_updated.len(), 1);
        let plain0_updated: String = lines0_updated[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            plain0_updated.ends_with("×4"),
            "expected ×4 suffix, got: {plain0_updated}"
        );
        assert_eq!(transcript.total_height(80), 1);

        // Markdown export contains ## User (×4) once
        let md = transcript.to_markdown();
        assert!(md.contains("## User (×4)"));
        assert_eq!(md.matches(nag).count(), 1);
    }

    #[test]
    fn test_thoughts_collapsed_and_expanded() {
        let mut transcript = Transcript::new();
        let thought_frame = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "messageId": "msg_th",
                    "content": {
                        "type": "text",
                        "text": "line 1\nline 2\nline 3\nline 4\nline 5\n"
                    }
                }
            }
        });
        transcript.apply_frame(&thought_frame);

        // Collapsed
        let lines_collapsed = transcript.render_block(0, 80);
        assert_eq!(lines_collapsed.len(), 1);
        let plain_c: String = lines_collapsed[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(plain_c.contains("thinking… 5 lines"));

        // Expanded
        transcript.toggle_thoughts();
        let lines_expanded = transcript.render_block(0, 80);
        assert_eq!(lines_expanded.len(), 5);
    }

    #[test]
    fn test_reset_replay_keeps_anchor_or_falls_back() {
        let mut transcript = Transcript::new();
        let mut viewport = Viewport::new();
        viewport.follow = false;

        let b1 = make_chunk_frame("user_message_chunk", "msg_1", "hello");
        let b2 = make_chunk_frame("agent_message_chunk", "msg_2", "world\n");
        transcript.apply_ui_event(&UiEvent::Transcript(b1), &mut viewport);
        transcript.apply_ui_event(&UiEvent::Transcript(b2), &mut viewport);

        viewport.anchor = (BlockKey::Text("msg_2".into()), 0);

        // Reset
        let reset_frame = json!({"method": "_honk/reset"});
        transcript.apply_ui_event(&UiEvent::Transcript(reset_frame), &mut viewport);

        // Replay contains msg_2
        let replay_b2 = make_chunk_frame("agent_message_chunk", "msg_2", "world replayed\n");
        transcript.apply_ui_event(&UiEvent::Transcript(replay_b2), &mut viewport);

        let replay_done = json!({"method": "_honk/replay_done"});
        transcript.apply_ui_event(&UiEvent::Transcript(replay_done), &mut viewport);

        // Anchor should still be msg_2
        assert_eq!(viewport.anchor.0, BlockKey::Text("msg_2".into()));

        // Now test fallback when block doesn't exist
        viewport.anchor = (BlockKey::User("nonexistent".into()), 0);
        let reset2 = json!({"method": "_honk/reset"});
        transcript.apply_ui_event(&UiEvent::Transcript(reset2), &mut viewport);
        let replay2_b2 = make_chunk_frame("agent_message_chunk", "msg_2", "world replayed 2\n");
        transcript.apply_ui_event(&UiEvent::Transcript(replay2_b2), &mut viewport);
        let replay_done2 = json!({"method": "_honk/replay_done"});
        transcript.apply_ui_event(&UiEvent::Transcript(replay_done2), &mut viewport);

        // Falls back to nearest (msg_2)
        assert_eq!(viewport.anchor.0, BlockKey::Text("msg_2".into()));
    }

    #[test]
    fn test_follow_mode_stays_at_bottom_on_new_content() {
        let mut transcript = Transcript::new();
        let mut viewport = Viewport::new();
        assert!(viewport.follow);

        for i in 1..=10 {
            let chunk = make_chunk_frame(
                "agent_message_chunk",
                &format!("msg_{i}"),
                &format!("line {i}\n"),
            );
            transcript.apply_ui_event(&UiEvent::Transcript(chunk), &mut viewport);
        }

        let lines = viewport.visible_lines(&mut transcript, 80, 4);
        assert_eq!(lines.len(), 4);
        let plain: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(plain.contains("line 10"));
        assert!(plain.contains("line 7"));

        // Add 2 more lines
        for i in 11..=12 {
            let chunk = make_chunk_frame(
                "agent_message_chunk",
                &format!("msg_{i}"),
                &format!("line {i}\n"),
            );
            transcript.apply_ui_event(&UiEvent::Transcript(chunk), &mut viewport);
        }

        let lines2 = viewport.visible_lines(&mut transcript, 80, 4);
        let plain2: String = lines2
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(plain2.contains("line 12"));
        assert!(plain2.contains("line 9"));
    }

    #[test]
    fn test_not_following_keeps_anchor_on_new_content() {
        let mut transcript = Transcript::new();
        let mut viewport = Viewport::new();

        for i in 1..=10 {
            let chunk = make_chunk_frame(
                "agent_message_chunk",
                &format!("msg_{i}"),
                &format!("line {i}\n"),
            );
            transcript.apply_ui_event(&UiEvent::Transcript(chunk), &mut viewport);
        }

        // Scroll to top
        viewport.top(&mut transcript, 80, 4);
        assert!(!viewport.follow);

        let lines = viewport.visible_lines(&mut transcript, 80, 4);
        let plain: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(plain.contains("line 1"));
        assert!(plain.contains("line 4"));

        // New content arrives at the bottom
        for i in 11..=15 {
            let chunk = make_chunk_frame(
                "agent_message_chunk",
                &format!("msg_{i}"),
                &format!("line {i}\n"),
            );
            transcript.apply_ui_event(&UiEvent::Transcript(chunk), &mut viewport);
        }

        // Viewport should STILL show lines 1..4!
        let lines2 = viewport.visible_lines(&mut transcript, 80, 4);
        let plain2: String = lines2
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(plain2.contains("line 1"));
        assert!(plain2.contains("line 4"));
    }

    #[test]
    fn test_scroll_ops_clamp_at_top_and_bottom() {
        let mut transcript = Transcript::new();
        let mut viewport = Viewport::new();

        for i in 1..=20 {
            let chunk = make_chunk_frame(
                "agent_message_chunk",
                &format!("msg_{i}"),
                &format!("line {i}\n"),
            );
            transcript.apply_ui_event(&UiEvent::Transcript(chunk), &mut viewport);
        }

        viewport.top(&mut transcript, 80, 5);
        assert_eq!(viewport.anchor.0, BlockKey::Text("msg_1".into()));
        assert!(!viewport.follow);

        // Scroll up past top clamps at top
        viewport.scroll_lines(-10, &mut transcript, 80, 5);
        assert_eq!(viewport.anchor.0, BlockKey::Text("msg_1".into()));

        // Page down 2 pages (10 lines)
        viewport.page(1, &mut transcript, 80, 5);
        viewport.page(1, &mut transcript, 80, 5);

        // Scroll down past bottom clamps and enables follow
        viewport.scroll_lines(30, &mut transcript, 80, 5);
        assert!(viewport.follow);

        // Half page up
        viewport.half_page(-1, &mut transcript, 80, 5);
        assert!(!viewport.follow);

        // Bottom
        viewport.bottom(&mut transcript, 80, 5);
        assert!(viewport.follow);
    }

    #[test]
    fn test_resize_invalidates_cache_and_rewraps() {
        let mut transcript = Transcript::new();
        let chunk = make_chunk_frame("agent_message_chunk", "msg_1", &"word ".repeat(30));
        transcript.apply_frame(&chunk);

        let lines80 = transcript.render_block(0, 80);
        let count80 = lines80.len();

        let lines40 = transcript.render_block(0, 40);
        let count40 = lines40.len();

        assert!(count40 > count80, "count40: {count40}, count80: {count80}");

        // Rendering again at 80 uses cached or returns exact same count80
        let lines80_again = transcript.render_block(0, 80);
        assert_eq!(lines80_again.len(), count80);
    }

    #[test]
    fn test_notices_survive_reset() {
        let mut transcript = Transcript::new();
        let mut viewport = Viewport::new();

        transcript.apply_ui_event(&UiEvent::Notice(Notice::warning("Notice 1")), &mut viewport);
        assert_eq!(transcript.blocks.len(), 1);

        let reset = json!({"method": "_honk/reset"});
        transcript.apply_ui_event(&UiEvent::Transcript(reset), &mut viewport);

        let chunk = make_chunk_frame("agent_message_chunk", "msg_1", "text\n");
        transcript.apply_ui_event(&UiEvent::Transcript(chunk), &mut viewport);

        let done = json!({"method": "_honk/replay_done"});
        transcript.apply_ui_event(&UiEvent::Transcript(done), &mut viewport);

        // Transcript now has msg_1 and the notice!
        assert_eq!(transcript.blocks.len(), 2);
        assert!(
            transcript
                .blocks
                .iter()
                .any(|b| b.kind == BlockKind::Notice && b.text.contains("Notice 1"))
        );
    }

    #[test]
    fn test_to_markdown_and_last_assistant_text() {
        let mut transcript = Transcript::new();
        let user = make_chunk_frame("user_message_chunk", "u1", "What is 2+2?\n");
        let th = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "messageId": "th1",
                    "content": {"type": "text", "text": "Computing math\n"}
                }
            }
        });
        let a1 = make_chunk_frame("agent_message_chunk", "a1", "4\n");
        let a2 = make_chunk_frame("agent_message_chunk", "a2", "Done!\n");

        transcript.apply_frame(&user);
        transcript.apply_frame(&th);
        transcript.apply_frame(&a1);
        transcript.apply_frame(&a2);

        assert_eq!(transcript.last_assistant_text(), Some("Done!\n"));

        let md = transcript.to_markdown();
        assert!(md.contains("What is 2+2?"));
        assert!(md.contains("Computing math"));
        assert!(md.contains("Done!"));
    }

    #[test]
    fn test_wrapped_line_style() {
        let line = Line::from("this is a line that will definitely wrap across multiple lines")
            .style(Style::default().bg(Color::Blue));
        let wrapped = wrap_line(line, 15);
        assert!(wrapped.len() > 1);
        for l in &wrapped {
            assert_eq!(l.style, Style::default().bg(Color::Blue));
        }
    }

    #[test]
    fn test_four_backtick_fence_keeps_three_backtick_content() {
        let mut transcript = Transcript::new();
        let chunk = make_chunk_frame("agent_message_chunk", "a1", "````markdown\n```\n````\n");
        transcript.apply_frame(&chunk);
        let lines = transcript.render_block(0, 80);
        let text_lines: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // The inner ``` must be present as content AND the closing fence must be present
        let fence_count = text_lines.iter().filter(|l| l.trim() == "```").count();
        assert_eq!(
            fence_count, 2,
            "expected 2 ``` lines (content + closing fence), got: {:?}",
            text_lines
        );
    }

    #[test]
    fn test_abort_rebuild_path() {
        use crate::chat::driver::ConnectionState;
        use crate::chat::types::{Ownership, RunState};
        let mut transcript = Transcript::new();
        let mut viewport = Viewport::new();

        let chunk = make_chunk_frame("user_message_chunk", "u1", "hello");
        transcript.apply_ui_event(&UiEvent::Transcript(chunk), &mut viewport);
        assert_eq!(transcript.blocks.len(), 1);

        let reset = json!({"method": "_honk/reset"});
        transcript.apply_ui_event(&UiEvent::Transcript(reset), &mut viewport);
        assert!(transcript.is_rebuilding);

        // Connection drops into reconnecting
        let snapshot = crate::chat::driver::StateSnapshot {
            run_state: RunState::Idle,
            ownership: Ownership::Unknown,
            queued_len: 0,
            replaying: false,
            prompt_in_flight: false,
            connection: ConnectionState::Reconnecting { attempt: 1 },
        };
        transcript.apply_ui_event(&UiEvent::State(snapshot), &mut viewport);
        assert!(!transcript.is_rebuilding);
        assert_eq!(transcript.blocks.len(), 1);
    }

    #[test]
    fn test_notice_ids_unique_across_reset() {
        let mut transcript = Transcript::new();
        let mut viewport = Viewport::new();

        transcript.apply_notice(Notice::info("n1"));
        let reset = json!({"method": "_honk/reset"});
        transcript.apply_ui_event(&UiEvent::Transcript(reset), &mut viewport);
        transcript.apply_notice(Notice::info("n2"));
        let done = json!({"method": "_honk/replay_done"});
        transcript.apply_ui_event(&UiEvent::Transcript(done), &mut viewport);
        transcript.apply_notice(Notice::info("n3"));

        let keys: Vec<BlockKey> = transcript.blocks.iter().map(|b| b.key.clone()).collect();
        let mut unique = keys.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(keys.len(), unique.len(), "keys had duplicates: {:?}", keys);
    }

    #[test]
    fn test_streaming_300_chunks_bounds_cache() {
        let mut transcript = Transcript::new();
        for i in 1..=300 {
            let chunk = make_chunk_frame("agent_message_chunk", "stream_msg", &format!("word{i} "));
            transcript.apply_frame(&chunk);
            transcript.render_block(0, 80);
        }
        assert_eq!(
            transcript.layout_cache.len(),
            1,
            "cache leaked intermediate streaming frames: len={}",
            transcript.layout_cache.len()
        );
    }

    #[test]
    fn test_5k_blocks_incremental_render_counter() {
        let mut transcript = Transcript::new();
        let mut viewport = Viewport::new();

        for i in 0..5000 {
            let chunk = make_chunk_frame(
                "user_message_chunk",
                &format!("u_{i}"),
                &format!("msg {i}\n"),
            );
            transcript.apply_frame(&chunk);
        }

        // Render viewport of height 20
        let lines = viewport.visible_lines(&mut transcript, 80, 20);
        assert_eq!(lines.len(), 20);
        let initial_renders = transcript.render_count;
        assert_eq!(initial_renders, 5000);

        // Now stream a chunk into the last block (4999)
        let chunk_last = make_chunk_frame("user_message_chunk", "u_4999", " appended\n");
        transcript.apply_frame(&chunk_last);

        // Render viewport again
        let _ = viewport.visible_lines(&mut transcript, 80, 20);
        let delta_renders = transcript.render_count - initial_renders;
        // Only the modified block should have been re-rendered!
        assert_eq!(
            delta_renders, 1,
            "prefix sums re-rendered untouched blocks: delta={delta_renders}"
        );
    }

    #[test]
    fn test_cancelled_run_marks_pending_tool_interrupted() {
        let mut transcript = Transcript::new();
        let call = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t1",
                    "title": "shell · ls",
                    "status": "in_progress",
                }
            }
        });
        transcript.apply_frame(&call);
        assert_eq!(transcript.blocks[0].status, "in_progress");

        // Run ends
        let run_end = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "goose": {
                            "activeRunId": null
                        }
                    }
                }
            }
        });
        transcript.apply_frame(&run_end);
        assert_eq!(transcript.blocks[0].status, "interrupted");
        let lines = transcript.render_block(0, 80);
        let plain: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(plain.contains('⊘'));
    }

    #[test]
    fn test_extension_results_notices_session_new_and_attached() {
        let mut transcript = Transcript::new();
        let mut viewport = Viewport::new();

        // SessionNew with developer success and failed extension
        let new_evt = UiEvent::SessionNew {
            result: json!({
                "_meta": {
                    "extensionResults": [
                        {"name": "developer", "success": true},
                        {"name": "broken_ext", "success": false}
                    ]
                }
            }),
        };
        transcript.apply_ui_event(&new_evt, &mut viewport);
        assert_eq!(transcript.blocks.len(), 1);
        assert!(transcript.blocks[0].text.contains("developer ✓"));
        assert!(transcript.blocks[0].text.contains("broken_ext ✗"));

        // Attached with failed extension emits warning notice
        let attach_evt = UiEvent::Attached {
            result: json!({
                "_meta": {
                    "extensionResults": [
                        {"name": "developer", "success": true},
                        {"name": "failed_ext_2", "success": false}
                    ]
                }
            }),
        };
        transcript.apply_ui_event(&attach_evt, &mut viewport);
        assert_eq!(transcript.blocks.len(), 2);
        assert!(transcript.blocks[1].text.contains("failed_ext_2 ✗"));
    }

    #[test]
    fn test_blank_line_separator_between_different_kinds() {
        let mut transcript = Transcript::new();

        // 0. User block
        let u1 = make_chunk_frame("user_message_chunk", "u1", "User line 1\n");
        transcript.apply_frame(&u1);
        let l0 = transcript.render_block(0, 80);
        assert_ne!(l0[0].to_string(), ""); // No leading blank line for block 0

        // 1. Assistant block
        let a1 = make_chunk_frame("agent_message_chunk", "a1", "Assistant reply\n");
        transcript.apply_frame(&a1);
        let l1 = transcript.render_block(1, 80);
        assert_eq!(l1[0].to_string(), ""); // Leading blank line before assistant

        // 2. Tool 1
        let t1 = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t1",
                    "title": "shell · ls",
                    "status": "completed",
                }
            }
        });
        transcript.apply_frame(&t1);
        let l2 = transcript.render_block(2, 80);
        assert_eq!(l2[0].to_string(), ""); // Leading blank line before tool group

        // 3. Tool 2 (consecutive tool in same group)
        let t2 = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t2",
                    "title": "shell · pwd",
                    "status": "completed",
                }
            }
        });
        transcript.apply_frame(&t2);
        let l3 = transcript.render_block(3, 80);
        assert_ne!(l3[0].to_string(), ""); // NO leading blank line between tools in group!

        // 4. User block 2
        let u2 = make_chunk_frame("user_message_chunk", "u2", "User line 2\n");
        transcript.apply_frame(&u2);
        let l4 = transcript.render_block(4, 80);
        assert_eq!(l4[0].to_string(), ""); // Leading blank line before next user block
    }

    #[test]
    fn test_wrap_line_coalesces_wide_tokens() {
        let long_word = "a".repeat(100);
        let line = Line::from(long_word);
        let wrapped = wrap_line(line, 40);
        assert_eq!(wrapped.len(), 3);
        // Each wrapped line must have only 1 span, not 40 spans!
        assert_eq!(wrapped[0].spans.len(), 1);
        assert_eq!(wrapped[1].spans.len(), 1);
        assert_eq!(wrapped[2].spans.len(), 1);
    }

    #[test]
    fn test_assistant_huge_message_plain_text_fallback() {
        let mut transcript = Transcript::new();
        // 250 KB assistant message
        let huge_text = format!("{}\n", "hello world ".repeat(22_000));
        assert!(huge_text.len() > 200 * 1024);
        let chunk = make_chunk_frame("agent_message_chunk", "huge_1", &huge_text);
        transcript.apply_frame(&chunk);
        let lines = transcript.render_block(0, 80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_tool_line_length_truncation_with_ellipsis() {
        let mut transcript = Transcript::new();
        let long_line = "a".repeat(300);
        let tool_call = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t_long",
                    "title": "shell · long",
                    "status": "completed",
                    "rawInput": long_line,
                }
            }
        });
        transcript.apply_frame(&tool_call);
        transcript.toggle_tools();
        let lines = transcript.render_block(0, 80);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(joined.contains('…'));
    }

    #[test]
    fn test_5k_blocks_with_huge_tool_outputs_relayout_fast() {
        let mut transcript = Transcript::new();
        let mut viewport = Viewport::new();

        let huge_json = format!("{{\"file\":\"{}\"}}", "x".repeat(1_000_000));

        for i in 0..5000 {
            if i % 250 == 0 && i > 0 {
                // Tool block with 1 MB input
                let tool_call = json!({
                    "method": "session/update",
                    "params": {
                        "update": {
                            "sessionUpdate": "tool_call",
                            "toolCallId": format!("tool_{i}"),
                            "title": format!("write · large_{i}.json"),
                            "status": "completed",
                            "rawInput": huge_json.clone(),
                        }
                    }
                });
                transcript.apply_frame(&tool_call);
            } else {
                let chunk = make_chunk_frame(
                    "user_message_chunk",
                    &format!("u_{i}"),
                    &format!("message {i}\n"),
                );
                transcript.apply_frame(&chunk);
            }
        }

        // Toggle tools to expanded
        let start = std::time::Instant::now();
        transcript.toggle_tools();
        let lines = viewport.visible_lines(&mut transcript, 80, 20);
        let elapsed = start.elapsed();

        assert_eq!(lines.len(), 20);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "relayout took too long: {:?}",
            elapsed
        );

        let total_cached_lines: usize = transcript
            .layout_cache
            .values()
            .map(|entry| entry.height)
            .sum();
        assert!(
            total_cached_lines < 500_000,
            "cached lines exceeded bound: {total_cached_lines}"
        );
    }

    #[test]
    fn test_transcript_usage_lifecycle() {
        use honk_wire::{Cost, Usage};

        let mut transcript = Transcript::new();
        assert_eq!(transcript.usage(), None);

        // 1. Live usage_update sets usage
        let f1 = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "usage_update",
                    "used": 150_000,
                    "size": 200_000,
                    "cost": {
                        "amount": 1.25,
                        "currency": "USD"
                    }
                }
            }
        });
        transcript.apply_frame(&f1);

        assert_eq!(
            transcript.usage(),
            Some(&Usage {
                used: 150_000,
                size: 200_000,
                cost: Some(Cost {
                    amount: 1.25,
                    currency: "USD".to_string()
                }),
            })
        );
        // Usage frames must create no blocks!
        assert_eq!(transcript.blocks.len(), 0);

        // 2. Later one replaces it (e.g. compaction drop 150000 -> 20000)
        let f2 = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "usage_update",
                    "used": 20_000,
                    "size": 200_000,
                    "cost": {
                        "amount": 1.30,
                        "currency": "USD"
                    }
                }
            }
        });
        transcript.apply_frame(&f2);

        assert_eq!(
            transcript.usage(),
            Some(&Usage {
                used: 20_000,
                size: 200_000,
                cost: Some(Cost {
                    amount: 1.30,
                    currency: "USD".to_string()
                }),
            })
        );
        assert_eq!(transcript.blocks.len(), 0);

        // 3. Reset + replay containing a usage frame -> new value after replay_done
        let reset_frame = json!({ "method": honk_wire::method::RESET });
        transcript.apply_frame(&reset_frame);
        // While rebuilding, usage is still accessible from transcript before replay finishes
        assert_eq!(transcript.usage().map(|u| u.used), Some(20_000));

        let replay_usage = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "usage_update",
                    "used": 50_000,
                    "size": 200_000,
                    "cost": {
                        "amount": 1.50,
                        "currency": "USD"
                    }
                }
            }
        });
        transcript.apply_frame(&replay_usage);
        // Still rebuilding, replay_done not received yet
        assert_eq!(transcript.usage().map(|u| u.used), Some(20_000));

        let replay_done = json!({ "method": honk_wire::method::REPLAY_DONE });
        transcript.apply_frame(&replay_done);
        // After replay_done, new generation's value wins
        assert_eq!(
            transcript.usage(),
            Some(&Usage {
                used: 50_000,
                size: 200_000,
                cost: Some(Cost {
                    amount: 1.50,
                    currency: "USD".to_string()
                }),
            })
        );
        assert_eq!(transcript.blocks.len(), 0);

        // 4. Reset + replay without usage -> old value kept
        transcript.apply_frame(&reset_frame);
        // Only content chunks during replay, no usage frame
        let chunk = make_chunk_frame("agent_message_chunk", "msg_1", "hello");
        transcript.apply_frame(&chunk);
        transcript.apply_frame(&replay_done);
        // Old usage value preserved!
        assert_eq!(transcript.usage().map(|u| u.used), Some(50_000));

        // 5. Rebuild abort keeps current value
        transcript.apply_frame(&reset_frame);
        let replay_usage2 = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "usage_update",
                    "used": 99_000,
                    "size": 200_000
                }
            }
        });
        transcript.apply_frame(&replay_usage2);
        transcript.abort_rebuild();
        // Aborted: kept the pre-reset value (50_000)
        assert_eq!(transcript.usage().map(|u| u.used), Some(50_000));
    }
}
