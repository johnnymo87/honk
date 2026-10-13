use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use edtui::EditorMode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Edtui(KeyEvent),
    EscKey(KeyEvent),
    Submit,
    Newline,
    ScrollLines(i32),
    ScrollHalfPage(i32),
    ScrollPage(i32),
    ScrollTop,
    ScrollBottom,
    ToggleTools,
    ToggleThoughts,
    OpenCommand,
    HistoryPrev,
    HistoryNext,
    Interrupt,
    LeaveInsertMode,
    Quit,
    PendingPrefix(char),
    Ignore,
}

use edtui::events::{KeyEventRegister, KeyInput};

/// Configure edtui's KeyEventHandler by removing keys that honk handles globally.
pub fn configure_key_handler(handler: &mut edtui::events::KeyEventHandler) {
    handler.remove(&KeyEventRegister::n(vec![KeyInput::ctrl('e')]));
    handler.remove(&KeyEventRegister::n(vec![KeyInput::ctrl('y')]));
    handler.remove(&KeyEventRegister::n(vec![KeyInput::ctrl('d')]));
    handler.remove(&KeyEventRegister::n(vec![KeyInput::ctrl('u')]));
    handler.remove(&KeyEventRegister::n(vec![KeyInput::ctrl('f')]));
    handler.remove(&KeyEventRegister::n(vec![KeyInput::ctrl('b')]));
    handler.remove(&KeyEventRegister::n(vec![KeyInput::ctrl('p')]));
    handler.remove(&KeyEventRegister::n(vec![KeyInput::ctrl('n')]));
    handler.remove(&KeyEventRegister::i(vec![KeyInput::ctrl('p')]));
    handler.remove(&KeyEventRegister::i(vec![KeyInput::ctrl('n')]));
    handler.remove(&KeyEventRegister::i(vec![KeyInput::ctrl('d')]));
    handler.remove(&KeyEventRegister::n(vec![KeyInput::new(':')]));
}

/// Helper resolving with default enhancement setting (false).
pub fn resolve_default(
    mode: EditorMode,
    composer_empty: bool,
    key_event: KeyEvent,
    pending_prefix: Option<char>,
) -> Action {
    resolve(mode, composer_empty, key_event, pending_prefix, false)
}

