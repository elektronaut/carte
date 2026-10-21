//! Per-process log storage.

use std::collections::VecDeque;

pub use carte_core::ansi::{Stripper, strip};

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

    /// The raw bytes of the last `n` lines, an unfinished last line counting as one.
    pub fn last_lines(&self, n: usize) -> Vec<u8> {
        let mut lines = 0;
        let mut start = 0;
        for (i, &b) in self.buf.iter().enumerate().rev() {
            if b == b'\n' && i + 1 < self.buf.len() {
                lines += 1;
                if lines == n {
                    start = i + 1;
                    break;
                }
            }
        }
        if n == 0 {
            return Vec::new();
        }
        self.buf.range(start..).copied().collect()
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
    // SAFETY: an all-zero `tm` is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers point at live locals.
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
    fn last_lines_are_raw_and_count_an_unfinished_line() {
        let mut buf = LogBuffer::new(100);
        buf.push(b"one\r\ntwo\r\n\x1b[1m(rdbg)\x1b[0m ");
        assert_eq!(buf.last_lines(2), b"two\r\n\x1b[1m(rdbg)\x1b[0m ");
        assert_eq!(buf.last_lines(10), buf.contents());
        assert_eq!(buf.last_lines(0), b"");
        buf.push(b"\n");
        assert_eq!(buf.last_lines(1), b"\x1b[1m(rdbg)\x1b[0m \n");
    }

    #[test]
    fn tail_lines_are_stripped() {
        let mut buf = LogBuffer::new(100);
        buf.push(b"\x1b[32mone\x1b[0m\r\ntwo\r\nthree\r\n");
        assert_eq!(buf.tail_lines(2), ["two", "three"]);
    }
}
