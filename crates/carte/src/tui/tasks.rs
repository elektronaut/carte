//! The IO side: daemon streams and calls, terminal input and a clock, each feeding
//! events into the app loop.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use carte_api::attach::{AttachRequest, Frame};
use carte_api::{
    ControlResponse, EnvResponse, Event as DaemonEvent, KubeServicesRequest, KubeServicesResponse,
    LogLine, MarkRequest, ProxiedRequest, ShareResponse, Snapshot, StopAllRequest, paths,
};
use http::Method;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyEventKind, MouseButton,
    MouseEventKind,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use super::app::{Command, Event};
use crate::cli::attach::Connection;
use crate::client::Client;

pub struct Tasks {
    client: Arc<Client>,
    socket: PathBuf,
    tx: UnboundedSender<Event>,
    logs: Option<JoinHandle<()>>,
    requests: Option<JoinHandle<()>>,
    /// Typing into a process: where keys go, and the task that sends them.
    typing: Option<(UnboundedSender<Vec<u8>>, JoinHandle<()>)>,
}

impl Tasks {
    pub fn start(socket: PathBuf, tx: UnboundedSender<Event>) -> Self {
        let client = Arc::new(Client::new(&socket));
        let socket = socket.clone();
        tokio::spawn(watch(client.clone(), tx.clone()));
        tokio::spawn(tick(tx.clone()));
        let input_tx = tx.clone();
        std::thread::spawn(move || read_terminal(input_tx));
        Tasks {
            client,
            socket,
            tx,
            logs: None,
            requests: None,
            typing: None,
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
            Command::FollowEvents {
                generation,
                request,
            } => {
                self.stop_logs();
                self.logs = Some(tokio::spawn(async move {
                    let line_tx = tx.clone();
                    let result = client
                        .stream(
                            Method::POST,
                            paths::EVENTS,
                            Some(&request),
                            |event: DaemonEvent| {
                                let line = LogLine {
                                    project: String::new(),
                                    app: String::new(),
                                    stack: String::new(),
                                    process: String::new(),
                                    at_ms: event.at_ms,
                                    text: crate::clock::event_line(&event),
                                    marker: false,
                                };
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
            Command::FollowRequests {
                generation,
                request,
            } => {
                self.stop_requests();
                self.requests = Some(tokio::spawn(async move {
                    let line_tx = tx.clone();
                    let result = client
                        .stream(
                            Method::POST,
                            paths::REQUESTS,
                            Some(&request),
                            |request: ProxiedRequest| {
                                let _ = line_tx.send(Event::Request {
                                    generation,
                                    request: Box::new(request),
                                });
                            },
                        )
                        .await;
                    let _ = tx.send(Event::RequestsEnded {
                        generation,
                        error: result.err().map(|e| format!("{e:#}")),
                    });
                }));
            }
            Command::StopRequests => self.stop_requests(),
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
            Command::FetchEnv { generation, target } => {
                tokio::spawn(async move {
                    let result = client
                        .post::<_, EnvResponse>(paths::PROCESS_ENV, &target)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::Env { generation, result });
                });
            }
            Command::FetchServices(project) => {
                tokio::spawn(async move {
                    let result = client
                        .post::<_, KubeServicesResponse>(
                            paths::KUBE_SERVICES,
                            &KubeServicesRequest {
                                project: project.clone(),
                            },
                        )
                        .await
                        .map(|response| response.services)
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::Services { project, result });
                });
            }
            Command::Attach { session, request } => {
                self.detach();
                let (keys_tx, keys) = tokio::sync::mpsc::unbounded_channel();
                let socket = self.socket.clone();
                let task = tokio::spawn(type_into(socket, session, request, keys, tx));
                self.typing = Some((keys_tx, task));
            }
            Command::Type(data) => {
                if let Some((keys, _)) = &self.typing {
                    let _ = keys.send(data);
                }
            }
            Command::Detach => self.detach(),
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

    fn detach(&mut self) {
        if let Some((_, task)) = self.typing.take() {
            task.abort();
        }
    }

    fn stop_logs(&mut self) {
        if let Some(task) = self.logs.take() {
            task.abort();
        }
    }

    fn stop_requests(&mut self) {
        if let Some(task) = self.requests.take() {
            task.abort();
        }
    }
}

/// Attaches to a process and sends it the keys typed, until the process exits or the
/// task is aborted.
async fn type_into(
    socket: PathBuf,
    session: u64,
    request: AttachRequest,
    mut keys: UnboundedReceiver<Vec<u8>>,
    tx: UnboundedSender<Event>,
) {
    let mut connection = match Connection::open(&socket, &request).await {
        Ok(connection) => connection,
        Err(err) => {
            let _ = tx.send(Event::Attached {
                session,
                result: Err(format!("{err:#}")),
            });
            return;
        }
    };
    let _ = tx.send(Event::Attached {
        session,
        result: Ok(connection.attached.clone()),
    });
    let ended = |message| Event::TypingEnded { session, message };
    loop {
        let typed = tokio::select! {
            data = keys.recv() => match data {
                Some(data) => data,
                None => return,
            },
            frame = connection.next() => {
                match frame {
                    Ok(Some(Frame::Output(data))) => {
                        let _ = tx.send(Event::TypingOutput { session, data });
                    }
                    Ok(Some(Frame::Ended(message))) => {
                        let _ = tx.send(ended(Some(message)));
                        return;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        let _ = tx.send(ended(None));
                        return;
                    }
                    Err(err) => {
                        let _ = tx.send(ended(Some(format!("{err:#}"))));
                        return;
                    }
                }
                continue;
            }
        };
        if connection.send(&Frame::Input(typed)).await.is_err() {
            let _ = tx.send(ended(None));
            return;
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
