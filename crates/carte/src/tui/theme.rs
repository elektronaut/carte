//! The colours the TUI draws with, by role, and the built-in themes.
//!
//! `Color::Reset` in `fg` and `bg` is the terminal's own colour. In any other role
//! it inherits: text takes `fg`, and a background leaves what is under it.

use ratatui::style::{Color, Style};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Stable, for the state file: `catppuccin-mocha`.
    pub id: String,
    /// For people: `Catppuccin Mocha`.
    pub label: String,
    pub fg: Color,
    /// Painted under everything.
    pub bg: Color,
    /// Secondary text, fold markers, stopped and disabled things.
    pub muted: Color,
    /// Keys in legends and help, stacks, pins.
    pub accent: Color,
    pub border: Color,
    pub border_focus: Color,
    /// Running and ready, notices.
    pub ok: Color,
    /// Starting, stopping, suspended, scroll positions, prompts.
    pub warn: Color,
    /// Crashes, problems, errors.
    pub error: Color,
    /// Shares and the tunnel.
    pub shared: Color,
    /// A process waiting at a debugger prompt.
    pub paused: Color,
    /// The background of the selected tree row, selected log lines and the jump
    /// prompt's pick.
    pub selection_bg: Color,
    /// What `muted` text becomes on the selected tree row.
    pub selection_fg: Color,
    /// Search matches in the logs.
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
            id: "terminal".into(),
            label: "Terminal".into(),
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

    /// The selected tree row.
    pub fn selection(&self) -> Style {
        on(Style::new(), self.selection_bg)
    }

    /// A selected log line or pick in a list: reversed in a theme that leaves the
    /// background to the terminal.
    pub fn line_selection(&self) -> Style {
        if self.bg == Color::Reset {
            Style::new().reversed()
        } else {
            self.selection()
        }
    }

    /// A search match in the logs; `current` on the current match's line.
    pub fn search_match(&self, current: bool) -> Style {
        if current {
            on(
                Style::new().fg(self.match_current_fg),
                self.match_current_bg,
            )
        } else {
            on(Style::new().fg(self.match_fg), self.match_bg).bold()
        }
    }

    /// The colour for the process name at `index`.
    pub fn prefix(&self, index: usize) -> Color {
        match self.prefixes.len() {
            0 => self.fg,
            n => self.prefixes[index % n],
        }
    }

    /// Text roles left at `Color::Reset` take `fg`.
    pub fn inherit(mut self) -> Self {
        let fg = self.fg;
        for role in self.text_roles_mut() {
            if *role == Color::Reset {
                *role = fg;
            }
        }
        self
    }

    /// Every truecolor role as the nearest of the 256 colours, for terminals without
    /// truecolor. The first 16 are left out, since terminals change them.
    pub fn indexed(mut self) -> Self {
        for role in self.roles_mut() {
            *role = nearest_indexed(*role);
        }
        self
    }

    fn text_roles_mut(&mut self) -> impl Iterator<Item = &mut Color> {
        [
            &mut self.muted,
            &mut self.accent,
            &mut self.border,
            &mut self.border_focus,
            &mut self.ok,
            &mut self.warn,
            &mut self.error,
            &mut self.shared,
            &mut self.paused,
            &mut self.selection_fg,
            &mut self.match_fg,
            &mut self.match_current_fg,
        ]
        .into_iter()
        .chain(self.prefixes.iter_mut())
    }

    fn roles_mut(&mut self) -> impl Iterator<Item = &mut Color> {
        [
            &mut self.fg,
            &mut self.bg,
            &mut self.muted,
            &mut self.accent,
            &mut self.border,
            &mut self.border_focus,
            &mut self.ok,
            &mut self.warn,
            &mut self.error,
            &mut self.shared,
            &mut self.paused,
            &mut self.selection_bg,
            &mut self.selection_fg,
            &mut self.match_fg,
            &mut self.match_bg,
            &mut self.match_current_fg,
            &mut self.match_current_bg,
        ]
        .into_iter()
        .chain(self.prefixes.iter_mut())
    }

    /// The theme as `colors` draws it, with each colour that is too faint moved
    /// until it reaches the contrast `contrast_failures` asks for. At 256 colours,
    /// each role then takes the closest of the 256 that keeps that contrast.
    fn legible(self, colors: Colors) -> Self {
        let theme = self.legible_truecolor();
        match colors {
            Colors::Truecolor => theme,
            Colors::Indexed => theme.pick_indexed(),
        }
    }

    /// Each colour that is too faint moved by the fewest whole percent that reach
    /// the contrast: `fg` and the hues toward black or white, which keeps their hue,
    /// up to `MAX_SHIFT`; `muted` and `selection_fg` toward `fg`, up to
    /// `MUTED_SHIFT`; `border` toward `bg`. Search match backgrounds move away from
    /// their text.
    fn legible_truecolor(mut self) -> Self {
        let backs = [self.bg, self.selection_bg];
        let away = away_from(self.bg);
        self.fg = toward(self.fg, away, FG_CONTRAST, &backs, MAX_SHIFT);
        let fg = self.fg;
        self.muted = toward(self.muted, fg, ROLE_CONTRAST, &backs, MUTED_SHIFT);
        self.selection_fg = toward(
            self.selection_fg,
            fg,
            ROLE_CONTRAST,
            &[self.selection_bg],
            MUTED_SHIFT,
        );
        for role in [
            &mut self.accent,
            &mut self.ok,
            &mut self.warn,
            &mut self.error,
            &mut self.shared,
            &mut self.paused,
        ]
        .into_iter()
        .chain(self.prefixes.iter_mut())
        {
            *role = toward(*role, away, ROLE_CONTRAST, &backs, MAX_SHIFT);
        }
        self.border_focus = toward(
            self.border_focus,
            away,
            ROLE_CONTRAST,
            &[self.bg],
            MAX_SHIFT,
        );
        self.border = toward(
            self.border,
            self.bg,
            BORDER_CONTRAST,
            &[self.border_focus],
            100,
        );
        for (text, back) in [
            (&mut self.match_fg, &mut self.match_bg),
            (&mut self.match_current_fg, &mut self.match_current_bg),
        ] {
            *back = toward(*back, away_from(*text), FG_CONTRAST, &[*text], MAX_SHIFT);
        }
        self
    }

    /// Each role as the closest of the 256 colours, in OKLab, that keeps the
    /// contrast. Coloured roles skip the greys, and the status colours and the
    /// accent each take a different one.
    fn pick_indexed(mut self) -> Self {
        let dark = luminance(self.bg).is_some_and(|l| l <= 0.18);
        let behind = |target: Color| {
            let target = luminance(target).unwrap_or(0.0);
            move |c: Color| {
                let l = luminance(c).unwrap_or(0.0);
                if dark { l <= target } else { l >= target }
            }
        };
        self.bg = closest_indexed(self.bg, false, behind(self.bg));
        let behind_selection = behind(self.selection_bg);
        self.selection_bg = closest_indexed(self.selection_bg, false, |c| {
            c != self.bg && behind_selection(c)
        });
        let backs = [self.bg, self.selection_bg];
        let passes = |need: f64, backs: &[Color]| {
            let backs = backs.to_vec();
            move |c: Color| {
                backs
                    .iter()
                    .all(|back| contrast(c, *back).is_none_or(|ratio| ratio >= need))
            }
        };
        self.fg = closest_indexed(self.fg, false, passes(FG_CONTRAST, &backs));
        self.muted = closest_indexed(self.muted, false, passes(ROLE_CONTRAST, &backs));
        self.selection_fg = closest_indexed(
            self.selection_fg,
            false,
            passes(ROLE_CONTRAST, &[self.selection_bg]),
        );
        let mut taken = Vec::new();
        for role in [
            &mut self.ok,
            &mut self.warn,
            &mut self.error,
            &mut self.shared,
            &mut self.paused,
            &mut self.accent,
        ] {
            let legible = passes(ROLE_CONTRAST, &backs);
            *role = closest_indexed(*role, true, |c| legible(c) && !taken.contains(&c));
            taken.push(*role);
        }
        for prefix in &mut self.prefixes {
            *prefix = closest_indexed(*prefix, true, passes(ROLE_CONTRAST, &backs));
        }
        self.border_focus =
            closest_indexed(self.border_focus, true, passes(ROLE_CONTRAST, &[self.bg]));
        self.border = closest_indexed(
            self.border,
            false,
            passes(BORDER_CONTRAST, &[self.border_focus]),
        );
        for (text, back) in [
            (&mut self.match_fg, &mut self.match_bg),
            (&mut self.match_current_fg, &mut self.match_current_bg),
        ] {
            *text = closest_indexed(*text, false, |_| true);
            *back = closest_indexed(*back, true, passes(FG_CONTRAST, &[*text]));
        }
        self
    }

    /// Pairs below the contrast this theme should have, as `role on background
    /// ratio`. Pairs with an ANSI colour are left out, since the terminal decides
    /// its value.
    #[cfg(test)]
    pub fn contrast_failures(&self) -> Vec<String> {
        let text = |c: Color| if c == Color::Reset { self.fg } else { c };
        let back = |c: Color| if c == Color::Reset { self.bg } else { c };
        let mut failures = Vec::new();
        let mut check = |name: &str, color: Color, back_name: &str, under: Color, need: f64| {
            if let Some(ratio) = contrast(text(color), back(under))
                && ratio < need
            {
                failures.push(format!("{name} on {back_name} {ratio:.2}"));
            }
        };
        for (back_name, under) in [("bg", self.bg), ("selection_bg", self.selection_bg)] {
            check("fg", self.fg, back_name, under, FG_CONTRAST);
            for (name, role) in [
                ("muted", self.muted),
                ("accent", self.accent),
                ("ok", self.ok),
                ("warn", self.warn),
                ("error", self.error),
                ("shared", self.shared),
                ("paused", self.paused),
            ] {
                check(name, role, back_name, under, ROLE_CONTRAST);
            }
            for (i, prefix) in self.prefixes.iter().enumerate() {
                check(
                    &format!("prefix {i}"),
                    *prefix,
                    back_name,
                    under,
                    ROLE_CONTRAST,
                );
            }
        }
        check(
            "selection_fg",
            self.selection_fg,
            "selection_bg",
            self.selection_bg,
            ROLE_CONTRAST,
        );
        check(
            "border_focus",
            self.border_focus,
            "bg",
            self.bg,
            ROLE_CONTRAST,
        );
        check(
            "border_focus",
            self.border_focus,
            "border",
            self.border,
            BORDER_CONTRAST,
        );
        check(
            "match_fg",
            self.match_fg,
            "match_bg",
            self.match_bg,
            FG_CONTRAST,
        );
        check(
            "match_current_fg",
            self.match_current_fg,
            "match_current_bg",
            self.match_current_bg,
            FG_CONTRAST,
        );
        failures
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::terminal()
    }
}

