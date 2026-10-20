//! One task per running process: spawn it, feed its output into the log, report
//! when it exits, and stop it on request.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use carte_api::LogLine;
use carte_api::attach::TermSize;
use carte_core::model::Readiness;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::terminal::Terminal;
use crate::logs::{LogBuffer, Stripper};
use crate::process::{IoMode, SpawnSpec, Spawner};

/// The output of one process: raw bytes for later use, stripped lines for search,
/// the merged view and following.
#[derive(Debug)]
pub struct ProcLog {
    pub raw: LogBuffer,
    lines: VecDeque<(u64, String)>,
    line_bytes: usize,
    cap: usize,
    stripper: Stripper,
    /// The same output on a screen that only settles after a longer quiet, and
    /// never in the middle of a redraw, for finding debugger prompts.
    screen: Stripper,
    /// A line ended on `screen` since [`ProcLog::take_answered`].
    answered: bool,
}

impl ProcLog {
    pub fn new(cap: usize) -> Self {
        ProcLog {
            raw: LogBuffer::new(cap),
            lines: VecDeque::new(),
            line_bytes: 0,
            cap,
            stripper: Stripper::default(),
            screen: Stripper::default(),
            answered: false,
        }
    }

    /// Adds output and returns the complete lines it finished.
    pub fn push(&mut self, data: &[u8]) -> Vec<(u64, String)> {
        self.raw.push(data);
        self.answered |= !self.screen.feed(data).is_empty();
        let lines = self.stripper.feed(data);
        self.add(lines)
    }

    /// Ends the lines left behind on the prompt screen, after a longer quiet, so
    /// a prompt answered by a process that then stays silent counts as answered.
    /// Nothing ends in the middle of a redraw.
    pub fn settle_screen(&mut self) {
        if self.screen.at_rest() {
            self.answered |= !self.screen.settle().is_empty();
        }
    }

    /// Lines [`ProcLog::settle_screen`] would end.
    pub fn screen_unsettled(&self) -> bool {
        self.screen.at_rest() && self.screen.unsettled()
    }

    /// Whether a line ended since the last call, which answers a prompt.
    pub fn take_answered(&mut self) -> bool {
        std::mem::take(&mut self.answered)
    }

    /// Ends the lines the output has left behind, for when it has gone quiet; see
    /// [`Stripper::settle`].
    pub fn settle(&mut self) -> Vec<(u64, String)> {
        let lines = self.stripper.settle();
        self.add(lines)
    }

    /// Lines [`ProcLog::settle`] would end.
    pub fn unsettled(&self) -> bool {
        self.stripper.unsettled()
    }

    /// Ends every line, a partial last one too, as when the process exits.
    pub fn flush(&mut self) -> Vec<(u64, String)> {
        let mut lines = self.stripper.close();
        let pending = self.stripper.pending();
        if !pending.is_empty() {
            lines.push(pending);
        }
        self.stripper = Stripper::default();
        self.screen = Stripper::default();
        self.add(lines)
    }

    fn add(&mut self, lines: Vec<String>) -> Vec<(u64, String)> {
        let at = now_ms();
        let lines: Vec<(u64, String)> = lines.into_iter().map(|line| (at, line)).collect();
        for line in &lines {
            self.append(line.clone());
        }
        lines
    }

    fn append(&mut self, line: (u64, String)) {
        self.line_bytes += line.1.len() + 1;
        self.lines.push_back(line);
        while self.line_bytes > self.cap {
            let Some((_, dropped)) = self.lines.pop_front() else {
                break;
            };
            self.line_bytes -= dropped.len() + 1;
        }
    }

    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &(u64, String)> {
        self.lines.iter()
    }

    /// The unfinished last line, without escape codes.
    pub fn pending(&self) -> String {
        self.screen.pending()
    }

    /// The debugger prompt the unfinished last line starts with, if it's short
    /// enough to be one.
    pub fn prompt(&self) -> Option<String> {
        if self.screen.pending_len() > 512 {
            return None;
        }
        carte_core::debugger::prompt(&self.screen.pending()).map(str::to_string)
    }

