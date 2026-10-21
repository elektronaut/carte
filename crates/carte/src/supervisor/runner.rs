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
use tokio::sync::{broadcast, mpsc, oneshot};

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
}

impl ProcLog {
    pub fn new(cap: usize) -> Self {
        ProcLog {
            raw: LogBuffer::new(cap),
            lines: VecDeque::new(),
            line_bytes: 0,
            cap,
            stripper: Stripper::default(),
        }
    }

    /// Adds output and returns the complete lines it finished.
    pub fn push(&mut self, data: &[u8]) -> Vec<(u64, String)> {
        self.raw.push(data);
        let lines = self.stripper.feed(data);
        self.add(lines)
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
        let mut lines = self.stripper.settle();
        let pending = self.stripper.pending();
        if !pending.is_empty() {
            lines.push(pending);
        }
        self.stripper = Stripper::default();
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
        self.stripper.pending()
    }

    /// The debugger prompt the unfinished last line starts with, if it's short
    /// enough to be one.
    pub fn prompt(&self) -> Option<String> {
        if self.stripper.pending_len() > 512 {
            return None;
        }
        carte_core::debugger::prompt(&self.stripper.pending()).map(str::to_string)
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
            loop {
                let unsettled = log.lock().unwrap().unsettled();
                let read = if unsettled {
                    tokio::time::timeout(Duration::from_millis(50), output.read(&mut buf))
                        .await
                        .ok()
                } else {
                    Some(output.read(&mut buf).await)
                };
                let data = match read {
                    Some(Ok(0) | Err(_)) => break,
                    Some(Ok(n)) => Some(&buf[..n]),
                    None => None,
                };
                let (lines, prompt) = {
                    let mut log = log.lock().unwrap();
                    let lines = match data {
                        Some(data) => log.push(data),
                        None => log.settle(),
                    };
                    let prompt = terminal.as_ref().and_then(|terminal| {
                        if let Some(data) = data {
                            terminal.output(data);
                        }
                        match &paused {
                            Some(prompt) if lines.is_empty() => Some(prompt.clone()),
                            _ => log.prompt(),
                        }
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

/// Waits until the process answers on its port, or `timeout` passes.
pub async fn wait_ready(port: u16, readiness: &Readiness, timeout: Duration) -> Result<(), String> {
    let check = async {
        loop {
            if probe(port, readiness).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(timeout, check).await.map_err(|_| {
        let what = match readiness {
            Readiness::Port => format!("accept connections on port {port}"),
            Readiness::Http(path) => format!("answer GET {path} on port {port}"),
        };
        format!(
            "did not {what} within {}",
            carte_core::duration::format(timeout)
        )
    })
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
        let mut log = ProcLog::new(1024 * 1024);
        let mut lines = Vec::new();
        let mut paused: Option<String> = None;
        let mut states = vec![None];
        for read in reads {
            let finished = log.push(read);
            paused = match &paused {
                Some(prompt) if finished.is_empty() => Some(prompt.clone()),
                _ => log.prompt(),
            };
            if states.last() != Some(&paused) {
                states.push(paused.clone());
            }
            lines.extend(finished.into_iter().map(|(_, text)| text));
        }
        lines.extend(log.settle().into_iter().map(|(_, text)| text));
        (lines, states, log.pending())
    }

    /// The pauses without a resume between two different prompts, which shows when
    /// a result and the next prompt arrive in different reads but not in one.
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
        }
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
    async fn readiness_times_out_with_a_readable_message() {
        let err = wait_ready(1, &Readiness::Port, Duration::from_millis(300))
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
        wait_ready(port, &Readiness::Port, Duration::from_secs(2))
            .await
            .unwrap();
    }
}
