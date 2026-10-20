//! `carte attach`: connects the terminal to a running process's pty.

use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use carte_api::attach::{self, AttachRequest, Attached, Frame, FrameReader, TermSize};
use carte_api::paths;
use carte_core::detach::DetachKeys;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use nix::sys::termios::{self, SetArg, Termios};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use super::{AttachArgs, control};
use crate::client::Client;
use crate::exit::{Exit, Failure};
use crate::paths::Paths;

/// An attach session from the client's side.
pub(crate) struct Connection {
    reader: ReadHalf<TokioIo<Upgraded>>,
    writer: WriteHalf<TokioIo<Upgraded>>,
    frames: FrameReader,
    buf: Vec<u8>,
    pub attached: Attached,
}

impl Connection {
    /// Attaches and waits for the daemon to say to what.
    pub async fn open(socket: &Path, request: &AttachRequest) -> anyhow::Result<Self> {
        let io = Client::new(socket)
            .upgrade(paths::ATTACH, attach::PROTOCOL, request)
            .await?;
        let (reader, writer) = tokio::io::split(io);
        let mut connection = Connection {
            reader,
            writer,
            frames: FrameReader::default(),
            buf: vec![0u8; 16 * 1024],
            attached: Attached {
                instance: String::new(),
                process: String::new(),
            },
        };
        match connection.next().await? {
            Some(Frame::Attached(attached)) => connection.attached = attached,
            _ => anyhow::bail!("the daemon didn't attach"),
        }
        Ok(connection)
    }

    /// The next frame from the daemon; `None` once it closes the connection. Nothing
    /// is lost when the future is dropped before it finishes.
    pub async fn next(&mut self) -> anyhow::Result<Option<Frame>> {
        loop {
            if let Some(frame) = self.frames.next_frame()? {
                return Ok(Some(frame));
            }
            let n = self.reader.read(&mut self.buf).await?;
            if n == 0 {
                return Ok(None);
            }
            self.frames.feed(&self.buf[..n]);
        }
    }

    pub async fn send(&mut self, frame: &Frame) -> std::io::Result<()> {
        self.writer.write_all(&frame.encode()).await
    }

    /// `app@stack/process`.
    pub fn label(&self) -> String {
        format!("{}/{}", self.attached.instance, self.attached.process)
    }
}

/// How a session ended.
enum End {
    Detached,
    Exited(String),
    Lost,
    /// A signal told the client to stop.
    Signalled(i32),
}

enum Step {
    Typed(Option<Vec<u8>>),
    Frame(anyhow::Result<Option<Frame>>),
    Resized,
    Signalled(i32),
    Quiet,
    TimeUp,
}

/// What a process may have left on that the terminal shouldn't keep after a detach:
/// colours, a hidden cursor, bracketed paste and mouse reporting.
const RESET_MODES: &[u8] =
    b"\x1b[0m\x1b[?25h\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";

pub fn run(paths: &Paths, args: AttachArgs) -> anyhow::Result<ExitCode> {
    let keys = match &args.detach_keys {
        Some(text) => text
            .parse::<DetachKeys>()
            .map_err(|err| Failure::new(Exit::Usage, err.to_string()))?,
        None => {
            carte_core::config::load_machine_config(&paths.machine_config())?
                .attach
                .detach_keys
        }
    };
    let tty = std::io::stdin().is_terminal();
    let request = AttachRequest {
        target: control::address(&args.target),
        process: args.process.clone(),
        size: tty.then(terminal_size).flatten(),
        replay: tty,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut signals = Signals::new()?;
        let socket = paths.socket();
        let mut connection = tokio::select! {
            connection = Connection::open(&socket, &request) => connection?,
            signal = stopped(&mut signals.stops) => return Ok(ExitCode::from(128 + signal as u8)),
        };
        let label = connection.label();
        if tty {
            eprintln!("Attached to {label}. {keys} detaches.");
        }
        let raw = tty.then(RawMode::enable).transpose()?;
        let timing = Timing {
            quiet: Duration::from_millis(args.quiet),
            wait: args.wait,
        };
        let end = session(&mut connection, &keys, tty, timing, &mut signals).await;
        if tty {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(RESET_MODES);
            let _ = stdout.flush();
        }
        drop(raw);
        Ok(match end? {
            End::Detached if tty => {
                eprintln!("\nDetached from {label}.");
                ExitCode::SUCCESS
            }
            End::Detached => ExitCode::SUCCESS,
            End::Exited(message) => {
                eprintln!("\n{}: {message}", connection.attached.instance);
                ExitCode::SUCCESS
            }
            End::Lost => {
                eprintln!("\nThe daemon closed the connection to {label}.");
                ExitCode::FAILURE
            }
            End::Signalled(signal) => {
                eprintln!("\nDetached from {label}.");
                ExitCode::from(128 + signal as u8)
            }
        })
    })
}

/// The signals that end a session, and the terminal's size changes.
struct Signals {
    resized: tokio::signal::unix::Signal,
    stops: [tokio::signal::unix::Signal; 3],
}

impl Signals {
    fn new() -> std::io::Result<Self> {
        Ok(Signals {
            resized: signal(SignalKind::window_change())?,
            stops: [
                signal(SignalKind::terminate())?,
                signal(SignalKind::hangup())?,
                signal(SignalKind::interrupt())?,
            ],
        })
    }
}