/// Resolves a key event into an Action.
pub fn resolve(
    mode: EditorMode,
    composer_empty: bool,
    key_event: KeyEvent,
    pending_prefix: Option<char>,
    enhanced_keyboard: bool,
) -> Action {
    // Release events are ignored
    if key_event.kind == KeyEventKind::Release {
        return Action::Ignore;
    }

    // Pending prefix resolution (Normal mode only)
    if mode == EditorMode::Normal
        && let Some(prefix) = pending_prefix
    {
        match prefix {
            'z' => {
                if key_event.modifiers == KeyModifiers::NONE {
                    match key_event.code {
                        KeyCode::Char('a') => return Action::ToggleTools,
                        KeyCode::Char('t') => return Action::ToggleThoughts,
                        _ => return Action::Edtui(key_event),
                    }
                }
                return Action::Edtui(key_event);
            }
            'g' => {
                if key_event.modifiers == KeyModifiers::NONE && key_event.code == KeyCode::Char('g')
                {
                    return Action::ScrollTop;
                }
                return Action::Edtui(key_event);
            }
            _ => return Action::Edtui(key_event),
        }
    }

    match mode {
        EditorMode::Normal => {
            // Control keys
            if key_event.modifiers == KeyModifiers::CONTROL {
                match key_event.code {
                    KeyCode::Char('e') => return Action::ScrollLines(1),
                    KeyCode::Char('y') => return Action::ScrollLines(-1),
                    KeyCode::Char('d') => return Action::ScrollHalfPage(1),
                    KeyCode::Char('u') => return Action::ScrollHalfPage(-1),
                    KeyCode::Char('f') => return Action::ScrollPage(1),
                    KeyCode::Char('b') => return Action::ScrollPage(-1),
                    KeyCode::Char('p') => return Action::HistoryPrev,
                    KeyCode::Char('n') => return Action::HistoryNext,
                    KeyCode::Char('c') => return Action::Interrupt,
                    _ => {}
                }
            }

            // Unmodified keys
            if key_event.modifiers == KeyModifiers::NONE {
                match key_event.code {
                    KeyCode::Up => {
                        if composer_empty {
                            return Action::ScrollLines(-3);
                        } else {
                            return Action::Edtui(key_event);
                        }
                    }
                    KeyCode::Down => {
                        if composer_empty {
                            return Action::ScrollLines(3);
                        } else {
                            return Action::Edtui(key_event);
                        }
                    }
                    KeyCode::Char(':') => return Action::OpenCommand,
                    KeyCode::Char('z') => return Action::PendingPrefix('z'),
                    KeyCode::Char('g') => {
                        if composer_empty {
                            return Action::PendingPrefix('g');
                        } else {
                            return Action::Edtui(key_event);
                        }
                    }
                    _ => {}
                }
            }

            // Shift-G
            if key_event.modifiers == KeyModifiers::SHIFT && key_event.code == KeyCode::Char('G') {
                if composer_empty {
                    return Action::ScrollBottom;
                } else {
                    return Action::Edtui(key_event);
                }
            }

            Action::Edtui(key_event)
        }
        EditorMode::Insert => {
            // Alt+<printable> in insert mode (fast typing Esc followed by key):
            // Treat as Esc followed by that key (vim behavior).
            // Alt-Enter is preserved as Newline.
            if key_event.modifiers.contains(KeyModifiers::ALT)
                && !key_event.modifiers.contains(KeyModifiers::CONTROL)
            {
                if key_event.code == KeyCode::Enter {
                    return Action::Newline;
                } else if let KeyCode::Char(c) = key_event.code
                    && !c.is_control()
                {
                    let mut next = key_event;
                    next.modifiers.remove(KeyModifiers::ALT);
                    return Action::EscKey(next);
                }
            }

            // Up/Down in insert mode: scroll transcript if empty, editor cursor if non-empty
            if key_event.modifiers == KeyModifiers::NONE {
                match key_event.code {
                    KeyCode::Up => {
                        if composer_empty {
                            return Action::ScrollLines(-3);
                        } else {
                            return Action::Edtui(key_event);
                        }
                    }
                    KeyCode::Down => {
                        if composer_empty {
                            return Action::ScrollLines(3);
                        } else {
                            return Action::Edtui(key_event);
                        }
                    }
                    _ => {}
                }
            }

            // Ctrl-C
            if key_event.modifiers == KeyModifiers::CONTROL && key_event.code == KeyCode::Char('c')
            {
                if composer_empty {
                    return Action::Interrupt;
                } else {
                    return Action::LeaveInsertMode;
                }
            }

            // Ctrl-D: quit if empty, no-op (Ignore) if non-empty (removed from edtui handler)
            if key_event.modifiers == KeyModifiers::CONTROL && key_event.code == KeyCode::Char('d')
            {
                if composer_empty {
                    return Action::Quit;
                } else {
                    return Action::Ignore;
                }
            }

            // Ctrl-P, Ctrl-N, Ctrl-J
            if key_event.modifiers == KeyModifiers::CONTROL {
                match key_event.code {
                    KeyCode::Char('p') => return Action::HistoryPrev,
                    KeyCode::Char('n') => return Action::HistoryNext,
                    KeyCode::Char('j') => return Action::Newline,
                    _ => {}
                }
            }

            // Enter / Alt-Enter / Shift-Enter
            if key_event.code == KeyCode::Enter {
                if key_event.modifiers.contains(KeyModifiers::ALT) {
                    return Action::Newline;
                }
                if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                    if enhanced_keyboard {
                        return Action::Newline;
                    } else if composer_empty {
                        return Action::Ignore;
                    } else {
                        return Action::Submit;
                    }
                }
                if key_event.modifiers.is_empty() {
                    if composer_empty {
                        return Action::Ignore;
                    } else {
                        return Action::Submit;
                    }
                }
            }

            Action::Edtui(key_event)
        }
        _ => {
            // Visual, Search, etc.
            if key_event.modifiers == KeyModifiers::CONTROL && key_event.code == KeyCode::Char('c')
            {
                return Action::Interrupt;
            }
            Action::Edtui(key_event)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

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

    fn alt(code: KeyCode) -> KeyEvent {
        press(code, KeyModifiers::ALT)
    }

    fn shift(c: char) -> KeyEvent {
        press(KeyCode::Char(c), KeyModifiers::SHIFT)
    }

    #[test]
    fn test_normal_mode_scroll_keys() {
        assert_eq!(
            resolve(EditorMode::Normal, true, ctrl('e'), None, false),
            Action::ScrollLines(1)
        );
        assert_eq!(
            resolve(EditorMode::Normal, true, ctrl('y'), None, false),
            Action::ScrollLines(-1)
        );
        assert_eq!(
            resolve(EditorMode::Normal, true, ctrl('d'), None, false),
            Action::ScrollHalfPage(1)
        );
        assert_eq!(
            resolve(EditorMode::Normal, true, ctrl('u'), None, false),
            Action::ScrollHalfPage(-1)
        );
        assert_eq!(
            resolve(EditorMode::Normal, true, ctrl('f'), None, false),
            Action::ScrollPage(1)
        );
        assert_eq!(
            resolve(EditorMode::Normal, true, ctrl('b'), None, false),
            Action::ScrollPage(-1)
        );
    }

    #[test]
    fn test_up_down_empty_vs_non_empty_in_both_modes() {
        let up = key(KeyCode::Up);
        let down = key(KeyCode::Down);

        // When composer is empty: Up/Down scroll transcript 3 lines in BOTH modes
        assert_eq!(
            resolve(EditorMode::Normal, true, up, None, false),
            Action::ScrollLines(-3)
        );
        assert_eq!(
            resolve(EditorMode::Normal, true, down, None, false),
            Action::ScrollLines(3)
        );
        assert_eq!(
            resolve(EditorMode::Insert, true, up, None, false),
            Action::ScrollLines(-3)
        );
        assert_eq!(
            resolve(EditorMode::Insert, true, down, None, false),
            Action::ScrollLines(3)
        );

        // When composer is NOT empty: Up/Down pass through to edtui in BOTH modes
        assert_eq!(
            resolve(EditorMode::Normal, false, up, None, false),
            Action::Edtui(up)
        );
        assert_eq!(
            resolve(EditorMode::Normal, false, down, None, false),
            Action::Edtui(down)
        );
        assert_eq!(
            resolve(EditorMode::Insert, false, up, None, false),
            Action::Edtui(up)
        );
        assert_eq!(
            resolve(EditorMode::Insert, false, down, None, false),
            Action::Edtui(down)
        );
    }

    #[test]
    fn test_normal_mode_gg_and_g_empty_vs_non_empty() {
        // When composer is empty:
        // 'g' without prefix returns PendingPrefix('g')
        assert_eq!(
            resolve(
                EditorMode::Normal,
                true,
                key(KeyCode::Char('g')),
                None,
                false
            ),
            Action::PendingPrefix('g')
        );
        // 'g' with prefix 'g' returns ScrollTop
        assert_eq!(
            resolve(
                EditorMode::Normal,
                true,
                key(KeyCode::Char('g')),
                Some('g'),
                false
            ),
            Action::ScrollTop
        );
        // 'G' (shift-G) when empty returns ScrollBottom
        assert_eq!(
            resolve(EditorMode::Normal, true, shift('G'), None, false),
            Action::ScrollBottom
        );

        // When composer is NOT empty:
        // 'g' and 'G' pass through to edtui
        let g_event = key(KeyCode::Char('g'));
        assert_eq!(
            resolve(EditorMode::Normal, false, g_event, None, false),
            Action::Edtui(g_event)
        );
        let shift_g_event = shift('G');
        assert_eq!(
            resolve(EditorMode::Normal, false, shift_g_event, None, false),
            Action::Edtui(shift_g_event)
        );
    }

    #[test]
    fn test_normal_mode_za_zt_toggles() {
        // 'z' without prefix -> PendingPrefix('z')
        assert_eq!(
            resolve(
                EditorMode::Normal,
                true,
                key(KeyCode::Char('z')),
                None,
                false
            ),
            Action::PendingPrefix('z')
        );
        // 'a' with prefix 'z' -> ToggleTools
        assert_eq!(
            resolve(
                EditorMode::Normal,
                true,
                key(KeyCode::Char('a')),
                Some('z'),
                false
            ),
            Action::ToggleTools
        );
        // 't' with prefix 'z' -> ToggleThoughts
        assert_eq!(
            resolve(
                EditorMode::Normal,
                true,
                key(KeyCode::Char('t')),
                Some('z'),
                false
            ),
            Action::ToggleThoughts
        );
        // any other key with prefix 'z' -> Edtui
        let x_event = key(KeyCode::Char('x'));
        assert_eq!(
            resolve(EditorMode::Normal, true, x_event, Some('z'), false),
            Action::Edtui(x_event)
        );
    }

    #[test]
    fn test_normal_mode_colon_opens_command() {
        assert_eq!(
            resolve(
                EditorMode::Normal,
                true,
                key(KeyCode::Char(':')),
                None,
                false
            ),
            Action::OpenCommand
        );
    }

    #[test]
    fn test_ctrl_p_ctrl_n_history() {
        assert_eq!(
            resolve(EditorMode::Normal, true, ctrl('p'), None, false),
            Action::HistoryPrev
        );
        assert_eq!(
            resolve(EditorMode::Normal, true, ctrl('n'), None, false),
            Action::HistoryNext
        );
        assert_eq!(
            resolve(EditorMode::Insert, true, ctrl('p'), None, false),
            Action::HistoryPrev
        );
        assert_eq!(
            resolve(EditorMode::Insert, true, ctrl('n'), None, false),
            Action::HistoryNext
        );
    }

    #[test]
    fn test_ctrl_d_conflict_normal_vs_insert() {
        // In Normal mode: ctrl-d is half page scroll down regardless of composer empty
        assert_eq!(
            resolve(EditorMode::Normal, true, ctrl('d'), None, false),
            Action::ScrollHalfPage(1)
        );
        assert_eq!(
            resolve(EditorMode::Normal, false, ctrl('d'), None, false),
            Action::ScrollHalfPage(1)
        );

        // In Insert mode: ctrl-d on empty composer is Quit
        assert_eq!(
            resolve(EditorMode::Insert, true, ctrl('d'), None, false),
            Action::Quit
        );
        // In Insert mode: ctrl-d on non-empty composer is Ignore (consistent with removal from edtui)
        assert_eq!(
            resolve(EditorMode::Insert, false, ctrl('d'), None, false),
            Action::Ignore
        );
    }

    #[test]
    fn test_ctrl_c_insert_with_vs_without_text_and_normal() {
        // Normal mode: Ctrl-C sends Interrupt
        assert_eq!(
            resolve(EditorMode::Normal, true, ctrl('c'), None, false),
            Action::Interrupt
        );
        assert_eq!(
            resolve(EditorMode::Normal, false, ctrl('c'), None, false),
            Action::Interrupt
        );

        // Insert mode with text: LeaveInsertMode only
        assert_eq!(
            resolve(EditorMode::Insert, false, ctrl('c'), None, false),
            Action::LeaveInsertMode
        );

        // Insert mode empty: Interrupt
        assert_eq!(
            resolve(EditorMode::Insert, true, ctrl('c'), None, false),
            Action::Interrupt
        );

        // Visual mode: Interrupt
        assert_eq!(
            resolve(EditorMode::Visual, false, ctrl('c'), None, false),
            Action::Interrupt
        );
    }

    #[test]
    fn test_insert_mode_enter_and_newlines() {
        let enter = key(KeyCode::Enter);
        // Empty composer: Enter ignored
        assert_eq!(
            resolve(EditorMode::Insert, true, enter, None, false),
            Action::Ignore
        );
        // Non-empty composer: Enter submits
        assert_eq!(
            resolve(EditorMode::Insert, false, enter, None, false),
            Action::Submit
        );

        // Alt-Enter inserts newline
        let alt_enter = alt(KeyCode::Enter);
        assert_eq!(
            resolve(EditorMode::Insert, false, alt_enter, None, false),
            Action::Newline
        );
        assert_eq!(
            resolve(EditorMode::Insert, true, alt_enter, None, false),
            Action::Newline
        );

        // Ctrl-J inserts newline
        let ctrl_j = ctrl('j');
        assert_eq!(
            resolve(EditorMode::Insert, false, ctrl_j, None, false),
            Action::Newline
        );
        assert_eq!(
            resolve(EditorMode::Insert, true, ctrl_j, None, false),
            Action::Newline
        );
    }

    #[test]
    fn test_alt_printable_in_insert_mode_yields_esc_key() {
        let alt_colon = press(KeyCode::Char(':'), KeyModifiers::ALT);
        assert_eq!(
            resolve(EditorMode::Insert, false, alt_colon, None, false),
            Action::EscKey(key(KeyCode::Char(':')))
        );

        let alt_q = press(KeyCode::Char('q'), KeyModifiers::ALT);
        assert_eq!(
            resolve(EditorMode::Insert, false, alt_q, None, false),
            Action::EscKey(key(KeyCode::Char('q')))
        );

        // Alt-Enter is still Newline
        let alt_enter = press(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(
            resolve(EditorMode::Insert, false, alt_enter, None, false),
            Action::Newline
        );
    }

    #[test]
    fn test_shift_enter_with_and_without_enhancement() {
        let shift_enter = press(KeyCode::Enter, KeyModifiers::SHIFT);

        // With enhancement flags pushed: Shift-Enter is always Newline
        assert_eq!(
            resolve(EditorMode::Insert, false, shift_enter, None, true),
            Action::Newline
        );
        assert_eq!(
            resolve(EditorMode::Insert, true, shift_enter, None, true),
            Action::Newline
        );

        // Without enhancement flags: Shift-Enter behaves like plain Enter (Submit if non-empty, Ignore if empty)
        assert_eq!(
            resolve(EditorMode::Insert, false, shift_enter, None, false),
            Action::Submit
        );
        assert_eq!(
            resolve(EditorMode::Insert, true, shift_enter, None, false),
            Action::Ignore
        );
    }

    #[test]
    fn test_release_events_ignored() {
        let release = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Release,
            state: KeyEventState::empty(),
        };
        assert_eq!(
            resolve(EditorMode::Insert, false, release, None, false),
            Action::Ignore
        );
    }

    #[test]
    fn test_other_keys_pass_to_edtui() {
        let h = key(KeyCode::Char('h'));
        assert_eq!(
            resolve(EditorMode::Normal, false, h, None, false),
            Action::Edtui(h)
        );

        let esc = key(KeyCode::Esc);
        assert_eq!(
            resolve(EditorMode::Insert, false, esc, None, false),
            Action::Edtui(esc)
        );
    }
}
