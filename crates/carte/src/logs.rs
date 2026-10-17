//! Per-process log storage and escape-code stripping.

use std::collections::VecDeque;

/// Raw output of one process, capped at a number of bytes. When the cap is
/// exceeded the oldest bytes are dropped, up to the next line break so the
/// buffer starts on a line boundary.
#[derive(Debug, Clone)]
pub struct LogBuffer {
    buf: VecDeque<u8>,
    cap: usize,
    total: u64,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        LogBuffer {
            buf: VecDeque::new(),
            cap,
            total: 0,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.total += data.len() as u64;
        let skip = data.len().saturating_sub(self.cap);
        if skip > 0 {
            self.buf.clear();
            self.buf.extend(&data[skip..]);
            if data[skip - 1] != b'\n' {
                self.drop_partial_line();
            }
        } else {
            self.buf.extend(data);
            self.trim();
        }
    }

    /// Changes the cap, trimming right away if the buffer is over it.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
        self.trim();
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Bytes ever pushed, including those trimmed away.
    pub fn total_written(&self) -> u64 {
        self.total
    }

    pub fn contents(&self) -> Vec<u8> {
        let (a, b) = self.buf.as_slices();
        [a, b].concat()
    }

    /// The last `n` lines, stripped of escape codes.
    pub fn tail_lines(&self, n: usize) -> Vec<String> {
        let text = strip(&self.contents());
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(n)..]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    fn trim(&mut self) {
        if self.buf.len() <= self.cap {
            return;
        }
        let excess = self.buf.len() - self.cap;
        let at_boundary = self.buf[excess - 1] == b'\n';
        self.buf.drain(..excess);
        if !at_boundary {
            self.drop_partial_line();
        }
    }

    fn drop_partial_line(&mut self) {
        if let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            self.buf.drain(..=nl);
        }
    }
}

/// The local time of day of a timestamp in milliseconds since the Unix epoch, as
/// `HH:MM:SS`.
pub fn clock(at_ms: u64) -> String {
    let secs = (at_ms / 1000) as libc::time_t;
    // SAFETY: an all-zero `tm` is a valid value, and both pointers point at live locals.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return "--:--:--".into();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// A rule `width` columns wide with a marker's local time in the middle.
pub fn marker_rule(at_ms: u64, width: usize) -> String {
    let time = format!(" {} ", clock(at_ms));
    let rest = width.saturating_sub(time.chars().count());
    let left = rest / 2;
    format!("{}{time}{}", "─".repeat(left), "─".repeat(rest - left))
}

/// Strips terminal escape codes from a complete chunk of output.
pub fn strip(data: &[u8]) -> String {
    let mut stripper = Stripper::default();
    let mut out = String::new();
    for line in stripper.feed(data) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&stripper.pending());
    out
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    /// OSC, DCS, SOS, PM and APC: everything up to BEL or ST is dropped.
    String,
    StringEscape,
}

/// Turns raw terminal output into plain lines, incrementally.
///
/// CSI, OSC and other escape sequences are dropped; a carriage return moves
/// back to the start of the line so later text overwrites it, backspace moves
/// back one column, and `ESC [ K` erases to the end of the line. Other control
/// characters except tab are dropped. Invalid UTF-8 becomes U+FFFD.
#[derive(Debug, Default, Clone)]
pub struct Stripper {
    state: State,
    csi: Vec<u8>,
    line: Vec<char>,
    cursor: usize,
    utf8: Vec<u8>,
}

impl Stripper {
    /// Feeds bytes and returns the lines they complete.
    pub fn feed(&mut self, data: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &b in data {
            self.byte(b, &mut lines);
        }
        lines
    }

    /// The current incomplete line.
    pub fn pending(&self) -> String {
        self.line.iter().collect()
    }

    fn byte(&mut self, b: u8, lines: &mut Vec<String>) {
        match self.state {
            State::Ground => self.ground(b, lines),
            State::Escape => {
                self.state = match b {
                    b'[' => {
                        self.csi.clear();
                        State::Csi
                    }
                    b']' | b'P' | b'X' | b'^' | b'_' => State::String,
                    0x20..=0x2f => State::EscapeIntermediate,
                    _ => State::Ground,
                }
            }
            State::EscapeIntermediate => {
                if !(0x20..=0x2f).contains(&b) {
                    self.state = State::Ground;
                }
            }
            State::Csi => match b {
                0x40..=0x7e => {
                    if b == b'K' {
                        self.erase_line();
                    }
                    self.state = State::Ground;
                }
                0x20..=0x3f => self.csi.push(b),
                0x1b => self.state = State::Escape,
                _ => self.state = State::Ground,
            },
            State::String => match b {
                0x07 => self.state = State::Ground,
                0x1b => self.state = State::StringEscape,
                _ => {}
            },
            State::StringEscape => {
                self.state = if b == b'\\' {
                    State::Ground
                } else {
                    State::String
                };
            }
        }
    }

    fn ground(&mut self, b: u8, lines: &mut Vec<String>) {
        if !self.utf8.is_empty() || b >= 0x80 {
            self.utf8_byte(b, lines);
            return;
        }
        match b {
            0x1b => self.state = State::Escape,
            b'\n' => {
                lines.push(self.line.iter().collect());
                self.line.clear();
                self.cursor = 0;
            }
            b'\r' => self.cursor = 0,
            0x08 => self.cursor = self.cursor.saturating_sub(1),
            b'\t' | 0x20..=0x7e => self.put(b as char),
            _ => {}
        }
    }

