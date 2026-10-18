use crate::chat::driver::{ConnectionState, StateSnapshot};
use crate::chat::types::Ownership;
use edtui::EditorMode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

pub fn shorten_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        if path == home {
            return "~".to_string();
        } else if let Some(stripped) = path.strip_prefix(&home)
            && stripped.starts_with('/')
        {
            return format!("~{stripped}");
        }
    }
    path.to_string()
}

pub struct StatusBar<'a> {
    pub session_id: &'a str,
    pub session_title: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub state_snapshot: Option<&'a StateSnapshot>,
    pub mode: EditorMode,
    pub usage: Option<&'a honk_wire::Usage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageLevel {
    Full,
    NoCost,
    TokensOnly,
}

pub fn usage_text(u: &honk_wire::Usage, level: UsageLevel) -> Option<String> {
    if u.used == 0 {
        return None;
    }

    let tokens = if u.used < 1000 {
        format!("{}", u.used)
    } else if u.used < 1_000_000 {
        format!("{:.1}K", u.used as f64 / 1_000.0)
    } else {
        format!("{:.1}M", u.used as f64 / 1_000_000.0)
    };

    let token_part = if level != UsageLevel::TokensOnly && u.size > 0 {
        let pct = ((u.used as f64 / u.size as f64) * 100.0).round() as u64;
        format!("{tokens} ({pct}%)")
    } else {
        tokens
    };

    let cost_str = if level == UsageLevel::Full {
        u.cost.as_ref().and_then(|c| {
            if c.amount <= 0.0 {
                None
            } else if c.currency == "USD" {
                Some(format!("${:.2}", c.amount))
            } else {
                Some(format!("{:.2} {}", c.amount, c.currency))
            }
        })
    } else {
        None
    };

    match cost_str {
        Some(cost) => Some(format!("{token_part} · {cost}")),
        None => Some(token_part),
    }
}

impl<'a> StatusBar<'a> {
    pub fn title_text(&self) -> &str {
        self.session_title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(self.session_id)
    }

    pub fn cwd_text(&self) -> &str {
        self.cwd.unwrap_or("")
    }

    pub fn title_and_cwd(&self, max_width: usize) -> String {
        let title = self.title_text();
        let cwd_raw = self.cwd_text();
        let full = if !cwd_raw.is_empty() {
            let shortened = shorten_home(cwd_raw);
            format!("{title} — {shortened}")
        } else {
            title.to_string()
        };

        if full.len() > max_width && max_width > 3 {
            format!("{}…", &full[..max_width - 1])
        } else {
            full
        }
    }

