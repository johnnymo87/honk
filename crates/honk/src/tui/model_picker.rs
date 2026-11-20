use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};
use std::collections::HashSet;

use crate::chat::types::ModelItem;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerOutcome {
    Stay,
    Close,
    Select(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerRow {
    Header(String),
    Model(ModelItem),
    NoMatch,
}

pub struct ModelPicker {
    pub query: String,
    pub current: Option<String>,
    pub models: Vec<ModelItem>,
    pub recent: Vec<String>,
    pub rows: Vec<PickerRow>,
    pub selected: Option<usize>,
    pub scroll_offset: usize,
}

pub fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut haystack_chars = haystack.chars().flat_map(char::to_lowercase);
    for n in needle.chars().flat_map(char::to_lowercase) {
        loop {
            match haystack_chars.next() {
                Some(h) if h == n => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

impl ModelPicker {
    pub fn new(current: Option<String>, models: Vec<ModelItem>, recent: Vec<String>) -> Self {
        let mut picker = Self {
            query: String::new(),
            current,
            models,
            recent,
            rows: Vec::new(),
            selected: None,
            scroll_offset: 0,
        };
        picker.recompute_rows();
        picker
    }

    pub fn recompute_rows(&mut self) {
        if self.query.is_empty() {
            let mut rows = Vec::new();
            let mut shown_ids = HashSet::new();

            // 1. Recent section
            if !self.recent.is_empty() {
                let mut recent_items = Vec::new();
                for id in &self.recent {
                    if shown_ids.contains(id) {
                        continue;
                    }
                    if !self.models.is_empty() && !self.models.iter().any(|m| &m.id == id) {
                        continue;
                    }
                    shown_ids.insert(id.clone());
                    let item = self
                        .models
                        .iter()
                        .find(|m| &m.id == id)
                        .cloned()
                        .unwrap_or_else(|| ModelItem {
                            id: id.clone(),
                            name: id.clone(),
                            source: String::new(),
                        });
                    recent_items.push(item);
                }
                if !recent_items.is_empty() {
                    rows.push(PickerRow::Header("Recent".to_string()));
                    for item in recent_items {
                        rows.push(PickerRow::Model(item));
                    }
                }
            }

            // 2. Configured section (source == "configured", minus those shown in Recent)
            let configured_items: Vec<ModelItem> = self
                .models
                .iter()
                .filter(|m| m.source == "configured" && !shown_ids.contains(&m.id))
                .cloned()
                .collect();
            if !configured_items.is_empty() {
                rows.push(PickerRow::Header("Configured".to_string()));
                for item in configured_items {
                    shown_ids.insert(item.id.clone());
                    rows.push(PickerRow::Model(item));
                }
            }

            // 3. goose section (rest)
            let goose_items: Vec<ModelItem> = self
                .models
                .iter()
                .filter(|m| !shown_ids.contains(&m.id))
                .cloned()
                .collect();
            if !goose_items.is_empty() {
                rows.push(PickerRow::Header("goose".to_string()));
                for item in goose_items {
                    shown_ids.insert(item.id.clone());
                    rows.push(PickerRow::Model(item));
                }
            }

            self.rows = rows;
            self.selected = self
                .rows
                .iter()
                .position(|r| matches!(r, PickerRow::Model(_)));
        } else {
            // Flat list of all models: models in original order, plus any recent id not in models only if models is empty
            let mut all_models = self.models.clone();
            if self.models.is_empty() {
                let known_ids: HashSet<String> = all_models.iter().map(|m| m.id.clone()).collect();
                for id in &self.recent {
                    if !known_ids.contains(id) {
                        all_models.push(ModelItem {
                            id: id.clone(),
                            name: id.clone(),
                            source: String::new(),
                        });
                    }
                }
            }

            let filtered: Vec<ModelItem> = all_models
                .into_iter()
                .filter(|m| {
                    is_subsequence(&self.query, &m.id) || is_subsequence(&self.query, &m.name)
                })
                .collect();

            if filtered.is_empty() {
                self.rows = vec![PickerRow::NoMatch];
                self.selected = None;
            } else {
                self.rows = filtered.into_iter().map(PickerRow::Model).collect();
                self.selected = Some(0);
            }
        }
        self.scroll_offset = 0;
    }

    pub fn move_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let start = match self.selected {
            Some(s) => s + 1,
            None => 0,
        };
        for i in start..self.rows.len() {
            if matches!(self.rows[i], PickerRow::Model(_)) {
                self.selected = Some(i);
                return;
            }
        }
    }

    pub fn move_up(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let start = match self.selected {
            Some(s) if s > 0 => s - 1,
            _ => return,
        };
        for i in (0..=start).rev() {
            if matches!(self.rows[i], PickerRow::Model(_)) {
                self.selected = Some(i);
                return;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        if key.kind == KeyEventKind::Release {
            return PickerOutcome::Stay;
        }

        if key.code == KeyCode::Esc {
            return PickerOutcome::Close;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return PickerOutcome::Close;
        }

        if key.code == KeyCode::Down
            || (key.modifiers == KeyModifiers::CONTROL
                && (key.code == KeyCode::Char('n') || key.code == KeyCode::Char('j')))
        {
            self.move_down();
            return PickerOutcome::Stay;
        }

        if key.code == KeyCode::Up
            || (key.modifiers == KeyModifiers::CONTROL
                && (key.code == KeyCode::Char('p') || key.code == KeyCode::Char('k')))
        {
            self.move_up();
            return PickerOutcome::Stay;
        }

        if key.code == KeyCode::Enter {
            if let Some(idx) = self.selected
                && let Some(PickerRow::Model(m)) = self.rows.get(idx)
            {
                if self.current.as_deref() == Some(&m.id) {
                    return PickerOutcome::Close;
                } else {
                    return PickerOutcome::Select(m.id.clone());
                }
            }
            return PickerOutcome::Stay;
        }

        if key.code == KeyCode::Backspace {
            if !self.query.is_empty() {
                self.query.pop();
                self.recompute_rows();
            }
            return PickerOutcome::Stay;
        }

        if let KeyCode::Char(c) = key.code
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
        {
            self.query.push(c);
            self.recompute_rows();
            return PickerOutcome::Stay;
        }

        PickerOutcome::Stay
    }

    pub fn popup_area(&self, area: Rect) -> Rect {
        if area.width == 0 || area.height == 0 {
            return Rect::default();
        }
        let width = area.width.saturating_sub(4).clamp(30, 65).min(area.width);
        let desired_height = (self.rows.len() as u16 + 4).max(7);
        let height = desired_height.min(18).min(area.height);
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        Rect::new(x, y, width, height)
    }

    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        let popup = self.popup_area(area);
        if popup.width < 6 || popup.height < 4 {
            return None;
        }
        let inner_x = popup.x + 1;
        let inner_y = popup.y + 1;
        let cursor_x = inner_x + 3 + self.query.len() as u16;
        Some((cursor_x.min(popup.right().saturating_sub(2)), inner_y))
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let popup = self.popup_area(area);
        if popup.width < 6 || popup.height < 4 {
            return;
        }

        Clear.render(popup, buf);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Select model ")
            .title_bottom(Line::from(" enter switch · esc close "));
        let inner = block.inner(popup);
        block.render(popup, buf);

        if inner.width < 2 || inner.height < 2 {
            return;
        }

        // Row 0: query input
        let mut query_spans = vec![Span::styled(" > ", Style::default().fg(Color::Yellow))];
        if self.query.is_empty() {
            query_spans.push(Span::styled(
                "(type to filter)",
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            query_spans.push(Span::styled(
                &self.query,
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        let query_line = Line::from(query_spans);
        buf.set_line(inner.x, inner.y, &query_line, inner.width);

        // Row 1: separator
        let sep = "─".repeat(inner.width as usize);
        let sep_line = Line::from(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        buf.set_line(inner.x, inner.y + 1, &sep_line, inner.width);

        // Available slots for rows
        let row_slots = inner.height.saturating_sub(2) as usize;
        if row_slots == 0 || self.rows.is_empty() {
            return;
        }

        // Adjust scroll offset to keep selected row visible
        if let Some(sel) = self.selected {
            if sel < self.scroll_offset {
                self.scroll_offset = sel;
            } else if sel >= self.scroll_offset + row_slots {
                self.scroll_offset = sel + 1 - row_slots;
            }
        }
        if self.scroll_offset + row_slots > self.rows.len() {
            self.scroll_offset = self.rows.len().saturating_sub(row_slots);
        }

        for slot in 0..row_slots {
            let row_idx = self.scroll_offset + slot;
            if row_idx >= self.rows.len() {
                break;
            }
            let y = inner.y + 2 + slot as u16;
            let is_selected = self.selected == Some(row_idx);

            match &self.rows[row_idx] {
                PickerRow::Header(h) => {
                    let line = Line::from(Span::styled(
                        format!(" {h}"),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ));
                    buf.set_line(inner.x, y, &line, inner.width);
                }
                PickerRow::Model(m) => {
                    let is_current = self.current.as_deref() == Some(&m.id);
                    let marker = if is_current { "● " } else { "  " };

                    let mut spans = vec![
                        Span::styled(
                            marker,
                            if is_current {
                                Style::default().fg(Color::Green)
                            } else {
                                Style::default()
                            },
                        ),
                        Span::raw(&m.id),
                    ];
                    if m.name != m.id {
                        spans.push(Span::styled(
                            format!("  {}", m.name),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }

                    let mut line = Line::from(spans);
                    if is_selected {
                        line = line.style(
                            Style::default()
                                .bg(Color::DarkGray)
                                .add_modifier(Modifier::BOLD),
                        );
                    }
                    buf.set_line(inner.x, y, &line, inner.width);
                }
                PickerRow::NoMatch => {
                    let line = Line::from(Span::styled(
                        "  no match",
                        Style::default().fg(Color::DarkGray),
                    ));
                    buf.set_line(inner.x, y, &line, inner.width);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_models() -> Vec<ModelItem> {
        vec![
            ModelItem {
                id: "claude-opus-5-5".to_string(),
                name: "Claude Opus 5.5".to_string(),
                source: "configured".to_string(),
            },
            ModelItem {
                id: "claude-haiku-4-5".to_string(),
                name: "Claude Haiku 4.5".to_string(),
                source: "configured".to_string(),
            },
            ModelItem {
                id: "gemini-3.8-flash".to_string(),
                name: "gemini-3.8-flash".to_string(),
                source: "goose".to_string(),
            },
            ModelItem {
                id: "gemini-2.5-pro".to_string(),
                name: "Gemini Pro 2.5".to_string(),
                source: "goose".to_string(),
            },
        ]
    }

    #[test]
    fn test_subsequence_matching() {
        assert!(is_subsequence("c5", "claude-opus-5-5"));
        assert!(is_subsequence("C5", "claude-opus-5-5"));
        assert!(is_subsequence("opus", "Claude Opus 5.5"));
        assert!(!is_subsequence("c5", "gemini-3.8-flash"));
        assert!(is_subsequence("", "anything"));
        assert!(is_subsequence("flash", "gemini-3.8-flash"));
    }

    #[test]
    fn test_sectioning_empty_query() {
        let models = sample_models();
        let recent = vec![
            "gemini-3.8-flash".to_string(),
            "claude-opus-5-5".to_string(),
            "old-custom-model".to_string(),
        ];
        let picker = ModelPicker::new(Some("gemini-3.8-flash".to_string()), models, recent);

        assert_eq!(picker.rows.len(), 7);
        assert_eq!(picker.rows[0], PickerRow::Header("Recent".to_string()));
        assert_eq!(
            picker.rows[1],
            PickerRow::Model(ModelItem {
                id: "gemini-3.8-flash".to_string(),
                name: "gemini-3.8-flash".to_string(),
                source: "goose".to_string(),
            })
        );
        assert_eq!(
            picker.rows[2],
            PickerRow::Model(ModelItem {
                id: "claude-opus-5-5".to_string(),
                name: "Claude Opus 5.5".to_string(),
                source: "configured".to_string(),
            })
        );
        // "old-custom-model" is not in sample_models, so it is omitted when models list is non-empty
        assert_eq!(picker.rows[3], PickerRow::Header("Configured".to_string()));
        assert_eq!(
            picker.rows[4],
            PickerRow::Model(ModelItem {
                id: "claude-haiku-4-5".to_string(),
                name: "Claude Haiku 4.5".to_string(),
                source: "configured".to_string(),
            })
        );
        assert_eq!(picker.rows[5], PickerRow::Header("goose".to_string()));
        assert_eq!(
            picker.rows[6],
            PickerRow::Model(ModelItem {
                id: "gemini-2.5-pro".to_string(),
                name: "Gemini Pro 2.5".to_string(),
                source: "goose".to_string(),
            })
        );
    }

    #[test]
    fn test_sectioning_empty_models_shows_recents() {
        let picker = ModelPicker::new(
            None,
            vec![],
            vec!["recent-a".to_string(), "recent-b".to_string()],
        );
        assert_eq!(picker.rows.len(), 3);
        assert_eq!(picker.rows[0], PickerRow::Header("Recent".to_string()));
        assert_eq!(
            picker.rows[1],
            PickerRow::Model(ModelItem {
                id: "recent-a".to_string(),
                name: "recent-a".to_string(),
                source: String::new(),
            })
        );
        assert_eq!(
            picker.rows[2],
            PickerRow::Model(ModelItem {
                id: "recent-b".to_string(),
                name: "recent-b".to_string(),
                source: String::new(),
            })
        );
    }

    #[test]
    fn test_sectioning_omits_empty_headers() {
        let models = vec![ModelItem {
            id: "m1".to_string(),
            name: "m1".to_string(),
            source: "goose".to_string(),
        }];
        let picker = ModelPicker::new(None, models, vec![]);
        assert_eq!(picker.rows.len(), 2);
        assert_eq!(picker.rows[0], PickerRow::Header("goose".to_string()));
        assert_eq!(
            picker.rows[1],
            PickerRow::Model(ModelItem {
                id: "m1".to_string(),
                name: "m1".to_string(),
                source: "goose".to_string(),
            })
        );
    }

    #[test]
    fn test_selection_skips_headers() {
        let models = sample_models();
        let recent = vec!["gemini-3.8-flash".to_string()];
        let mut picker = ModelPicker::new(None, models, recent);

        assert_eq!(picker.selected, Some(1));

        picker.move_down();
        assert_eq!(picker.selected, Some(3));

        picker.move_down();
        assert_eq!(picker.selected, Some(4));

        picker.move_down();
        assert_eq!(picker.selected, Some(6));

        // Clamped at bottom
        picker.move_down();
        assert_eq!(picker.selected, Some(6));

        picker.move_up();
        assert_eq!(picker.selected, Some(4));

        picker.move_up();
        assert_eq!(picker.selected, Some(3));

        picker.move_up();
        assert_eq!(picker.selected, Some(1));

        // Clamped at top
        picker.move_up();
        assert_eq!(picker.selected, Some(1));
    }

    #[test]
    fn test_filtering_hides_headers_and_dedup() {
        let models = sample_models();
        let recent = vec!["gemini-3.8-flash".to_string()];
        let mut picker = ModelPicker::new(None, models, recent);

        let outcome = picker.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(outcome, PickerOutcome::Stay);
        let outcome = picker.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(outcome, PickerOutcome::Stay);
        let outcome = picker.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert_eq!(outcome, PickerOutcome::Stay);

        assert_eq!(picker.query, "gem");
        assert_eq!(picker.rows.len(), 2);
        assert_eq!(
            picker.rows[0],
            PickerRow::Model(ModelItem {
                id: "gemini-3.8-flash".to_string(),
                name: "gemini-3.8-flash".to_string(),
                source: "goose".to_string(),
            })
        );
        assert_eq!(
            picker.rows[1],
            PickerRow::Model(ModelItem {
                id: "gemini-2.5-pro".to_string(),
                name: "Gemini Pro 2.5".to_string(),
                source: "goose".to_string(),
            })
        );
        assert_eq!(picker.selected, Some(0));

        // Filter with no match
        picker.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        picker.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert_eq!(picker.rows, vec![PickerRow::NoMatch]);
        assert_eq!(picker.selected, None);

        // Backspace recovers
        picker.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        picker.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(picker.rows.len(), 2);
        assert_eq!(picker.selected, Some(0));
    }

    #[test]
    fn test_enter_and_esc_results() {
        let models = sample_models();
        let recent = vec![
            "claude-opus-5-5".to_string(),
            "gemini-3.8-flash".to_string(),
        ];
        let mut picker = ModelPicker::new(Some("claude-opus-5-5".to_string()), models, recent);

        // Row 1 is claude-opus-5-5 (current). Enter should Close without sending command.
        assert_eq!(picker.selected, Some(1));
        let outcome = picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(outcome, PickerOutcome::Close);

        // Recreate and select gemini-3.8-flash (not current)
        let mut picker = ModelPicker::new(
            Some("claude-opus-5-5".to_string()),
            sample_models(),
            vec![
                "claude-opus-5-5".to_string(),
                "gemini-3.8-flash".to_string(),
            ],
        );
        picker.move_down();
        assert_eq!(picker.selected, Some(2));
        let outcome = picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            outcome,
            PickerOutcome::Select("gemini-3.8-flash".to_string())
        );

        // Esc closes
        let outcome = picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(outcome, PickerOutcome::Close);

        // Ctrl-C closes
        let outcome = picker.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(outcome, PickerOutcome::Close);
    }

    #[test]
    fn test_picker_render_and_scroll() {
        let models: Vec<ModelItem> = (0..20)
            .map(|i| ModelItem {
                id: format!("model-{i}"),
                name: format!("Model {i}"),
                source: "goose".to_string(),
            })
            .collect();
        let mut picker = ModelPicker::new(None, models, vec![]);

        // Buffer 80x24
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
        picker.render(Rect::new(0, 0, 80, 24), &mut buf);
        assert_eq!(picker.scroll_offset, 0);

        // Move down past visible height
        for _ in 0..15 {
            picker.move_down();
        }
        picker.render(Rect::new(0, 0, 80, 24), &mut buf);
        // Selection must be >= scroll_offset and < scroll_offset + row_slots
        let sel = picker.selected.unwrap();
        assert!(sel >= picker.scroll_offset);
        assert!(picker.scroll_offset > 0);
    }

    #[test]
    fn test_picker_tiny_buffer_no_panic() {
        let models = sample_models();
        let mut picker = ModelPicker::new(None, models, vec![]);

        // 20x5 buffer (narrow width < 30): bounded within buffer, renders without panic
        let area20x5 = Rect::new(0, 0, 20, 5);
        let popup20x5 = picker.popup_area(area20x5);
        assert!(popup20x5.right() <= area20x5.right());
        assert!(popup20x5.bottom() <= area20x5.bottom());
        let mut buf20x5 = Buffer::empty(area20x5);
        picker.render(area20x5, &mut buf20x5);
        let cursor20x5 = picker.cursor_position(area20x5);
        if let Some((cx, cy)) = cursor20x5 {
            assert!(cx < area20x5.right());
            assert!(cy < area20x5.bottom());
        }

        // 8x3 buffer: too small, bails cleanly without panic
        let area8x3 = Rect::new(0, 0, 8, 3);
        let popup8x3 = picker.popup_area(area8x3);
        assert!(popup8x3.right() <= area8x3.right());
        assert!(popup8x3.bottom() <= area8x3.bottom());
        let mut buf8x3 = Buffer::empty(area8x3);
        picker.render(area8x3, &mut buf8x3);
        assert_eq!(picker.cursor_position(area8x3), None);

        // 0x0 buffer: bails cleanly
        let area0 = Rect::new(0, 0, 0, 0);
        let mut buf0 = Buffer::empty(area0);
        picker.render(area0, &mut buf0);
        assert_eq!(picker.cursor_position(area0), None);
    }
}
