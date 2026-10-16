//! Keybindings: which key means what in each mode.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Logs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing a filter for the focused pane.
    Filter,
    Confirm(Confirm),
    Help,
    Problems,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    StopAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Start,
    Stop,
    Restart,
    TogglePin,
    ToggleDisable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Quit,
    Move(isize),
    PageUp,
    PageDown,
    Top,
    Bottom,
    Collapse,
    Expand,
    Toggle,
    SwitchFocus,
    StartFilter,
    ClearFilter,
    FilterChar(char),
    FilterBackspace,
    FilterDone,
    RunningOnly,
    Control(Control),
    StopIdle,
    StopAll,
    Open,
    Help,
    Problems,
    Yes,
    Close,
}

/// The help popup's contents, grouped by section.
pub const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigate",
        &[
            ("↑↓ j k", "move"),
            ("←→ h l", "fold, unfold"),
            ("enter space", "fold or unfold"),
            ("g G", "top, bottom"),
            ("PgUp PgDn", "page"),
            ("tab", "switch between tree and logs"),
            ("/", "filter the focused pane"),
            ("esc", "clear the filter"),
            ("a", "show only running"),
        ],
    ),
    (
        "Control the selection",
        &[
            ("s", "start"),
            ("x", "stop"),
            ("r", "restart"),
            ("p", "pin or unpin"),
            ("d", "disable or enable"),
            ("o", "open in the browser"),
        ],
    ),
    (
        "Everything",
        &[
            ("I", "stop idle instances"),
            ("X", "stop all instances"),
            ("!", "config problems"),
            ("?", "this help"),
            ("q", "quit"),
        ],
    ),
];

pub fn intent(mode: &Mode, focus: Focus, key: KeyEvent) -> Option<Intent> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(Intent::Quit);
    }
    match mode {
        Mode::Filter => filter(key),
        Mode::Confirm(_) => Some(match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Intent::Yes,
            _ => Intent::Close,
        }),
        Mode::Help | Mode::Problems => Some(Intent::Close),
        Mode::Normal => normal(focus, key),
    }
}

fn filter(key: KeyEvent) -> Option<Intent> {
    Some(match key.code {
        KeyCode::Esc => Intent::ClearFilter,
        KeyCode::Enter | KeyCode::Down | KeyCode::Up | KeyCode::Tab => Intent::FilterDone,
        KeyCode::Backspace => Intent::FilterBackspace,
        KeyCode::Char(c) => Intent::FilterChar(c),
        _ => return None,
    })
}

fn normal(focus: Focus, key: KeyEvent) -> Option<Intent> {
    let shared = match key.code {
        KeyCode::Char('q') => Some(Intent::Quit),
        KeyCode::Char('?') => Some(Intent::Help),
        KeyCode::Char('!') => Some(Intent::Problems),
        KeyCode::Tab | KeyCode::BackTab => Some(Intent::SwitchFocus),
        KeyCode::Char('/') => Some(Intent::StartFilter),
        KeyCode::Esc => Some(Intent::ClearFilter),
        KeyCode::Down | KeyCode::Char('j') => Some(Intent::Move(1)),
        KeyCode::Up | KeyCode::Char('k') => Some(Intent::Move(-1)),
        KeyCode::PageDown => Some(Intent::PageDown),
        KeyCode::PageUp => Some(Intent::PageUp),
        KeyCode::Home | KeyCode::Char('g') => Some(Intent::Top),
        KeyCode::End | KeyCode::Char('G') => Some(Intent::Bottom),
        KeyCode::Char('s') => Some(Intent::Control(Control::Start)),
        KeyCode::Char('x') => Some(Intent::Control(Control::Stop)),
        KeyCode::Char('r') => Some(Intent::Control(Control::Restart)),
        KeyCode::Char('p') => Some(Intent::Control(Control::TogglePin)),
        KeyCode::Char('d') => Some(Intent::Control(Control::ToggleDisable)),
        KeyCode::Char('o') => Some(Intent::Open),
        KeyCode::Char('I') => Some(Intent::StopIdle),
        KeyCode::Char('X') => Some(Intent::StopAll),
        KeyCode::Char('a') => Some(Intent::RunningOnly),
        _ => None,
    };
    if shared.is_some() {
        return shared;
    }
    match (focus, key.code) {
        (Focus::Tree, KeyCode::Left | KeyCode::Char('h')) => Some(Intent::Collapse),
        (Focus::Tree, KeyCode::Right | KeyCode::Char('l')) => Some(Intent::Expand),
        (Focus::Tree, KeyCode::Enter | KeyCode::Char(' ')) => Some(Intent::Toggle),
        (Focus::Logs, KeyCode::Left | KeyCode::Char('h')) => Some(Intent::SwitchFocus),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn letters_are_text_while_filtering() {
        let c = key(KeyCode::Char('q'));
        assert_eq!(intent(&Mode::Normal, Focus::Tree, c), Some(Intent::Quit));
        assert_eq!(
            intent(&Mode::Filter, Focus::Tree, c),
            Some(Intent::FilterChar('q'))
        );
        assert_eq!(
            intent(&Mode::Filter, Focus::Tree, key(KeyCode::Esc)),
            Some(Intent::ClearFilter)
        );
    }

    #[test]
    fn ctrl_c_always_quits() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        for mode in [Mode::Normal, Mode::Filter, Mode::Help] {
            assert_eq!(intent(&mode, Focus::Logs, ctrl_c), Some(Intent::Quit));
        }
    }

    #[test]
    fn folding_keys_belong_to_the_tree() {
        let l = key(KeyCode::Char('l'));
        assert_eq!(intent(&Mode::Normal, Focus::Tree, l), Some(Intent::Expand));
        assert_eq!(intent(&Mode::Normal, Focus::Logs, l), None);
    }

    #[test]
    fn confirmation_takes_y_or_anything_else() {
        let mode = Mode::Confirm(Confirm::StopAll);
        assert_eq!(
            intent(&mode, Focus::Tree, key(KeyCode::Char('y'))),
            Some(Intent::Yes)
        );
        assert_eq!(
            intent(&mode, Focus::Tree, key(KeyCode::Char('n'))),
            Some(Intent::Close)
        );
    }
}
