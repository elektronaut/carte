//! The colours the TUI draws with, by role.

use ratatui::style::{Color, Style};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub fg: Color,
    /// Painted under everything; `Color::Reset` keeps the terminal's own.
    pub bg: Color,
    /// Secondary text, fold markers, stopped and disabled things.
    pub muted: Color,
    /// Keys in legends and help, stacks, pins.
    pub accent: Color,
    pub border: Color,
    pub border_focus: Color,
    /// Running and ready, notices.
    pub ok: Color,
    /// Starting, stopping, scroll positions, prompts.
    pub warn: Color,
    /// Crashes, problems, errors.
    pub error: Color,
    /// Shares and the tunnel.
    pub shared: Color,
    /// A process waiting at a debugger prompt.
    pub paused: Color,
    /// The background of the selected tree row while the tree has the focus.
    pub selection_bg: Color,
    /// What `muted` text becomes on the selected tree row.
    pub selection_fg: Color,
    /// Search matches in the logs. A background of `Color::Reset` leaves it as it is.
    pub match_fg: Color,
    pub match_bg: Color,
    /// Matches on the current match's line.
    pub match_current_fg: Color,
    pub match_current_bg: Color,
    /// Process names in merged logs, one per process in turn.
    pub prefixes: Vec<Color>,
}

impl Theme {
    /// The terminal's own ANSI colours.
    pub fn terminal() -> Self {
        Theme {
            name: "terminal".into(),
            fg: Color::Reset,
            bg: Color::Reset,
            muted: Color::DarkGray,
            accent: Color::Blue,
            border: Color::DarkGray,
            border_focus: Color::Blue,
            ok: Color::Green,
            warn: Color::Yellow,
            error: Color::Red,
            shared: Color::Magenta,
            paused: Color::Magenta,
            selection_bg: Color::DarkGray,
            selection_fg: Color::Gray,
            match_fg: Color::Yellow,
            match_bg: Color::Reset,
            match_current_fg: Color::Black,
            match_current_bg: Color::Yellow,
            prefixes: vec![
                Color::Cyan,
                Color::Magenta,
                Color::Yellow,
                Color::Blue,
                Color::Green,
                Color::LightRed,
            ],
        }
    }

    /// Text and background for the whole screen.
    pub fn base(&self) -> Style {
        Style::new().fg(self.fg).bg(self.bg)
    }

    /// A search match in the logs; `current` on the current match's line.
    pub fn search_match(&self, current: bool) -> Style {
        let (fg, bg) = if current {
            (self.match_current_fg, self.match_current_bg)
        } else {
            (self.match_fg, self.match_bg)
        };
        let style = Style::new().fg(fg);
        let style = if bg == Color::Reset {
            style
        } else {
            style.bg(bg)
        };
        if current { style } else { style.bold() }
    }

    /// The colour for the process name at `index`.
    pub fn prefix(&self, index: usize) -> Color {
        match self.prefixes.len() {
            0 => self.fg,
            n => self.prefixes[index % n],
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::terminal()
    }
}
