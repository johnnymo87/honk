//! Composer rendering with visual-row wrapping and scrolling.
//!
//! edtui wraps long lines, but its viewport scrolls by *logical* lines only: a single
//! wrapped line taller than the editor area cannot be scrolled through, so the cursor
//! and the text being typed vanish below the area. honk also sized the composer by
//! logical lines, so even a two-row wrapped line was clipped to one row.
//!
//! Here the editor is rendered off-screen at its full wrapped height (so edtui never
//! needs to scroll), then the window of visual rows that contains the cursor is copied
//! into the frame. The composer grows with the number of visual rows up to a cap.

use edtui::{EditorMode, EditorState, EditorTheme, EditorView};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthChar;

/// Columns a tab occupies; passed to edtui so its wrapping matches [`visual_rows`].
pub const TAB_WIDTH: usize = 2;
/// Maximum composer height in rows.
pub const MAX_HEIGHT: u16 = 8;
/// Upper bound on the off-screen render height. Beyond it edtui's own logical-line
/// scrolling takes over (only reachable with very large pastes).
const MAX_RENDER_ROWS: usize = 4000;

fn char_width(ch: char) -> usize {
    if ch == '\t' {
        TAB_WIDTH
    } else {
        ch.width().unwrap_or(0)
    }
}

/// Number of screen rows one logical line occupies when wrapped at `width`, using the
/// same greedy per-char fit as edtui's `LineWrapper::wrap_spans`. An empty line is one row.
pub fn line_rows(line: &[char], width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut rows = 1;
    let mut used = 0;
    for &ch in line {
        let w = char_width(ch);
        if used + w > width {
            rows += 1;
            used = 0;
        }
        used += w;
    }
    rows
}

/// Total wrapped rows of the composer content at `width`.
pub fn visual_rows(state: &EditorState, width: usize) -> usize {
    state
        .lines
        .iter_row()
        .map(|line| line_rows(line, width))
        .sum::<usize>()
        .max(1)
}

/// The composer content rendered at full height, ready to be windowed into the frame.
pub struct Rendered {
    buf: Buffer,
    /// Rows of content, including a trailing row holding only the cursor (edtui puts an
    /// insert-mode cursor at the end of an exactly full row on the next row).
    pub rows: usize,
    /// Cursor position relative to the content (x = column, y = visual row).
    pub cursor: Position,
}

impl Rendered {
    /// Renders `state` off-screen at `width` columns.
    pub fn new(state: &mut EditorState, width: u16) -> Self {
        let width = width.max(1);
        let content_rows = visual_rows(state, width as usize);
        // +1 leaves room for a cursor placed after an exactly full last row.
        let height = (content_rows + 1).min(MAX_RENDER_ROWS) as u16;
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let theme = EditorTheme::default().hide_status_line();
        EditorView::new(state)
            .wrap(true)
            .tab_width(TAB_WIDTH)
            .theme(theme)
            .render(area, &mut buf);

        let cursor = state.cursor_screen_position().unwrap_or_default();
        let cursor = Position::new(cursor.x.min(width - 1), cursor.y.min(height - 1));
        let rows = content_rows.max(cursor.y as usize + 1).min(height as usize);
        Self { buf, rows, cursor }
    }

    /// Composer height for this content: grows with visual rows, 1..=MAX_HEIGHT.
    pub fn height(&self) -> u16 {
        (self.rows as u16).clamp(1, MAX_HEIGHT)
    }

    /// Copies the visible window of rows into `frame_buf` at `area`, adjusting `scroll`
    /// so the cursor row stays visible. Returns the cursor's screen position.
    pub fn blit(&self, area: Rect, frame_buf: &mut Buffer, scroll: &mut usize) -> Position {
        *scroll = follow(
            *scroll,
            self.cursor.y as usize,
            area.height as usize,
            self.rows,
        );
        let src = self.buf.area;
        for dy in 0..area.height {
            let sy = *scroll + dy as usize;
            if sy >= src.height as usize {
                break;
            }
            for dx in 0..area.width.min(src.width) {
                let cell = self.buf[(dx, sy as u16)].clone();
                frame_buf[(area.x + dx, area.y + dy)] = cell;
            }
        }
        Position::new(
            area.x + self.cursor.x.min(area.width.saturating_sub(1)),
            area.y + (self.cursor.y as usize - *scroll) as u16,
        )
    }
}