/// Whether colours are drawn as they are or as the nearest of 256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colors {
    Truecolor,
    Indexed,
}

impl Colors {
    /// Whether the terminal says it shows 24-bit colour.
    pub fn detect() -> Self {
        if std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit") {
            Colors::Truecolor
        } else {
            Colors::Indexed
        }
    }
}

/// Every built-in theme, `terminal` first, legible in `colors`.
pub fn builtins(colors: Colors) -> Vec<Theme> {
    let mut themes = vec![Theme::terminal()];
    themes.extend(raw_builtins().into_iter().map(|t| t.legible(colors)));
    themes
}

/// The built-ins other than `terminal`, as their palettes have them.
fn raw_builtins() -> Vec<Theme> {
    vec![
        catppuccin("latte", "Latte", &LATTE, LATTE.subtext0),
        catppuccin("frappe", "Frappé", &FRAPPE, FRAPPE.overlay2),
        catppuccin("macchiato", "Macchiato", &MACCHIATO, MACCHIATO.overlay2),
        catppuccin("mocha", "Mocha", &MOCHA, MOCHA.overlay2),
        tokyonight_night(),
        tokyonight_day(),
        gruvbox_dark(),
        gruvbox_light(),
        dracula(),
        alucard(),
        nord(),
        solarized_dark(),
        solarized_light(),
        rose_pine(),
        rose_pine_dawn(),
    ]
}

