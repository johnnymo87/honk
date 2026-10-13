use crate::chat::driver::{ConnectionState, StateSnapshot};
use crate::chat::types::Ownership;
use edtui::EditorMode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

pub struct StatusBar<'a> {
    pub session_id: &'a str,
    pub session_title: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub state_snapshot: Option<&'a StateSnapshot>,
    pub mode: EditorMode,
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

    pub fn to_line(&self, _width: u16) -> Line<'static> {
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

        let base_style = Style::default().bg(Color::DarkGray).fg(Color::White);
        spans.push(Span::styled(" ", base_style));

        // Title
        let title_style = Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        spans.push(Span::styled(self.title_text().to_string(), title_style));

        // CWD
        let cwd = self.cwd_text();
        if !cwd.is_empty() {
            spans.push(Span::styled(
                format!("  {}", cwd),
                Style::default().bg(Color::DarkGray).fg(Color::Gray),
            ));
        }

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
        };
        assert_eq!(bar.title_text(), "My Special Session");
        assert_eq!(bar.cwd_text(), "/tmp/test-project");

        let bar_fallback = StatusBar {
            session_id: "ses_123",
            session_title: None,
            cwd: None,
            state_snapshot: None,
            mode: EditorMode::Normal,
        };
        assert_eq!(bar_fallback.title_text(), "ses_123");
        assert_eq!(bar_fallback.cwd_text(), "");
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
        };
        assert_eq!(bar.queued_text(), Some("queued: 3".into()));

        let snap_zero = make_snapshot(RunState::Idle, Ownership::Unknown, 0, ConnectionState::Live);
        let bar = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: Some(&snap_zero),
            mode: EditorMode::Normal,
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
        };
        assert_eq!(bar_insert.mode_text(), "INSERT");

        let bar_normal = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: None,
            mode: EditorMode::Normal,
        };
        assert_eq!(bar_normal.mode_text(), "NORMAL");

        let bar_visual = StatusBar {
            session_id: "ses",
            session_title: None,
            cwd: None,
            state_snapshot: None,
            mode: EditorMode::Visual,
        };
        assert_eq!(bar_visual.mode_text(), "VISUAL");
    }
}
