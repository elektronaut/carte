//! The log pane's buffer: the lines followed for the selected node, the search or
//! filter and the scroll position.

use std::collections::VecDeque;

use carte_api::LogLine;
use unicode_width::UnicodeWidthStr;

/// What a line is prefixed with, which depends on how wide the merged view is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefix {
    None,
    Process,
    AppProcess,
    Full,
}

impl Prefix {
    pub fn label(self, line: &LogLine) -> Option<String> {
        if line.marker {
            return None;
        }
        match self {
            Prefix::None => None,
            Prefix::Process => Some(line.process.clone()),
            Prefix::AppProcess => Some(format!("{}/{}", line.app, line.process)),
            Prefix::Full => Some(format!("{}@{}/{}", line.app, line.stack, line.process)),
        }
    }
}

/// The lines a pane of some height shows.
#[derive(Debug)]
pub struct Window<'a> {
    pub lines: Vec<(u64, &'a LogLine)>,
    /// Rows of the oldest line that don't fit above the pane.
    pub skip: usize,
    /// Columns before each line's text, which its wrapped rows start at too.
    pub indent: usize,
}

/// Text in lower case one character at a time, so each folded character stands for
/// one character of the original.
pub fn fold(text: &str) -> Vec<char> {
    text.chars().map(fold_char).collect()
}

/// A character's lower case, or the first character of it when it takes several,
/// like `İ`. A final sigma folds like any other.
fn fold_char(c: char) -> char {
    match c.to_lowercase().next().unwrap_or(c) {
        'ς' => 'σ',
        lower => lower,
    }
}

/// The byte range of the first match of `needle`, folded with `fold`, in `text` at or
/// after byte `from`, ignoring case. It walks `text` itself, so the range always
/// falls on its character boundaries.
pub fn find(text: &str, needle: &[char], from: usize) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let rest = &text[from..];
    rest.char_indices().find_map(|(start, _)| {
        let mut chars = rest[start..].char_indices();
        for &wanted in needle {
            let (_, c) = chars.next()?;
            if fold_char(c) != wanted {
                return None;
            }
        }
        let end = chars.next().map_or(rest.len(), |(i, _)| start + i);
        Some((from + start, from + end))
    })
}

#[derive(Debug)]
pub struct Logs {
    pub generation: u64,
    /// The address the stream follows; a new one means a new stream.
    pub key: Option<String>,
    pub prefix: Prefix,
    lines: VecDeque<LogLine>,
    /// Lines dropped from the front so far; a line's sequence number is this plus its
    /// index, which stays put while lines come and go.
    dropped: u64,
    /// Prefix labels in order of appearance, which picks their colours.
    labels: Vec<String>,
    /// Text to highlight; every line stays visible unless `filter` is on.
    pub search: String,
    /// Hide the lines that don't match the search.
    pub filter: bool,
    /// The sequence numbers of the lines that match the search, oldest first.
    matched: VecDeque<u64>,
    /// The match `n` and `N` last moved to, by sequence number.
    pub current: Option<u64>,
    /// Lines above the bottom; 0 follows new lines.
    pub scroll: usize,
    pub ended: Option<String>,
    /// Rows the pane showed last time it was drawn.
    pub height: usize,
    /// Columns the pane showed last time it was drawn, which long lines wrap at; 0
    /// before the first draw.
    pub width: usize,
    /// Keep the view still while lines arrive, even when following.
    pub hold: bool,
}

const CAPACITY: usize = 5000;

impl Default for Logs {
    fn default() -> Self {
        Logs {
            generation: 0,
            key: None,
            prefix: Prefix::None,
            lines: VecDeque::new(),
            dropped: 0,
            labels: Vec::new(),
            search: String::new(),
            filter: false,
            matched: VecDeque::new(),
            current: None,
            scroll: 0,
            ended: None,
            height: 0,
            width: 0,
            hold: false,
        }
    }
}

impl Logs {
    /// Starts over for a new stream and returns its generation.
    pub fn restart(&mut self, key: Option<String>, prefix: Prefix) -> u64 {
        self.generation += 1;
        self.key = key;
        self.prefix = prefix;
        self.dropped += self.lines.len() as u64;
        self.lines.clear();
        self.matched.clear();
        self.hold = false;
        self.labels.clear();
        self.current = None;
        self.scroll = 0;
        self.ended = None;
        self.generation
    }

