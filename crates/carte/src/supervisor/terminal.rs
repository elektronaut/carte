//! A process's pty as attached clients share it: output as it comes, input from any
//! of them, and a size that fits the smallest attached terminal. While no attached
//! terminal can, carte answers the process's cursor-position queries itself.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use carte_api::attach::TermSize;
use tokio::sync::{broadcast, watch};

use super::runner::ProcLog;
use crate::process::Pty;

pub struct Terminal {
    pty: Pty,
    log: Arc<Mutex<ProcLog>>,
    /// The size the process started with, which it gets back when the last
    /// terminal detaches.
    configured: TermSize,
    output: broadcast::Sender<Bytes>,
    clients: Mutex<Clients>,
    /// Held while one client's input is written, so another's can't split it.
    writing: tokio::sync::Mutex<()>,
    ended: watch::Sender<Option<String>>,
}

/// Clients that sent a size are terminals, which answer cursor-position queries
/// themselves; the others, like the terminal UI and MCP `type`, show or return
/// output without one.
struct Clients {
    next: u64,
    sizes: BTreeMap<u64, Option<TermSize>>,
    current: TermSize,
    queries: CursorQueries,
    answers: Answers,
}

impl Terminal {
    pub(super) fn new(pty: Pty, log: Arc<Mutex<ProcLog>>, size: TermSize) -> Arc<Self> {
        Arc::new(Terminal {
            pty,
            log,
            configured: size,
            output: broadcast::channel(1024).0,
            clients: Mutex::new(Clients {
                next: 0,
                sizes: BTreeMap::new(),
                current: size,
                queries: CursorQueries::default(),
                answers: Answers::default(),
            }),
            writing: tokio::sync::Mutex::new(()),
            ended: watch::channel(None).0,
        })
    }

    /// Hands output to the attached clients, and answers the cursor-position queries
    /// in it that no attached terminal got whole. Called with the log locked, right
    /// after the output went into it, so a client that joins gets each byte once.
    pub(super) fn output(self: &Arc<Self>, data: &[u8]) {
        if self.output.receiver_count() > 0 {
            let _ = self.output.send(Bytes::copy_from_slice(data));
        }
        let start_writer = {
            let mut clients = self.clients.lock().unwrap();
            let terminals = clients.terminals();
            let unanswered = clients.queries.feed(data, &terminals);
            clients.answers.add(unanswered)
        };
        if start_writer {
            let terminal = self.clone();
            tokio::spawn(async move { terminal.write_answers().await });
        }
    }

    /// Writes the pending answers until none are left; the one writer of them.
    async fn write_answers(&self) {
        loop {
            let pending = self.clients.lock().unwrap().answers.take();
            if pending == 0 {
                return;
            }
            if !self.pty.reads_keys() {
                self.clients.lock().unwrap().answers.stop();
                return;
            }
            if self.write(&CURSOR_ANSWER.repeat(pending)).await.is_err() {
                self.clients.lock().unwrap().answers.stop();
                return;
            }
        }
    }

    /// Marks the process as gone; attached clients get `message` and are let go.
    pub(super) fn end(&self, message: String) {
        self.ended.send_replace(Some(message));
    }

    /// Attaches a client. `size` takes part in sizing the pty; with `replay` the
    /// client gets the last screen of output, as many lines as its terminal has rows;
    /// see [`replayable`].
    pub fn join(self: &Arc<Self>, size: Option<TermSize>, replay: bool) -> Client {
        let size = size.filter(|s| s.cols > 0 && s.rows > 0);
        let (output, replay, id) = {
            let log = self.log.lock().unwrap();
            let output = self.output.subscribe();
            let rows = size.unwrap_or(self.configured).rows;
            let screen = if replay {
                replayable(&log.raw.last_lines(usize::from(rows)))
            } else {
                Vec::new()
            };
            let mut clients = self.clients.lock().unwrap();
            clients.next += 1;
            let id = clients.next;
            clients.sizes.insert(id, size);
            if size.is_some() && replay {
                clients.queries.replayed_to(id);
            }
            self.fit(&mut clients);
            (output, screen, id)
        };
        Client {
            terminal: self.clone(),
            id,
            output,
            replay,
            ended: self.ended.subscribe(),
        }
    }

    /// Clients attached right now.
    pub fn clients(&self) -> usize {
        self.clients.lock().unwrap().sizes.len()
    }

    /// The pty's size right now.
    pub fn size(&self) -> TermSize {
        self.clients.lock().unwrap().current
    }

    /// Writes all of `data` before anyone else's input.
    pub async fn write(&self, data: &[u8]) -> io::Result<()> {
        let _turn = self.writing.lock().await;
        self.pty.write(data).await
    }

