use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use edtui::EditorMode;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::widgets::Paragraph;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::chat::driver::{ConnectionState, StateSnapshot, UiCommand, UiEvent};
use crate::chat::types::Notice;
use crate::tui::composer;
use crate::tui::keys::{Action, configure_key_handler, resolve};
use crate::tui::quote;
use crate::tui::status::StatusBar;
use crate::tui::transcript::{Transcript, Viewport};

/// Most replies `/quote` loads; the composer re-renders its whole content every frame.
const MAX_QUOTE: usize = 50;

/// Transcript rows kept visible above a maximized composer.
const MAXIMIZED_TRANSCRIPT_ROWS: u16 = 3;

/// Blank columns left and right of everything honk chat draws, as in opencode's session
/// view (`paddingLeft={2} paddingRight={2}` around both transcript and prompt).
const SIDE_PADDING: u16 = 2;

/// Narrowest content width the padding may leave; below it the full width is used.
const MIN_PADDED_CONTENT_WIDTH: u16 = 20;

/// The frame area minus the side padding: transcript, status bar, composer and command
/// line all lay out (and wrap) within it.
fn content_area(area: Rect) -> Rect {
    if area.width < MIN_PADDED_CONTENT_WIDTH + 2 * SIDE_PADDING {
        return area;
    }
    Rect {
        x: area.x + SIDE_PADDING,
        width: area.width - 2 * SIDE_PADDING,
        ..area
    }
}

/// If `line` is the local command `name` (e.g. `/quote`), its argument (possibly empty).
/// Mirrors how honk chat recognizes commands: trimmed, and `//text` is a literal.
fn local_command<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let t = line.trim();
    if t.starts_with("//") {
        return None;
    }
    if t == name {
        return Some("");
    }
    t.strip_prefix(name)
        .filter(|rest| rest.starts_with(' '))
        .map(str::trim)
}

pub struct App {
    pub session_id: String,
    pub session_title: Option<String>,
    pub cwd: Option<String>,
    pub transcript: Transcript,
    pub viewport: Viewport,
    pub composer: edtui::EditorState,
    pub event_handler: edtui::EditorEventHandler,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    pub history_stash: String,
    pub command_line: Option<String>,
    pub state_snapshot: Option<StateSnapshot>,
    pub pending_prefix: Option<char>,
    pub enhanced_keyboard: bool,
    pub should_quit: bool,
    pub exit_code: Option<i32>,
    pub fatal_error: Option<String>,
    pub cmd_tx: mpsc::UnboundedSender<UiCommand>,
    pub last_transcript_area: Rect,
    pub pending_dump: bool,
    /// First visible visual row of the composer (see `tui::composer`).
    pub composer_scroll: usize,
    /// `zm`: the composer takes most of the screen (see `/quote`).
    pub composer_maximized: bool,
}

impl App {
    pub fn new(session_id: impl Into<String>, cmd_tx: mpsc::UnboundedSender<UiCommand>) -> Self {
        let mut key_handler = edtui::events::KeyEventHandler::vim_mode();
        configure_key_handler(&mut key_handler);
        let event_handler = edtui::EditorEventHandler::new(key_handler);

        let mut composer = edtui::EditorState::default();
        composer.mode = EditorMode::Insert;

        Self {
            session_id: session_id.into(),
            session_title: None,
            cwd: None,
            transcript: Transcript::new(),
            viewport: Viewport::new(),
            composer,
            event_handler,
            history: Vec::new(),
            history_index: None,
            history_stash: String::new(),
            command_line: None,
            state_snapshot: None,
            pending_prefix: None,
            enhanced_keyboard: false,
            should_quit: false,
            exit_code: None,
            fatal_error: None,
            cmd_tx,
            last_transcript_area: Rect::new(0, 0, 80, 24),
            pending_dump: false,
            composer_scroll: 0,
            composer_maximized: false,
        }
    }

    pub fn is_composer_empty(&self) -> bool {
        self.get_composer_text().is_empty()
    }

    pub fn get_composer_text(&self) -> String {
        self.composer.lines.to_string()
    }

    pub fn set_composer_text(&mut self, text: &str) {
        self.composer.lines = if text.is_empty() {
            edtui::Lines::default()
        } else {
            edtui::Lines::from(text)
        };
        let rows = self.composer.lines.len();
        if rows > 0 {
            let last_row = rows - 1;
            let last_col = self.composer.lines.len_col(last_row).unwrap_or(0);
            self.composer.cursor = edtui::Index2::new(last_row, last_col);
        } else {
            self.composer.cursor = edtui::Index2::new(0, 0);
        }
    }

    pub fn connection_state(&self) -> &ConnectionState {
        self.state_snapshot
            .as_ref()
            .map(|s| &s.connection)
            .unwrap_or(&ConnectionState::Live)
    }

    pub fn is_idle(&self) -> bool {
        match &self.state_snapshot {
            Some(s) => !s.run_state.is_running() && !s.prompt_in_flight,
            None => true,
        }
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        // Stopped state + any key -> UiCommand::RetryAttach (in addition to key's normal effect)
        if matches!(self.connection_state(), ConnectionState::Stopped { .. }) {
            let _ = self.cmd_tx.send(UiCommand::RetryAttach);
        }

        // If app command line is active
        if let Some(mut cmd) = self.command_line.take() {
            match key_event.code {
                KeyCode::Esc => {
                    // Esc closes command line
                    return;
                }
                KeyCode::Char('c') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Ctrl-C closes command line
                    return;
                }
                KeyCode::Enter => {
                    // Execute command line
                    self.execute_app_command(&cmd);
                    return;
                }
                KeyCode::Backspace => {
                    cmd.pop();
                    self.command_line = Some(cmd);
                    return;
                }
                KeyCode::Char(c)
                    if key_event.modifiers.is_empty()
                        || key_event.modifiers == KeyModifiers::SHIFT =>
                {
                    cmd.push(c);
                    self.command_line = Some(cmd);
                    return;
                }
                _ => {
                    self.command_line = Some(cmd);
                    return;
                }
            }
        }

        let mode = self.composer.mode;
        let composer_empty = self.is_composer_empty();
        let action = resolve(
            mode,
            composer_empty,
            key_event,
            self.pending_prefix,
            self.enhanced_keyboard,
        );

        // Reset pending prefix unless new action is a prefix
        if let Action::PendingPrefix(c) = action {
            self.pending_prefix = Some(c);
            return;
        } else {
            self.pending_prefix = None;
        }

        let (width, height) = (
            self.last_transcript_area.width,
            self.last_transcript_area.height as usize,
        );

