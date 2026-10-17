//! The log pane's buffer: the lines followed for the selected node, the local filter
//! and the scroll position.

use std::collections::VecDeque;

use carte_api::LogLine;

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
        match self {
            Prefix::None => None,
            Prefix::Process => Some(line.process.clone()),
            Prefix::AppProcess => Some(format!("{}/{}", line.app, line.process)),
            Prefix::Full => Some(format!("{}@{}/{}", line.app, line.stack, line.process)),
        }
    }
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
    pub filter: String,
    /// Lines above the bottom; 0 follows new lines.
    pub scroll: usize,
    pub ended: Option<String>,
    /// Rows the pane showed last time it was drawn.
    pub height: usize,
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
            filter: String::new(),
            scroll: 0,
            ended: None,
            height: 0,
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
        self.hold = false;
        self.labels.clear();
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
        if (self.scroll > 0 || self.hold) && self.matches(&line) {
            self.scroll += 1;
        }
        self.lines.push_back(line);
        if self.lines.len() > CAPACITY {
            self.lines.pop_front();
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

    fn matches(&self, line: &LogLine) -> bool {
        self.filter.is_empty()
            || line
                .text
                .to_lowercase()
                .contains(&self.filter.to_lowercase())
    }

    pub fn visible(&self) -> Vec<&LogLine> {
        self.lines.iter().filter(|l| self.matches(l)).collect()
    }

    /// Visible lines with their sequence numbers.
    fn numbered(&self) -> Vec<(u64, &LogLine)> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| self.matches(l))
            .map(|(i, l)| (self.dropped + i as u64, l))
            .collect()
    }

    /// The lines that fit in `height` rows at the current scroll position, with their
    /// sequence numbers.
    pub fn window_numbered(&self, height: usize) -> Vec<(u64, &LogLine)> {
        let visible = self.numbered();
        let scroll = self.scroll.min(visible.len().saturating_sub(height));
        let end = visible.len() - scroll;
        visible[end.saturating_sub(height)..end].to_vec()
    }

    #[cfg(test)]
    pub fn window(&self, height: usize) -> Vec<&LogLine> {
        self.window_numbered(height)
            .into_iter()
            .map(|(_, line)| line)
            .collect()
    }

    /// The sequence number of the line on a row of the pane, or of the last line when
    /// the row is below it.
    pub fn seq_at(&self, row: usize) -> Option<u64> {
        let window = self.window_numbered(self.height);
        window.get(row).or(window.last()).map(|(seq, _)| *seq)
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
                Some(label) => format!("{label} {}", line.text),
                None => line.text.clone(),
            })
            .collect();
        (lines.join("\n"), lines.len())
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.visible().len().saturating_sub(self.height.max(1));
        self.scroll = self.scroll.min(max).saturating_add_signed(delta).min(max);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = self.visible().len().saturating_sub(self.height.max(1));
    }

    pub fn follow(&mut self) {
        self.scroll = 0;
    }

    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.scroll = 0;
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
            project: "make".into(),
            app: "make-billing".into(),
            stack: "main".into(),
            process: "web".into(),
            at_ms: 0,
            text: text.into(),
        }
    }

    fn texts(lines: Vec<&LogLine>) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
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
    fn filters_case_insensitively() {
        let mut logs = Logs::default();
        let generation = logs.restart(None, Prefix::Process);
        logs.push(generation, line("GET /up"));
        logs.push(generation, line("Booting"));
        logs.set_filter("get".into());
        assert_eq!(texts(logs.visible()), ["GET /up"]);
    }
}