    /// Gives the pty the smallest size among the clients that have one, or the
    /// configured size when none has.
    fn fit(&self, clients: &mut Clients) {
        let size = clients
            .sizes
            .values()
            .flatten()
            .copied()
            .reduce(|a, b| TermSize {
                cols: a.cols.min(b.cols),
                rows: a.rows.min(b.rows),
            })
            .unwrap_or(self.configured);
        if size != clients.current && self.pty.resize(size.cols, size.rows).is_ok() {
            clients.current = size;
        }
    }
}

impl Clients {
    fn terminals(&self) -> Vec<u64> {
        self.sizes
            .iter()
            .filter(|(_, size)| size.is_some())
            .map(|(id, _)| *id)
            .collect()
    }
}

/// A cursor-position query, which Reline sends before each prompt and then waits
/// half a second for the answer to.
const CURSOR_QUERY: &[u8] = b"\x1b[6n";

/// The top left corner, Reline's own guess when nobody answers. A real position near
/// the bottom would make it scroll.
const CURSOR_ANSWER: &[u8] = b"\x1b[1;1R";

/// Finds cursor-position queries in output however it is split into reads, and tells
/// which of them no attached terminal got whole.
#[derive(Debug, Default)]
struct CursorQueries {
    /// How much of a query the output so far ends with.
    matched: usize,
    /// The terminals that got the query in progress from its first byte.
    seen_by: Vec<u64>,
}

impl CursorQueries {
    /// Reads `data`, which went to the attached `terminals`, and returns how many
    /// queries ended in it that none of them got whole.
    fn feed(&mut self, data: &[u8], terminals: &[u64]) -> usize {
        let mut unanswered = 0;
        for &byte in data {
            if byte == CURSOR_QUERY[self.matched] {
                self.matched += 1;
            } else {
                self.matched = usize::from(byte == CURSOR_QUERY[0]);
            }
            if self.matched == 1 {
                self.seen_by.clear();
                self.seen_by.extend_from_slice(terminals);
            }
            if self.matched == CURSOR_QUERY.len() {
                self.matched = 0;
                if !self.seen_by.iter().any(|id| terminals.contains(id)) {
                    unanswered += 1;
                }
            }
        }
        unanswered
    }

    /// Terminal `id` joined with the last screen of output, so it got the start of a
    /// query in progress.
    fn replayed_to(&mut self, id: u64) {
        if self.matched > 0 {
            self.seen_by.push(id);
        }
    }
}

/// Answers waiting to be written, by at most one writer at a time. A process that
/// asks without reading gets a few; more would be stale by the time it reads them.
#[derive(Debug, Default)]
struct Answers {
    pending: usize,
    writing: bool,
}

impl Answers {
    /// Adds `n` answers, and returns whether a writer has to start for them.
    fn add(&mut self, n: usize) -> bool {
        if n == 0 {
            return false;
        }
        self.pending = (self.pending + n).min(4);
        !std::mem::replace(&mut self.writing, true)
    }

    /// Hands the writer the pending answers; with none, it stops.
    fn take(&mut self) -> usize {
        let pending = std::mem::take(&mut self.pending);
        self.writing = pending > 0;
        pending
    }

    /// The writer gave up.
    fn stop(&mut self) {
        *self = Answers::default();
    }
}

/// The last screen of output as a terminal can be shown it again: from the first
/// line that starts within its last MiB, so it never starts inside an escape
/// sequence, and without the queries in it, which were answered or given up on and
/// whose answers would now arrive as typed input.
fn replayable(data: &[u8]) -> Vec<u8> {
    let over = data.len().saturating_sub(1024 * 1024);
    let data = if over == 0 {
        data
    } else {
        match data[over - 1..].iter().position(|&b| b == b'\n') {
            Some(at) => &data[over + at..],
            None => &[],
        }
    };
    without_queries(data)
}

/// `data` without ENQ and the complete escape sequences a terminal answers, and
/// without APC, PM and SOS strings, as kitty answers its graphics commands.
fn without_queries(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut at = 0;
    while at < data.len() {
        match data[at] {
            0x05 => at += 1,
            0x1b => match escape(&data[at..]) {
                Some((len, query)) => {
                    if !query {
                        out.extend_from_slice(&data[at..at + len]);
                    }
                    at += len;
                }
                None => {
                    out.push(0x1b);
                    at += 1;
                }
            },
            b => {
                out.push(b);
                at += 1;
            }
        }
    }
    out
}

