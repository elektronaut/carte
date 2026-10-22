//! Keybindings: which key means what in each mode.

use carte_api::ShareRequest;
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
    /// Typing a name to jump to.
    Jump,
    /// Typing into the selected process.
    Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    StopAll,
    /// The provider carries one app at a time and `shared` has it.
    ReplaceShare {
        request: ShareRequest,
        shared: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Start,
    Stop,
    Restart,
    TogglePin,
    ToggleDisable,
    /// Writes a temporary pin or disable to the project file.
    Save,
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
    /// Switch the logs between searching and filtering while typing.
    ToggleLogFilter,
    CycleShow,
    ToggleMouse,
    /// Show the tab with this number below the details.
    PickTab(u8),
    /// Open the selected row of the tab, like a request.
    OpenRow,
    /// Show the tab this many places along.
    StepTab(isize),
    /// Give the right side the whole main area, or go back.
    ToggleZoom,
    Control(Control),
    StopIdle,
    StopAll,
    Open,
    OpenPublic,
    /// Add a marker with the time to the selection's logs.
    Mark,
    CopyUrl,
    CopyPublicUrl,
    StartJump,
    /// Moves to the next older match of the logs search, or the next newer one.
    FindMatch {
        older: bool,
    },
    /// Share or unshare the selected app; with `auth`, share it behind a password.
    Share {
        auth: bool,
    },
    /// Start typing into the selected process.
    StartTyping,
    /// A key to type into the process.
    Typed(KeyEvent),
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
            ("/", "filter the tree, search the logs"),
            ("ctrl-f", "filter or search the logs, while typing"),
            ("n N", "older, newer match in the logs"),
            ("esc", "clear the filter or search"),
            ("a", "show all, running or shared"),
            ("1 2 3 5", "show logs, events, env, requests"),
            ("[ ]", "previous, next tab"),
            ("z", "zoom the details and tabs, or go back"),
            ("enter esc", "open a request, back to the list"),
            ("drag", "copy log lines"),
            ("-", "mark the logs with the time"),
            ("m", "mouse on or off"),
            ("ctrl-p", "jump to a project, stack, app or process"),
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
            ("S", "save a temporary pin or disable"),
            ("o", "open in the browser"),
            ("w", "share on the web, or stop sharing"),
            ("W", "share behind a password"),
            ("O", "open the shared address"),
            ("y Y", "copy the address, the shared address"),
            ("t", "type into the process, like at a debugger prompt"),
        ],
    ),
    (
        "Everything",
        &[
            ("I", "stop idle instances"),
            ("X", "stop all instances"),
            ("!", "problems"),
            ("?", "this help"),
            ("q", "quit"),
        ],
    ),
];

pub fn intent(mode: &Mode, focus: Focus, key: KeyEvent) -> Option<Intent> {
    if *mode == Mode::Type {
        return Some(Intent::Typed(key));
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => return Some(Intent::Quit),
            KeyCode::Char('p') if *mode == Mode::Jump => return Some(Intent::Move(-1)),
            KeyCode::Char('n') if *mode == Mode::Jump => return Some(Intent::Move(1)),
            KeyCode::Char('p') if *mode == Mode::Normal => return Some(Intent::StartJump),
            KeyCode::Char('f') if *mode == Mode::Filter && focus == Focus::Logs => {
                return Some(Intent::ToggleLogFilter);
            }
            KeyCode::Char(_) if matches!(mode, Mode::Filter | Mode::Jump) => return None,
            _ => {}
        }
    }
    match mode {
        Mode::Filter => filter(key),
        Mode::Jump => Some(match key.code {
            KeyCode::Esc => Intent::Close,
            KeyCode::Enter => Intent::FilterDone,
            KeyCode::Down | KeyCode::Tab => Intent::Move(1),
            KeyCode::Up | KeyCode::BackTab => Intent::Move(-1),
            KeyCode::Backspace => Intent::FilterBackspace,
            KeyCode::Char(c) => Intent::FilterChar(c),
            _ => return None,
        }),
        Mode::Confirm(_) => Some(match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Intent::Yes,
            _ => Intent::Close,
        }),
        Mode::Help | Mode::Problems => Some(Intent::Close),
        Mode::Normal => normal(focus, key),
        Mode::Type => Some(Intent::Typed(key)),
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
        KeyCode::Char('S') => Some(Intent::Control(Control::Save)),
        KeyCode::Char('o') => Some(Intent::Open),
        KeyCode::Char('O') => Some(Intent::OpenPublic),
        KeyCode::Char('-') => Some(Intent::Mark),
        KeyCode::Char('y') => Some(Intent::CopyUrl),
        KeyCode::Char('Y') => Some(Intent::CopyPublicUrl),
        KeyCode::Char('w') => Some(Intent::Share { auth: false }),
        KeyCode::Char('W') => Some(Intent::Share { auth: true }),
        KeyCode::Char('I') => Some(Intent::StopIdle),
        KeyCode::Char('X') => Some(Intent::StopAll),
        KeyCode::Char('a') => Some(Intent::CycleShow),
        KeyCode::Char(c @ '1'..='5') => Some(Intent::PickTab(c as u8 - b'0')),
        KeyCode::Char('[') => Some(Intent::StepTab(-1)),
        KeyCode::Char(']') => Some(Intent::StepTab(1)),
        KeyCode::Char('z') => Some(Intent::ToggleZoom),
        KeyCode::Char('m') => Some(Intent::ToggleMouse),
        KeyCode::Char('t') => Some(Intent::StartTyping),
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
        (Focus::Logs, KeyCode::Enter) => Some(Intent::OpenRow),
        (Focus::Logs, KeyCode::Char('n')) => Some(Intent::FindMatch { older: true }),
        (Focus::Logs, KeyCode::Char('N')) => Some(Intent::FindMatch { older: false }),
        _ => None,
    }
}

