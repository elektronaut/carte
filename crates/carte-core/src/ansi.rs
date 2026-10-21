//! Turning raw terminal output into plain text.

use std::collections::VecDeque;

/// Strips terminal escape codes from a complete chunk of output.
pub fn strip(data: &[u8]) -> String {
    let mut stripper = Stripper::default();
    let mut out = String::new();
    let mut lines = stripper.feed(data);
    lines.extend(stripper.close());
    for line in lines {
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
    /// OSC, DCS, SOS, PM and APC, `osc` for OSC: dropped up to BEL, ST, CAN, SUB
    /// or another escape, an OSC up to a newline or carriage return too, and the
    /// others up to a newline after [`MAX_STRING`] bytes.
    String {
        osc: bool,
    },
    StringEscape,
    /// The first byte of U+009C, ST, inside a string.
    StringC2 {
        osc: bool,
    },
}

/// How many bytes a DCS, SOS, PM or APC string drops before a newline ends it.
const MAX_STRING: usize = 1024 * 1024;

/// The farthest right a cursor movement takes the cursor.
const MAX_COLUMN: usize = 4096;

/// The longest a line gets, in characters, before it wraps.
pub const MAX_LINE: usize = 16 * 1024;

/// The most CSI parameter bytes kept; the rest are dropped.
const MAX_CSI: usize = 32;

/// Turns raw terminal output into plain lines, incrementally.
///
/// CSI, OSC and other escape sequences are dropped; a carriage return moves
/// back to the start of the line so later text overwrites it, backspace and
/// `ESC [ n D` move back, `ESC [ n C` forward and `ESC [ n G` to a column, and
/// `ESC [ K` erases to the end of the line.
///
/// Rows a newline leaves stay open, as a small screen that `ESC [ A`, `ESC [ B`
/// and newlines move around in, until text is written on a row the cursor
/// reached by a newline: then the rows above it are lines. Text written on rows
/// reached by `ESC [ B`, like a completion dialog, finishes nothing. So Reline
/// making room below its prompt, or drawing a dialog there, leaves the prompt line
/// unfinished however its output is split into reads. [`Stripper::settle`] ends
/// the open rows above the cursor, for when output has gone quiet, but a move up
/// opens them again; more than a screen of open rows ends the oldest. So a settle
/// in the middle of a redraw can repeat lines, or end rows left on the screen,
/// like a completion menu's, half-typed input or blank rows made room with. Text
/// written after moving up past those rows is dropped until the cursor is back,
/// so redraws of earlier rows, like a multi-line progress bar, don't reach the
/// log, and a move up or down of more than a screen counts as one. Other control
/// characters except tab are dropped, C1 controls too. Invalid UTF-8 becomes
/// U+FFFD. A line longer than [`MAX_LINE`] wraps.
///
/// An escape sequence cut short by a control character or by text it can't hold
/// ends there, so a stray ESC loses nothing after it. An OSC, DCS, SOS, PM or APC
/// string also ends at CAN, SUB, U+009C or any other escape sequence, an OSC at a
/// newline or carriage return, and any other at a newline once it is longer than
/// [`MAX_STRING`].
#[derive(Debug, Default, Clone)]
pub struct Stripper {
    state: State,
    csi: Vec<u8>,
    /// Bytes the current string has dropped.
    string_len: usize,
    line: Vec<char>,
    cursor: usize,
    utf8: Vec<u8>,
    /// Rows the cursor is above the open rows, in lines already ended. Text
    /// written there is dropped.
    row: isize,
    /// Open rows above the cursor's, top first.
    above: Vec<Vec<char>>,
    /// Open rows below the cursor's, nearest first.
    below: VecDeque<Vec<char>>,
    /// The cursor reached its row by `ESC [ A` or `ESC [ B` rather than a newline.
    moved: bool,
    /// Lines ended since the last feed returned.
    ended: Vec<String>,
    /// Rows [`Stripper::settle`] ended, nearest last, which a move up opens again.
    settled: Vec<Vec<char>>,
}

/// The farthest a move up reaches into lines already ended.
const ROWS_UP: isize = 24;

/// The most rows a [`Stripper`] keeps open above and below the cursor's and
/// settled, together.
pub const MAX_OPEN_ROWS: usize = 2 * ROWS_UP as usize;

impl Stripper {
    /// Feeds bytes and returns the lines they complete.
    pub fn feed(&mut self, data: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &b in data {
            self.byte(b, &mut lines);
        }
        lines.append(&mut self.ended);
        lines
    }

    /// Ends the open rows above the cursor and returns them as lines, for when the
    /// output has gone quiet. A move up still reaches them: they open again, and
    /// end again as more lines, so a settle in the middle of a redraw repeats
    /// lines rather than losing them.
    pub fn settle(&mut self) -> Vec<String> {
        self.settled.extend(self.above.iter().cloned());
        let over = self.settled.len().saturating_sub(ROWS_UP as usize);
        self.settled.drain(..over);
        self.close()
    }

    /// Whether there are open rows that [`Stripper::settle`] would end.
    pub fn unsettled(&self) -> bool {
        !self.above.is_empty()
    }

    /// Whether the cursor reached its row by a newline, not by moving up or down,
    /// as it does in the middle of a redraw.
    pub fn at_rest(&self) -> bool {
        !self.moved
    }

    /// Ends the open rows above the cursor and returns them as lines, as when the
    /// output has ended.
    pub fn close(&mut self) -> Vec<String> {
        self.above
            .drain(..)
            .map(|line| line.into_iter().collect())
            .collect()
    }

    fn end_above(&mut self) {
        let rows = self.above.drain(..).map(|line| line.into_iter().collect());
        self.ended.extend(rows);
        self.settled.clear();
    }

    fn up(&mut self, n: usize) {
        self.moved = true;
        let mut n = n.min(ROWS_UP as usize * 2);
        if self.row == 0 {
            while n > 0 {
                let Some(line) = self.above.pop().or_else(|| self.settled.pop()) else {
                    break;
                };
                self.below
                    .push_front(std::mem::replace(&mut self.line, line));
                n -= 1;
            }
        }
        self.row = (self.row - n as isize).max(-ROWS_UP);
    }

    fn down(&mut self, n: usize, newline: bool) {
        self.moved = !newline;
        let mut n = n.min(ROWS_UP as usize);
        let back = n.min(self.row.unsigned_abs());
        self.row += back as isize;
        n -= back;
        for _ in 0..n {
            let next = self.below.pop_front().unwrap_or_default();
            self.above.push(std::mem::replace(&mut self.line, next));
        }
        let over = self.above.len().saturating_sub(ROWS_UP as usize);
        if over > 0 {
            let rows = self
                .above
                .drain(..over)
                .map(|line| line.into_iter().collect());
            self.ended.extend(rows);
            self.settled.clear();
        }
    }

    /// The current incomplete line.
    pub fn pending(&self) -> String {
        self.line.iter().collect()
    }

    /// The length of the current incomplete line, in characters.
    pub fn pending_len(&self) -> usize {
        self.line.len()
    }

    /// How many rows besides the cursor's are open or settled.
    pub fn open_rows(&self) -> usize {
        self.above.len() + self.below.len() + self.settled.len()
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
                    b']' | b'P' | b'X' | b'^' | b'_' => {
                        self.string_len = 0;
                        State::String { osc: b == b']' }
                    }
                    0x20..=0x2f => State::EscapeIntermediate,
                    0x30..=0x7e => State::Ground,
                    _ => return self.interrupt(b, lines),
                }
            }
            State::EscapeIntermediate => match b {
                0x20..=0x2f => {}
                0x30..=0x7e => self.state = State::Ground,
                _ => self.interrupt(b, lines),
            },
            State::Csi => match b {
                0x40..=0x7e => {
                    match b {
                        b'A' => self.up(self.count()),
                        b'B' => self.down(self.count(), false),
                        _ if self.row != 0 => {}
                        b'K' => self.erase_line(),
                        b'G' => self.cursor = self.count().saturating_sub(1).min(MAX_COLUMN),
                        b'C' => {
                            self.cursor = self
                                .cursor
                                .saturating_add(self.count())
                                .min(MAX_COLUMN.max(self.line.len()))
                        }
                        b'D' => self.cursor = self.cursor.saturating_sub(self.count()),
                        _ => {}
                    }
                    self.state = State::Ground;
                }
                0x20..=0x3f => {
                    if self.csi.len() < MAX_CSI {
                        self.csi.push(b)
                    }
                }
                _ => self.interrupt(b, lines),
            },
            State::String { osc } => {
                self.string_len = self.string_len.saturating_add(1);
                match b {
                    0x07 | 0x18 | 0x1a => self.state = State::Ground,
                    0x1b => self.state = State::StringEscape,
                    0xc2 => self.state = State::StringC2 { osc },
                    b'\n' | b'\r' if osc => self.interrupt(b, lines),
                    b'\n' if self.string_len > MAX_STRING => self.interrupt(b, lines),
                    _ => {}
                }
            }
            State::StringC2 { osc } => {
                self.state = State::String { osc };
                if b == 0x9c {
                    self.state = State::Ground;
                } else {
                    self.byte(b, lines);
                }
            }
            State::StringEscape => {
                if b == b'\\' {
                    self.state = State::Ground;
                } else {
                    self.state = State::Escape;
                    self.byte(b, lines);
                }
            }
        }
    }

    /// Ends an escape sequence at a byte that can't be part of it, as if no
    /// sequence had started: ESC starts another, CAN and SUB are dropped, and other
    /// controls and text are output.
    fn interrupt(&mut self, b: u8, lines: &mut Vec<String>) {
        self.state = State::Ground;
        self.ground(b, lines);
    }

    fn ground(&mut self, b: u8, lines: &mut Vec<String>) {
        if !self.utf8.is_empty() || b >= 0x80 {
            self.utf8_byte(b, lines);
            return;
        }
        match b {
            0x1b => self.state = State::Escape,
            b'\n' => {
                if self.row == 0 {
                    self.cursor = 0;
                }
                self.down(1, true);
            }
            _ if self.row != 0 && matches!(b, b'\r' | 0x08) => {}
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

    /// A cursor movement's count: its parameter, 1 when there is none, and as
    /// large as it gets when it's too large to read.
    fn count(&self) -> usize {
        if !self.csi.is_empty() && self.csi.iter().all(u8::is_ascii_digit) {
            let n = std::str::from_utf8(&self.csi)
                .ok()
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(usize::MAX);
            return n.max(1);
        }
        1
    }

    fn put(&mut self, c: char) {
        if self.row != 0 || (c.is_control() && c != '\t') {
            return;
        }
        if self.cursor >= MAX_LINE {
            self.cursor = 0;
            self.down(1, true);
        }
        if !self.moved {
            self.end_above();
        }
        while self.line.len() < self.cursor {
            self.line.push(' ');
        }
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
    fn cursor_movements_place_later_text() {
        assert_eq!(strip(b"irb(main)> 1+\x1b[2D2 \n"), "irb(main)> 2 \n");
        assert_eq!(strip(b"(rdbg) cont\x1b[1Gxx\n"), "xxdbg) cont\n");
        assert_eq!(strip(b"(rdbg) \x1b[8Gc\x1b[G(\n"), "(rdbg) c\n");
        assert_eq!(strip(b"ab\x1b[3Cc\n"), "ab   c\n");
        assert_eq!(strip(b"abc\x1b[D\x1b[KX\n"), "abX\n");
    }

    #[test]
    fn cursor_movements_stop_at_a_column_limit() {
        assert_eq!(strip(b"\x1b[999999999Gx").len(), MAX_COLUMN + 1);
        assert_eq!(
            strip(b"\x1b[99999999999999999999999Cx").len(),
            MAX_COLUMN + 1
        );
        let mut s = Stripper::default();
        s.feed(b"\x1b[4000C\x1b[4000C\x1b[4000Cx");
        assert_eq!(s.pending().len(), MAX_COLUMN + 1);
        let mut s = Stripper::default();
        s.feed(&b"a".repeat(5000));
        s.feed(b"\x1b[Cx");
        let line = s.pending();
        assert_eq!(line.len(), 5001, "{}", &line[4090..]);
        assert!(line[..5000].bytes().all(|b| b == b'a'), "never moves back");
        assert!(line.ends_with('x'));
    }

    #[test]
    fn text_written_on_other_rows_leaves_the_current_line_alone() {
        assert_eq!(
            strip(
                b"[1] pry(main)> 1\r\n\x1b[1A\x1b[0G[1] pry(main)> \x1b[1m1\x1b[0m\x1b[1B\r=> 1\r\n"
            ),
            "[1] pry(main)> 1\n=> 1\n"
        );
        let mut s = Stripper::default();
        s.feed(b"irb(main):001> Str\x1b[1B\x1b[1G\x1b[KString\x1b[1B\x1b[1G\x1b[KStruct\x1b[2A\x1b[18G");
        assert_eq!(s.pending(), "irb(main):001> Str");
        assert!(s.feed(b"\x1b[3B\rnext\n").is_empty());
        assert_eq!(
            s.settle(),
            ["irb(main):001> Str", "String", "Struct", "next"],
            "writing over the rows below and settling ends them"
        );
    }

    #[test]
    fn newlines_reline_moves_back_over_dont_end_the_line() {
        let mut s = Stripper::default();
        s.feed(b"\x1b[?25l\x1b[1G\x1b[K\x1b[1G\x1b[0mirb(main):001> \x1b[0m\x1b[?25h\x1b[16G");
        assert_eq!(s.pending(), "irb(main):001> ");
        let lines = s.feed(b"\x1b[?25l\r\n\r\n\x1b[2A\x1b[16G\x1b[0mS\x1b[0m\x1b[1B\x1b[1G\x1b[K\x1b[16G\x1b[97;100mSTDERR \x1b[0m\x1b[1B\x1b[1G\x1b[K\x1b[16G\x1b[97;100mSTDIN  \x1b[0m\x1b[?25h\x1b[17G\x1b[2A");
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(s.pending(), "irb(main):001> S");
        let lines = s.feed(b"\x1b[?25l\x1b[1G\x1b[K\x1b[1B\x1b[1G\x1b[K\x1b[1B\x1b[1G\x1b[K\x1b[?25h\x1b[1G\x1b[2Airb(main):001> S\r\r\n\x1b[1G\x1b[?2004l=> nil\r\n\x1b[?2004h\x1b[6n\x1b[?25l\x1b[1G\x1b[K\x1b[1G\x1b[0mirb(main):002> \x1b[0m\x1b[?25h\x1b[16G");
        assert_eq!(lines, ["irb(main):001> S", "=> nil"]);
        assert_eq!(s.pending(), "irb(main):002> ");
    }

    #[test]
    fn huge_moves_up_and_down_count_as_a_screen() {
        let mut s = Stripper::default();
        s.feed(b"top\n\x1b[99999999999999999999999A\x1b[999999999Bx");
        assert!(s.pending().starts_with('x'));
        let lines = s.settle();
        assert!(lines.len() <= ROWS_UP as usize + 1, "{}", lines.len());
    }

    #[test]
    fn a_move_up_reaches_a_screen_at_most() {
        let mut s = Stripper::default();
        s.feed(b"top\n");
        assert_eq!(s.settle(), ["top"]);
        s.feed(b"\x1b[999A");
        let mut lines = s.feed(&b"redrawn\n".repeat(30));
        lines.extend(s.settle());
        assert_eq!(lines.len(), 30 - 24, "{lines:?}");
        let lines = s.feed(&b"\n".repeat(30));
        assert_eq!(lines.len(), 30 - 24, "a screen of open rows at most");
    }

    #[test]
    fn a_line_ends_with_text_after_it_or_when_settled() {
        let mut s = Stripper::default();
        assert!(s.feed(b"one\r\n").is_empty());
        assert!(s.unsettled());
        assert_eq!(s.settle(), ["one"]);
        assert!(!s.unsettled());
        assert!(s.feed(b"\x1b[1Atwo\n").is_empty());
        assert_eq!(s.settle(), ["two"], "a move up opens a settled row again");
        assert!(s.feed(b"three\n").is_empty());
        assert_eq!(s.feed(b"four"), ["three"]);
        assert!(s.feed(b"\x1b[2A\x1b[2Agone\n").is_empty());
        assert!(
            s.settle().is_empty(),
            "rows ended by text after them stay ended"
        );
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
        lines.extend(s.settle());
        assert_eq!(lines, ["blåbær"]);
    }

    #[test]
    fn escape_split_across_feeds() {
        let mut s = Stripper::default();
        assert!(s.feed(b"a\x1b[3").is_empty());
        assert!(s.feed(b"1mb\n").is_empty());
        assert_eq!(s.settle(), ["ab"]);
    }

    #[test]
    fn an_escape_ends_an_unterminated_string() {
        assert_eq!(strip(b"\x1b]0;t\x1b[31mred\n"), "red\n");
        assert_eq!(strip(b"\x1b]0;t\x1b\x1b]0;u\x07x\n"), "x\n");
    }

    #[test]
    fn cancel_and_substitute_end_a_string() {
        assert_eq!(strip(b"\x1bPjunk\x18after\n"), "after\n");
        assert_eq!(strip(b"\x1b_junk\x1aafter\n"), "after\n");
        assert_eq!(strip(b"a\x1b[31\x18b\n"), "ab\n");
    }

    #[test]
    fn a_newline_ends_an_unterminated_osc() {
        assert_eq!(strip(b"\x1b]0;cut off\nnext\n"), "\nnext\n");
        assert_eq!(strip(b"(rdbg) \x1b]c\r\nnext\n"), "(rdbg) \nnext\n");
    }

    #[test]
    fn a_carriage_return_ends_an_unterminated_osc() {
        assert_eq!(
            strip(b"\x1b]0;cut off\rprogress 10%\rprogress 20%"),
            "progress 20%"
        );
    }

    #[test]
    fn st_as_utf8_ends_a_string() {
        assert_eq!(strip("\x1b]0;title\u{9c}after\n".as_bytes()), "after\n");
        assert_eq!(strip("\x1bPq#0\u{9c}after\n".as_bytes()), "after\n");
        assert_eq!(strip("\x1b]0;©\x07after\n".as_bytes()), "after\n");
        let mut s = Stripper::default();
        s.feed(b"\x1b]0;title\xc2");
        s.feed(b"\x9cafter");
        assert_eq!(s.pending(), "after");
    }

    #[test]
    fn a_long_unterminated_dcs_ends_at_a_newline() {
        let mut s = Stripper::default();
        s.feed(b"\x1bP");
        let mut lines = Vec::new();
        for i in 0..256 * 1024 {
            lines.extend(s.feed(format!("line {i}\n").as_bytes()));
        }
        lines.extend(s.close());
        assert!(!lines.is_empty());
        assert_eq!(lines.last().unwrap(), "line 262143");
        assert!(lines.len() > 100_000, "{}", lines.len());
    }

    #[test]
    fn a_lone_c2_inside_a_string_changes_nothing() {
        assert_eq!(strip(b"\x1b]0;a\xc2\x1b\\b"), "b");
        assert_eq!(strip(b"\x1b]0;a\xc2\nb"), "\nb");
        assert_eq!(strip(b"\x1bPa\xc2\xc2\x9cb"), "b");
    }

    #[test]
    fn newlines_stay_inside_dcs_and_other_strings() {
        assert_eq!(strip(b"a\x1bPq#0;2;0;0;0\n#0~~@@\n-\x1b\\b\n"), "ab\n");
        assert_eq!(strip(b"a\x1b_Gf=100;AAAA\r\nBBBB\x1b\\b\n"), "ab\n");
    }

    #[test]
    fn control_characters_cut_an_escape_short() {
        assert_eq!(strip(b"a\x1b\nb\n"), "a\nb\n");
        assert_eq!(strip(b"a\x1b(\nb\n"), "a\nb\n");
        assert_eq!(strip(b"abc\x1b[\rX\n"), "Xbc\n");
        assert_eq!(strip("a\x1bå\n".as_bytes()), "aå\n");
    }

    #[test]
    fn a_line_without_an_end_wraps() {
        let mut s = Stripper::default();
        let mut lines = Vec::new();
        for _ in 0..10 {
            lines.extend(s.feed(&b"a".repeat(1024 * 1024)));
            assert!(s.pending_len() <= MAX_LINE, "{}", s.pending_len());
        }
        lines.extend(s.close());
        lines.push(s.pending());
        assert!(lines.iter().all(|line| line.len() <= MAX_LINE));
        assert_eq!(
            lines.iter().map(String::len).sum::<usize>(),
            10 * 1024 * 1024
        );
        assert_eq!(strip(&b"a".repeat(MAX_LINE + 1)).lines().count(), 2);
    }

    #[test]
    fn csi_parameters_stop_at_a_limit() {
        let mut s = Stripper::default();
        s.feed(b"a\x1b[");
        for _ in 0..1024 {
            s.feed(&b"1".repeat(1024));
            assert!(s.csi.len() <= MAX_CSI, "{}", s.csi.len());
        }
        s.feed(b"mb");
        assert_eq!(s.pending(), "ab");
    }

    #[test]
    fn drops_c1_controls() {
        let text = strip("a\u{9b}31mb\u{9d}0;t\u{9c}c\n".as_bytes());
        assert!(
            !text.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c)),
            "{text:?}"
        );
        assert_eq!(text, "a31mb0;tc\n");
    }

    #[test]
    fn invalid_utf8_becomes_replacement() {
        assert_eq!(strip(b"a\xffb\xc3c"), "a\u{fffd}b\u{fffd}c");
    }
}