/// The built-in theme with this id.
#[cfg(test)]
pub fn named(id: &str, colors: Colors) -> Option<Theme> {
    builtins(colors).into_iter().find(|theme| theme.id == id)
}

const FG_CONTRAST: f64 = 4.5;
const ROLE_CONTRAST: f64 = 3.0;
/// Between `border_focus` and `border`, so the focused pane stands out.
const BORDER_CONTRAST: f64 = 1.5;
/// How far, in percent, `legible` may move a colour.
const MAX_SHIFT: u8 = 50;
const MUTED_SHIFT: u8 = 60;
/// OKLab chroma below which a colour counts as grey.
const GREY_CHROMA: f64 = 0.03;

fn on(style: Style, bg: Color) -> Style {
    if bg == Color::Reset {
        style
    } else {
        style.bg(bg)
    }
}

const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Black next to light colours, white next to dark ones.
fn away_from(color: Color) -> Color {
    if luminance(color).is_some_and(|l| l > 0.18) {
        Color::Rgb(0, 0, 0)
    } else {
        Color::Rgb(255, 255, 255)
    }
}

/// `top` over `under` at `alpha`; ANSI colours stay as `under` has them.
fn blend(top: Color, under: Color, alpha: f64) -> Color {
    match (top, under) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix =
                |a: u8, b: u8| (f64::from(a) * alpha + f64::from(b) * (1.0 - alpha)).round() as u8;
            Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
        }
        _ => under,
    }
}

/// `color` moved toward `goal` by the fewest whole percent, up to `max`, that
/// give it `need` against every one of `backs`. Unmoved when no shift is enough.
fn toward(color: Color, goal: Color, need: f64, backs: &[Color], max: u8) -> Color {
    (0..=max)
        .map(|percent| blend(goal, color, f64::from(percent) / 100.0))
        .find(|candidate| {
            backs
                .iter()
                .all(|back| contrast(*candidate, *back).is_none_or(|ratio| ratio >= need))
        })
        .unwrap_or(color)
}