/// Waits for SIGTERM, SIGHUP or SIGINT and returns its number.
async fn stopped([term, hup, int]: &mut [tokio::signal::unix::Signal; 3]) -> i32 {
    tokio::select! {
        _ = term.recv() => libc::SIGTERM,
        _ = hup.recv() => libc::SIGHUP,
        _ = int.recv() => libc::SIGINT,
    }
}

/// Without a terminal: how long the output must be quiet after the input ends, and
/// how long to wait for that at most.
struct Timing {
    quiet: Duration,
    wait: Duration,
}

/// Copies typing to the process and its output to the terminal until the detach keys,
/// the end of the process, the end of the connection or a signal. Without a terminal,
/// stdin is typed as it is, and once it has run out the session detaches when the
/// output has been quiet for `timing.quiet`, or after `timing.wait` at the latest.
async fn session(
    connection: &mut Connection,
    keys: &DetachKeys,
    tty: bool,
    timing: Timing,
    signals: &mut Signals,
) -> anyhow::Result<End> {
    let (typed_tx, mut typed) = mpsc::channel::<Vec<u8>>(16);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = vec![0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if typed_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    let mut filter = keys.filter();
    let mut stdout = std::io::stdout();
    let mut input_open = true;
    let mut deadline = None;
    loop {
        let time_up = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        let step = tokio::select! {
            data = typed.recv(), if input_open => Step::Typed(data),
            frame = connection.next() => Step::Frame(frame),
            _ = signals.resized.recv(), if tty => Step::Resized,
            signal = stopped(&mut signals.stops) => Step::Signalled(signal),
            () = tokio::time::sleep(timing.quiet), if !input_open => Step::Quiet,
            () = time_up => Step::TimeUp,
        };
        match step {
            Step::Typed(Some(data)) => {
                let (data, detach) = if tty {
                    filter.feed(&data)
                } else {
                    (data, false)
                };
                if !data.is_empty() {
                    connection.send(&Frame::Input(data)).await?;
                }
                if detach {
                    return Ok(End::Detached);
                }
            }
            Step::Typed(None) => {
                input_open = false;
                deadline = Some(tokio::time::Instant::now() + timing.wait);
            }
            Step::Frame(frame) => match frame? {
                Some(Frame::Output(data)) => {
                    stdout.write_all(&data)?;
                    stdout.flush()?;
                }
                Some(Frame::Ended(message)) => return Ok(End::Exited(message)),
                Some(_) => {}
                None => return Ok(End::Lost),
            },
            Step::Resized => {
                if let Some(size) = terminal_size() {
                    connection.send(&Frame::Resize(size)).await?;
                }
            }
            Step::Signalled(signal) => return Ok(End::Signalled(signal)),
            Step::Quiet | Step::TimeUp => return Ok(End::Detached),
        }
    }
}

/// Types `input` into a process and returns what it printed until its output was
/// quiet for `quiet`, or for `wait` at most, without escape codes and keeping the
/// last 64 KB: for agents, which have no terminal to attach.
pub(crate) fn type_into(
    paths: &Paths,
    target: &super::Target,
    process: Option<String>,
    input: Vec<u8>,
    wait: Duration,
    quiet: Duration,
) -> anyhow::Result<String> {
    let request = AttachRequest {
        target: control::address(target),
        process,
        size: None,
        replay: false,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut connection = Connection::open(&paths.socket(), &request).await?;
        connection.send(&Frame::Input(input)).await?;
        let mut output = Vec::new();
        let mut ended = None;
        let deadline = tokio::time::Instant::now() + wait;
        let mut cut = false;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(quiet.min(left), connection.next()).await {
                Err(_) => break,
                Ok(frame) => match frame? {
                    Some(Frame::Output(data)) => {
                        output.extend_from_slice(&data);
                        let over = output.len().saturating_sub(64 * 1024);
                        if over > 0 {
                            output.drain(..over);
                            cut = true;
                        }
                    }
                    Some(Frame::Ended(message)) => {
                        ended = Some(message);
                        break;
                    }
                    Some(_) => {}
                    None => break,
                },
            }
        }
        let mut text = crate::logs::strip(&output);
        if cut {
            text.insert_str(0, "[earlier output cut]\n");
        }
        if let Some(message) = ended {
            text.push_str(&format!("\n{}: {message}", connection.attached.instance));
        }
        Ok(format!("{}:\n{text}", connection.label()))
    })
}

fn terminal_size() -> Option<TermSize> {
    let (cols, rows) = ratatui::crossterm::terminal::size().ok()?;
    (cols > 0 && rows > 0).then_some(TermSize { cols, rows })
}

/// The terminal in raw mode until dropped: every key reaches the process as typed.
struct RawMode {
    saved: Termios,
}

impl RawMode {
    fn enable() -> anyhow::Result<Self> {
        let stdin = std::io::stdin();
        let saved = termios::tcgetattr(&stdin).context("reading the terminal's mode")?;
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw)
            .context("switching the terminal to raw mode")?;
        Ok(RawMode { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.saved);
    }
}
