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

use crate::chat::driver::UiEvent;
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExpansionFlags {
    pub thoughts: bool,
    pub tools: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LayoutKey {
    pub key: BlockKey,
    pub width: u16,
    pub revision: u64,
    pub flags: ExpansionFlags,
}

pub struct Transcript {
    pub blocks: Vec<Block>,
    pub block_indices: HashMap<BlockKey, usize>,
    pub thoughts_expanded: bool,
    pub tools_expanded: bool,
    pub layout_cache: HashMap<LayoutKey, Vec<Line<'static>>>,
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
            layout_cache: HashMap::new(),
            pending_rebuild: None,
            is_rebuilding: false,
            next_notice_id: 1,
            anchor_idx_before_reset: None,
            anchor_key_before_reset: None,
            anchor_offset_before_reset: 0,
        }
    }

    pub fn apply_frame(&mut self, frame: &Value) -> bool {
        match frame.get("method").and_then(Value::as_str) {
            Some(honk_wire::method::RESET) => {
                self.is_rebuilding = true;
                let mut fresh = Transcript::new();
                fresh.thoughts_expanded = self.thoughts_expanded;
                fresh.tools_expanded = self.tools_expanded;
                fresh.next_notice_id = self.next_notice_id;
                self.pending_rebuild = Some(Box::new(fresh));
                false
            }
            Some(honk_wire::method::REPLAY_DONE) => {
                if let Some(mut fresh) = self.pending_rebuild.take() {
                    // Re-append local notices from self to fresh
                    for b in &self.blocks {
                        if b.kind == BlockKind::Notice {
                            let idx = fresh.blocks.len();
                            fresh.blocks.push(b.clone());
                            fresh.block_indices.insert(b.key.clone(), idx);
                        }
                    }
                    self.blocks = fresh.blocks;
                    self.block_indices = fresh.block_indices;
                    self.layout_cache.clear();
                    self.is_rebuilding = false;
                    self.next_notice_id = fresh.next_notice_id;
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

        let block = &mut self.blocks[idx];
        block.revision += 1;

        match &key {
            BlockKey::Tool(_) => {
                if let Some(t) = u.get("title").and_then(Value::as_str) {
                    block.title = t.to_string();
                }
                if let Some(s) = u.get("status").and_then(Value::as_str) {
                    block.status = s.to_string();
                }
                if let Some(raw_in) = u.get("rawInput") {
                    block.input_text = extract_input_text(raw_in);
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
                if let Some(t) = honk_wire::text_of(u) {
                    block.text.push_str(t);
                }
            }
        }
    }

    pub fn apply_notice(&mut self, notice: Notice) {
        let key = BlockKey::Notice(format!("notice_{}", self.next_notice_id));
        self.next_notice_id += 1;
        let idx = self.blocks.len();
        self.blocks.push(Block {
            key: key.clone(),
            kind: BlockKind::Notice,
            revision: 1,
            text: notice.text.clone(),
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
            _ => {}
        }
    }

    pub fn toggle_thoughts(&mut self) {
        self.thoughts_expanded = !self.thoughts_expanded;
    }

    pub fn toggle_tools(&mut self) {
        self.tools_expanded = !self.tools_expanded;
    }

    pub fn render_block(&mut self, block_idx: usize, width: u16) -> Vec<Line<'static>> {
        if block_idx >= self.blocks.len() {
            return Vec::new();
        }
        let block = &self.blocks[block_idx];
        let key = LayoutKey {
            key: block.key.clone(),
            width,
            revision: block.revision,
            flags: ExpansionFlags {
                thoughts: self.thoughts_expanded,
                tools: self.tools_expanded,
            },
        };

        if let Some(lines) = self.layout_cache.get(&key) {
            return lines.clone();
        }

        let lines = match block.kind {
            BlockKind::User => render_user(&block.text, width),
            BlockKind::Assistant => render_assistant(&block.text, width),
            BlockKind::Thought => render_thought(&block.text, self.thoughts_expanded, width),
            BlockKind::Tool => render_tool(block, self.tools_expanded, width),
            BlockKind::Notice => render_notice(block, width),
        };

        self.layout_cache.insert(key, lines.clone());
        lines
    }

    pub fn block_height(&mut self, block_idx: usize, width: u16) -> usize {
        self.render_block(block_idx, width).len()
    }

    pub fn prefix_sums(&mut self, width: u16) -> Vec<usize> {
        let mut sums = Vec::with_capacity(self.blocks.len() + 1);
        sums.push(0);
        let mut current = 0;
        for i in 0..self.blocks.len() {
            let h = self.block_height(i, width);
            current += h;
            sums.push(current);
        }
        sums
    }

    pub fn total_height(&mut self, width: u16) -> usize {
        let sums = self.prefix_sums(width);
        *sums.last().unwrap_or(&0)
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            match b.kind {
                BlockKind::User => {
                    out.push_str("## User\n\n");
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

pub fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let max_width = width.max(1);
    let total_width: usize = line
        .spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();

    if total_width <= max_width && !line.spans.iter().any(|s| s.content.contains('\n')) {
        return vec![line];
    }

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
            result.push(Line::from(std::mem::take(&mut current_spans)));
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
            } else if current_width + token.width <= max_width {
                current_spans.push(Span::styled(token.text, token.style));
                current_width += token.width;
            } else {
                result.push(Line::from(std::mem::take(&mut current_spans)));
                current_width = 0;
            }
            continue;
        }

        // Non-whitespace
        is_start_of_original_line = false;
        if current_width + token.width <= max_width {
            current_spans.push(Span::styled(token.text, token.style));
            current_width += token.width;
        } else {
            if current_width > 0 {
                result.push(Line::from(std::mem::take(&mut current_spans)));
                current_width = 0;
            }

            if token.width <= max_width {
                current_spans.push(Span::styled(token.text, token.style));
                current_width += token.width;
            } else {
                // Token itself exceeds max_width (e.g. 100-char continuous string)
                for c in token.text.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
                    if current_width + cw > max_width && current_width > 0 {
                        result.push(Line::from(std::mem::take(&mut current_spans)));
                        current_width = 0;
                    }
                    current_spans.push(Span::styled(c.to_string(), token.style));
                    current_width += cw;
                }
            }
        }
    }

    if !current_spans.is_empty() || result.is_empty() {
        result.push(Line::from(current_spans));
    }

    result
}

fn render_user(raw: &str, width: u16) -> Vec<Line<'static>> {
    let prefix_style = Style::default()
        .add_modifier(Modifier::BOLD)
        .fg(Color::Yellow);
    let mut out = Vec::new();
    let lines: Vec<&str> = raw.lines().collect();
    if lines.is_empty() {
        out.push(Line::from(Span::styled("> ", prefix_style)));
        return out;
    }
    for (i, l) in lines.iter().enumerate() {
        let prefix = if i == 0 { "> " } else { "  " };
        let spans = vec![
            Span::styled(prefix.to_string(), prefix_style),
            Span::raw(l.to_string()),
        ];
        out.extend(wrap_line(Line::from(spans), width as usize));
    }
    out
}

fn has_unclosed_fence(s: &str) -> bool {
    let mut open = false;
    for line in s.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            open = !open;
        }
    }
    open
}