/// WCAG relative luminance, for truecolor and the 256 colours from 16 up.
fn luminance(color: Color) -> Option<f64> {
    let (r, g, b) = rgb(color)?;
    Some(0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b))
}

/// WCAG contrast ratio, for colours with a known value.
fn contrast(a: Color, b: Color) -> Option<f64> {
    let (a, b) = (luminance(a)?, luminance(b)?);
    Some((a.max(b) + 0.05) / (a.min(b) + 0.05))
}

fn nearest_indexed(color: Color) -> Color {
    closest_indexed(color, true, |_| true)
}

/// The colour from 16 up closest to `color` in OKLab among those `allowed` lets
/// through. A grey stays grey, and with `keep_hue` a colour stays a colour. ANSI
/// colours and `Reset` stay as they are, and so does `color` when nothing is
/// allowed.
fn closest_indexed(color: Color, keep_hue: bool, allowed: impl Fn(Color) -> bool) -> Color {
    let Some(want) = oklab(color) else {
        return color;
    };
    let grey = chroma(want) < GREY_CHROMA;
    (16..=255)
        .map(Color::Indexed)
        .filter(|c| {
            let candidate_grey = chroma(oklab(*c).expect("an xterm colour")) < GREY_CHROMA;
            let fits = match (grey, keep_hue) {
                (true, _) => candidate_grey,
                (false, true) => !candidate_grey,
                (false, false) => true,
            };
            fits && allowed(*c)
        })
        .min_by(|a, b| {
            let distance = |c: &Color| distance(want, oklab(*c).expect("an xterm colour"));
            distance(a).total_cmp(&distance(b))
        })
        .unwrap_or(color)
}

fn rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(index @ 16..) => Some(xterm(index)),
        _ => None,
    }
}