    pub fn push(&mut self, generation: u64, line: LogLine) {
        if generation != self.generation {
            return;
        }
        if let Some(label) = self.prefix.label(&line)
            && !self.labels.contains(&label)
        {
            self.labels.push(label);
        }
        let seq = self.dropped + self.lines.len() as u64;
        if self.matches(&line, &fold(&self.search)) {
            self.matched.push_back(seq);
        }
        if (self.scroll > 0 || self.hold) && self.shown(seq, &line) {
            self.scroll += 1;
        }
        self.lines.push_back(line);
        if self.lines.len() > CAPACITY {
            self.lines.pop_front();
            if self.matched.front() == Some(&self.dropped) {
                self.matched.pop_front();
            }
            self.dropped += 1;
        }
    }

    pub fn end(&mut self, generation: u64, error: Option<String>) {
        if generation == self.generation {
            self.ended = Some(error.unwrap_or_else(|| "the stream ended".into()));
        }
    }

    pub fn color_index(&self, label: &str) -> usize {
        self.labels.iter().position(|l| l == label).unwrap_or(0)
    }

    /// Whether a line holds `needle`, the search folded with `fold`.
    fn matches(&self, line: &LogLine, needle: &[char]) -> bool {
        !line.marker && find(&line.text, needle, 0).is_some()
    }

    fn filtering(&self) -> bool {
        self.filter && !self.search.is_empty()
    }

    /// Whether the filter keeps a line. Markers always stay.
    fn shown(&self, seq: u64, line: &LogLine) -> bool {
        !self.filtering() || line.marker || self.matched.binary_search(&seq).is_ok()
    }

    #[cfg(test)]
    pub fn visible(&self) -> Vec<&LogLine> {
        self.numbered().into_iter().map(|(_, l)| l).collect()
    }

    /// The lines the filter keeps, with their sequence numbers.
    fn numbered(&self) -> Vec<(u64, &LogLine)> {
        self.lines
            .iter()
            .enumerate()
            .map(|(i, l)| (self.dropped + i as u64, l))
            .filter(|&(seq, l)| self.shown(seq, l))
            .collect()
    }

    /// How many lines the filter keeps.
    fn shown_count(&self) -> usize {
        if self.filtering() {
            self.matched.len() + self.lines.iter().filter(|l| l.marker).count()
        } else {
            self.lines.len()
        }
    }

    /// The sequence numbers of the lines that match the search, oldest first.
    pub fn match_seqs(&self) -> Vec<u64> {
        self.matched.iter().copied().collect()
    }

    /// Moves to the next older match (`older`) or the next newer one, starting from
    /// the newest, and scrolls it into the middle of the pane. False when nothing matches.
    pub fn find(&mut self, older: bool) -> bool {
        let seqs = self.match_seqs();
        let next = match (self.current, older) {
            (None, _) => seqs.last(),
            (Some(at), true) => seqs.iter().rev().find(|&&s| s < at).or(seqs.last()),
            (Some(at), false) => seqs.iter().find(|&&s| s > at).or(seqs.first()),
        };
        let Some(&seq) = next else {
            self.current = None;
            return false;
        };
        self.current = Some(seq);
        let shown = self.numbered();
        let index = shown.iter().position(|&(s, _)| s == seq).unwrap_or(0);
        let below = shown.len() - 1 - index;
        self.scroll = below.saturating_sub(self.height / 2);
        self.scroll_by(0);
        true
    }