    pub fn tail(&self, n: usize) -> Vec<String> {
        let skip = self.lines.len().saturating_sub(n);
        self.lines
            .iter()
            .skip(skip)
            .map(|(_, l)| l.clone())
            .collect()
    }

    pub fn bytes(&self) -> usize {
        self.raw.len() + self.line_bytes
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Identifies a process in log lines.
#[derive(Debug, Clone)]
pub struct LogLabel {
    pub project: String,
    pub app: String,
    pub stack: String,
    pub process: String,
}

pub enum Event {
    Started {
        pid: u32,
        /// The pty clients attach to; `None` with pipes.
        terminal: Option<Arc<Terminal>>,
    },
    SpawnFailed(String),
    /// Its last line became a debugger prompt waiting for input (`Some`, the prompt),
    /// or stopped being one.
    Paused(Option<String>),
    /// `code` is `None` when a signal ended the process.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// How long output must be quiet before lines left on the prompt screen end, so a
/// prompt answered by a process that then stays silent counts as answered. Reline
/// writes each redraw in one go, so a second of quiet in the middle of one is rare.
const SCREEN_QUIET: Duration = Duration::from_secs(1);

/// How long output must be quiet before a pause ends whose cursor sits on a blank
/// line, though no line ended: pry leaves the cursor on a blank row it moved down
/// to after rewriting an answered prompt, where the screen never settles. A row
/// with text, like a completion dialog's, keeps the pause.
/// Longer than Reline's half-second wait for a cursor position before each prompt
/// and than any stall inside one redraw, short enough not to mislead for long.
const PROMPT_QUIET: Duration = Duration::from_secs(3);

/// Whether the process waits at a debugger prompt now: it stays at the one it was
/// at until a line ends on the prompt screen, or, after [`PROMPT_QUIET`], until the
/// cursor's line is blank.
fn pause_after(paused: &Option<String>, log: &mut ProcLog, gone_quiet: bool) -> Option<String> {
    let answered = log.take_answered();
    match paused {
        Some(prompt) if !answered && !(gone_quiet && log.pending().trim().is_empty()) => {
            Some(prompt.clone())
        }
        _ => log.prompt(),
    }
}

/// Runs a process until it exits or `stop` fires. Every event goes to `events`.
pub async fn run(
    spawner: Arc<Spawner>,
    spec: SpawnSpec,
    log: Arc<Mutex<ProcLog>>,
    label: LogLabel,
    log_tx: broadcast::Sender<LogLine>,
    stop: oneshot::Receiver<Duration>,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut handle = match spawner.spawn(&spec).await {
        Ok(handle) => handle,
        Err(err) => {
            let _ = events.send(Event::SpawnFailed(err.to_string()));
            return;
        }
    };
    let terminal = match (handle.pty(), spec.io) {
        (Some(pty), IoMode::Pty { cols, rows }) => {
            Some(Terminal::new(pty, log.clone(), TermSize { cols, rows }))
        }
        _ => None,
    };
    let _ = events.send(Event::Started {
        pid: handle.pid(),
        terminal: terminal.clone(),
    });

    let reader = handle.take_output().map(|mut output| {
        let log = log.clone();
        let label = label.clone();
        let log_tx = log_tx.clone();
        let terminal = terminal.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            let mut paused: Option<String> = None;
            let mut quiet_since = tokio::time::Instant::now();
            loop {
                let wait = {
                    let log = log.lock().unwrap();
                    let quiet = quiet_since.elapsed();
                    [
                        log.unsettled().then_some(Duration::from_millis(50)),
                        log.screen_unsettled()
                            .then(|| SCREEN_QUIET.saturating_sub(quiet)),
                        (paused.is_some() && quiet < PROMPT_QUIET).then(|| PROMPT_QUIET - quiet),
                    ]
                    .into_iter()
                    .flatten()
                    .min()
                };
                let read = match wait {
                    Some(wait) => tokio::time::timeout(wait, output.read(&mut buf)).await.ok(),
                    None => Some(output.read(&mut buf).await),
                };
                let data = match read {
                    Some(Ok(0) | Err(_)) => break,
                    Some(Ok(n)) => {
                        quiet_since = tokio::time::Instant::now();
                        Some(&buf[..n])
                    }
                    None => None,
                };
                let (lines, prompt) = {
                    let mut log = log.lock().unwrap();
                    let quiet = quiet_since.elapsed();
                    let lines = match data {
                        Some(data) => log.push(data),
                        None => {
                            if quiet >= SCREEN_QUIET {
                                log.settle_screen();
                            }
                            log.settle()
                        }
                    };
                    let prompt = terminal.as_ref().and_then(|terminal| {
                        if let Some(data) = data {
                            terminal.output(data);
                        }
                        pause_after(&paused, &mut log, data.is_none() && quiet >= PROMPT_QUIET)
                    });
                    (lines, prompt)
                };
                broadcast_lines(&log_tx, &label, lines);
                if prompt.is_some() != paused.is_some() {
                    paused = prompt.clone();
                    let _ = events.send(Event::Paused(prompt));
                }
            }
        })
    });

    let status = tokio::select! {
        status = handle.wait() => status,
        Ok(grace) = stop => handle.stop(grace).await,
    };

    if let Some(reader) = reader {
        // A self-daemonised child may hold the pty open forever.
        let abort = reader.abort_handle();
        if tokio::time::timeout(Duration::from_millis(500), reader)
            .await
            .is_err()
        {
            abort.abort();
        }
    }
    let flushed = log.lock().unwrap().flush();
    broadcast_lines(&log_tx, &label, flushed);

    let event = match status {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            Event::Exited {
                code: status.code(),
                signal: status.signal(),
            }
        }
        Err(err) => Event::SpawnFailed(format!("lost track of the process: {err}")),
    };
    if let Some(terminal) = terminal {
        let how = match &event {
            Event::Exited {
                code: Some(code), ..
            } => format!("exited with code {code}"),
            Event::Exited {
                signal: Some(signal),
                ..
            } => format!("was killed by signal {signal}"),
            _ => "exited".to_string(),
        };
        terminal.end(format!("{} {how}", label.process));
    }
    let _ = events.send(event);
}