/// New scroll offset (first visible row) so that `cursor_row` is inside a window of
/// `height` rows, scrolling as little as possible, and never past the content end.
pub fn follow(scroll: usize, cursor_row: usize, height: usize, rows: usize) -> usize {
    if height == 0 {
        return cursor_row;
    }
    let mut scroll = scroll.min(rows.saturating_sub(height));
    if cursor_row < scroll {
        scroll = cursor_row;
    } else if cursor_row >= scroll + height {
        scroll = cursor_row + 1 - height;
    }
    scroll
}

/// Terminals and tmux usually send CR (sometimes CRLF) between pasted lines; edtui
/// only treats LF as a line break, so a raw CR would join everything into one line.
pub fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Inserts pasted text. In insert mode the text goes at the cursor, as if typed (edtui's
/// paste is vim's `p`, which puts it after the cursor and, at the end of a line, before
/// the last char). Other modes keep edtui's vim paste behavior.
pub fn paste(state: &mut EditorState, handler: &edtui::EditorEventHandler, text: &str) {
    let text = normalize_newlines(text);
    if state.mode == EditorMode::Insert {
        for ch in text.chars() {
            state.execute(edtui::actions::InsertChar(ch));
        }
    } else {
        handler.on_paste_event(text, state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_rows_wraps_greedily() {
        let chars = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(line_rows(&chars(""), 10), 1);
        assert_eq!(line_rows(&chars("abc"), 10), 1);
        assert_eq!(line_rows(&chars(&"a".repeat(10)), 10), 1);
        assert_eq!(line_rows(&chars(&"a".repeat(11)), 10), 2);
        assert_eq!(line_rows(&chars(&"a".repeat(25)), 10), 3);
        // A wide char that doesn't fit in the last column moves to the next row.
        assert_eq!(line_rows(&chars("aaa🙂"), 4), 2);
        assert_eq!(line_rows(&chars("\t\t"), 3), 2);
    }

    #[test]
    fn follow_scrolls_minimally() {
        // Cursor inside window: unchanged.
        assert_eq!(follow(2, 4, 5, 20), 2);
        // Below: bottom-align.
        assert_eq!(follow(0, 9, 5, 20), 5);
        // Above: top-align.
        assert_eq!(follow(5, 3, 5, 20), 3);
        // Content shrank: never scroll past the end.
        assert_eq!(follow(10, 2, 5, 6), 1);
        assert_eq!(follow(3, 0, 8, 2), 0);
    }

    #[test]
    fn normalize_newlines_handles_cr_and_crlf() {
        assert_eq!(normalize_newlines("a\rb\r\nc\nd"), "a\nb\nc\nd");
    }

    #[test]
    fn rendered_matches_edtui_wrapping() {
        let mut state = EditorState::new(edtui::Lines::from("aaaaaaaaaabbbbb\nc"));
        state.mode = EditorMode::Insert;
        let r = Rendered::new(&mut state, 10);
        assert_eq!(r.rows, 3);
        let row = |y: u16| (0..10).map(|x| r.buf[(x, y)].symbol()).collect::<String>();
        assert_eq!(row(0), "aaaaaaaaaa");
        assert_eq!(row(1).trim_end(), "bbbbb");
        assert_eq!(row(2).trim_end(), "c");
    }

    #[test]
    fn cursor_after_exactly_full_row_gets_its_own_row() {
        let mut state = EditorState::new(edtui::Lines::from("aaaaaaaaaa"));
        state.mode = EditorMode::Insert;
        state.cursor = edtui::Index2::new(0, 10);
        let r = Rendered::new(&mut state, 10);
        assert_eq!(r.cursor, Position::new(0, 1));
        assert_eq!(r.rows, 2);
        assert_eq!(r.height(), 2);
    }
}