    fn utf8_byte(&mut self, b: u8, lines: &mut Vec<String>) {
        if !self.utf8.is_empty() && b & 0xc0 != 0x80 {
            self.utf8.clear();
            self.put(char::REPLACEMENT_CHARACTER);
            self.ground(b, lines);
            return;
        }
        self.utf8.push(b);
        let expected = match self.utf8[0] {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        if self.utf8.len() < expected {
            return;
        }
        let c = std::str::from_utf8(&self.utf8)
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        self.utf8.clear();
        self.put(c);
    }

    fn put(&mut self, c: char) {
        if self.cursor < self.line.len() {
            self.line[self.cursor] = c;
        } else {
            self.line.push(c);
        }
        self.cursor += 1;
    }

    fn erase_line(&mut self) {
        match self.csi.as_slice() {
            b"" | b"0" => self.line.truncate(self.cursor),
            b"1" => {
                let end = self.cursor.min(self.line.len());
                self.line[..end].fill(' ');
            }
            b"2" => self.line.clear(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_rules_centre_the_time() {
        let rule = marker_rule(1_760_000_000_000, 30);
        let time = clock(1_760_000_000_000);
        assert_eq!(rule.chars().count(), 30, "{rule}");
        assert_eq!(rule, format!("────────── {time} ──────────"));
        assert_eq!(marker_rule(1_760_000_000_000, 4), format!(" {time} "));
    }

    #[test]
    fn clock_shows_the_time_of_day() {
        let time = clock(1_760_000_000_000);
        assert_eq!(time.len(), 8, "{time}");
        assert_eq!(time.matches(':').count(), 2, "{time}");
        assert_eq!(clock(1_760_000_000_999), time);
    }

    #[test]
    fn buffer_keeps_everything_under_the_cap() {
        let mut buf = LogBuffer::new(100);
        buf.push(b"one\n");
        buf.push(b"two\n");
        assert_eq!(buf.contents(), b"one\ntwo\n");
        assert_eq!(buf.total_written(), 8);
    }

    #[test]
    fn buffer_trims_to_a_line_boundary() {
        let mut buf = LogBuffer::new(10);
        buf.push(b"first line\nsecond\nthird\n");
        assert_eq!(buf.contents(), b"third\n");
        assert_eq!(buf.total_written(), 24);
    }

    #[test]
    fn buffer_trims_mid_line_without_a_newline() {
        let mut buf = LogBuffer::new(4);
        buf.push(b"abcdefgh");
        assert_eq!(buf.contents(), b"efgh");
    }

    #[test]
    fn buffer_keeps_a_whole_line_cut_at_its_start() {
        let mut buf = LogBuffer::new(10);
        buf.push(b"aaaa\n");
        buf.push(b"bbbb\n");
        buf.push(b"cccc\n");
        assert_eq!(buf.contents(), b"bbbb\ncccc\n");
    }

    #[test]
    fn buffer_shrinks_when_the_cap_is_lowered() {
        let mut buf = LogBuffer::new(100);
        buf.push(b"aaaa\nbbbb\ncccc\n");
        buf.set_cap(6);
        assert_eq!(buf.contents(), b"cccc\n");
    }

    #[test]
    fn tail_lines_are_stripped() {
        let mut buf = LogBuffer::new(100);
        buf.push(b"\x1b[32mone\x1b[0m\r\ntwo\r\nthree\r\n");
        assert_eq!(buf.tail_lines(2), ["two", "three"]);
    }

    #[test]
    fn strips_csi_colours() {
        assert_eq!(strip(b"\x1b[1;31mred\x1b[0m plain"), "red plain");
    }

    #[test]
    fn strips_osc_with_bel_and_st() {
        assert_eq!(
            strip(b"\x1b]0;title\x07a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\b"),
            "alinkb"
        );
    }

    #[test]
    fn strips_charset_and_two_byte_escapes() {
        assert_eq!(strip(b"\x1b(Bx\x1b=y\x1b7z"), "xyz");
    }

    #[test]
    fn pty_line_endings_become_plain_newlines() {
        assert_eq!(strip(b"one\r\ntwo\r\n"), "one\ntwo\n");
    }

    #[test]
    fn carriage_return_overwrites() {
        assert_eq!(strip(b"progress 10%\rprogress 100%\n"), "progress 100%\n");
        assert_eq!(strip(b"abcdef\rXY\n"), "XYcdef\n");
    }

    #[test]
    fn erase_line_after_carriage_return() {
        assert_eq!(strip(b"long status\r\x1b[Kok\n"), "ok\n");
        assert_eq!(strip(b"long status\r\x1b[2Kok\n"), "ok\n");
    }

    #[test]
    fn backspace_moves_back() {
        assert_eq!(strip(b"abc\x08\x08X\n"), "aXc\n");
    }

    #[test]
    fn drops_other_control_characters() {
        assert_eq!(strip(b"a\x07b\x00c\td"), "abc\td");
    }

    #[test]
    fn keeps_utf8_split_across_feeds() {
        let mut s = Stripper::default();
        let bytes = "blåbær\n".as_bytes();
        let mut lines = s.feed(&bytes[..3]);
        lines.extend(s.feed(&bytes[3..]));
        assert_eq!(lines, ["blåbær"]);
    }

    #[test]
    fn escape_split_across_feeds() {
        let mut s = Stripper::default();
        assert!(s.feed(b"a\x1b[3").is_empty());
        assert_eq!(s.feed(b"1mb\n"), ["ab"]);
    }

    #[test]
    fn invalid_utf8_becomes_replacement() {
        assert_eq!(strip(b"a\xffb\xc3c"), "a\u{fffd}b\u{fffd}c");
    }
}
