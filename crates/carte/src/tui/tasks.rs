//! The IO side: daemon streams and calls, terminal input and a clock, each feeding
//! events into the app loop.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use carte_api::{
    ControlResponse, LogLine, MarkRequest, ShareResponse, Snapshot, StopAllRequest, paths,
};
use http::Method;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyEventKind, MouseButton,
    MouseEventKind,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::app::{Command, Event};
use crate::client::Client;

pub struct Tasks {
    client: Arc<Client>,
    tx: UnboundedSender<Event>,
    logs: Option<JoinHandle<()>>,
}

impl Tasks {
    pub fn start(socket: PathBuf, tx: UnboundedSender<Event>) -> Self {
        let client = Arc::new(Client::new(&socket));
        tokio::spawn(watch(client.clone(), tx.clone()));
        tokio::spawn(tick(tx.clone()));
        let input_tx = tx.clone();
        std::thread::spawn(move || read_terminal(input_tx));
        Tasks {
            client,
            tx,
            logs: None,
        }
    }

    pub fn run(&mut self, command: Command) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        match command {
            Command::Control { request, verb } => {
                tokio::spawn(async move {
                    let result = client
                        .post::<_, ControlResponse>(paths::CONTROL, &request)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::ControlDone { verb, result });
                });
            }
            Command::StopAll { idle_only } => {
                tokio::spawn(async move {
                    let result = client
                        .post::<_, ControlResponse>(paths::STOP_ALL, &StopAllRequest { idle_only })
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::ControlDone {
                        verb: "stopping",
                        result,
                    });
                });
            }
            Command::FollowLogs {
                generation,
                request,
            } => {
                self.stop_logs();
                self.logs = Some(tokio::spawn(async move {
                    let line_tx = tx.clone();
                    let result = client
                        .stream(
                            Method::POST,
                            paths::LOGS,
                            Some(&request),
                            |line: LogLine| {
                                let _ = line_tx.send(Event::Log { generation, line });
                            },
                        )
                        .await;
                    let _ = tx.send(Event::LogsEnded {
                        generation,
                        error: result.err().map(|e| format!("{e:#}")),
                    });
                }));
            }
            Command::StopLogs => self.stop_logs(),
            Command::Share(request) => {
                tokio::spawn(async move {
                    let result = client
                        .post::<_, ShareResponse>(paths::SHARE, &request)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::Shared(result));
                });
            }
            Command::Unshare(request) => {
                tokio::spawn(async move {
                    let result = client
                        .post::<_, Vec<String>>(paths::UNSHARE, &request)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::Unshared(result));
                });
            }
            Command::Mark(target) => {
                tokio::spawn(async move {
                    let result = client
                        .post::<_, LogLine>(paths::MARK, &MarkRequest { target })
                        .await;
                    if let Err(err) = result {
                        let _ = tx.send(Event::ControlDone {
                            verb: "marking",
                            result: Err(format!("{err:#}")),
                        });
                    }
                });
            }
            Command::MouseCapture(on) => set_mouse_capture(on),
            Command::Copy(text) => copy(&text),
            Command::Open(url) => {
                let opener = if cfg!(target_os = "macos") {
                    "open"
                } else {
                    "xdg-open"
                };
                let spawned = std::process::Command::new(opener)
                    .arg(&url)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                if let Err(err) = spawned {
                    let _ = tx.send(Event::ControlDone {
                        verb: "opening",
                        result: Err(format!("couldn't open {url}: {err}")),
                    });
                }
            }
        }
    }

    fn stop_logs(&mut self) {
        if let Some(task) = self.logs.take() {
            task.abort();
        }
    }
}

/// Follows snapshots, and keeps reconnecting while the daemon is away.
async fn watch(client: Arc<Client>, tx: UnboundedSender<Event>) {
    loop {
        let result = client
            .stream(
                Method::GET,
                paths::WATCH,
                None::<&()>,
                |snapshot: Snapshot| {
                    let _ = tx.send(Event::Snapshot(Box::new(snapshot)));
                },
            )
            .await;
        let message = match result {
            Ok(()) => "the daemon closed the connection".to_string(),
            Err(err) => format!("{err:#}"),
        };
        if tx.send(Event::Disconnected(message)).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Redraws countdowns and expires notices.
async fn tick(tx: UnboundedSender<Event>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        if tx.send(Event::Tick).is_err() {
            return;
        }
    }
}

fn read_terminal(tx: UnboundedSender<Event>) {
    loop {
        let event = match event::read() {
            Ok(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => Event::Key(key),
            Ok(TermEvent::Resize(..)) => Event::Redraw,
            Ok(TermEvent::Mouse(mouse))
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp
                        | MouseEventKind::ScrollDown
                        | MouseEventKind::Down(MouseButton::Left)
                        | MouseEventKind::Drag(MouseButton::Left)
                        | MouseEventKind::Up(MouseButton::Left)
                ) =>
            {
                Event::Mouse(mouse)
            }
            Ok(_) => continue,
            Err(_) => return,
        };
        if tx.send(event).is_err() {
            return;
        }
    }
}

/// Puts text on the clipboard: `pbcopy` on this Mac, or OSC 52 for the terminal to
/// copy, which also reaches the local clipboard over SSH.
fn copy(text: &str) {
    use std::io::Write;
    let remote = std::env::var_os("SSH_CONNECTION").is_some();
    if !remote && cfg!(target_os = "macos") {
        let copied = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child
                    .stdin
                    .take()
                    .expect("piped stdin")
                    .write_all(text.as_bytes())?;
                child.wait()
            });
        if copied.is_ok_and(|status| status.success()) {
            return;
        }
    }
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}

pub fn set_mouse_capture(on: bool) {
    let mut out = std::io::stdout();
    let _ = if on {
        ratatui::crossterm::execute!(out, EnableMouseCapture)
    } else {
        ratatui::crossterm::execute!(out, DisableMouseCapture)
    };
}