        match action {
            Action::ScrollLines(delta) => {
                self.viewport
                    .scroll_lines(delta, &mut self.transcript, width, height);
            }
            Action::ScrollHalfPage(dir) => {
                self.viewport
                    .half_page(dir, &mut self.transcript, width, height);
            }
            Action::ScrollPage(dir) => {
                self.viewport.page(dir, &mut self.transcript, width, height);
            }
            Action::ScrollTop => {
                self.viewport.top(&mut self.transcript, width, height);
            }
            Action::ScrollBottom => {
                self.viewport.bottom(&mut self.transcript, width, height);
            }
            Action::ToggleTools => {
                self.transcript.toggle_tools();
            }
            Action::ToggleThoughts => {
                self.transcript.toggle_thoughts();
            }
            Action::ToggleJson => {
                self.transcript.toggle_json();
            }
            Action::ToggleMaximize => {
                self.composer_maximized = !self.composer_maximized;
            }
            Action::OpenCommand => {
                self.command_line = Some(String::new());
            }
            Action::Submit => {
                let text = self.get_composer_text();
                let trimmed = text.trim();
                if let Some(arg) = local_command(trimmed, "/quote") {
                    let arg = arg.to_string();
                    self.set_composer_text("");
                    self.quote(&arg, false);
                } else if local_command(trimmed, "/yank").is_some() {
                    self.set_composer_text("");
                    self.reset_history_recall();
                    let mut stdout = std::io::stdout();
                    self.yank_last_assistant(&mut stdout);
                } else if !trimmed.is_empty() {
                    let _ = self.cmd_tx.send(UiCommand::Line(text.clone()));
                    self.history.push(text);
                    self.reset_history_recall();
                    self.set_composer_text("");
                    self.composer.mode = EditorMode::Insert;
                    self.composer_maximized = false;
                }
            }
            Action::Newline => {
                self.composer.execute(edtui::actions::LineBreak(1));
            }
            Action::HistoryPrev => {
                if !self.history.is_empty() {
                    if self.history_index.is_none() {
                        self.history_stash = self.get_composer_text();
                        let idx = self.history.len() - 1;
                        self.history_index = Some(idx);
                        let text = self.history[idx].clone();
                        self.set_composer_text(&text);
                    } else if let Some(idx) = self.history_index
                        && idx > 0
                    {
                        let new_idx = idx - 1;
                        self.history_index = Some(new_idx);
                        let text = self.history[new_idx].clone();
                        self.set_composer_text(&text);
                    }
                }
            }
            Action::HistoryNext => {
                if let Some(idx) = self.history_index {
                    if idx + 1 < self.history.len() {
                        let new_idx = idx + 1;
                        self.history_index = Some(new_idx);
                        let text = self.history[new_idx].clone();
                        self.set_composer_text(&text);
                    } else {
                        self.history_index = None;
                        let stash = std::mem::take(&mut self.history_stash);
                        self.set_composer_text(&stash);
                    }
                }
            }
            Action::Interrupt => {
                // A maximized draft (e.g. a loaded quote) is cleared even mid-run, so the
                // reflex "Ctrl-C to discard" cannot cancel the run instead.
                if (self.is_idle() || self.composer_maximized) && !self.is_composer_empty() {
                    self.set_composer_text("");
                    self.composer_maximized = false;
                } else {
                    let _ = self.cmd_tx.send(UiCommand::Interrupt);
                }
            }
            Action::LeaveInsertMode => {
                self.composer.mode = EditorMode::Normal;
            }
            Action::Quit => {
                let _ = self.cmd_tx.send(UiCommand::Eof);
            }
            Action::Edtui(key) => {
                self.event_handler.on_key_event(key, &mut self.composer);
            }
            Action::EscKey(next_key) => {
                self.composer.mode = EditorMode::Normal;
                self.handle_key(next_key);
            }
            Action::PendingPrefix(_) | Action::Ignore => {}
        }
    }

    pub fn execute_app_command(&mut self, raw: &str) {
        let cmd = raw.strip_prefix(':').unwrap_or(raw).trim();
        match cmd {
            "q" | "quit" => {
                let _ = self.cmd_tx.send(UiCommand::Eof);
            }
            "yank" => {
                let mut stdout = std::io::stdout();
                self.yank_last_assistant(&mut stdout);
            }
            "dump" => {
                self.pending_dump = true;
            }
            cmd if local_command(cmd, "quote").is_some() => {
                let arg = local_command(cmd, "quote").unwrap_or("").to_string();
                self.quote(&arg, true);
            }
            "cancel" => {
                let _ = self.cmd_tx.send(UiCommand::Line("/cancel".into()));
            }
            "cancel!" => {
                let _ = self.cmd_tx.send(UiCommand::Line("/cancel!".into()));
            }
            "drop" => {
                let _ = self.cmd_tx.send(UiCommand::Line("/drop".into()));
            }
            "approve" => {
                let _ = self.cmd_tx.send(UiCommand::Line("/approve".into()));
            }
            "help" => {
                let _ = self.cmd_tx.send(UiCommand::Line("/help".into()));
            }
            "json" => {
                self.transcript.toggle_json();
            }
            cmd if cmd.starts_with("turns ") || cmd == "turns" => {
                let _ = self.cmd_tx.send(UiCommand::Line(format!("/{cmd}")));
            }
            cmd if cmd == "compact" || cmd.starts_with("compact ") => {
                let _ = self.cmd_tx.send(UiCommand::Line(format!("/{cmd}")));
            }
            cmd if cmd == "undo" || cmd.starts_with("undo ") => {
                let _ = self.cmd_tx.send(UiCommand::Line(format!("/{cmd}")));
            }
            cmd if cmd == "redo" || cmd.starts_with("redo ") => {
                let _ = self.cmd_tx.send(UiCommand::Line(format!("/{cmd}")));
            }
            cmd if cmd == "mcp" || cmd.starts_with("mcp ") => {
                let _ = self.cmd_tx.send(UiCommand::Line(format!("/{cmd}")));
            }
            other => {
                self.transcript
                    .apply_notice(Notice::warning(format!("unknown command: :{other}")));
            }
        }
    }

    fn reset_history_recall(&mut self) {
        self.history_index = None;
        self.history_stash.clear();
    }

    /// `/quote [N]` / `:quote [N]`: loads the last N coordinator replies, quoted, into the
    /// composer (see `tui::quote`). `keep_draft` keeps the current composer text below them.
    pub fn quote(&mut self, arg: &str, keep_draft: bool) {
        let n = if arg.is_empty() {
            3
        } else {
            match arg.parse::<usize>() {
                Ok(n) if n > MAX_QUOTE => {
                    self.transcript.apply_notice(Notice::info(format!(
                        "/quote is capped at {MAX_QUOTE} replies"
                    )));
                    MAX_QUOTE
                }
                Ok(n) if n >= 1 => n,
                _ => {
                    self.transcript.apply_notice(Notice::warning(
                        "invalid /quote argument: must be a positive integer",
                    ));
                    return;
                }
            }
        };
        // The core's run state, not the transcript's: the transcript's `active_run` is not
        // carried across a replay swap, so right after attaching it can read idle mid-run.
        let running = !self.is_idle();
        let Some(q) = quote::build(&self.transcript.blocks, n, running, quote::local_time) else {
            self.transcript
                .apply_notice(Notice::warning("no replies to quote"));
            return;
        };
        if q.count < n {
            self.transcript
                .apply_notice(Notice::info(format!("only {} replies to quote", q.count)));
        }
        let draft = if keep_draft {
            self.get_composer_text()
        } else {
            String::new()
        };
        let text = if draft.trim().is_empty() {
            q.text
        } else {
            format!("{}\n\n{}", q.text, draft)
        };
        self.set_composer_text(&text);
        self.composer.cursor = edtui::Index2::new(0, 0);
        self.composer.mode = EditorMode::Normal;
        self.composer_scroll = 0;
        self.composer_maximized = true;
        self.reset_history_recall();
    }

    pub fn yank_last_assistant<W: Write>(&mut self, writer: &mut W) {
        if let Some(text) = self.transcript.last_assistant_text() {
            use base64::Engine;
            let encoded = base64::prelude::BASE64_STANDARD.encode(text.as_bytes());
            if encoded.len() > 100 * 1024 {
                self.transcript.apply_notice(Notice::warning(format!(
                    "assistant message too large to yank via OSC 52 ({} KB > 100 KB limit)",
                    encoded.len() / 1024
                )));
                return;
            }
            let osc = format!("\x1b]52;c;{encoded}\x07");
            let _ = writer.write_all(osc.as_bytes());
            let _ = writer.flush();
        }
    }

    pub fn suspend_terminal(&self) -> anyhow::Result<()> {
        let mut stdout = std::io::stdout();
        let _ = crossterm::terminal::disable_raw_mode();
        if self.enhanced_keyboard {
            let _ = crossterm::execute!(stdout, crossterm::event::PopKeyboardEnhancementFlags);
        }
        let _ = crossterm::execute!(
            stdout,
            crossterm::event::DisableBracketedPaste,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = stdout.write_all(b"\x1b[?1007l");
        let _ = stdout.flush();
        Ok(())
    }

    pub fn resume_terminal(&self) -> anyhow::Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(
            stdout,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::cursor::Hide
        )?;
        let _ = stdout.write_all(b"\x1b[?1007h");
        if self.enhanced_keyboard {
            let _ = crossterm::execute!(
                stdout,
                crossterm::event::PushKeyboardEnhancementFlags(
                    crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                )
            );
        }
        let _ = stdout.flush();
        Ok(())
    }

    pub fn dump_transcript(&mut self) -> anyhow::Result<()> {
        let md = self.transcript.to_markdown();
        let mut temp = tempfile::Builder::new()
            .prefix("honk-dump-")
            .suffix(".md")
            .tempfile()?;
        temp.write_all(md.as_bytes())?;
        temp.flush()?;
        let path = temp.path().to_path_buf();

        let prog = std::env::var("PAGER")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "less -R".to_string());

        let _ = self.suspend_terminal();

        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{} \"$1\"", prog))
            .arg("sh")
            .arg(&path)
            .status();

        let _ = self.resume_terminal();

        Ok(())
    }

    pub fn handle_paste(&mut self, text: String) {
        composer::paste(&mut self.composer, &self.event_handler, &text);
    }

    pub fn handle_ui_event(&mut self, event: UiEvent) {
        self.transcript.apply_ui_event(&event, &mut self.viewport);
        match event {
            UiEvent::Attached { result } => {
                if let Some(title) = result.get("title").and_then(Value::as_str) {
                    self.session_title = Some(title.to_string());
                } else if let Some(title) = result
                    .get("session")
                    .and_then(|s| s.get("title"))
                    .and_then(Value::as_str)
                {
                    self.session_title = Some(title.to_string());
                }
                if let Some(cwd) = result
                    .get("cwd")
                    .or_else(|| result.pointer("/_meta/workingDir"))
                    .or_else(|| result.pointer("/session/cwd"))
                    .and_then(Value::as_str)
                {
                    self.cwd = Some(cwd.to_string());
                }
                if let Some(id) = result.get("sessionId").and_then(Value::as_str)
                    && self.session_id.is_empty()
                {
                    self.session_id = id.to_string();
                }
                self.update_terminal_title();
            }
            UiEvent::SessionNew { result } => {
                if let Some(title) = result.get("title").and_then(Value::as_str) {
                    self.session_title = Some(title.to_string());
                } else if let Some(title) = result
                    .get("session")
                    .and_then(|s| s.get("title"))
                    .and_then(Value::as_str)
                {
                    self.session_title = Some(title.to_string());
                }
                if let Some(cwd) = result
                    .get("cwd")
                    .or_else(|| result.pointer("/_meta/workingDir"))
                    .or_else(|| result.pointer("/session/cwd"))
                    .and_then(Value::as_str)
                {
                    self.cwd = Some(cwd.to_string());
                }
                if let Some(id) = result.get("sessionId").and_then(Value::as_str) {
                    self.session_id = id.to_string();
                }
                self.update_terminal_title();
            }
            UiEvent::SessionInfo { title, cwd } => {
                if let Some(t) = title {
                    self.session_title = Some(t);
                    self.update_terminal_title();
                }
                if let Some(c) = cwd {
                    self.cwd = Some(c);
                }
            }
            UiEvent::State(snapshot) => {
                self.state_snapshot = Some(snapshot);
            }
            UiEvent::Notice(notice) => {
                if let Some(ref text) = notice.returned_text
                    && self.is_composer_empty()
                {
                    self.set_composer_text(text);
                    self.composer.mode = EditorMode::Insert;
                }
                if let Some(ref text) = notice.returned_clear
                    && self.get_composer_text() == *text
                {
                    self.set_composer_text("");
                }
            }
            UiEvent::Fatal(err) => {
                self.fatal_error = Some(err);
                self.should_quit = true;
            }
            UiEvent::Exit(code) => {
                self.exit_code = Some(code);
                self.should_quit = true;
            }
            UiEvent::Transcript(_) => {}
        }
    }

    pub fn update_terminal_title(&self) {
        let title = self.session_title.as_deref().unwrap_or(&self.session_id);
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(format!("\x1b]0;{title}\x07").as_bytes());
        let _ = stdout.flush();
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = content_area(frame.area());
        if area.width == 0 || area.height == 0 {
            return;
        }

        // The composer spans the content width; render it first to learn its wrapped height.
        let rendered_composer = composer::Rendered::new(&mut self.composer, area.width);
        // Rows left for the composer after the status bar, the command line and a few
        // transcript rows.
        let reserved = 1 + u16::from(self.command_line.is_some()) + MAXIMIZED_TRANSCRIPT_ROWS;
        let available = area.height.saturating_sub(reserved).max(1);
        let composer_height = if self.composer_maximized {
            available
        } else {
            rendered_composer.height().min(available)
        };

        let chunks = if self.command_line.is_some() {
            Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(composer_height),
                Constraint::Length(1),
            ])
            .split(area)
        } else {
            Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(composer_height),
            ])
            .split(area)
        };

        let transcript_area = chunks[0];
        let status_area = chunks[1];
        let composer_area = chunks[2];

        self.last_transcript_area = transcript_area;

        // 1. Transcript pane
        let lines = self.viewport.visible_lines(
            &mut self.transcript,
            transcript_area.width,
            transcript_area.height as usize,
        );
        let p = Paragraph::new(lines);
        frame.render_widget(p, transcript_area);

        // 2. Status bar
        let status_bar = StatusBar {
            session_id: &self.session_id,
            session_title: self.session_title.as_deref(),
            cwd: self.cwd.as_deref(),
            state_snapshot: self.state_snapshot.as_ref(),
            mode: self.composer.mode,
            usage: self.transcript.usage(),
        };
        frame.render_widget(status_bar, status_area);

        // 3. Composer
        let composer_cursor =
            rendered_composer.blit(composer_area, frame.buffer_mut(), &mut self.composer_scroll);

        // 4. Command line if active
        if let Some(cmd_area) = chunks.get(3)
            && let Some(ref cmd) = self.command_line
        {
            let cmd_text = format!(":{}", cmd);
            let p = Paragraph::new(cmd_text);
            frame.render_widget(p, *cmd_area);
            let cursor_x = cmd_area.x + 1 + cmd.len() as u16;
            frame.set_cursor_position(Position::new(
                cursor_x.min(cmd_area.right().saturating_sub(1)),
                cmd_area.y,
            ));
            return;
        }

        frame.set_cursor_position(composer_cursor);
    }
}