/// The length of the CSI sequence or string `data` starts with, and whether a
/// terminal answers it; `None` when it doesn't start with a complete one.
fn escape(data: &[u8]) -> Option<(usize, bool)> {
    match *data.get(1)? {
        b'[' => {
            let params = data[2..]
                .iter()
                .take_while(|b| (0x30..=0x3f).contains(*b))
                .count();
            let between = data[2 + params..]
                .iter()
                .take_while(|b| (0x20..=0x2f).contains(*b))
                .count();
            let end = 2 + params + between;
            let last = *data.get(end).filter(|b| (0x40..=0x7e).contains(*b))?;
            let query = csi_query(&data[2..2 + params], &data[2 + params..end], last);
            Some((end + 1, query))
        }
        kind @ (b']' | b'P' | b'_' | b'^' | b'X') => {
            let body = &data[2..];
            let (len, end) = body.iter().enumerate().find_map(|(i, &b)| match b {
                0x07 | 0x18 | 0x1a => Some((i, i + 1)),
                0x1b if body.get(i + 1) == Some(&b'\\') => Some((i, i + 2)),
                _ => None,
            })?;
            let body = &body[..len];
            let query = match kind {
                b']' => body
                    .split(|&b| b == b';')
                    .skip(1)
                    .any(|part| part == b"?" || part.ends_with(b"=?")),
                b'P' => body.starts_with(b"$q") || body.starts_with(b"+q"),
                _ => true,
            };
            Some((2 + end, query))
        }
        _ => None,
    }
}

/// Whether a terminal answers the CSI sequence with these parameter and
/// intermediate bytes and final byte: device status and attributes, modes, its
/// version, keyboard modes, sizes and other reports.
fn csi_query(params: &[u8], between: &[u8], last: u8) -> bool {
    let private = params.first().copied();
    match (last, between) {
        (b'n' | b'c' | b'x', b"") => true,
        (b'p' | b'w', b"$") | (b'y', b"*") | (b'|', b"#") => true,
        (b'q', b"") => private == Some(b'>'),
        (b'u' | b'm' | b'S', b"") => private == Some(b'?'),
        (b't', b"") => {
            let first = params.split(|&b| b == b';').next().unwrap_or_default();
            matches!(
                first,
                b"11" | b"13" | b"14" | b"15" | b"16" | b"18" | b"19" | b"20" | b"21"
            )
        }
        _ => false,
    }
}

/// One attached client; dropping it detaches.
pub struct Client {
    terminal: Arc<Terminal>,
    id: u64,
    pub output: broadcast::Receiver<Bytes>,
    /// The last screen of output from before it joined.
    pub replay: Vec<u8>,
    /// How the process exited, once it has.
    pub ended: watch::Receiver<Option<String>>,
}

impl Client {
    pub async fn write(&self, data: &[u8]) -> io::Result<()> {
        self.terminal.write(data).await
    }

    /// Writes the client's input from another task, so a process that doesn't read
    /// can't hold up the client.
    pub fn writer(&self) -> Writer {
        Writer(self.terminal.clone())
    }

    pub fn resize(&self, size: TermSize) {
        if size.cols == 0 || size.rows == 0 {
            return;
        }
        let mut clients = self.terminal.clients.lock().unwrap();
        clients.sizes.insert(self.id, Some(size));
        self.terminal.fit(&mut clients);
    }
}

pub struct Writer(Arc<Terminal>);