    pub fn run_state_text(&self) -> &'static str {
        match self.state_snapshot {
            Some(s) => {
                if s.ownership == Ownership::Pending {
                    "waiting"
                } else if s.run_state.is_running() {
                    if s.ownership == Ownership::Foreign {
                        "running (other client)"
                    } else {
                        "running"
                    }
                } else {
                    "idle"
                }
            }
            None => "idle",
        }
    }

    pub fn queued_text(&self) -> Option<String> {
        self.state_snapshot.and_then(|s| {
            if s.queued_len > 0 {
                Some(format!("queued: {}", s.queued_len))
            } else {
                None
            }
        })
    }

    pub fn connection_text(&self) -> String {
        match self.state_snapshot.map(|s| &s.connection) {
            Some(ConnectionState::Connecting) => "live".to_string(),
            Some(ConnectionState::Live) => "live".to_string(),
            Some(ConnectionState::Reconnecting { attempt }) => format!("reconnecting ({attempt})"),
            Some(ConnectionState::Stopped { reason }) => {
                format!("stopped: {reason} — press any key to retry")
            }
            Some(ConnectionState::Detached) => "detached".to_string(),
            None => "live".to_string(),
        }
    }

    pub fn mode_text(&self) -> &'static str {
        match self.mode {
            EditorMode::Insert => "INSERT",
            EditorMode::Normal => "NORMAL",
            EditorMode::Visual => "VISUAL",
            _ => "NORMAL",
        }
    }

    pub fn to_line(&self, width: u16) -> Line<'static> {
        let base_style = Style::default().bg(Color::DarkGray).fg(Color::White);

        let title_style = Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);

        let build_left_spans = |max_title_w: usize| -> Vec<Span<'static>> {
            let mut spans = Vec::new();

            // Mode badge
            let mode_style = Style::default()
                .fg(Color::Black)
                .bg(match self.mode {
                    EditorMode::Insert => Color::Green,
                    EditorMode::Normal => Color::Blue,
                    EditorMode::Visual => Color::Magenta,
                    _ => Color::White,
                })
                .add_modifier(Modifier::BOLD);
            spans.push(Span::styled(format!(" {} ", self.mode_text()), mode_style));

            spans.push(Span::styled(" ", base_style));

            // Title — CWD
            let display_title = self.title_and_cwd(max_title_w);
            spans.push(Span::styled(display_title, title_style));

            // Run state
            let run_state = self.run_state_text();
            let run_style = Style::default().bg(Color::DarkGray).fg(match run_state {
                "running" => Color::Green,
                "running (other client)" => Color::Yellow,
                "waiting" => Color::Cyan,
                _ => Color::DarkGray,
            });
            spans.push(Span::styled(format!("  [{}]", run_state), run_style));

            // Queued
            if let Some(queued) = self.queued_text() {
                spans.push(Span::styled(
                    format!("  [{}]", queued),
                    Style::default().bg(Color::DarkGray).fg(Color::LightYellow),
                ));
            }

            // Connection
            let conn = self.connection_text();
            let conn_style = Style::default().bg(Color::DarkGray).fg(if conn == "live" {
                Color::Green
            } else if conn.starts_with("reconnecting") {
                Color::Yellow
            } else if conn.starts_with("stopped") {
                Color::Red
            } else {
                Color::Gray
            });
            spans.push(Span::styled(format!("  {}", conn), conn_style));

            spans
        };

        let mut chosen = None;
        if let Some(u) = self.usage {
            let badge_w = unicode_width::UnicodeWidthStr::width(self.mode_text()) + 2;
            let spacer_w = 1;
            let run_state_w = unicode_width::UnicodeWidthStr::width(self.run_state_text()) + 4;
            let queued_w = self
                .queued_text()
                .map(|q| unicode_width::UnicodeWidthStr::width(q.as_str()) + 4)
                .unwrap_or(0);
            let conn_text = self.connection_text();
            let conn_w = unicode_width::UnicodeWidthStr::width(conn_text.as_str()) + 2;
            let non_title_left = badge_w + spacer_w + run_state_w + queued_w + conn_w;

            let levels = [UsageLevel::Full, UsageLevel::NoCost, UsageLevel::TokensOnly];
            let mut candidates = Vec::new();
            for lvl in levels {
                if let Some(text) = usage_text(u, lvl)
                    && !candidates.contains(&text)
                {
                    candidates.push(text);
                }
            }

            for cand in candidates {
                let cand_w = unicode_width::UnicodeWidthStr::width(cand.as_str());
                let max_title_w = (width as usize)
                    .saturating_sub(non_title_left + 2 + cand_w)
                    .max(10);
                let left_spans = build_left_spans(max_title_w);
                let left_w: usize = left_spans
                    .iter()
                    .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                if left_w + 2 + cand_w <= width as usize {
                    let padding = (width as usize) - left_w - cand_w;
                    chosen = Some((left_spans, padding, cand));
                    break;
                }
            }
        }

        let spans = if let Some((mut left_spans, padding, cand)) = chosen {
            left_spans.push(Span::styled(" ".repeat(padding), base_style));
            let usage_style = Style::default().bg(Color::DarkGray).fg(Color::Gray);
            left_spans.push(Span::styled(cand, usage_style));
            left_spans
        } else {
            let max_title_w = (width as usize).saturating_sub(45).max(10);
            build_left_spans(max_title_w)
        };

        Line::from(spans).style(base_style)
    }
}

