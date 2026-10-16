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

/// The selection a stream follows; a new key means a new stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub address: String,
    /// Instances a merged stream covers. The daemon fixes them when the stream
    /// starts, so a stack whose instances change needs a new one.
    pub instances: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct Logs {
    pub generation: u64,
    pub key: Option<Key>,
    pub prefix: Prefix,
    lines: VecDeque<LogLine>,
    /// Prefix labels in order of appearance, which picks their colours.
    labels: Vec<String>,
    pub filter: String,
    /// Lines above the bottom; 0 follows new lines.
    pub scroll: usize,
    pub ended: Option<String>,
    /// Rows the pane showed last time it was drawn.
    pub height: usize,
}

const CAPACITY: usize = 5000;

impl Default for Logs {
    fn default() -> Self {
        Logs {
            generation: 0,
            key: None,
            prefix: Prefix::None,
            lines: VecDeque::new(),
            labels: Vec::new(),
            filter: String::new(),
            scroll: 0,
            ended: None,
            height: 0,
        }
    }
}

impl Logs {
    /// Starts over for a new stream and returns its generation.
    pub fn restart(&mut self, key: Option<Key>, prefix: Prefix) -> u64 {
        self.generation += 1;
        self.key = key;
        self.prefix = prefix;
        self.lines.clear();
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
        if self.scroll > 0 && self.matches(&line) {
            self.scroll += 1;
        }
        self.lines.push_back(line);
        if self.lines.len() > CAPACITY {
            self.lines.pop_front();
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

    /// The lines that fit in `height` rows at the current scroll position.
    pub fn window(&self, height: usize) -> Vec<&LogLine> {
        let visible = self.visible();
        let scroll = self.scroll.min(visible.len().saturating_sub(height));
        let end = visible.len() - scroll;
        visible[end.saturating_sub(height)..end].to_vec()
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
    fn filters_case_insensitively() {
        let mut logs = Logs::default();
        let generation = logs.restart(None, Prefix::Process);
        logs.push(generation, line("GET /up"));
        logs.push(generation, line("Booting"));
        logs.set_filter("get".into());
        assert_eq!(texts(logs.visible()), ["GET /up"]);
    }
}