fn to_owned_line(line: Line<'_>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect();
    Line::from(spans).style(line.style)
}

fn render_assistant(raw: &str, width: u16) -> Vec<Line<'static>> {
    let last_nl = raw.rfind('\n');
    let mut out_lines = Vec::new();
    let (committed, tail) = match last_nl {
        Some(idx) => (&raw[..=idx], &raw[idx + 1..]),
        None => ("", raw),
    };

    if !committed.is_empty() {
        let md_text = tui_markdown::from_str(committed);
        let unclosed = has_unclosed_fence(committed);
        let mut lines = md_text.lines;
        if unclosed && !lines.is_empty() {
            // Drop synthesized closing fence line
            if let Some(last) = lines.last() {
                let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
                if text.trim() == "```" || text.trim() == "~~~" {
                    lines.pop();
                }
            }
        }
        for line in lines {
            let owned = to_owned_line(line);
            out_lines.extend(wrap_line(owned, width as usize));
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

fn render_tool(block: &Block, expanded: bool, width: u16) -> Vec<Line<'static>> {
    let (glyph, glyph_style) = match block.status.as_str() {
        "completed" | "success" => ("✓ ", Style::default().fg(Color::Green)),
        "failed" | "error" => ("✗ ", Style::default().fg(Color::Red)),
        _ => ("◌ ", Style::default().fg(Color::Yellow)),
    };
    let title_style = Style::default().add_modifier(Modifier::BOLD);
    let mut header_spans = vec![
        Span::styled(glyph.to_string(), glyph_style),
        Span::styled(block.title.clone(), title_style),
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
            let in_lines: Vec<&str> = block.input_text.lines().collect();
            let limit = 40.min(in_lines.len());
            for l in &in_lines[..limit] {
                out.extend(wrap_line(Line::from(format!("    {}", l)), width as usize));
            }
            if in_lines.len() > 40 {
                out.push(Line::from(Span::styled(
                    format!("    … {} more lines", in_lines.len() - 40),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
        }
        if !block.output_text.is_empty() {
            out.push(Line::from(Span::styled(
                "  Output:",
                Style::default().add_modifier(Modifier::DIM),
            )));
            let out_lines: Vec<&str> = block.output_text.lines().collect();
            let limit = 40.min(out_lines.len());
            for l in &out_lines[..limit] {
                out.extend(wrap_line(Line::from(format!("    {}", l)), width as usize));
            }
            if out_lines.len() > 40 {
                out.push(Line::from(Span::styled(
                    format!("    … {} more lines", out_lines.len() - 40),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
        }
    }

    out
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

        let sums = transcript.prefix_sums(width);
        let total_h = *sums.last().unwrap_or(&0);

        let (v_start, v_end) = if total_h <= height {
            self.follow = true;
            self.anchor = (transcript.blocks[0].key.clone(), 0);
            (0, total_h)
        } else if self.follow {
            let start = total_h - height;
            for i in 0..transcript.blocks.len() {
                if sums[i] <= start && start < sums[i + 1] {
                    self.anchor = (transcript.blocks[i].key.clone(), start - sums[i]);
                    break;
                }
            }
            (start, total_h)
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
            (start, (start + height).min(total_h))
        };

        let mut out = Vec::new();
        for i in 0..transcript.blocks.len() {
            let b_start = sums[i];
            let b_end = sums[i + 1];
            if b_end <= v_start || b_start >= v_end {
                continue;
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
        let sums = transcript.prefix_sums(width);
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

        for i in 0..transcript.blocks.len() {
            if sums[i] <= new_start && new_start < sums[i + 1] {
                self.anchor = (transcript.blocks[i].key.clone(), new_start - sums[i]);
                break;
            }
        }
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
        for line in &lines {
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
        for line in &lines {
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
}
