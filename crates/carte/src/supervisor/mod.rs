//! The supervisor owns every instance and process. It runs as one task; everything
//! else talks to it through a [`Handle`].

mod actor;
mod events;
pub mod groups;
pub mod runner;
pub mod share;
mod snapshot;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use carte_api::{
    Action, Address, ControlRequest, ControlResponse, Event, ExternalRequest, LogLine, LogsRequest,
    ShareRequest, ShareResponse, Snapshot, StartFailure, TunnelStatus,
};
use carte_core::model::{InstanceKey, World};
pub use events::EventFilter;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::paths::Paths;
use crate::process::Spawner;

/// Where a ready instance answers HTTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Upstream {
    pub port: u16,
}

/// Why an instance couldn't serve a request, in words for an error page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootFailure {
    pub title: String,
    pub detail: String,
    pub log: Vec<String>,
    /// The failure may go away by itself (still booting, being stopped).
    pub transient: bool,
}

const NO_WEB_PROCESS: &str = "No web process";

impl BootFailure {
    fn new(title: impl Into<String>, detail: impl Into<String>) -> Self {
        BootFailure {
            title: title.into(),
            detail: detail.into(),
            log: Vec::new(),
            transient: false,
        }
    }

    /// The instance is ready but has no HTTP process to forward to.
    fn no_web_process(app: &str) -> Self {
        BootFailure::new(NO_WEB_PROCESS, format!("{app} has no HTTP process"))
    }

    pub fn is_no_web_process(&self) -> bool {
        self.title == NO_WEB_PROCESS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    RequestStarted,
    RequestEnded,
    ConnectionOpened,
    ConnectionClosed,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Time between SIGTERM and SIGKILL when stopping a process.
    pub grace: Duration,
    /// How long an HTTP process may take to become ready.
    pub boot_timeout: Duration,
    /// How often idle timers and limits are checked.
    pub tick: Duration,
    /// The CA files injected as `SSL_CERT_FILE` and friends.
    pub ca_bundle: PathBuf,
    pub ca_cert: PathBuf,
}

impl Options {
    pub fn new(paths: &Paths) -> Self {
        Options {
            grace: Duration::from_secs(10),
            boot_timeout: Duration::from_secs(120),
            tick: Duration::from_secs(1),
            ca_bundle: paths.ca_bundle(),
            ca_cert: paths.ca_cert(),
        }
    }
}

pub(crate) enum Msg {
    EnsureReady {
        key: InstanceKey,
        reply: oneshot::Sender<Result<Upstream, BootFailure>>,
    },
    Activity(InstanceKey, Activity),
    /// Answers with the instances the action applied to.
    Control {
        request: ControlRequest,
        reply: oneshot::Sender<Result<(ControlResponse, Vec<InstanceKey>), String>>,
    },
    StopAll {
        idle_only: bool,
        reply: oneshot::Sender<ControlResponse>,
    },
    Logs {
        request: LogsRequest,
        reply: oneshot::Sender<Result<(Vec<LogLine>, LogFilter), String>>,
    },
    Mark {
        target: Address,
        reply: oneshot::Sender<Result<LogLine, String>>,
    },
    Events {
        target: Option<Address>,
        tail: usize,
        reply: oneshot::Sender<Result<(Vec<Event>, EventFilter), String>>,
    },
    Reload(World),
    RestartCheckout(PathBuf),
    EnvChanged(PathBuf),
    Share {
        request: ShareRequest,
        reply: oneshot::Sender<Result<ShareResponse, String>>,
    },
    Unshare {
        target: Address,
        reply: oneshot::Sender<Result<Vec<String>, String>>,
    },
    ExternalRequest(InstanceKey, ExternalRequest),
    Tunnel(TunnelStatus),
    Shutdown(oneshot::Sender<()>),
    Internal(actor::Internal),
}

/// Which log lines a follower wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFilter {
    pub project: String,
    /// The stack's slug.
    pub stack: Option<String>,
    pub app: Option<String>,
    pub process: Option<String>,
    pub search: Option<String>,
}

impl LogFilter {
    pub fn matches(&self, line: &LogLine) -> bool {
        if line.marker {
            let overlaps = |wanted: &Option<String>, field: &str| {
                field.is_empty() || wanted.as_deref().is_none_or(|w| w == field)
            };
            return self.project == line.project
                && overlaps(&self.stack, &line.stack)
                && overlaps(&self.app, &line.app)
                && overlaps(&self.process, &line.process);
        }
        self.project == line.project
            && self.stack.as_ref().is_none_or(|s| *s == line.stack)
            && self.app.as_ref().is_none_or(|a| *a == line.app)
            && self.process.as_ref().is_none_or(|p| *p == line.process)
            && self
                .search
                .as_ref()
                .is_none_or(|s| line.text.contains(s.as_str()))
    }
}

#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Msg>,
    snapshot: watch::Receiver<Arc<Snapshot>>,
    world: watch::Receiver<Arc<World>>,
    shared: watch::Receiver<Arc<share::SharedRoutes>>,
    logs: broadcast::Sender<LogLine>,
    events: broadcast::Sender<Event>,
}