impl<'a> Widget for StatusBar<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let line = self.to_line(area.width);
        buf.set_style(area, Style::default().bg(Color::DarkGray));
        buf.set_line(area.x, area.y, &line, area.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::types::RunState;

    fn make_snapshot(
        run_state: RunState,
        ownership: Ownership,
        queued_len: usize,
        connection: ConnectionState,
    ) -> StateSnapshot {
        StateSnapshot {
            run_state,
            ownership,
            queued_len,
            replaying: false,
            prompt_in_flight: false,
            connection,
        }
    }

    #[test]
    fn test_title_and_cwd() {
        let bar = StatusBar {
            session_id: "ses_123",
            session_title: Some("My Special Session"),
            cwd: Some("/tmp/test-project"),
            state_snapshot: None,
            mode: EditorMode::Normal,
            usage: None,
        };
        assert_eq!(bar.title_text(), "My Special Session");
        assert_eq!(bar.cwd_text(), "/tmp/test-project");
        assert_eq!(
            bar.title_and_cwd(80),
            "My Special Session — /tmp/test-project"
        );

        let bar_fallback = StatusBar {
            session_id: "ses_123",
            session_title: None,
            cwd: None,
            state_snapshot: None,
            mode: EditorMode::Normal,
            usage: None,
        };
        assert_eq!(bar_fallback.title_text(), "ses_123");
        assert_eq!(bar_fallback.cwd_text(), "");
        assert_eq!(bar_fallback.title_and_cwd(80), "ses_123");
    }

    #[test]
    fn test_shorten_home() {
        if let Ok(home) = std::env::var("HOME") {
            let path = format!("{home}/projects/honk");
            assert_eq!(shorten_home(&path), "~/projects/honk");
            assert_eq!(shorten_home(&home), "~");
        }
        assert_eq!(shorten_home("/var/log"), "/var/log");
    }

    #[test]
    fn test_status_bar_renders_at_width_140() {
        let bar = StatusBar {
            session_id: "20261013_133",
            session_title: Some("Shell echo hello test"),
            cwd: Some("/tmp/opencode/chatcwd"),
            state_snapshot: None,
            mode: EditorMode::Normal,
            usage: None,
        };
        let line = bar.to_line(140);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("Shell echo hello test — /tmp/opencode/chatcwd"));
        assert!(rendered.contains("NORMAL"));
        assert!(rendered.contains("idle"));
        assert!(rendered.contains("live"));
    }

    #[test]
    fn test_run_states_and_ownership() {
        // Pending -> "waiting"
        let snap_pending = make_snapshot(
            RunState::Running("run_1".into()),
            Ownership::Pending,
            0,
            ConnectionState::Live,
        );
        let bar = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: Some(&snap_pending),
            mode: EditorMode::Insert,
            usage: None,
        };
        assert_eq!(bar.run_state_text(), "waiting");

        // Own running -> "running"
        let snap_own = make_snapshot(
            RunState::Running("run_1".into()),
            Ownership::Own,
            0,
            ConnectionState::Live,
        );
        let bar = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: Some(&snap_own),
            mode: EditorMode::Insert,
            usage: None,
        };
        assert_eq!(bar.run_state_text(), "running");

        // Foreign running -> "running (other client)"
        let snap_foreign = make_snapshot(
            RunState::Running("run_1".into()),
            Ownership::Foreign,
            0,
            ConnectionState::Live,
        );
        let bar = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: Some(&snap_foreign),
            mode: EditorMode::Insert,
            usage: None,
        };
        assert_eq!(bar.run_state_text(), "running (other client)");

        // Idle -> "idle"
        let snap_idle = make_snapshot(RunState::Idle, Ownership::Unknown, 0, ConnectionState::Live);
        let bar = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: Some(&snap_idle),
            mode: EditorMode::Normal,
            usage: None,
        };
        assert_eq!(bar.run_state_text(), "idle");
    }

    #[test]
    fn test_queued_counts() {
        let snap_queued =
            make_snapshot(RunState::Idle, Ownership::Unknown, 3, ConnectionState::Live);
        let bar = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: Some(&snap_queued),
            mode: EditorMode::Normal,
            usage: None,
        };
        assert_eq!(bar.queued_text(), Some("queued: 3".into()));

        let snap_zero = make_snapshot(RunState::Idle, Ownership::Unknown, 0, ConnectionState::Live);
        let bar = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: Some(&snap_zero),
            mode: EditorMode::Normal,
            usage: None,
        };
        assert_eq!(bar.queued_text(), None);
    }

    #[test]
    fn test_connection_states() {
        let snap_reconnecting = make_snapshot(
            RunState::Idle,
            Ownership::Unknown,
            0,
            ConnectionState::Reconnecting { attempt: 2 },
        );
        let bar = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: Some(&snap_reconnecting),
            mode: EditorMode::Normal,
            usage: None,
        };
        assert_eq!(bar.connection_text(), "reconnecting (2)");

        let snap_stopped = make_snapshot(
            RunState::Idle,
            Ownership::Unknown,
            0,
            ConnectionState::Stopped {
                reason: "connection reset".into(),
            },
        );
        let bar = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: Some(&snap_stopped),
            mode: EditorMode::Normal,
            usage: None,
        };
        assert_eq!(
            bar.connection_text(),
            "stopped: connection reset — press any key to retry"
        );

        let snap_detached = make_snapshot(
            RunState::Idle,
            Ownership::Unknown,
            0,
            ConnectionState::Detached,
        );
        let bar = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: Some(&snap_detached),
            mode: EditorMode::Normal,
            usage: None,
        };
        assert_eq!(bar.connection_text(), "detached");
    }

    #[test]
    fn test_mode_indicator() {
        let bar_insert = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: None,
            mode: EditorMode::Insert,
            usage: None,
        };
        assert_eq!(bar_insert.mode_text(), "INSERT");

        let bar_normal = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: None,
            mode: EditorMode::Normal,
            usage: None,
        };
        assert_eq!(bar_normal.mode_text(), "NORMAL");

        let bar_visual = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: None,
            mode: EditorMode::Visual,
            usage: None,
        };
        assert_eq!(bar_visual.mode_text(), "VISUAL");
    }

    #[test]
    fn test_usage_formatter() {
        use honk_wire::{Cost, Usage};

        // 812 -> "812"
        let u_812 = Usage {
            used: 812,
            size: 0,
            cost: None,
        };
        assert_eq!(
            usage_text(&u_812, UsageLevel::Full),
            Some("812".to_string())
        );

        // 1000/0 size -> "1.0K"
        let u_1000 = Usage {
            used: 1000,
            size: 0,
            cost: None,
        };
        assert_eq!(
            usage_text(&u_1000, UsageLevel::Full),
            Some("1.0K".to_string())
        );

        // {45231, 200000, $1.84} -> "45.2K (23%) · $1.84"
        let u_full = Usage {
            used: 45231,
            size: 200000,
            cost: Some(Cost {
                amount: 1.84,
                currency: "USD".to_string(),
            }),
        };
        assert_eq!(
            usage_text(&u_full, UsageLevel::Full),
            Some("45.2K (23%) · $1.84".to_string())
        );
        // Level degradation:
        assert_eq!(
            usage_text(&u_full, UsageLevel::NoCost),
            Some("45.2K (23%)".to_string())
        );
        assert_eq!(
            usage_text(&u_full, UsageLevel::TokensOnly),
            Some("45.2K".to_string())
        );

        // size 0 -> no pct
        let u_no_pct = Usage {
            used: 45231,
            size: 0,
            cost: Some(Cost {
                amount: 1.84,
                currency: "USD".to_string(),
            }),
        };
        assert_eq!(
            usage_text(&u_no_pct, UsageLevel::Full),
            Some("45.2K · $1.84".to_string())
        );

        // pct > 100 (used 210000/size 200000 -> "(105%)")
        let u_over_100 = Usage {
            used: 210000,
            size: 200000,
            cost: None,
        };
        assert_eq!(
            usage_text(&u_over_100, UsageLevel::Full),
            Some("210.0K (105%)".to_string())
        );

        // 1_500_000 -> "1.5M"
        let u_mega = Usage {
            used: 1_500_000,
            size: 0,
            cost: None,
        };
        assert_eq!(
            usage_text(&u_mega, UsageLevel::Full),
            Some("1.5M".to_string())
        );

        // EUR
        let u_eur = Usage {
            used: 45231,
            size: 200000,
            cost: Some(Cost {
                amount: 1.84,
                currency: "EUR".to_string(),
            }),
        };
        assert_eq!(
            usage_text(&u_eur, UsageLevel::Full),
            Some("45.2K (23%) · 1.84 EUR".to_string())
        );

        // cost 0 omitted
        let u_zero_cost = Usage {
            used: 45231,
            size: 200000,
            cost: Some(Cost {
                amount: 0.0,
                currency: "USD".to_string(),
            }),
        };
        assert_eq!(
            usage_text(&u_zero_cost, UsageLevel::Full),
            Some("45.2K (23%)".to_string())
        );

        // cost negative omitted
        let u_neg_cost = Usage {
            used: 45231,
            size: 200000,
            cost: Some(Cost {
                amount: -0.5,
                currency: "USD".to_string(),
            }),
        };
        assert_eq!(
            usage_text(&u_neg_cost, UsageLevel::Full),
            Some("45.2K (23%)".to_string())
        );

        // used 0 -> hidden
        let u_zero_used = Usage {
            used: 0,
            size: 200000,
            cost: Some(Cost {
                amount: 1.84,
                currency: "USD".to_string(),
            }),
        };
        assert_eq!(usage_text(&u_zero_used, UsageLevel::Full), None);
    }

    #[test]
    fn test_status_line_with_usage() {
        use honk_wire::{Cost, Usage};

        let u_full = Usage {
            used: 45231,
            size: 200000,
            cost: Some(Cost {
                amount: 1.84,
                currency: "USD".to_string(),
            }),
        };

        // at width 140 the line ends with the segment flush right (line width == 140)
        let bar_140 = StatusBar {
            session_id: "ses_1",
            session_title: Some("Title"),
            cwd: Some("/tmp/cwd"),
            state_snapshot: None,
            mode: EditorMode::Normal,
            usage: Some(&u_full),
        };
        let line_140 = bar_140.to_line(140);
        let rendered_140: String = line_140.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(line_140.width(), 140);
        assert!(rendered_140.ends_with("45.2K (23%) · $1.84"));

        // at a narrow width the cost is dropped first, then pct, then the segment
        let line_50 = bar_140.to_line(50);
        let rendered_50: String = line_50.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(line_50.width(), 50);
        assert!(rendered_50.ends_with("45.2K (23%)"));
        assert!(!rendered_50.contains("$1.84"));

        let line_43 = bar_140.to_line(43);
        let rendered_43: String = line_43.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(line_43.width(), 43);
        assert!(rendered_43.ends_with("45.2K"));
        assert!(!rendered_43.contains("(23%)"));

        let line_35 = bar_140.to_line(35);
        let rendered_35: String = line_35.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!rendered_35.contains("45.2K"));

        // No usage and used == 0 produce identical line output
        let u_zero_used = Usage {
            used: 0,
            size: 200000,
            cost: Some(Cost {
                amount: 1.84,
                currency: "USD".to_string(),
            }),
        };
        let bar_none = StatusBar {
            session_id: "ses_1",
            session_title: Some("Title"),
            cwd: Some("/tmp/cwd"),
            state_snapshot: None,
            mode: EditorMode::Normal,
            usage: None,
        };
        let bar_zero = StatusBar {
            session_id: "ses_1",
            session_title: Some("Title"),
            cwd: Some("/tmp/cwd"),
            state_snapshot: None,
            mode: EditorMode::Normal,
            usage: Some(&u_zero_used),
        };
        assert_eq!(bar_none.to_line(140), bar_zero.to_line(140));
    }

    #[test]
    fn test_status_line_title_budget_width_80() {
        use honk_wire::{Cost, Usage};

        let u_full = Usage {
            used: 45231,
            size: 200000,
            cost: Some(Cost {
                amount: 1.84,
                currency: "USD".to_string(),
            }),
        };

        let title_30 = "A very long session title 1234";
        assert_eq!(title_30.len(), 30);

        let bar = StatusBar {
            session_id: "ses_1",
            session_title: Some(title_30),
            cwd: None,
            state_snapshot: None,
            mode: EditorMode::Normal,
            usage: Some(&u_full),
        };

        let line = bar.to_line(80);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(line.width(), 80);
        assert!(rendered.ends_with("45.2K (23%) · $1.84"));
        assert!(
            rendered.contains(title_30),
            "title of length 30 must not be truncated, got: {rendered}"
        );
        assert!(!rendered.contains('…'), "must not contain ellipsis");
    }
}
