//! One task per running process: spawn it, feed its output into the log, report
//! when it exits, and stop it on request.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use carte_api::LogLine;
use carte_core::model::Readiness;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::logs::{LogBuffer, Stripper};
use crate::process::{SpawnSpec, Spawner};

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
        let at = now_ms();
        let lines: Vec<(u64, String)> = self
            .stripper
            .feed(data)
            .into_iter()
            .map(|line| (at, line))
            .collect();
        for line in &lines {
            self.append(line.clone());
        }
        lines
    }

    /// Ends a partial last line, as when the process exits.
    pub fn flush(&mut self) -> Option<(u64, String)> {
        let pending = self.stripper.pending();
        if pending.is_empty() {
            return None;
        }
        self.stripper = Stripper::default();
        let line = (now_ms(), pending);
        self.append(line.clone());
        Some(line)
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
    },
    SpawnFailed(String),
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
    let _ = events.send(Event::Started { pid: handle.pid() });

    let reader = handle.take_output().map(|mut output| {
        let log = log.clone();
        let label = label.clone();
        let log_tx = log_tx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match output.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let lines = log.lock().unwrap().push(&buf[..n]);
                        broadcast_lines(&log_tx, &label, lines);
                    }
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
    broadcast_lines(&log_tx, &label, flushed.into_iter().collect());

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
        assert_eq!(log.flush().map(|l| l.1), Some("six".into()));
        assert_eq!(log.tail(10), vec!["three", "four", "five", "six"]);
    }

    #[test]
    fn log_strips_escape_codes() {
        let mut log = ProcLog::new(1024);
        log.push(b"\x1b[32mgreen\x1b[0m\n");
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