impl Writer {
    pub async fn write(&self, data: &[u8]) -> io::Result<()> {
        self.0.write(data).await
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut clients = self.terminal.clients.lock().unwrap();
        clients.sizes.remove(&self.id);
        self.terminal.fit(&mut clients);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: &[u8] =
        b"\x1b[1G\xe2\x96\xbd\x1b[6n\x1b[1G\x1b[K\x1b[6n\x1b[?25l\x1b[0m>> \x1b[4G";

    #[test]
    fn finds_queries_however_the_output_is_split() {
        for at in 0..=OUTPUT.len() {
            let mut queries = CursorQueries::default();
            let (one, two) = OUTPUT.split_at(at);
            let found = queries.feed(one, &[]) + queries.feed(two, &[]);
            assert_eq!(found, 2, "split at {at}");
        }
        let mut queries = CursorQueries::default();
        let found: usize = OUTPUT.iter().map(|b| queries.feed(&[*b], &[])).sum();
        assert_eq!(found, 2, "a byte at a time");
    }

    #[test]
    fn other_sequences_are_no_queries() {
        let mut queries = CursorQueries::default();
        assert_eq!(
            queries.feed(b"\x1b[16n \x1b[6;1R \x1b[?6n \x1b6n [6n \x1b[6", &[]),
            0
        );
        assert_eq!(queries.feed(b"m \x1b\x1b[6n", &[]), 1);
    }

    #[test]
    fn a_query_an_attached_terminal_got_whole_is_left_to_it() {
        let mut queries = CursorQueries::default();
        assert_eq!(queries.feed(b"\x1b[6n", &[1]), 0);
        assert_eq!(queries.feed(b"\x1b[", &[1]), 0);
        assert_eq!(queries.feed(b"6n", &[1, 2]), 0);
        assert_eq!(queries.feed(b"\x1b[6n", &[]), 1, "nobody attached");
    }

    #[test]
    fn a_query_whose_start_a_terminal_missed_is_answered() {
        let mut queries = CursorQueries::default();
        assert_eq!(queries.feed(b"\x1b[", &[]), 0);
        assert_eq!(queries.feed(b"6n", &[1]), 1, "joined in the middle");
        assert_eq!(queries.feed(b"\x1b[", &[1]), 0);
        assert_eq!(queries.feed(b"6n", &[2]), 1, "left in the middle");
    }

    #[test]
    fn a_terminal_that_joins_with_the_start_of_a_query_answers_it() {
        let mut queries = CursorQueries::default();
        assert_eq!(queries.feed(b"\x1b[", &[]), 0);
        queries.replayed_to(1);
        assert_eq!(queries.feed(b"6n", &[1]), 0);
    }

    #[test]
    fn one_writer_writes_a_few_answers_at_most() {
        let mut answers = Answers::default();
        assert!(!answers.add(0), "nothing to write");
        assert!(answers.add(1), "the first answer starts a writer");
        for _ in 0..100 {
            assert!(!answers.add(1), "the writer is still at it");
        }
        assert_eq!(answers.take(), 4);
        assert!(!answers.add(2), "the writer comes back for more");
        assert_eq!(answers.take(), 2);
        assert_eq!(answers.take(), 0);
        assert!(answers.add(1), "a writer that ran out has stopped");
        answers.stop();
        assert!(answers.add(1), "so has one that gave up");
    }

    #[test]
    fn replays_leave_out_whole_queries() {
        assert_eq!(
            replayable(OUTPUT),
            b"\x1b[1G\xe2\x96\xbd\x1b[1G\x1b[K\x1b[?25l\x1b[0m>> \x1b[4G"
        );
        assert_eq!(replayable(b">> \x1b[6"), b">> \x1b[6");
        assert_eq!(replayable(b">> \x1b]11;"), b">> \x1b]11;");
    }

    #[test]
    fn replays_leave_out_every_query_a_terminal_answers() {
        assert_eq!(replayable(b"a\x1b[c\x1b]11;?\x07b"), b"ab");
        let queries: &[&[u8]] = &[
            b"\x1b[5n",
            b"\x1b[?6n",
            b"\x1b[>c",
            b"\x1b[=0c",
            b"\x1b[?2004$p",
            b"\x1b[4$p",
            b"\x1b[>0q",
            b"\x1b[?u",
            b"\x1b[?4m",
            b"\x1b[14t",
            b"\x1b[18;0t",
            b"\x1b[?1;1S",
            b"\x1b]10;?\x1b\\",
            b"\x1b]4;1;?\x07",
            b"\x1b]52;c;?\x07",
            b"\x1bP$qm\x1b\\",
            b"\x1bP+q544e\x1b\\",
            b"\x1b_Gi=1,a=q;AAAA\x1b\\",
            b"\x05",
        ];
        for query in queries {
            let data = [b"a".as_slice(), query, b"b"].concat();
            assert_eq!(
                replayable(&data),
                b"ab",
                "{:?}",
                String::from_utf8_lossy(query)
            );
        }
    }

    #[test]
    fn replays_keep_what_isnt_a_query() {
        let kept: &[&[u8]] = &[
            b"\x1b[31m",
            b"\x1b[?1049h",
            b"\x1b[2 q",
            b"\x1b[22;0t",
            b"\x1b[>4;1m",
            b"\x1b[>1u",
            b"\x1b]0;title\x07",
            b"\x1b]8;;http://x\x1b\\",
            b"\x1b]52;c;aGk=\x07",
            b"\x1b(B",
            b"\x1b7",
        ];
        for seq in kept {
            let data = [b"a".as_slice(), seq, b"b"].concat();
            assert_eq!(
                replayable(&data),
                data,
                "{:?}",
                String::from_utf8_lossy(seq)
            );
        }
    }

    #[test]
    fn a_long_replay_starts_at_a_line() {
        let mut data = b"\x1b]0;".to_vec();
        data.extend(b"x".repeat(1024 * 1024));
        data.extend(b"\x07line\nnext\n");
        assert_eq!(replayable(&data), b"next\n");
        let mut data = b"a".repeat(1024 * 1024 - 4);
        data.extend(b"\nb\nc\n");
        assert_eq!(replayable(&data), b"b\nc\n");
        let mut data = b"a\n".to_vec();
        data.extend(b"b".repeat(1024 * 1024));
        assert_eq!(replayable(&data).len(), 1024 * 1024, "at a line already");
        assert!(replayable(&b"a".repeat(2 * 1024 * 1024)).is_empty());
    }
}