    /// The lines that fill `height` rows at the current scroll position, long lines
    /// wrapped at the pane's width, ending with the newest one shown.
    pub fn window_rows(&self, height: usize) -> Window<'_> {
        let visible = self.numbered();
        let scroll = self.scroll.min(visible.len().saturating_sub(height));
        let end = visible.len() - scroll;
        let candidates = &visible[end.saturating_sub(height)..end];
        let indent = self.indent(candidates);
        let mut rows = 0;
        let mut start = end;
        while start > end.saturating_sub(height) && rows < height {
            start -= 1;
            rows += self.rows(visible[start].1, indent);
        }
        Window {
            lines: visible[start..end].to_vec(),
            skip: rows.saturating_sub(height),
            indent,
        }
    }

    /// The columns before a line's text: its prefix label, padded to the widest in
    /// the window, and a space.
    fn indent(&self, lines: &[(u64, &LogLine)]) -> usize {
        lines
            .iter()
            .filter_map(|(_, line)| self.prefix.label(line))
            .map(|label| label.width() + 1)
            .max()
            .unwrap_or(0)
    }

    /// How many rows a line takes in the pane, wrapped after `indent` columns.
    pub fn rows(&self, line: &LogLine, indent: usize) -> usize {
        if line.marker || self.width == 0 {
            return 1;
        }
        let room = self.width.saturating_sub(indent).max(1);
        line.text.width().div_ceil(room).max(1)
    }

    #[cfg(test)]
    pub fn window(&self, height: usize) -> Vec<&LogLine> {
        self.window_rows(height)
            .lines
            .into_iter()
            .map(|(_, line)| line)
            .collect()
    }

    /// The sequence number of the line on a row of the pane, or of the last line when
    /// the row is below it.
    pub fn seq_at(&self, row: usize) -> Option<u64> {
        let window = self.window_rows(self.height);
        let mut row = row + window.skip;
        for (seq, line) in &window.lines {
            let rows = self.rows(line, window.indent);
            if row < rows {
                return Some(*seq);
            }
            row -= rows;
        }
        window.lines.last().map(|(seq, _)| *seq)
    }

    /// The visible lines between two sequence numbers, inclusive, as plain text with
    /// their prefixes.
    pub fn text_between(&self, a: u64, b: u64) -> (String, usize) {
        let range = a.min(b)..=a.max(b);
        let lines: Vec<String> = self
            .numbered()
            .into_iter()
            .filter(|(seq, _)| range.contains(seq))
            .map(|(_, line)| match self.prefix.label(line) {
                _ if line.marker => crate::logs::marker_rule(line.at_ms, 40),
                Some(label) => format!("{label} {}", line.text),
                None => line.text.clone(),
            })
            .collect();
        (lines.join("\n"), lines.len())
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.shown_count().saturating_sub(self.height.max(1));
        self.scroll = self.scroll.min(max).saturating_add_signed(delta).min(max);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = self.shown_count().saturating_sub(self.height.max(1));
    }

    pub fn follow(&mut self) {
        self.scroll = 0;
    }

    /// Searches for `search` from the newest line.
    pub fn set_search(&mut self, search: String) {
        self.search = search;
        let needle = fold(&self.search);
        self.matched = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| self.matches(l, &needle))
            .map(|(i, _)| self.dropped + i as u64)
            .collect();
        self.current = None;
        if !self.find(true) {
            self.scroll = 0;
        }
    }

    /// Switches between highlighting the matches and hiding the lines without one.
    pub fn toggle_filter(&mut self) {
        self.filter = !self.filter;
        let search = std::mem::take(&mut self.search);
        self.set_search(search);
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> LogLine {
        LogLine {
            project: "nova".into(),
            app: "nova-billing".into(),
            stack: "main".into(),
            process: "web".into(),
            at_ms: 0,
            text: text.into(),
            marker: false,
        }
    }

    fn texts(lines: Vec<&LogLine>) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn long_lines_wrap_and_fill_the_window_from_the_bottom() {
        let mut logs = Logs::default();
        let generation = logs.restart(None, Prefix::Process);
        logs.width = 14;
        for text in ["one", "a line of twenty-one!", "two", "three"] {
            logs.push(generation, line(text));
        }
        // "web " takes 4 columns, leaving 10 per row: the long line takes 3 rows.
        let window = logs.window_rows(5);
        assert_eq!(window.indent, 4);
        assert_eq!(
            texts(window.lines.iter().map(|(_, l)| *l).collect()),
            ["a line of twenty-one!", "two", "three"]
        );
        assert_eq!(window.skip, 0);
        let window = logs.window_rows(4);
        assert_eq!(
            window.skip, 1,
            "the long line's first row is above the pane"
        );
        assert_eq!(logs.rows(&line("a line of twenty-one!"), 4), 3);
        assert_eq!(logs.rows(&line(""), 4), 1);

        logs.height = 4;
        let seq = |text: &str| {
            logs.numbered()
                .into_iter()
                .find(|(_, l)| l.text == text)
                .map(|(seq, _)| seq)
        };
        assert_eq!(logs.seq_at(0), seq("a line of twenty-one!"));
        assert_eq!(logs.seq_at(1), seq("a line of twenty-one!"));
        assert_eq!(logs.seq_at(2), seq("two"));
        assert_eq!(logs.seq_at(3), seq("three"));
        assert_eq!(logs.seq_at(9), seq("three"));
    }

    #[test]
    fn markers_never_match_a_search_and_have_no_label() {
        let mut logs = Logs::default();
        let generation = logs.restart(None, Prefix::Process);
        logs.push(generation, line("GET /up"));
        logs.push(
            generation,
            LogLine {
                process: String::new(),
                marker: true,
                ..line("")
            },
        );
        logs.set_search("e".into());
        assert_eq!(logs.match_seqs().len(), 1);
        let visible = logs.visible();
        assert_eq!(visible.len(), 2);
        assert!(visible[1].marker);
        assert_eq!(Prefix::Process.label(visible[1]), None);
    }

    #[test]
    fn drops_lines_from_old_streams() {
        let mut logs = Logs::default();
        let old = logs.restart(None, Prefix::None);
        let new = logs.restart(None, Prefix::None);
        logs.push(old, line("stale"));
        logs.push(new, line("fresh"));
        assert_eq!(texts(logs.visible()), ["fresh"]);
    }

    #[test]
    fn scrolling_up_holds_the_view_while_lines_arrive() {
        let mut logs = Logs::default();
        let generation = logs.restart(None, Prefix::None);
        for i in 0..10 {
            logs.push(generation, line(&format!("{i}")));
        }
        logs.height = 3;
        assert_eq!(texts(logs.window(3)), ["7", "8", "9"]);
        logs.scroll_by(2);
        assert_eq!(texts(logs.window(3)), ["5", "6", "7"]);
        logs.push(generation, line("10"));
        assert_eq!(texts(logs.window(3)), ["5", "6", "7"]);
        logs.follow();
        assert_eq!(texts(logs.window(3)), ["8", "9", "10"]);
        logs.scroll_to_top();
        assert_eq!(texts(logs.window(3)), ["0", "1", "2"]);
    }

    #[test]
    fn selections_keep_their_lines_while_lines_come_and_go() {
        let mut logs = Logs::default();
        let generation = logs.restart(None, Prefix::Process);
        for i in 0..CAPACITY {
            logs.push(generation, line(&format!("{i}")));
        }
        logs.height = 3;
        logs.hold = true;
        let (first, last) = (logs.seq_at(0).unwrap(), logs.seq_at(1).unwrap());
        logs.push(generation, line("new"));
        assert_eq!(
            texts(logs.window(3)),
            ["4997", "4998", "4999"],
            "a hold keeps the view still"
        );
        assert_eq!(
            logs.text_between(last, first),
            ("web 4997\nweb 4998".to_string(), 2)
        );
        assert_eq!(
            logs.seq_at(10),
            logs.seq_at(2),
            "below the lines is the last"
        );
    }

    #[test]
    fn search_keeps_every_line_and_steps_through_matches() {
        let mut logs = Logs::default();
        let generation = logs.restart(None, Prefix::Process);
        for i in 0..20 {
            let text = if i % 5 == 0 {
                format!("GET /up {i}")
            } else {
                format!("{i}")
            };
            logs.push(generation, line(&text));
        }
        logs.height = 3;
        logs.set_search("get".into());
        assert_eq!(logs.visible().len(), 20, "nothing is filtered away");
        assert_eq!(logs.match_seqs(), [0, 5, 10, 15]);
        assert_eq!(logs.current, Some(15), "starts at the newest");
        assert_eq!(texts(logs.window(3)), ["14", "GET /up 15", "16"]);
        assert!(logs.find(true));
        assert_eq!(logs.current, Some(10));
        assert_eq!(texts(logs.window(3)), ["9", "GET /up 10", "11"]);
        assert!(logs.find(false));
        assert_eq!(logs.current, Some(15));
        assert!(logs.find(false), "wraps around");
        assert_eq!(logs.current, Some(0));
        assert_eq!(texts(logs.window(3)), ["GET /up 0", "1", "2"]);

        logs.set_search("nothing".into());
        assert_eq!(logs.current, None);
        assert_eq!(logs.scroll, 0);
    }

    #[test]
    fn matching_ignores_case_beyond_ascii() {
        for (text, search) in [
            ("BLÅBÆRSYLTETØY", "blåbærsyltetøy"),
            ("Ærlig talt", "ær"),
            ("ØST", "øst"),
            ("Ålesund", "å"),
            ("ΣΩΜΑ", "σωμα"),
            ("σῶμα", "Σ"),
            ("ΟΔΟΣ", "οδος"),
            ("οδος", "ΟΔΟΣ"),
            ("İstanbul", "istanbul"),
            ("İSTANBUL", "İs"),
        ] {
            assert!(find(text, &fold(search), 0).is_some(), "{search} in {text}");
        }
        assert_eq!(find("xÆøy", &fold("æØ"), 0), Some((1, 5)), "byte offsets");
        assert_eq!(find("æ æ", &fold("Æ"), 2), Some((3, 5)));
        assert_eq!(
            find("aİb", &fold("i"), 0),
            Some((1, 3)),
            "İ stays one character"
        );
        assert_eq!(find("abc", &fold("d"), 0), None);
        assert_eq!(find("abc", &[], 0), None);

        let mut logs = Logs::default();
        let generation = logs.restart(None, Prefix::None);
        logs.push(generation, line("Kjører på Østlandet"));
        logs.push(generation, line("ingen treff"));
        logs.set_search("ØSTLANDET".into());
        assert_eq!(logs.match_seqs(), [0]);
        logs.push(generation, line("østlandet igjen"));
        assert_eq!(logs.match_seqs(), [0, 2], "lines that arrive match too");
    }

    #[test]
    fn the_filter_hides_lines_without_a_match() {
        let mut logs = Logs::default();
        let generation = logs.restart(None, Prefix::Process);
        for i in 0..20 {
            let text = if i % 5 == 0 {
                format!("GET /up {i}")
            } else {
                format!("{i}")
            };
            logs.push(generation, line(&text));
        }
        logs.push(
            generation,
            LogLine {
                marker: true,
                ..line("")
            },
        );
        logs.height = 2;
        logs.set_search("get".into());
        logs.toggle_filter();
        assert!(logs.filter);
        assert_eq!(
            texts(logs.visible()),
            ["GET /up 0", "GET /up 5", "GET /up 10", "GET /up 15", ""],
            "markers stay"
        );
        assert_eq!(logs.current, Some(15));
        logs.scroll_to_top();
        assert_eq!(logs.scroll, 3);
        assert_eq!(texts(logs.window(2)), ["GET /up 0", "GET /up 5"]);
        logs.push(generation, line("21"));
        assert_eq!(logs.scroll, 3, "a hidden line doesn't move the view");
        logs.push(generation, line("GET /up 22"));
        assert_eq!(logs.scroll, 4);
        assert_eq!(texts(logs.window(2)), ["GET /up 0", "GET /up 5"]);

        assert!(logs.find(true));
        assert_eq!(logs.current, Some(10), "n steps over hidden lines");
        logs.hold = true;
        let (first, last) = (logs.seq_at(0).unwrap(), logs.seq_at(1).unwrap());
        assert_eq!(
            logs.text_between(first, last),
            ("web GET /up 10\nweb GET /up 15".to_string(), 2),
            "a copy holds only the shown lines"
        );
        logs.push(generation, line("23"));
        logs.push(generation, line("get 24"));
        assert_eq!(seq_texts(&logs, first, last), ["GET /up 10", "GET /up 15"]);
        logs.hold = false;

        logs.toggle_filter();
        assert_eq!(logs.visible().len(), 25);
        logs.filter = true;
        logs.set_search(String::new());
        assert_eq!(logs.visible().len(), 25, "no search, nothing hidden");
    }

    /// The text of the lines a held view shows on its first two rows.
    fn seq_texts(logs: &Logs, first: u64, last: u64) -> Vec<String> {
        assert_eq!(logs.seq_at(0), Some(first), "the view held still");
        assert_eq!(logs.seq_at(1), Some(last));
        logs.window(2).iter().map(|l| l.text.clone()).collect()
    }
}