fn linear(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// OKLab lightness and the a and b axes.
fn oklab(color: Color) -> Option<[f64; 3]> {
    let (r, g, b) = rgb(color)?;
    let (r, g, b) = (linear(r), linear(g), linear(b));
    let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
    let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
    let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
    Some([
        0.210_454_255_3 * l + 0.793_617_785 * m - 0.004_072_046_8 * s,
        1.977_998_495_1 * l - 2.428_592_205 * m + 0.450_593_709_9 * s,
        0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766 * s,
    ])
}

fn chroma(lab: [f64; 3]) -> f64 {
    lab[1].hypot(lab[2])
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum()
}

/// The RGB value of an xterm colour from 16 up.
fn xterm(index: u8) -> (u8, u8, u8) {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    if index >= 232 {
        let gray = 8 + (index - 232) * 10;
        return (gray, gray, gray);
    }
    let i = index - 16;
    (
        LEVELS[usize::from(i / 36)],
        LEVELS[usize::from(i / 6 % 6)],
        LEVELS[usize::from(i % 6)],
    )
}

// https://github.com/catppuccin/palette/blob/main/palette.json
// Mapping: https://github.com/catppuccin/catppuccin/blob/main/docs/style-guide.md
struct Catppuccin {
    mauve: Color,
    red: Color,
    peach: Color,
    yellow: Color,
    green: Color,
    teal: Color,
    blue: Color,
    lavender: Color,
    pink: Color,
    text: Color,
    subtext1: Color,
    subtext0: Color,
    overlay2: Color,
    overlay0: Color,
    base: Color,
}

const LATTE: Catppuccin = Catppuccin {
    mauve: hex(0x8839ef),
    red: hex(0xd20f39),
    peach: hex(0xfe640b),
    yellow: hex(0xdf8e1d),
    green: hex(0x40a02b),
    teal: hex(0x179299),
    blue: hex(0x1e66f5),
    lavender: hex(0x7287fd),
    pink: hex(0xea76cb),
    text: hex(0x4c4f69),
    subtext1: hex(0x5c5f77),
    subtext0: hex(0x6c6f85),
    overlay2: hex(0x7c7f93),
    overlay0: hex(0x9ca0b0),
    base: hex(0xeff1f5),
};

const FRAPPE: Catppuccin = Catppuccin {
    mauve: hex(0xca9ee6),
    red: hex(0xe78284),
    peach: hex(0xef9f76),
    yellow: hex(0xe5c890),
    green: hex(0xa6d189),
    teal: hex(0x81c8be),
    blue: hex(0x8caaee),
    lavender: hex(0xbabbf1),
    pink: hex(0xf4b8e4),
    text: hex(0xc6d0f5),
    subtext1: hex(0xb5bfe2),
    subtext0: hex(0xa5adce),
    overlay2: hex(0x949cbb),
    overlay0: hex(0x737994),
    base: hex(0x303446),
};

const MACCHIATO: Catppuccin = Catppuccin {
    mauve: hex(0xc6a0f6),
    red: hex(0xed8796),
    peach: hex(0xf5a97f),
    yellow: hex(0xeed49f),
    green: hex(0xa6da95),
    teal: hex(0x8bd5ca),
    blue: hex(0x8aadf4),
    lavender: hex(0xb7bdf8),
    pink: hex(0xf5bde6),
    text: hex(0xcad3f5),
    subtext1: hex(0xb8c0e0),
    subtext0: hex(0xa5adcb),
    overlay2: hex(0x939ab7),
    overlay0: hex(0x6e738d),
    base: hex(0x24273a),
};

const MOCHA: Catppuccin = Catppuccin {
    mauve: hex(0xcba6f7),
    red: hex(0xf38ba8),
    peach: hex(0xfab387),
    yellow: hex(0xf9e2af),
    green: hex(0xa6e3a1),
    teal: hex(0x94e2d5),
    blue: hex(0x89b4fa),
    lavender: hex(0xb4befe),
    pink: hex(0xf5c2e7),
    text: hex(0xcdd6f4),
    subtext1: hex(0xbac2de),
    subtext0: hex(0xa6adc8),
    overlay2: hex(0x9399b2),
    overlay0: hex(0x6c7086),
    base: hex(0x1e1e2e),
};

/// `muted` is Overlay 1 in the style guide, or the nearest of the subtle text
/// colours that is legible on the selection.
fn catppuccin(id: &str, flavor: &str, p: &Catppuccin, muted: Color) -> Theme {
    Theme {
        id: format!("catppuccin-{id}"),
        label: format!("Catppuccin {flavor}"),
        fg: p.text,
        bg: p.base,
        muted,
        accent: p.blue,
        border: p.overlay0,
        border_focus: p.lavender,
        ok: p.green,
        warn: p.yellow,
        error: p.red,
        shared: p.pink,
        paused: p.mauve,
        selection_bg: blend(p.overlay2, p.base, 0.25),
        selection_fg: p.subtext1,
        match_fg: p.base,
        match_bg: p.teal,
        match_current_fg: p.base,
        match_current_bg: p.red,
        prefixes: vec![p.teal, p.mauve, p.yellow, p.blue, p.green, p.peach],
    }
}

// https://github.com/folke/tokyonight.nvim/blob/main/extras/lua/tokyonight_night.lua
fn tokyonight_night() -> Theme {
    let bg = hex(0x1a1b26);
    let fg = hex(0xc0caf5);
    let comment = hex(0x565f89);
    let blue = hex(0x7aa2f7);
    let orange = hex(0xff9e64);
    Theme {
        id: "tokyonight-night".into(),
        label: "Tokyo Night".into(),
        fg,
        bg,
        muted: comment,
        accent: blue,
        border: hex(0x3b4261),
        border_focus: hex(0x27a1b9),
        ok: hex(0x9ece6a),
        warn: hex(0xe0af68),
        error: hex(0xf7768e),
        shared: hex(0xbb9af7),
        paused: orange,
        selection_bg: hex(0x283457),
        selection_fg: hex(0xa9b1d6),
        match_fg: fg,
        match_bg: hex(0x3d59a1),
        match_current_fg: bg,
        match_current_bg: orange,
        prefixes: vec![
            hex(0x7dcfff),
            hex(0xbb9af7),
            hex(0xe0af68),
            blue,
            hex(0x9ece6a),
            orange,
        ],
    }
}

// https://github.com/folke/tokyonight.nvim/blob/main/extras/lua/tokyonight_day.lua
fn tokyonight_day() -> Theme {
    let bg = hex(0xe1e2e7);
    let blue = hex(0x2e7de9);
    let orange = hex(0xb15c00);
    Theme {
        id: "tokyonight-day".into(),
        label: "Tokyo Night Day".into(),
        fg: hex(0x3760bf),
        bg,
        muted: hex(0x68709a),
        accent: blue,
        border: hex(0xa8aecb),
        border_focus: hex(0x4094a3),
        ok: hex(0x587539),
        warn: hex(0x8c6c3e),
        error: hex(0xf52a65),
        shared: hex(0x9854f1),
        paused: orange,
        selection_bg: hex(0xc4c8da),
        selection_fg: hex(0x6172b0),
        match_fg: bg,
        match_bg: hex(0x7890dd),
        match_current_fg: bg,
        match_current_bg: orange,
        prefixes: vec![
            hex(0x007197),
            hex(0x9854f1),
            hex(0x8c6c3e),
            blue,
            hex(0x587539),
            orange,
        ],
    }
}

// https://github.com/morhetz/gruvbox/blob/master/colors/gruvbox.vim
fn gruvbox_dark() -> Theme {
    let bg0 = hex(0x282828);
    let yellow = hex(0xfabd2f);
    let blue = hex(0x83a598);
    let orange = hex(0xfe8019);
    Theme {
        id: "gruvbox-dark".into(),
        label: "Gruvbox Dark".into(),
        fg: hex(0xebdbb2),
        bg: bg0,
        muted: hex(0x928374),
        accent: blue,
        border: hex(0x665c54),
        border_focus: blue,
        ok: hex(0xb8bb26),
        warn: yellow,
        error: hex(0xfb4934),
        shared: hex(0xd3869b),
        paused: orange,
        selection_bg: hex(0x3c3836),
        selection_fg: hex(0xa89984),
        match_fg: bg0,
        match_bg: yellow,
        match_current_fg: bg0,
        match_current_bg: orange,
        prefixes: vec![
            hex(0x8ec07c),
            hex(0xd3869b),
            yellow,
            blue,
            hex(0xb8bb26),
            orange,
        ],
    }
}

// https://github.com/morhetz/gruvbox/blob/master/colors/gruvbox.vim
fn gruvbox_light() -> Theme {
    let bg0 = hex(0xfbf1c7);
    let yellow = hex(0xb57614);
    let blue = hex(0x076678);
    let orange = hex(0xaf3a03);
    Theme {
        id: "gruvbox-light".into(),
        label: "Gruvbox Light".into(),
        fg: hex(0x3c3836),
        bg: bg0,
        muted: hex(0x7c6f64),
        accent: blue,
        border: hex(0xbdae93),
        border_focus: blue,
        ok: hex(0x79740e),
        warn: yellow,
        error: hex(0x9d0006),
        shared: hex(0x8f3f71),
        paused: orange,
        selection_bg: hex(0xebdbb2),
        selection_fg: hex(0x665c54),
        match_fg: bg0,
        match_bg: yellow,
        match_current_fg: bg0,
        match_current_bg: orange,
        prefixes: vec![
            hex(0x427b58),
            hex(0x8f3f71),
            yellow,
            blue,
            hex(0x79740e),
            orange,
        ],
    }
}

// https://draculatheme.com/spec
fn dracula() -> Theme {
    let background = hex(0x282a36);
    let comment = hex(0x6272a4);
    let purple = hex(0xbd93f9);
    let cyan = hex(0x8be9fd);
    let orange = hex(0xffb86c);
    let yellow = hex(0xf1fa8c);
    let pink = hex(0xff79c6);
    let green = hex(0x50fa7b);
    Theme {
        id: "dracula".into(),
        label: "Dracula".into(),
        fg: hex(0xf8f8f2),
        bg: background,
        muted: comment,
        accent: purple,
        border: comment,
        border_focus: purple,
        ok: green,
        warn: orange,
        error: hex(0xff5555),
        shared: pink,
        paused: cyan,
        selection_bg: hex(0x44475a),
        selection_fg: comment,
        match_fg: background,
        match_bg: yellow,
        match_current_fg: background,
        match_current_bg: orange,
        prefixes: vec![cyan, purple, yellow, pink, green, orange],
    }
}

// https://draculatheme.com/spec
fn alucard() -> Theme {
    let background = hex(0xfffbeb);
    let comment = hex(0x6c664b);
    let purple = hex(0x644ac9);
    let cyan = hex(0x036a96);
    let orange = hex(0xa34d14);
    let yellow = hex(0x846e15);
    let pink = hex(0xa3144d);
    let green = hex(0x14710a);
    Theme {
        id: "alucard".into(),
        label: "Alucard".into(),
        fg: hex(0x1f1f1f),
        bg: background,
        muted: comment,
        accent: purple,
        border: comment,
        border_focus: purple,
        ok: green,
        warn: orange,
        error: hex(0xcb3a2a),
        shared: pink,
        paused: cyan,
        selection_bg: hex(0xcfcfde),
        selection_fg: comment,
        match_fg: background,
        match_bg: yellow,
        match_current_fg: background,
        match_current_bg: orange,
        prefixes: vec![cyan, purple, yellow, pink, green, orange],
    }
}

// https://www.nordtheme.com/docs/colors-and-palettes
// https://github.com/nordtheme/nord/blob/develop/src/nord.css
fn nord() -> Theme {
    let nord0 = hex(0x2e3440);
    let nord3 = hex(0x4c566a);
    let nord8 = hex(0x88c0d0);
    let nord12 = hex(0xd08770);
    let nord13 = hex(0xebcb8b);
    let nord15 = hex(0xb48ead);
    Theme {
        id: "nord".into(),
        label: "Nord".into(),
        fg: hex(0xd8dee9),
        bg: nord0,
        muted: nord3,
        accent: nord8,
        border: nord3,
        border_focus: nord8,
        ok: hex(0xa3be8c),
        warn: nord13,
        error: hex(0xbf616a),
        shared: nord15,
        paused: nord12,
        selection_bg: hex(0x434c5e),
        selection_fg: nord3,
        match_fg: nord0,
        match_bg: nord8,
        match_current_fg: nord0,
        match_current_bg: nord13,
        prefixes: vec![
            hex(0x8fbcbb),
            nord15,
            nord13,
            hex(0x81a1c1),
            hex(0xa3be8c),
            nord12,
        ],
    }
}

// https://github.com/altercation/solarized/blob/master/README.md
struct Solarized {
    yellow: Color,
    orange: Color,
    red: Color,
    magenta: Color,
    violet: Color,
    blue: Color,
    cyan: Color,
    green: Color,
}

const SOLARIZED: Solarized = Solarized {
    yellow: hex(0xb58900),
    orange: hex(0xcb4b16),
    red: hex(0xdc322f),
    magenta: hex(0xd33682),
    violet: hex(0x6c71c4),
    blue: hex(0x268bd2),
    cyan: hex(0x2aa198),
    green: hex(0x859900),
};

/// The base colours, from the background up: base03, base02, base01, base00,
/// base0 and base1 on dark; their mirror on light.
fn solarized(id: &str, label: &str, base: [Color; 6]) -> Theme {
    let s = &SOLARIZED;
    let [background, highlight, comment, _, body, emphasis] = base;
    Theme {
        id: id.into(),
        label: label.into(),
        fg: body,
        bg: background,
        muted: comment,
        accent: s.blue,
        border: comment,
        border_focus: s.blue,
        ok: s.green,
        warn: s.yellow,
        error: s.red,
        shared: s.magenta,
        paused: s.violet,
        selection_bg: highlight,
        selection_fg: emphasis,
        match_fg: background,
        match_bg: s.yellow,
        match_current_fg: background,
        match_current_bg: s.orange,
        prefixes: vec![s.cyan, s.violet, s.yellow, s.blue, s.green, s.orange],
    }
}

fn solarized_dark() -> Theme {
    solarized(
        "solarized-dark",
        "Solarized Dark",
        [
            hex(0x002b36),
            hex(0x073642),
            hex(0x586e75),
            hex(0x657b83),
            hex(0x839496),
            hex(0x93a1a1),
        ],
    )
}

fn solarized_light() -> Theme {
    solarized(
        "solarized-light",
        "Solarized Light",
        [
            hex(0xfdf6e3),
            hex(0xeee8d5),
            hex(0x93a1a1),
            hex(0x839496),
            hex(0x657b83),
            hex(0x586e75),
        ],
    )
}

// https://github.com/rose-pine/palette/blob/main/palette.json
// https://github.com/rose-pine/neovim/blob/main/lua/rose-pine/palette.lua
struct RosePine {
    base: Color,
    overlay: Color,
    subtle: Color,
    text: Color,
    love: Color,
    gold: Color,
    rose: Color,
    pine: Color,
    foam: Color,
    iris: Color,
    highlight_med: Color,
}

/// `muted` is Subtle: Rosé Pine's Muted is too faint on the selection.
fn rose_pine_theme(id: &str, label: &str, p: &RosePine) -> Theme {
    Theme {
        id: id.into(),
        label: label.into(),
        fg: p.text,
        bg: p.base,
        muted: p.subtle,
        accent: p.foam,
        border: p.highlight_med,
        border_focus: p.rose,
        ok: p.pine,
        warn: p.gold,
        error: p.love,
        shared: p.iris,
        paused: p.rose,
        selection_bg: p.overlay,
        selection_fg: p.subtle,
        match_fg: p.base,
        match_bg: p.gold,
        match_current_fg: p.base,
        match_current_bg: p.rose,
        prefixes: vec![p.foam, p.iris, p.gold, p.pine, p.rose, p.love],
    }
}

fn rose_pine() -> Theme {
    rose_pine_theme(
        "rose-pine",
        "Rosé Pine",
        &RosePine {
            base: hex(0x191724),
            overlay: hex(0x26233a),
            subtle: hex(0x908caa),
            text: hex(0xe0def4),
            love: hex(0xeb6f92),
            gold: hex(0xf6c177),
            rose: hex(0xebbcba),
            pine: hex(0x31748f),
            foam: hex(0x9ccfd8),
            iris: hex(0xc4a7e7),
            highlight_med: hex(0x403d52),
        },
    )
}

fn rose_pine_dawn() -> Theme {
    rose_pine_theme(
        "rose-pine-dawn",
        "Rosé Pine Dawn",
        &RosePine {
            base: hex(0xfaf4ed),
            overlay: hex(0xf2e9e1),
            subtle: hex(0x797593),
            text: hex(0x464261),
            love: hex(0xb4637a),
            gold: hex(0xea9d34),
            rose: hex(0xd7827e),
            pine: hex(0x286983),
            foam: hex(0x56949f),
            iris: hex(0x907aa9),
            highlight_med: hex(0xdfdad9),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_is_legible_in_truecolor_and_256_colours() {
        for colors in [Colors::Truecolor, Colors::Indexed] {
            for theme in builtins(colors) {
                let failures = theme.contrast_failures();
                assert!(failures.is_empty(), "{} {colors:?}: {failures:?}", theme.id);
            }
        }
    }

    #[test]
    fn status_colours_stay_distinct_and_coloured() {
        for colors in [Colors::Truecolor, Colors::Indexed] {
            for theme in builtins(colors).into_iter().skip(1) {
                let roles = [
                    ("ok", theme.ok),
                    ("warn", theme.warn),
                    ("error", theme.error),
                    ("shared", theme.shared),
                    ("paused", theme.paused),
                    ("accent", theme.accent),
                ];
                for (i, (name, color)) in roles.iter().enumerate() {
                    let lab = oklab(*color).unwrap();
                    assert!(
                        chroma(lab) >= GREY_CHROMA,
                        "{} {colors:?}: {name} {color:?} is grey",
                        theme.id
                    );
                    for (other, other_color) in &roles[i + 1..] {
                        assert_ne!(
                            color, other_color,
                            "{} {colors:?}: {name} and {other}",
                            theme.id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_colour_far_off_its_contrast_isnt_moved() {
        let theme = Theme {
            warn: hex(0xffffff),
            ..raw_builtins().remove(0)
        }
        .legible(Colors::Truecolor);
        assert_eq!(theme.warn, hex(0xffffff), "white on Latte's light base");
        assert!(!theme.contrast_failures().is_empty());
    }

    #[test]
    fn ansi_colours_are_left_alone() {
        let theme = Theme {
            fg: hex(0xc0caf5),
            bg: hex(0x1a1b26),
            selection_bg: hex(0x283457),
            ..Theme::terminal()
        }
        .legible(Colors::Truecolor);
        assert_eq!(theme.muted, Color::DarkGray);
        assert_eq!(theme.match_bg, Color::Reset);
        assert_eq!(theme.prefixes, Theme::terminal().prefixes);
    }

    #[test]
    fn a_reset_match_background_is_checked_against_bg() {
        let theme = Theme {
            fg: hex(0xffffff),
            bg: hex(0xffffff),
            selection_bg: hex(0xffffff),
            match_fg: hex(0xfefefe),
            match_bg: Color::Reset,
            ..Theme::terminal()
        };
        assert!(
            theme
                .contrast_failures()
                .iter()
                .any(|f| f.starts_with("match_fg on match_bg")),
            "{:?}",
            theme.contrast_failures()
        );
    }

    #[test]
    fn selected_lines_reverse_only_without_a_painted_background() {
        use ratatui::style::Modifier;
        assert!(
            Theme::terminal()
                .line_selection()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        let nord = named("nord", Colors::Truecolor).unwrap();
        assert_eq!(nord.line_selection(), Style::new().bg(nord.selection_bg));
    }

    #[test]
    fn ids_are_unique_and_terminal_comes_first() {
        let themes = builtins(Colors::Truecolor);
        assert_eq!(themes[0], Theme::terminal());
        let mut ids: Vec<&str> = themes.iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), themes.len());
    }

    #[test]
    fn contrast_follows_wcag() {
        let white = Color::Rgb(255, 255, 255);
        let black = Color::Rgb(0, 0, 0);
        assert_eq!(contrast(white, black).map(|r| r.round()), Some(21.0));
        assert_eq!(contrast(white, white), Some(1.0));
        assert_eq!(contrast(Color::Red, black), None);
    }

    #[test]
    fn without_truecolor_every_role_is_one_of_256() {
        let theme = named("catppuccin-mocha", Colors::Indexed).unwrap();
        let mut copy = theme.clone();
        assert!(copy.roles_mut().all(|c| matches!(c, Color::Indexed(16..))));
        assert_eq!(nearest_indexed(hex(0xffffff)), Color::Indexed(231));
        assert_eq!(nearest_indexed(hex(0x080808)), Color::Indexed(232));
        assert_eq!(Theme::terminal().indexed(), Theme::terminal());
    }

    #[test]
    fn reset_text_roles_take_fg() {
        let theme = Theme {
            fg: hex(0x123456),
            muted: Color::Reset,
            ..Theme::terminal()
        }
        .inherit();
        assert_eq!(theme.muted, hex(0x123456));
        assert_eq!(
            theme.match_bg,
            Color::Reset,
            "a background inherits as it is"
        );
    }
}