fn broadcast_lines(tx: &broadcast::Sender<LogLine>, label: &LogLabel, lines: Vec<(u64, String)>) {
    if tx.receiver_count() == 0 {
        return;
    }
    for (at_ms, text) in lines {
        let _ = tx.send(LogLine {
            project: label.project.clone(),
            app: label.app.clone(),
            stack: label.stack.clone(),
            process: label.process.clone(),
            at_ms,
            text,
            marker: false,
        });
    }
}

/// Sleeps for `timeout`, not counting the time `paused` is true: a process at a
/// debugger prompt doesn't run out of time.
pub async fn held_sleep(timeout: Duration, mut paused: watch::Receiver<bool>) {
    let mut left = timeout;
    loop {
        if *paused.borrow_and_update() {
            if paused.changed().await.is_err() {
                break;
            }
            continue;
        }
        let since = tokio::time::Instant::now();
        tokio::select! {
            () = tokio::time::sleep(left) => return,
            changed = paused.changed() => {
                left = left.saturating_sub(since.elapsed());
                if changed.is_err() {
                    break;
                }
            }
        }
    }
    tokio::time::sleep(left).await;
}

/// Waits until the process answers on its port, or `timeout` passes. While `paused`
/// is true it isn't asked, and the time doesn't count.
pub async fn wait_ready(
    port: u16,
    readiness: &Readiness,
    timeout: Duration,
    paused: watch::Receiver<bool>,
) -> Result<(), String> {
    let mut running = paused.clone();
    let check = async {
        loop {
            let _ = running.wait_for(|paused| !paused).await;
            if probe(port, readiness).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::select! {
        () = check => Ok(()),
        () = held_sleep(timeout, paused) => {
            let what = match readiness {
                Readiness::Port => format!("accept connections on port {port}"),
                Readiness::Http(path) => format!("answer GET {path} on port {port}"),
            };
            Err(format!(
                "did not {what} within {}",
                carte_core::duration::format(timeout)
            ))
        }
    }
}

async fn probe(port: u16, readiness: &Readiness) -> bool {
    for ip in [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ] {
        let Ok(Ok(stream)) =
            tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(ip)).await
        else {
            continue;
        };
        if crate::loopback::connected_to_itself(&stream, stream.local_addr(), stream.peer_addr()) {
            continue;
        }
        return match readiness {
            Readiness::Port => true,
            Readiness::Http(path) => http_ok(stream, port, path).await,
        };
    }
    false
}

async fn http_ok(mut stream: TcpStream, port: u16, path: &str) -> bool {
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut head = [0u8; 12];
    let Ok(Ok(_)) =
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut head)).await
    else {
        return false;
    };
    let status = std::str::from_utf8(&head[9..12])
        .ok()
        .and_then(|s| s.parse::<u16>().ok());
    matches!(status, Some(code) if code < 500)
}