/// The bytes a terminal sends for a key.
pub fn key_bytes(key: KeyEvent) -> Vec<u8> {
    let mut bytes = match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => match c {
            'a'..='z' | 'A'..='Z' | '@' | '[' | '\\' | ']' | '^' | '_' => {
                vec![(c.to_ascii_uppercase() as u8) & 0x1f]
            }
            ' ' | '2' => vec![0],
            '3' => vec![0x1b],
            '4'..='7' => vec![c as u8 - b'4' + 0x1c],
            '8' => vec![0x7f],
            _ => c.to_string().into_bytes(),
        },
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => Vec::new(),
    };
    if key.modifiers.contains(KeyModifiers::ALT) && !bytes.is_empty() {
        bytes.insert(0, 0x1b);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_typed_while_typing() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            intent(&Mode::Type, Focus::Logs, ctrl_c),
            Some(Intent::Typed(ctrl_c))
        );
        assert_eq!(
            intent(&Mode::Normal, Focus::Logs, key(KeyCode::Char('t'))),
            Some(Intent::StartTyping)
        );
    }

    #[test]
    fn keys_become_what_a_terminal_sends() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(key_bytes(ctrl('c')), [3]);
        assert_eq!(key_bytes(ctrl('q')), [0x11]);
        assert_eq!(key_bytes(ctrl(']')), [0x1d]);
        assert_eq!(
            [ctrl('4'), ctrl('5'), ctrl('6'), ctrl('7')].map(key_bytes),
            [[0x1c], [0x1d], [0x1e], [0x1f]],
            "ctrl-\\, ctrl-], ctrl-^ and ctrl-_ as crossterm reports them"
        );
        assert_eq!(key_bytes(ctrl('2')), [0]);
        assert_eq!(key_bytes(ctrl(' ')), [0]);
        assert_eq!(key_bytes(key(KeyCode::Char('å'))), "å".as_bytes());
        assert_eq!(key_bytes(key(KeyCode::Enter)), b"\r");
        assert_eq!(key_bytes(key(KeyCode::Up)), b"\x1b[A");
        assert_eq!(key_bytes(key(KeyCode::Backspace)), [0x7f]);
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)),
            b"\x1bb"
        );
        assert!(key_bytes(key(KeyCode::F(5))).is_empty());
    }

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
        let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(
            intent(&Mode::Filter, Focus::Logs, ctrl_f),
            Some(Intent::ToggleLogFilter)
        );
        assert_eq!(
            intent(&Mode::Filter, Focus::Tree, ctrl_f),
            None,
            "the tree only filters, and ctrl-f doesn't type"
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
    fn number_keys_pick_tabs_in_either_pane() {
        for focus in [Focus::Tree, Focus::Logs] {
            assert_eq!(
                intent(&Mode::Normal, focus, key(KeyCode::Char('3'))),
                Some(Intent::PickTab(3))
            );
            assert_eq!(
                intent(&Mode::Normal, focus, key(KeyCode::Char(']'))),
                Some(Intent::StepTab(1))
            );
        }
        assert_eq!(
            intent(&Mode::Normal, Focus::Tree, key(KeyCode::Char('6'))),
            None
        );
        assert_eq!(
            intent(&Mode::Filter, Focus::Tree, key(KeyCode::Char('1'))),
            Some(Intent::FilterChar('1'))
        );
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
