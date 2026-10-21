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

/// What a process may have left on that the terminal shouldn't keep after a detach,
/// after a CAN that ends any escape sequence its output stopped in the middle of:
/// colours, a hidden cursor, bracketed paste, mouse and focus reporting, application
/// cursor keys and keypad, the kitty keyboard protocol, modifyOtherKeys, the
/// line-drawing charset and a scroll region, which is reset without moving the
/// cursor.
const RESET_MODES: &[u8] = b"\x18\x1b[0m\x1b[?25h\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?1l\x1b>\x1b[<u\x1b[>4;0m\x1b(B\x1b7\x1b[r\x1b8";

/// What to write on detach: [`RESET_MODES`], leaving the alternate screen first if
/// the session's output left the terminal on it.
fn reset(screen: &Screen) -> Vec<u8> {
    let mut out = RESET_MODES.to_vec();
    if screen.alternate {
        out.splice(1..1, b"\x1b[?1049l".iter().copied());
    }
    out
}

/// Follows output to tell whether it left the terminal on the alternate screen.
#[derive(Debug, Default)]
struct Screen {
    alternate: bool,
    /// The bytes after the ESC of a private mode sequence in progress.
    sequence: Option<Vec<u8>>,
}

impl Screen {
    fn feed(&mut self, data: &[u8]) {
        for &b in data {
            if b == 0x1b {
                self.sequence = Some(Vec::new());
                continue;
            }
            let Some(sequence) = &mut self.sequence else {
                continue;
            };
            sequence.push(b);
            let done = match (sequence.as_slice(), b) {
                ([b'['] | [b'[', b'?'], _) => false,
                ([b'[', b'?', .., _], b'0'..=b'9' | b';') => sequence.len() > 32,
                ([b'[', b'?', params @ .., _], b'h' | b'l') => {
                    let alternate = params
                        .split(|&b| b == b';')
                        .any(|mode| matches!(mode, b"47" | b"1047" | b"1049"));
                    if alternate {
                        self.alternate = b == b'h';
                    }
                    true
                }
                _ => true,
            };
            if done {
                self.sequence = None;
            }
        }
    }
}

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
            quiet: args.quiet,
            wait: args.wait,
        };
        let mut screen = Screen::default();
        let end = session(
            &mut connection,
            &keys,
            tty,
            timing,
            &mut signals,
            &mut screen,
        )
        .await;
        if tty {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(&reset(&screen));
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
    screen: &mut Screen,
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
                    screen.feed(&data);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn detach_after(output: &[&[u8]]) -> Vec<u8> {
        let mut screen = Screen::default();
        for data in output {
            screen.feed(data);
        }
        reset(&screen)
    }

    #[test]
    fn a_detach_ends_an_unfinished_string_first() {
        let reset = detach_after(&[b"\x1b]0;x"]);
        assert_eq!(reset[0], 0x18, "CAN ends the title string");
        assert_eq!(reset, RESET_MODES);
    }

    #[test]
    fn a_detach_leaves_the_alternate_screen_the_session_entered() {
        let reset = detach_after(&[b"\x1b[?1049h\x1b[?1h\x1b="]);
        assert!(reset.starts_with(b"\x18\x1b[?1049l"), "{reset:?}");
        for mode in [
            b"\x1b[?1l".as_slice(),
            b"\x1b>",
            b"\x1b[?1004l",
            b"\x1b[<u",
            b"\x1b[>4;0m",
            b"\x1b(B",
            b"\x1b7\x1b[r\x1b8",
        ] {
            assert!(
                reset.windows(mode.len()).any(|w| w == mode),
                "{:?}",
                String::from_utf8_lossy(mode)
            );
        }
    }

    #[test]
    fn the_alternate_screen_is_followed_across_reads() {
        assert!(detach_after(&[b"\x1b[?10", b"49h"]).starts_with(b"\x18\x1b[?1049l"));
        assert!(detach_after(&[b"\x1b[?25;47h"]).starts_with(b"\x18\x1b[?1049l"));
        assert_eq!(
            detach_after(&[b"\x1b[?1049h less \x1b[?1049l"]),
            RESET_MODES,
            "left already"
        );
        assert_eq!(detach_after(&[b"\x1b[1049h \x1b[?10049h"]), RESET_MODES);
    }
}