static RESTORED: AtomicBool = AtomicBool::new(false);

pub struct TerminalGuard {
    pub enhanced_keyboard: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal(self.enhanced_keyboard);
    }
}

pub fn restore_terminal(enhanced_keyboard: bool) {
    if RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut stdout = std::io::stdout();
    let _ = crossterm::terminal::disable_raw_mode();
    if enhanced_keyboard {
        let _ = crossterm::execute!(stdout, crossterm::event::PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(
        stdout,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    // Disable alternate scroll mode and pop title from stack
    let _ = stdout.write_all(b"\x1b[?1007l\x1b[23;0t");
    let _ = stdout.flush();
}

pub fn setup_terminal() -> anyhow::Result<(
    ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    TerminalGuard,
)> {
    RESTORED.store(false, Ordering::SeqCst);
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste,
        crossterm::cursor::Hide
    )?;

    // Push terminal title onto stack and enable alternate scroll
    stdout.write_all(b"\x1b[22;0t\x1b[?1007h")?;
    stdout.flush()?;

    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = crossterm::execute!(
            stdout,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        );
    }

    let guard = TerminalGuard {
        enhanced_keyboard: enhanced,
    };

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(enhanced);
        default_hook(info);
    }));

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let terminal = ratatui::Terminal::new(backend)?;
    Ok((terminal, guard))
}