/// A free port on loopback, found by binding port 0.
pub fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_keeps_lines_within_the_cap() {
        let mut log = ProcLog::new(20);
        let lines = log.push(b"one\ntwo\nthree\nfour\nfive\nsix");
        assert_eq!(lines.len(), 5);
        assert_eq!(log.tail(10), vec!["two", "three", "four", "five"]);
        let flushed: Vec<String> = log.flush().into_iter().map(|l| l.1).collect();
        assert_eq!(flushed, ["six"]);
        assert_eq!(log.tail(10), vec!["three", "four", "five", "six"]);
    }

    #[test]
    fn a_prompt_redrawn_by_reline_is_still_a_prompt() {
        let mut log = ProcLog::new(64 * 1024);
        log.push(b"[3, 12] in app/models/order.rb\r\n=>  12|     binding.break\r\n");
        log.push(b"\x1b[?2004h\x1b[1G(rdbg) \x1b[8G");
        assert_eq!(log.prompt().as_deref(), Some("(rdbg)"));
        log.push(b"\x1b[1G\x1b[K(rdbg) inf\x1b[11G\x1b[1B\x1b[1G\x1b[K info\x1b[1A\x1b[11G");
        assert_eq!(log.prompt().as_deref(), Some("(rdbg)"));
        log.push(b"o\r\n\x1b[?2004l");
        assert_eq!(log.prompt(), None);
        log.push(b"\x1b[?2004h\x1b[1G\xe2\x96\xbd\x1b[6n\x1b[1G\x1b[K\x1b[6n\x1b[?25l\x1b[1G\x1b[K\x1b[1G\x1b[0m(rdbg) \x1b[0m\x1b[?25h\x1b[8G");
        assert_eq!(
            log.prompt().as_deref(),
            Some("(rdbg)"),
            "after reline's width probe"
        );
        let mut lines = log.push(b"\x1b[?25l\x1b[1G\x1b[K\x1b[?25h\x1b[1G(rdbg) \x1b[36m\x1b[4mc\x1b[0m\x1b[2m    # continue command\x1b[0m \r\r\n\x1b[1G\x1b[?2004l");
        lines.extend(log.settle());
        assert_eq!(lines.len(), 1, "answering ends the line once settled");
        assert_eq!(log.prompt(), None);
        log.push(b"irb:rdbg(#<Order:0x01>):002> ");
        assert_eq!(
            log.prompt().as_deref(),
            Some("irb:rdbg(#<Order:0x01>):002>")
        );
    }

    #[test]
    fn completion_room_at_the_bottom_of_the_terminal_keeps_the_prompt() {
        let mut log = ProcLog::new(64 * 1024);
        log.push(b"\x1b[?25l\x1b[1G\x1b[K\x1b[1G\x1b[0mirb(main):001> \x1b[0m\x1b[?25h\x1b[16G");
        assert_eq!(log.prompt().as_deref(), Some("irb(main):001>"));
        let lines = log.push(b"\x1b[?25l\r\n\r\n\x1b[2A\x1b[16G\x1b[0mS\x1b[0m\x1b[1B\x1b[1G\x1b[K\x1b[16G\x1b[97;100mSTDERR \x1b[0m\x1b[1B\x1b[1G\x1b[K\x1b[16G\x1b[97;100mSTDIN  \x1b[0m\x1b[?25h\x1b[17G\x1b[2A");
        assert!(lines.is_empty(), "still paused while typing: {lines:?}");
        assert_eq!(log.prompt().as_deref(), Some("irb(main):001>"));
        let lines = log.push(b"\x1b[?25l\x1b[1G\x1b[K\x1b[1B\x1b[1G\x1b[K\x1b[1B\x1b[1G\x1b[K\x1b[?25h\x1b[1G\x1b[2Airb(main):001> S\r\r\n\x1b[1G\x1b[?2004l=> nil\r\n\x1b[?2004h\x1b[6n\x1b[?25l\x1b[1G\x1b[K\x1b[1G\x1b[0mirb(main):002> \x1b[0m\x1b[?25h\x1b[16G");
        let texts: Vec<&str> = lines.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(texts, ["irb(main):001> S", "=> nil"]);
        assert_eq!(log.prompt().as_deref(), Some("irb(main):002>"));
    }

    /// Captured debugger sessions (irb, pry, rdbg, with Reline), each as the reads a
    /// pty gave, at most 1024 bytes each.
    fn sessions() -> Vec<(String, Vec<Vec<u8>>)> {
        let json: serde_json::Value =
            serde_json::from_str(include_str!("testdata/reline-sessions.json")).unwrap();
        let hex = |text: &str| -> Vec<u8> {
            (0..text.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
                .collect()
        };
        json.as_object()
            .unwrap()
            .iter()
            .map(|(name, chunks)| {
                let chunks = chunks
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|chunk| hex(chunk.as_str().unwrap()))
                    .collect();
                (name.clone(), chunks)
            })
            .collect()
    }

    /// What the output reader makes of the reads: the lines, whether it is paused
    /// and at which prompt, each change in turn, and the unfinished line at the end.
    type Replayed = (Vec<String>, Vec<Option<String>>, String);

    fn replay<'a>(reads: impl IntoIterator<Item = &'a [u8]>) -> Replayed {
        replay_with(reads.into_iter().map(Step::Read))
    }

    /// What reaches the output reader: a read, a short quiet that settles the
    /// log, a second's that settles the prompt screen too, or one long enough to
    /// end a pause whose prompt is gone.
    #[derive(Clone, Copy)]
    enum Step<'a> {
        Read(&'a [u8]),
        Settle,
        Quiet,
        LongQuiet,
    }

    fn replay_with<'a>(steps: impl IntoIterator<Item = Step<'a>>) -> Replayed {
        let mut log = ProcLog::new(1024 * 1024);
        let mut lines = Vec::new();
        let mut paused: Option<String> = None;
        let mut states = vec![None];
        for step in steps {
            let finished = match step {
                Step::Read(read) => log.push(read),
                Step::Settle => log.settle(),
                Step::Quiet | Step::LongQuiet => {
                    log.settle_screen();
                    log.settle()
                }
            };
            paused = pause_after(&paused, &mut log, matches!(step, Step::LongQuiet));
            if states.last() != Some(&paused) {
                states.push(paused.clone());
            }
            lines.extend(finished.into_iter().map(|(_, text)| text));
        }
        lines.extend(log.settle().into_iter().map(|(_, text)| text));
        (lines, states, log.pending())
    }

    /// The pauses without a resume between two different prompts, which shows when
    /// a result and the next prompt arrive in different reads but not in one: a
    /// `None` between `Some(a)` and `Some(b)` with `a` other than `b` is dropped.
    /// So the comparison can't tell a missed resume between two different
    /// breakpoints.
    fn normalized((lines, states, pending): Replayed) -> Replayed {
        let mut kept: Vec<Option<String>> = Vec::new();
        for (i, state) in states.iter().enumerate() {
            let between = state.is_none()
                && matches!(
                    (kept.last(), states.get(i + 1)),
                    (Some(Some(before)), Some(Some(after))) if before != after
                );
            if !between && kept.last() != Some(state) {
                kept.push(state.clone());
            }
        }
        (lines, kept, pending)
    }

    /// Except `rdbgfork`, whose prompt is wider than its pty, where detection
    /// depends on where reads end.
    #[test]
    fn captured_sessions_read_the_same_however_their_reads_are_split() {
        let mut seed: u64 = 0x5eed;
        let mut random = move |below: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % below
        };
        for (name, chunks) in sessions() {
            if name == "rdbgfork" {
                continue;
            }
            let whole = normalized(replay(chunks.iter().map(Vec::as_slice)));
            let bytes = chunks.iter().flat_map(|chunk| chunk.chunks(1));
            assert_eq!(normalized(replay(bytes)), whole, "{name}, a byte at a time");
            for _ in 0..50 {
                let mut reads = Vec::new();
                for chunk in &chunks {
                    let mut at = 0;
                    while at < chunk.len() {
                        let n = (1 + random(64)).min(chunk.len() - at);
                        reads.push(&chunk[at..at + n]);
                        at += n;
                    }
                }
                assert_eq!(normalized(replay(reads)), whole, "{name}, split at random");
            }
            let mut merged: Vec<Vec<u8>> = Vec::new();
            for chunk in &chunks {
                match merged.last_mut() {
                    Some(last) if last.len() + chunk.len() <= 1024 => last.extend(chunk),
                    _ => merged.push(chunk.clone()),
                }
            }
            let (lines, _, pending) = replay(merged.iter().map(Vec::as_slice));
            assert_eq!(
                (&lines, &pending),
                (&whole.0, &whole.2),
                "{name}, reads merged"
            );
            let quiet = chunks
                .iter()
                .flat_map(|chunk| [Step::Read(chunk), Step::Quiet]);
            let (_, states, pending) = normalized(replay_with(quiet));
            assert_eq!(
                (&states, &pending),
                (&whole.1, &whole.2),
                "{name}, quiet after each read"
            );
        }
    }

    /// The reads of a session with `step` at offset `at` of read `i`.
    fn with_step_at<'a>(
        chunks: &'a [Vec<u8>],
        i: usize,
        at: usize,
        step: Step<'a>,
    ) -> Vec<Step<'a>> {
        let mut steps: Vec<Step> = chunks[..i].iter().map(|c| Step::Read(c)).collect();
        steps.extend([
            Step::Read(&chunks[i][..at]),
            step,
            Step::Read(&chunks[i][at..]),
        ]);
        steps.extend(chunks[i + 1..].iter().map(|c| Step::Read(c)));
        steps
    }

    /// Whether `expected` appears in `lines` in order, with other lines between.
    fn in_order(expected: &[String], lines: &[String]) -> bool {
        let mut lines = lines.iter();
        expected.iter().all(|line| lines.any(|l| l == line))
    }

    /// A settle of the log at any point, even in the middle of a redraw, leaves the
    /// pauses alone and loses no line, though it may repeat some or log rows left on
    /// the screen.
    #[test]
    fn settling_the_log_at_any_point_leaves_the_pauses_alone() {
        for (name, chunks) in sessions() {
            if name == "rdbgfork" {
                continue;
            }
            let whole = normalized(replay(chunks.iter().map(Vec::as_slice)));
            for (i, chunk) in chunks.iter().enumerate() {
                for at in 0..=chunk.len() {
                    let steps = with_step_at(&chunks, i, at, Step::Settle);
                    let (lines, states, pending) = normalized(replay_with(steps));
                    assert_eq!(
                        (&states, &pending),
                        (&whole.1, &whole.2),
                        "{name}, settled at {at} of read {i}"
                    );
                    assert!(
                        in_order(&whole.0, &lines),
                        "{name}, settled at {at} of read {i}: {lines:?}"
                    );
                }
            }
        }
    }

    /// A second of quiet at any point leaves the pauses alone, except in the middle
    /// of irb making room below its prompt at the bottom of the terminal, where
    /// the newlines it writes first look like an answer: there the pause ends for
    /// a moment and starts again at the same prompt.
    #[test]
    fn a_quiet_second_at_any_point_leaves_the_pauses_alone_but_one() {
        for (name, chunks) in sessions() {
            if name == "rdbgfork" {
                continue;
            }
            let whole = normalized(replay(chunks.iter().map(Vec::as_slice)));
            let mut resumes = 0;
            for (i, chunk) in chunks.iter().enumerate() {
                for at in 0..=chunk.len() {
                    let steps = with_step_at(&chunks, i, at, Step::Quiet);
                    let (lines, states, pending) = normalized(replay_with(steps));
                    assert_eq!(pending, whole.2, "{name}, quiet at {at} of read {i}");
                    assert!(
                        in_order(&whole.0, &lines),
                        "{name}, quiet at {at} of read {i}: {lines:?}"
                    );
                    if states == whole.1 {
                        continue;
                    }
                    let mut resumed = whole.1.clone();
                    let at_prompt = whole.1[1].clone();
                    resumed.splice(2..2, [None, at_prompt]);
                    assert!(
                        name == "irbbottom" && i == 6 && states == resumed,
                        "{name}, quiet at {at} of read {i}: {states:?}"
                    );
                    resumes += 1;
                }
            }
            assert_eq!(
                resumes > 0,
                name == "irbbottom",
                "{name}: {resumes} resumes"
            );
        }
    }

    /// pry rewrites an answered prompt and leaves the cursor on a row it moved down
    /// to, so only a longer quiet ends its pause when nothing follows.
    #[test]
    fn a_long_quiet_after_pry_is_answered_ends_the_pause() {
        let rewrite: &[u8] = b"\x1b[1A\x1b[0G";
        for (name, chunks) in sessions() {
            if !name.contains("pry") {
                continue;
            }
            let answered = chunks
                .iter()
                .position(|chunk| chunk.windows(rewrite.len()).any(|w| w == rewrite))
                .unwrap_or_else(|| panic!("{name} rewrites no prompt"));
            let reads = chunks[..=answered].iter().map(|c| Step::Read(c));
            let (_, states, _) = replay_with(reads.clone().chain([Step::Quiet]));
            assert!(
                states.last().unwrap().is_some(),
                "{name}: a second isn't enough"
            );
            let (_, states, _) = replay_with(reads.chain([Step::LongQuiet]));
            assert_eq!(states.last(), Some(&None), "{name}: {states:?}");
        }
    }

    /// A long quiet after any read ends no pause whose prompt is still there. Not
    /// after a read that filled the pty's 1024-byte buffer: the rest of that write
    /// is already waiting, and a quiet there could look like pry's answer.
    #[test]
    fn a_long_quiet_at_a_prompt_leaves_it_alone() {
        for (name, chunks) in sessions() {
            if name == "rdbgfork" {
                continue;
            }
            let whole = normalized(replay(chunks.iter().map(Vec::as_slice)));
            let steps = chunks.iter().flat_map(|chunk| {
                let quiet = (chunk.len() < 1024).then_some(Step::LongQuiet);
                std::iter::once(Step::Read(chunk)).chain(quiet)
            });
            let (_, states, pending) = normalized(replay_with(steps));
            assert_eq!((&states, &pending), (&whole.1, &whole.2), "{name}");
        }
    }

    #[test]
    fn a_prompt_answered_by_a_process_that_then_stays_silent_ends_the_pause() {
        let steps = [
            Step::Read(b"\x1b[?2004h\x1b[1G(rdbg) \x1b[8G"),
            Step::Read(b"\x1b[1G(rdbg) c\r\r\n\x1b[1G\x1b[?2004l"),
            Step::Settle,
        ];
        let (_, states, _) = replay_with(steps);
        assert_eq!(
            states,
            [None, Some("(rdbg)".to_string())],
            "a short quiet isn't enough"
        );
        let (lines, states, _) = replay_with(steps.into_iter().chain([Step::Quiet]));
        assert_eq!(states, [None, Some("(rdbg)".to_string()), None]);
        assert_eq!(lines, ["(rdbg) c"]);
    }

    #[test]
    fn captured_sessions_pause_at_each_prompt_and_resume_on_answers() {
        let sessions: std::collections::BTreeMap<_, _> = sessions().into_iter().collect();
        let irb = |id: &str, n: &str| Some(format!("irb(#<Foo:0x0000000{id}>):00{n}>"));
        let pry = |n: &str| Some(format!("[{n}] pry(#<Foo>)>"));
        let rdbg = || Some("(rdbg)".to_string());
        let expected: [(&str, Vec<Option<String>>, &str); 13] = [
            (
                "dsr-irb-answer",
                vec![None, irb("1242c4960", "1"), irb("1242c4960", "2")],
                "=> 1",
            ),
            (
                "dsr-irb-silent",
                vec![None, irb("124cd4b08", "1"), irb("124cd4b08", "2")],
                "=> 1",
            ),
            ("dsr-pry-answer", vec![None, pry("1"), pry("2")], "=> 1"),
            ("dsr-pry-silent", vec![None, pry("1"), pry("2")], "=> 1"),
            ("dsr-rdbg-answer", vec![None, rdbg(), None, rdbg()], "1"),
            ("dsr-rdbg-silent", vec![None, rdbg(), None, rdbg()], "1"),
            (
                "irbbottom",
                vec![None, irb("1244349d0", "1"), irb("1244349d0", "2")],
                "=> String",
            ),
            (
                "irbcomp",
                vec![None, irb("124be49f0", "1"), irb("124be49f0", "2")],
                "=> String",
            ),
            (
                "irbconsole",
                vec![
                    None,
                    Some("irb:rdbg(#<Foo:0x000000011f2f53a8>):002>".into()),
                    None,
                ],
                "done",
            ),
            (
                "irbdebug",
                vec![
                    None,
                    irb("124fb49e0", "1"),
                    Some("irb:rdbg(#<Foo:0x0000000124fb49e0>):002>".into()),
                    None,
                ],
                "done",
            ),
            ("pry-v1", vec![None, pry("1"), None], "done"),
            ("prybottom", vec![None, pry("1"), pry("2")], "=> String"),
            ("prycolor", vec![None, pry("1"), pry("2")], "=> 1"),
        ];
        assert!(
            sessions["irbcomp"].iter().any(|read| read.len() == 1024),
            "the completion dialog arrives split across reads"
        );
        for (name, states, last) in expected {
            let (lines, got, _) = normalized(replay(sessions[name].iter().map(Vec::as_slice)));
            assert_eq!(got, states, "{name}");
            assert_eq!(
                lines.last().map(String::as_str),
                Some(last),
                "{name}: {lines:?}"
            );
        }
    }

    #[test]
    fn a_long_unfinished_line_is_never_a_prompt() {
        let mut log = ProcLog::new(64 * 1024);
        log.push(b"(rdbg) ");
        log.push(&[b'x'; 600]);
        assert_eq!(log.prompt(), None);
    }

    #[test]
    fn log_strips_escape_codes() {
        let mut log = ProcLog::new(1024);
        log.push(b"\x1b[32mgreen\x1b[0m\n");
        log.settle();
        assert_eq!(log.tail(1), vec!["green"]);
    }

    #[tokio::test]
    async fn a_held_sleep_stands_still_while_paused() {
        let (paused, rx) = watch::channel(false);
        let started = tokio::time::Instant::now();
        let sleeping = tokio::spawn(held_sleep(Duration::from_millis(400), rx));
        tokio::time::sleep(Duration::from_millis(100)).await;
        paused.send_replace(true);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!sleeping.is_finished(), "time doesn't count while paused");
        paused.send_replace(false);
        sleeping.await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(1000));
    }

    #[tokio::test]
    async fn readiness_times_out_with_a_readable_message() {
        let (_paused, rx) = watch::channel(false);
        let err = wait_ready(1, &Readiness::Port, Duration::from_millis(300), rx)
            .await
            .unwrap_err();
        assert!(err.contains("accept connections on port 1"), "{err}");
    }

    #[tokio::test]
    async fn readiness_sees_a_listener() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_paused, rx) = watch::channel(false);
        wait_ready(port, &Readiness::Port, Duration::from_secs(2), rx)
            .await
            .unwrap();
    }
}