impl Handle {
    /// Starts the instance if needed and waits until it serves HTTP. The boot goes
    /// on if this future is dropped.
    pub async fn ensure_ready(&self, key: InstanceKey) -> Result<Upstream, BootFailure> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::EnsureReady { key, reply });
        rx.await
            .unwrap_or_else(|_| Err(BootFailure::new("carte is shutting down", "")))
    }

    pub fn activity(&self, key: InstanceKey, activity: Activity) {
        self.send(Msg::Activity(key, activity));
    }

    /// Applies a control action; with `wait`, an `up` or `restart` answers once every
    /// instance it started is ready or has failed.
    pub async fn control(&self, request: ControlRequest) -> Result<ControlResponse, String> {
        let wait = request.wait && matches!(request.action, Action::Up | Action::Restart);
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Control { request, reply });
        let (mut response, keys) = rx
            .await
            .unwrap_or_else(|_| Err("carte is shutting down".into()))?;
        if wait {
            for key in keys {
                if let Err(failure) = self.ensure_ready(key.clone()).await
                    && !failure.is_no_web_process()
                {
                    response.failures.push(StartFailure {
                        instance: format!("{}@{}", key.app, key.stack.slug()),
                        title: failure.title,
                        detail: failure.detail,
                        log: failure.log,
                    });
                }
            }
        }
        Ok(response)
    }

    pub async fn stop_all(&self, idle_only: bool) -> ControlResponse {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::StopAll { idle_only, reply });
        rx.await.unwrap_or(ControlResponse {
            affected: vec![],
            messages: vec![],
            failures: vec![],
        })
    }

    /// Buffered lines for a target, oldest first, and the filter for following it.
    pub async fn logs(&self, request: LogsRequest) -> Result<(Vec<LogLine>, LogFilter), String> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Logs { request, reply });
        rx.await
            .unwrap_or_else(|_| Err("carte is shutting down".into()))
    }

    /// Adds a marker to the logs of a target and returns it.
    pub async fn mark(&self, target: Address) -> Result<LogLine, String> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Mark { target, reply });
        rx.await
            .unwrap_or_else(|_| Err("carte is shutting down".into()))
    }

    pub fn follow_logs(&self) -> broadcast::Receiver<LogLine> {
        self.logs.subscribe()
    }

    /// Recent events about a target (every event for `None`), oldest first, and the
    /// filter for following them.
    pub async fn events(
        &self,
        target: Option<Address>,
        tail: usize,
    ) -> Result<(Vec<Event>, EventFilter), String> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Events {
            target,
            tail,
            reply,
        });
        rx.await
            .unwrap_or_else(|_| Err("carte is shutting down".into()))
    }

    pub fn follow_events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snapshot.clone()
    }

    pub fn world(&self) -> Arc<World> {
        self.world.borrow().clone()
    }

    pub async fn share(&self, request: ShareRequest) -> Result<ShareResponse, String> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Share { request, reply });
        rx.await
            .unwrap_or_else(|_| Err("carte is shutting down".into()))
    }

    /// Ends the share of a stack; returns the apps that were shared.
    pub async fn unshare(&self, target: Address) -> Result<Vec<String>, String> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Unshare { target, reply });
        rx.await
            .unwrap_or_else(|_| Err("carte is shutting down".into()))
    }

    /// What the share listener may serve right now.
    pub fn shared_routes(&self) -> Arc<share::SharedRoutes> {
        self.shared.borrow().clone()
    }

    pub fn watch_shared(&self) -> watch::Receiver<Arc<share::SharedRoutes>> {
        self.shared.clone()
    }

    pub fn external_request(&self, key: InstanceKey, request: ExternalRequest) {
        self.send(Msg::ExternalRequest(key, request));
    }

    pub fn tunnel_status(&self, status: TunnelStatus) {
        self.send(Msg::Tunnel(status));
    }

    /// Restarts every running instance of a checkout (`tmp/restart.txt`).
    pub fn restart_checkout(&self, path: PathBuf) {
        self.send(Msg::RestartCheckout(path));
    }

    /// Marks a checkout's running instances as needing a restart for new tool versions.
    pub fn env_changed(&self, path: PathBuf) {
        self.send(Msg::EnvChanged(path));
    }

    pub fn reload(&self, world: World) {
        self.send(Msg::Reload(world));
    }

    /// Stops every process and waits for them to exit.
    pub async fn shutdown(&self) {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Shutdown(reply));
        let _ = rx.await;
    }

    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }
}

/// The environment apps build on: carte's own, minus carte's settings.
pub fn base_env() -> BTreeMap<String, String> {
    std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .filter(|(k, _)| !k.starts_with("CARTE_"))
        .collect()
}

/// Starts the supervisor task.
pub fn start(world: World, paths: Paths, spawner: Spawner, options: Options) -> Handle {
    actor::start(world, paths, spawner, options)
}