/// Drains warning and error notices still queued for the TUI.
fn unseen_warnings(ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(evt) = ui_rx.try_recv() {
        if let UiEvent::Notice(n) = evt
            && n.kind != crate::chat::NoticeKind::Info
        {
            out.push(n.text);
        }
    }
    out
}

pub async fn run_app(
    mut app: App,
    mut ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    handle: crate::chat::driver::DriverHandle,
) -> anyhow::Result<i32> {
    let (mut terminal, _guard) = setup_terminal()?;
    app.enhanced_keyboard = _guard.enhanced_keyboard;

    let mut event_stream = crossterm::event::EventStream::new();
    let mut render_interval = tokio::time::interval(std::time::Duration::from_millis(33));
    render_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    terminal.draw(|f| app.draw(f))?;
    let mut dirty = false;

    loop {
        if app.should_quit {
            break;
        }

        tokio::select! {
            _ = render_interval.tick() => {
                if dirty {
                    terminal.draw(|f| app.draw(f))?;
                    dirty = false;
                }
            }
            _ = sigterm.recv() => {
                break;
            }
            _ = sighup.recv() => {
                break;
            }
            _ = sigint.recv() => {
                break;
            }
            maybe_event = futures::StreamExt::next(&mut event_stream) => {
                match maybe_event {
                    Some(Ok(event)) => {
                        match event {
                            crossterm::event::Event::Key(key_event) => {
                                app.handle_key(key_event);
                                dirty = true;
                            }
                            crossterm::event::Event::Paste(text) => {
                                app.handle_paste(text);
                                dirty = true;
                            }
                            crossterm::event::Event::Resize(_w, _h) => {
                                dirty = true;
                            }
                            _ => {}
                        }
                    }
                    Some(Err(_)) => {
                        // Pty died or input stream error -> exit cleanly
                        break;
                    }
                    None => break,
                }
            }
            maybe_ui_event = ui_rx.recv() => {
                match maybe_ui_event {
                    Some(event) => {
                        let is_fatal = matches!(&event, UiEvent::Fatal(_));
                        let is_exit = matches!(&event, UiEvent::Exit(_));
                        app.handle_ui_event(event);
                        dirty = true;
                        if is_fatal || is_exit {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }

        if app.pending_dump {
            app.pending_dump = false;
            let markdown = app.transcript.to_markdown();

            // 1. Drop event_stream so crossterm doesn't read stdin while pager runs
            drop(event_stream);

            // 2. Suspend terminal
            let _ = app.suspend_terminal();

            // 3. Write markdown to tempfile and run pager via tokio::process
            let temp_opt = tempfile::Builder::new()
                .prefix("honk-dump-")
                .suffix(".md")
                .tempfile();

            if let Ok(mut temp) = temp_opt {
                use std::io::Write;
                let _ = temp.write_all(markdown.as_bytes());
                let _ = temp.flush();
                let path = temp.path().to_path_buf();

                let prog = std::env::var("PAGER")
                    .or_else(|_| std::env::var("EDITOR"))
                    .unwrap_or_else(|_| "less -R".to_string());

                let _ = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!("{} \"$1\"", prog))
                    .arg("sh")
                    .arg(&path)
                    .status()
                    .await;
            }

            // 4. Resume terminal
            let _ = app.resume_terminal();

            // 5. Clear terminal
            let _ = terminal.clear();

            // 6. Recreate event stream
            event_stream = crossterm::event::EventStream::new();

            dirty = true;
        }
    }

    drop(_guard);

    // Join before any process::exit below, so the driver's own shutdown (which
    // waits briefly for a pigeon registration in flight) runs on every exit path.
    let _ = handle.send(UiCommand::Shutdown);
    let _ = handle.join().await;
    // Warnings that arrived after the TUI stopped drawing (a pigeon or oc-tags
    // failure right after a quick `:q`, or one not confirmed by the exit
    // grace) would otherwise be lost; the terminal is restored, so print them.
    for text in unseen_warnings(&mut ui_rx) {
        eprintln!("honk: {text}");
    }

    if let Some(err) = app.fatal_error {
        eprintln!("{err}");
        std::process::exit(1);
    }
    if let Some(code) = app.exit_code {
        std::process::exit(code);
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseen_warnings_keeps_warnings_and_errors_only() {
        use crate::chat::Notice;
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(UiEvent::Notice(Notice::info("Tag: fine"))).unwrap();
        tx.send(UiEvent::Notice(Notice::warning("Tag: not applied")))
            .unwrap();
        tx.send(UiEvent::Exit(0)).unwrap();
        tx.send(UiEvent::Notice(Notice::error("boom"))).unwrap();
        assert_eq!(
            unseen_warnings(&mut rx),
            vec!["Tag: not applied".to_string(), "boom".to_string()]
        );
        assert!(unseen_warnings(&mut rx).is_empty());
    }
    use crate::chat::types::{Ownership, RunState};
    use crate::tui::BlockKind;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        press(code, KeyModifiers::empty())
    }

    fn ctrl(c: char) -> KeyEvent {
        press(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn shift(c: char) -> KeyEvent {
        press(KeyCode::Char(c), KeyModifiers::SHIFT)
    }

    #[test]
    fn test_submit_sends_line_and_clears_composer() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.set_composer_text("hello honk");
        app.composer.mode = EditorMode::Insert;

        app.handle_key(key(KeyCode::Enter));

        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            UiCommand::Line("hello honk".into())
        );
        assert!(app.is_composer_empty());
        assert_eq!(app.composer.mode, EditorMode::Insert);
    }

    #[test]
    fn test_history_recall() {
        let (cmd_tx, mut _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.composer.mode = EditorMode::Insert;

        // Submit first prompt
        app.set_composer_text("first prompt");
        app.handle_key(key(KeyCode::Enter));

        // Submit second prompt
        app.set_composer_text("second prompt");
        app.handle_key(key(KeyCode::Enter));

        // Recall prev (second prompt)
        app.handle_key(ctrl('p'));
        assert_eq!(app.get_composer_text(), "second prompt");

        // Recall prev again (first prompt)
        app.handle_key(ctrl('p'));
        assert_eq!(app.get_composer_text(), "first prompt");

        // Forward to next (second prompt)
        app.handle_key(ctrl('n'));
        assert_eq!(app.get_composer_text(), "second prompt");

        // Forward past newest restores stash (empty)
        app.handle_key(ctrl('n'));
        assert_eq!(app.get_composer_text(), "");
    }

    #[test]
    fn test_returned_text_refills_composer_when_empty() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);

        let notice = Notice::returned("Your message did not run", "my rejected prompt");

        // When empty: fills composer
        app.handle_ui_event(UiEvent::Notice(notice));
        assert_eq!(app.get_composer_text(), "my rejected prompt");

        // When not empty: does NOT overwrite composer
        app.set_composer_text("user is currently typing something else");
        let notice2 = Notice::returned("Another failure", "another rejected prompt");
        app.handle_ui_event(UiEvent::Notice(notice2));
        assert_eq!(
            app.get_composer_text(),
            "user is currently typing something else"
        );
    }

    #[test]
    fn test_returned_clear_clears_matching_composer_text() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);

        // When composer matches returned_clear: clears composer
        app.set_composer_text("undone prompt");
        let notice = Notice::returned_clear("restored 1 turn", "undone prompt");
        app.handle_ui_event(UiEvent::Notice(notice));
        assert_eq!(app.get_composer_text(), "");

        // When composer has been modified: does NOT clear composer
        app.set_composer_text("undone prompt with user edit");
        let notice2 = Notice::returned_clear("restored 1 turn", "undone prompt");
        app.handle_ui_event(UiEvent::Notice(notice2));
        assert_eq!(app.get_composer_text(), "undone prompt with user edit");

        // When composer is already empty: remains empty
        app.set_composer_text("");
        let notice3 = Notice::returned_clear("restored 1 turn", "undone prompt");
        app.handle_ui_event(UiEvent::Notice(notice3));
        assert_eq!(app.get_composer_text(), "");
    }

    #[test]
    fn test_fast_esc_colon_in_insert_mode_opens_command() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.composer.mode = EditorMode::Insert;

        // Fast Esc + : arrives as Alt+:
        let alt_colon = press(KeyCode::Char(':'), KeyModifiers::ALT);
        app.handle_key(alt_colon);

        assert_eq!(app.composer.mode, EditorMode::Normal);
        assert_eq!(app.command_line, Some(String::new()));
    }

    #[test]
    fn test_colon_q_sends_eof() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.composer.mode = EditorMode::Normal;

        // Open command line with ':'
        app.handle_key(key(KeyCode::Char(':')));
        assert_eq!(app.command_line, Some(String::new()));

        // Type 'q'
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.command_line, Some("q".into()));

        // Enter
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::Eof);
        assert!(app.command_line.is_none());
    }

    #[test]
    fn test_colon_slash_commands() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.composer.mode = EditorMode::Normal;

        let commands = [
            ("cancel", "/cancel"),
            ("cancel!", "/cancel!"),
            ("drop", "/drop"),
            ("approve", "/approve"),
            ("help", "/help"),
            ("turns 5", "/turns 5"),
            ("turns off", "/turns off"),
            ("compact", "/compact"),
            ("compact now", "/compact now"),
            ("mcp", "/mcp"),
            ("mcp enable slack-ro", "/mcp enable slack-ro"),
        ];

        for (typed, expected_cmd) in commands {
            app.handle_key(key(KeyCode::Char(':')));
            for c in typed.chars() {
                app.handle_key(key(KeyCode::Char(c)));
            }
            app.handle_key(key(KeyCode::Enter));
            assert_eq!(
                cmd_rx.try_recv().unwrap(),
                UiCommand::Line(expected_cmd.into())
            );
            assert!(app.command_line.is_none());
        }
    }

    #[test]
    fn test_colon_yank() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.composer.mode = EditorMode::Normal;

        // Feed assistant message chunk
        let chunk = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "msg_ast",
                    "content": {
                        "type": "text",
                        "text": "Hello, assistant text here!\n"
                    }
                }
            }
        });
        app.handle_ui_event(UiEvent::Transcript(chunk));

        assert_eq!(
            app.transcript.last_assistant_text(),
            Some("Hello, assistant text here!\n")
        );

        // Execute yank to test buffer (no escape codes leaking to stdout)
        let mut buf = Vec::new();
        app.yank_last_assistant(&mut buf);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.starts_with("\x1b]52;c;"));
        assert!(output.ends_with("\x07"));

        // Refuse with notice when text is above 100 KB base64
        let large_text = "A".repeat(80_000); // 80 KB raw -> ~106 KB base64
        let chunk_large = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "msg_large",
                    "content": {
                        "type": "text",
                        "text": large_text
                    }
                }
            }
        });
        app.handle_ui_event(UiEvent::Transcript(chunk_large));

        let mut buf2 = Vec::new();
        app.yank_last_assistant(&mut buf2);
        assert!(buf2.is_empty(), "Large text must not be written to sink");
        // Check that a notice was applied
        let last_block = app.transcript.blocks.last().unwrap();
        assert_eq!(last_block.kind, BlockKind::Notice);
    }

    #[test]
    fn test_colon_dump() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.composer.mode = EditorMode::Normal;

        let chunk = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "msg_dump",
                    "content": {
                        "type": "text",
                        "text": "# Markdown Header\nSome body text\n"
                    }
                }
            }
        });
        app.handle_ui_event(UiEvent::Transcript(chunk));

        // Execute :dump
        app.handle_key(key(KeyCode::Char(':')));
        for c in "dump".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert!(app.pending_dump);
        let md = app.transcript.to_markdown();
        assert!(md.contains("Markdown Header"));
    }

    #[test]
    fn test_za_expands_tools_and_zt_toggles_thoughts() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.composer.mode = EditorMode::Normal;

        // Feed a tool call
        let tool_frame = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t1",
                    "title": "shell · ls",
                    "status": "completed"
                }
            }
        });
        app.handle_ui_event(UiEvent::Transcript(tool_frame));
        assert!(!app.transcript.tools_expanded);

        // 'z' then 'a' toggles tools
        app.handle_key(key(KeyCode::Char('z')));
        assert_eq!(app.pending_prefix, Some('z'));
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.pending_prefix, None);
        assert!(app.transcript.tools_expanded);

        // 'z' then 't' toggles thoughts
        assert!(!app.transcript.thoughts_expanded);
        app.handle_key(key(KeyCode::Char('z')));
        app.handle_key(key(KeyCode::Char('t')));
        assert_eq!(app.pending_prefix, None);
        assert!(app.transcript.thoughts_expanded);

        // 'z' then 'j' toggles json
        assert!(!app.transcript.json_expanded);
        app.handle_key(key(KeyCode::Char('z')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.pending_prefix, None);
        assert!(app.transcript.json_expanded);

        // ':json' toggles json back off
        app.execute_app_command(":json");
        assert!(!app.transcript.json_expanded);
    }

    #[test]
    fn test_scrolling_changes_visible_lines() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.last_transcript_area = Rect::new(0, 0, 80, 5);
        app.composer.mode = EditorMode::Normal;

        // Feed multiple text blocks
        for i in 0..20 {
            let chunk = json!({
                "method": "session/update",
                "params": {
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": format!("msg_{i}"),
                        "content": {
                            "type": "text",
                            "text": format!("Line {i}\n")
                        }
                    }
                }
            });
            app.handle_ui_event(UiEvent::Transcript(chunk));
        }

        // Initially follows tail
        let lines_tail = app.viewport.visible_lines(&mut app.transcript, 80, 5);
        let tail_text: Vec<String> = lines_tail.iter().map(|l| l.to_string()).collect();
        assert!(!tail_text.is_empty());

        // Scroll up using Up arrow (3 lines)
        app.handle_key(key(KeyCode::Up));
        let lines_up = app.viewport.visible_lines(&mut app.transcript, 80, 5);
        let up_text: Vec<String> = lines_up.iter().map(|l| l.to_string()).collect();
        assert_ne!(tail_text, up_text);

        // Scroll to top with gg (when composer empty)
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('g')));
        let lines_top = app.viewport.visible_lines(&mut app.transcript, 80, 5);
        let top_text: Vec<String> = lines_top.iter().map(|l| l.to_string()).collect();
        assert_ne!(up_text, top_text);
        assert!(top_text[0].contains("Line 0"));

        // Scroll to bottom with G
        app.handle_key(shift('G'));
        let lines_bottom = app.viewport.visible_lines(&mut app.transcript, 80, 5);
        let bottom_text: Vec<String> = lines_bottom.iter().map(|l| l.to_string()).collect();
        assert_eq!(tail_text, bottom_text);
    }

    #[test]
    fn test_stopped_state_any_key_sends_retry_attach() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.composer.mode = EditorMode::Normal;

        app.state_snapshot = Some(StateSnapshot {
            run_state: RunState::Idle,
            ownership: Ownership::Unknown,
            queued_len: 0,
            replaying: false,
            prompt_in_flight: false,
            connection: ConnectionState::Stopped {
                reason: "closed".into(),
            },
            mcp_enabled: Vec::new(),
        });

        // Any key sends RetryAttach AND performs normal key
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::RetryAttach);
    }

    #[test]
    fn test_ctrl_c_interrupt_and_leave_insert() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);

        // Normal mode: Ctrl-C sends Interrupt
        app.composer.mode = EditorMode::Normal;
        app.handle_key(ctrl('c'));
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::Interrupt);

        // Insert mode with text: leaves insert mode, does not send interrupt
        app.composer.mode = EditorMode::Insert;
        app.set_composer_text("important draft");
        app.handle_key(ctrl('c'));
        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(app.composer.mode, EditorMode::Normal);
        assert_eq!(app.get_composer_text(), "important draft");

        // Insert mode empty: sends Interrupt
        app.composer.mode = EditorMode::Insert;
        app.set_composer_text("");
        app.handle_key(ctrl('c'));
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::Interrupt);
    }

    #[test]
    fn test_ctrl_c_idle_with_text_clears_composer() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);
        app.composer.mode = EditorMode::Normal;
        app.set_composer_text("some draft in normal mode");

        // When idle and composer non-empty in normal mode: clears composer, does NOT send Interrupt
        app.handle_key(ctrl('c'));
        assert!(app.is_composer_empty());
        assert!(cmd_rx.try_recv().is_err());

        // When composer is now empty in normal mode: sends Interrupt
        app.handle_key(ctrl('c'));
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::Interrupt);

        // When active run is running and composer is non-empty in normal mode: sends Interrupt (does NOT clear)
        app.set_composer_text("draft during active run");
        app.state_snapshot = Some(StateSnapshot {
            run_state: RunState::Running("run_1".into()),
            ownership: Ownership::Own,
            queued_len: 0,
            replaying: false,
            prompt_in_flight: false,
            connection: ConnectionState::Live,
            mcp_enabled: Vec::new(),
        });
        app.handle_key(ctrl('c'));
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::Interrupt);
        assert_eq!(app.get_composer_text(), "draft during active run");
    }

    #[test]
    fn test_fatal_and_exit_events() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);

        app.handle_ui_event(UiEvent::Fatal("Authentication failed".into()));
        assert!(app.should_quit);
        assert_eq!(app.fatal_error, Some("Authentication failed".into()));

        app.should_quit = false;
        app.handle_ui_event(UiEvent::Exit(42));
        assert!(app.should_quit);
        assert_eq!(app.exit_code, Some(42));
    }

    #[test]
    fn test_status_bar_buffer_contents_in_app_draw() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("test_session_id", cmd_tx);
        app.handle_ui_event(UiEvent::Attached {
            result: json!({
                "title": "My Active Session",
                "cwd": "/tmp/test-work",
                "sessionId": "test_session_id"
            }),
        });

        app.handle_ui_event(UiEvent::State(StateSnapshot {
            run_state: RunState::Running("run_1".into()),
            ownership: Ownership::Foreign,
            queued_len: 2,
            replaying: false,
            prompt_in_flight: false,
            connection: ConnectionState::Live,
            mcp_enabled: Vec::new(),
        }));

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| app.draw(f)).unwrap();

        let buffer = terminal.backend().buffer();
        let buffer_str: String = buffer.content.iter().map(|cell| cell.symbol()).collect();

        assert!(buffer_str.contains("My Active Session — /tmp/test-work"));
        assert!(buffer_str.contains("running (other client)"));
        assert!(buffer_str.contains("queued: 2"));
        assert!(buffer_str.contains("live"));
        assert!(buffer_str.contains("INSERT"));
    }

    #[test]
    fn test_status_bar_usage_in_app_draw() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("test_session_id", cmd_tx);
        app.handle_ui_event(UiEvent::Attached {
            result: json!({
                "title": "My Session",
                "cwd": "/tmp/work",
                "sessionId": "test_session_id"
            }),
        });

        let usage_frame = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "usage_update",
                    "used": 45231,
                    "size": 200000,
                    "cost": {
                        "amount": 1.84,
                        "currency": "USD"
                    }
                }
            }
        });
        app.handle_ui_event(UiEvent::Transcript(usage_frame));

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| app.draw(f)).unwrap();

        let buffer = terminal.backend().buffer();
        let buffer_str: String = buffer.content.iter().map(|cell| cell.symbol()).collect();

        assert!(
            buffer_str.contains("45.2K (23%) · $1.84"),
            "buffer did not contain usage readout: {buffer_str}"
        );
    }

    // ---- composer wrapping and paste ----

    /// Draws `app` on a `w`x`h` test terminal and returns each screen row as a string,
    /// plus the cursor position.
    fn render_rows(app: &mut App, w: u16, h: u16) -> (Vec<String>, Position) {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect();
        (rows, cursor)
    }

    fn type_str(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
    }

    /// The composer is the bottom block of rows below the status bar; returns (top row, rows).
    fn composer_rows(rows: &[String], status_marker: &str) -> (usize, Vec<String>) {
        let status = rows
            .iter()
            .rposition(|r| r.contains(status_marker))
            .expect("status bar row");
        (status + 1, rows[status + 1..].to_vec())
    }

    #[test]
    fn test_typing_past_width_wraps_and_stays_visible() {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_wrap", cmd_tx);
        // 45 chars at width 20 = 3 visual rows.
        let text = "aaaaaaaaaabbbbbbbbbbccccccccccddddddddddeeeee";
        type_str(&mut app, text);
        assert_eq!(app.get_composer_text(), text);

        let (rows, cursor) = render_rows(&mut app, 20, 12);
        let (top, composer) = composer_rows(&rows, "INSERT");
        assert_eq!(
            composer.len(),
            3,
            "composer grows to 3 visual rows: {rows:#?}"
        );
        assert_eq!(composer[0], "aaaaaaaaaabbbbbbbbbb");
        assert_eq!(composer[1], "ccccccccccdddddddddd");
        assert_eq!(composer[2].trim_end(), "eeeee");
        // Cursor sits right after the last typed char, inside the composer.
        assert_eq!(cursor, Position::new(5, (top + 2) as u16));
    }

    #[test]
    fn test_long_line_taller_than_composer_follows_cursor() {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_wrap", cmd_tx);
        // One logical line of 12 visual rows at width 10; composer caps at 8.
        let text: String = (0..12)
            .map(|i| char::from(b'a' + i as u8).to_string().repeat(10))
            .collect::<String>()
            + "Z";
        type_str(&mut app, &text);

        let (rows, cursor) = render_rows(&mut app, 10, 20);
        let (top, composer) = composer_rows(&rows, "INSERT");
        assert_eq!(composer.len(), 8, "{rows:#?}");
        // The cursor's visual row (the 13th, holding "Z") is the bottom row.
        assert_eq!(composer[7].trim_end(), "Z", "{rows:#?}");
        assert_eq!(composer[6], "llllllllll");
        assert_eq!(cursor, Position::new(1, (top + 7) as u16));

        // Jumping to the start of the line scrolls back to the first visual row.
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('0')));
        let (rows, cursor) = render_rows(&mut app, 10, 20);
        let (top, composer) = composer_rows(&rows, "NORMAL");
        assert_eq!(composer[0], "aaaaaaaaaa", "{rows:#?}");
        assert_eq!(cursor, Position::new(0, top as u16));
    }

    #[test]
    fn test_paste_with_carriage_returns_becomes_lines() {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_paste", cmd_tx);
        // tmux and many terminals send CR (or CRLF) between pasted lines.
        app.handle_paste("one\rtwo\r\nthree".to_string());
        assert_eq!(app.get_composer_text(), "one\ntwo\nthree");

        let (rows, _) = render_rows(&mut app, 30, 12);
        let (_, composer) = composer_rows(&rows, "INSERT");
        let composer: Vec<&str> = composer.iter().map(|r| r.trim()).collect();
        assert_eq!(composer, vec!["one", "two", "three"]);
    }

    #[test]
    fn test_side_padding_insets_every_region() {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_pad", cmd_tx);
        let reply = "w".repeat(50);
        app.handle_ui_event(UiEvent::Transcript(json!({
            "method": "session/update",
            "params": {"update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "msg_pad",
                "content": {"type": "text", "text": reply}
            }}
        })));
        // 45 chars wrap at the 36-column content width, not the 40-column terminal.
        let text = "a".repeat(36) + "bbbbbbbbb";
        type_str(&mut app, &text);

        let (rows, cursor) = render_rows(&mut app, 40, 16);
        assert_eq!(app.last_transcript_area, Rect::new(2, 0, 36, 13));
        for row in &rows {
            assert!(
                row.starts_with("  ") && row.ends_with("  "),
                "two blank columns each side: {rows:#?}"
            );
        }
        // The reply wraps within the content width.
        let reply_rows: Vec<&str> = rows
            .iter()
            .map(|r| r.trim())
            .filter(|r| r.contains('w'))
            .collect();
        assert_eq!(reply_rows.len(), 2, "{rows:#?}");
        assert!(reply_rows[0].len() <= 36, "{rows:#?}");

        let (top, composer) = composer_rows(&rows, "INSERT");
        assert_eq!(composer[0], format!("  {}  ", "a".repeat(36)));
        assert_eq!(composer[1].trim_end(), "  bbbbbbbbb");
        assert_eq!(cursor, Position::new(2 + 9, (top + 1) as u16));

        // The `:` command line is inset too, and so is its cursor.
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char(':')));
        type_str(&mut app, "dump");
        let (rows, cursor) = render_rows(&mut app, 40, 16);
        assert_eq!(rows[15].trim_end(), "  :dump");
        assert_eq!(cursor, Position::new(2 + 5, 15));
    }

    #[test]
    fn test_narrow_terminal_drops_side_padding() {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_pad", cmd_tx);
        type_str(&mut app, &"a".repeat(23));
        let (rows, cursor) = render_rows(&mut app, 23, 12);
        let (top, composer) = composer_rows(&rows, "INSERT");
        assert_eq!(composer[0], "a".repeat(23), "{rows:#?}");
        assert_eq!(cursor, Position::new(0, (top + 1) as u16));
        // 24 columns is the narrowest that keeps the padding (20 content columns).
        assert_eq!(content_area(Rect::new(0, 0, 24, 5)), Rect::new(2, 0, 20, 5));
        assert_eq!(content_area(Rect::new(0, 0, 23, 5)), Rect::new(0, 0, 23, 5));
    }

    #[test]
    fn test_paste_long_text_wraps() {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_paste", cmd_tx);
        app.handle_paste("x".repeat(25));
        let (rows, _) = render_rows(&mut app, 10, 12);
        let (_, composer) = composer_rows(&rows, "INSERT");
        let composer: Vec<&str> = composer.iter().map(|r| r.trim_end()).collect();
        assert_eq!(composer, vec!["xxxxxxxxxx", "xxxxxxxxxx", "xxxxx"]);
    }

    #[test]
    fn test_paste_in_insert_mode_goes_at_cursor() {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_paste", cmd_tx);
        type_str(&mut app, "abc");
        app.handle_paste("XYZ".to_string());
        assert_eq!(app.get_composer_text(), "abcXYZ");
        type_str(&mut app, "!");
        assert_eq!(app.get_composer_text(), "abcXYZ!");

        // Mid-line: cursor between 'a' and 'b'.
        let mut app2 = App::new("ses_paste2", mpsc::unbounded_channel().0);
        type_str(&mut app2, "ab");
        app2.handle_key(key(KeyCode::Left));
        app2.handle_paste("1\n2".to_string());
        assert_eq!(app2.get_composer_text(), "a1\n2b");
    }

    fn update(kind: &str, id: &str, text: &str, created: i64) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "ses_q",
            "update": {"sessionUpdate": kind, "messageId": id,
                "content": {"type": "text", "text": text},
                "_meta": {"goose": {"created": created, "messageId": id}}}}})
    }

    fn quote_app() -> (App, mpsc::UnboundedReceiver<UiCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_q", cmd_tx);
        for f in [
            update("user_message_chunk", "u1", "what's up?", 100),
            update("agent_message_chunk", "a1", "reply one", 101),
            update(
                "user_message_chunk",
                "u2",
                "[sw wake run=r reason=mail] Woken",
                200,
            ),
            update("agent_thought_chunk", "t2", "hmm", 201),
            update("agent_message_chunk", "a2", "reply two", 202),
            update(
                "user_message_chunk",
                "u3",
                "<swarm_message v=\"1\" from=\"ses_w\">\nx",
                300,
            ),
        ] {
            app.transcript.apply_frame(&f);
        }
        (app, cmd_rx)
    }

    #[test]
    fn test_slash_quote_fills_maximized_normal_composer_and_sends_nothing() {
        let (mut app, mut cmd_rx) = quote_app();
        type_str(&mut app, "/quote 2");
        app.handle_key(key(KeyCode::Enter));

        assert!(cmd_rx.try_recv().is_err(), "/quote must not go upstream");
        let text = app.get_composer_text();
        assert!(text.starts_with("# "), "{text}");
        assert!(text.contains("(you)\n> reply one\n\n# "), "{text}");
        assert!(text.ends_with("(wake: mail)\n> reply two"), "{text}");
        assert!(!text.contains("Woken") && !text.contains("hmm") && !text.contains("what's up"));
        assert_eq!(app.composer.mode, EditorMode::Normal);
        assert_eq!(app.composer.cursor, edtui::Index2::new(0, 0));
        assert!(app.composer_maximized);
        assert!(app.history.is_empty());
    }

    #[test]
    fn test_colon_quote_keeps_draft_below() {
        let (mut app, mut cmd_rx) = quote_app();
        app.set_composer_text("my draft");
        app.execute_app_command("quote 1");
        assert!(cmd_rx.try_recv().is_err());
        let text = app.get_composer_text();
        assert!(text.ends_with("> reply two\n\nmy draft"), "{text}");
        assert!(!text.contains("reply one"));
    }

    #[test]
    fn test_quote_with_nothing_to_quote_warns() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_q", cmd_tx);
        type_str(&mut app, "/quote");
        app.handle_key(key(KeyCode::Enter));
        assert!(cmd_rx.try_recv().is_err());
        assert!(app.is_composer_empty());
        assert!(!app.composer_maximized);
        let last = app.transcript.blocks.last().unwrap();
        assert_eq!(last.kind, BlockKind::Notice);
        assert!(last.text.contains("no replies to quote"));
    }

    #[test]
    fn test_quote_bad_arg_and_literal_escape() {
        let (mut app, mut cmd_rx) = quote_app();
        app.execute_app_command("quote zero");
        assert!(app.is_composer_empty());
        assert!(
            app.transcript
                .blocks
                .last()
                .unwrap()
                .text
                .contains("invalid /quote")
        );
        // `//quote` is the literal escape and goes upstream untouched.
        type_str(&mut app, "//quote");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            UiCommand::Line("//quote".into())
        );
        // `/quoted` is not the command.
        type_str(&mut app, "/quoted");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            UiCommand::Line("/quoted".into())
        );
    }

    #[test]
    fn test_slash_yank_stays_local() {
        let (mut app, mut cmd_rx) = quote_app();
        type_str(&mut app, "/yank");
        app.handle_key(key(KeyCode::Enter));
        assert!(cmd_rx.try_recv().is_err());
        assert!(app.is_composer_empty());
    }

    #[test]
    fn test_zm_maximizes_composer_and_submit_resets() {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_max", cmd_tx);
        type_str(&mut app, "hi");
        let (rows, _) = render_rows(&mut app, 20, 20);
        assert_eq!(composer_rows(&rows, "INSERT").1.len(), 1);

        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('z')));
        app.handle_key(key(KeyCode::Char('m')));
        assert!(app.composer_maximized);
        let (rows, _) = render_rows(&mut app, 20, 20);
        // 20 rows - status bar - 3 transcript rows.
        assert_eq!(composer_rows(&rows, "NORMAL").1.len(), 16);

        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.composer.mode, EditorMode::Insert);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.is_composer_empty(), "submitted");
        assert!(!app.composer_maximized);
    }

    #[test]
    fn test_maximized_composer_on_tiny_terminal_does_not_panic() {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_tiny", cmd_tx);
        app.composer_maximized = true;
        app.command_line = Some(String::new());
        for h in 1..6 {
            render_rows(&mut app, 10, h);
        }
    }

    #[test]
    fn test_ctrl_c_clears_maximized_draft_even_mid_run() {
        let (mut app, mut cmd_rx) = quote_app();
        app.state_snapshot = Some(StateSnapshot {
            run_state: RunState::Running("r".into()),
            ownership: Ownership::Foreign,
            queued_len: 0,
            replaying: false,
            prompt_in_flight: false,
            connection: ConnectionState::Live,
            mcp_enabled: Vec::new(),
        });
        // Running: the quote's last turn is marked in progress.
        app.transcript
            .apply_frame(&update("agent_message_chunk", "a3", "partial", 301));
        app.set_composer_text("");
        app.execute_app_command("quote 1");
        assert!(
            app.get_composer_text().contains(", in progress)"),
            "{}",
            app.get_composer_text()
        );
        assert!(!app.is_idle());
        app.handle_key(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.is_composer_empty());
        assert!(!app.composer_maximized);
        assert!(cmd_rx.try_recv().is_err(), "no interrupt sent");
    }

    #[test]
    fn test_execute_app_command_undo_and_redo() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut app = App::new("ses_1", cmd_tx);

        app.execute_app_command(":undo");
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::Line("/undo".into()));

        app.execute_app_command("undo");
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::Line("/undo".into()));

        app.execute_app_command(":redo");
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::Line("/redo".into()));

        app.execute_app_command("redo");
        assert_eq!(cmd_rx.try_recv().unwrap(), UiCommand::Line("/redo".into()));
    }
}
