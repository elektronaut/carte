//! The supervisor owns every instance and process. It runs as one task; everything
//! else talks to it through a [`Handle`].

mod actor;
mod events;
pub mod groups;
pub mod runner;
pub mod share;
mod snapshot;
mod teardown;
pub mod terminal;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use carte_api::attach::Attached;
use carte_api::{
    Action, Address, ControlRequest, ControlResponse, Event, EventKind, EventSubject,
    ExternalRequest, LogLine, LogsRequest, ShareRequest, ShareResponse, Snapshot, StartFailure,
    TunnelStatus,
};
use carte_core::model::{InstanceKey, World};
use carte_core::prune::Orphan;
pub use events::EventFilter;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::paths::Paths;
use crate::process::Spawner;
use crate::prune;

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
const NOT_RUNNING: &str = "Not running";

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

    /// A request that may not start the instance found it stopped.
    fn not_running(app: &str) -> Self {
        BootFailure::new(NOT_RUNNING, format!("{app} isn't running"))
    }

    pub fn is_not_running(&self) -> bool {
        self.title == NOT_RUNNING
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
    /// The wait before restarting a crashed service; it doubles with each crash.
    pub restart_backoff: Duration,
    /// Crashes within this long of each other count toward giving up on a process.
    pub crash_window: Duration,
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
            restart_backoff: Duration::from_secs(1),
            crash_window: Duration::from_secs(60),
            ca_bundle: paths.ca_bundle(),
            ca_cert: paths.ca_cert(),
        }
    }
}

pub(crate) enum Msg {
    EnsureReady {
        key: InstanceKey,
        /// The request's path, which may belong to a process other than the HTTP one.
        path: Option<String>,
        reply: oneshot::Sender<Result<Upstream, BootFailure>>,
        /// The start is for a proxied request, which came from a page of this
        /// instance when `Some(Some(_))`.
        request: Option<Option<InstanceKey>>,
        /// When false, an instance that isn't starting or ready is left alone and
        /// the reply is [`BootFailure::is_not_running`].
        start: bool,
    },
    Activity(InstanceKey, Activity),
    /// Keeps a running instance from idling out; answers with the instance and the
    /// hold's id for `Release`.
    Hold {
        target: Address,
        reply: oneshot::Sender<Result<(InstanceKey, u64), String>>,
    },
    Release(InstanceKey, u64),
    /// The pty of a running process; `process` when the address names none.
    Attach {
        target: Address,
        process: Option<String>,
        reply: oneshot::Sender<Result<(Attached, Arc<terminal::Terminal>), AttachError>>,
    },
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
    /// An event from outside the supervisor, like a kube project's forward.
    Event {
        kind: EventKind,
        message: String,
        subjects: Vec<EventSubject>,
    },
    Shutdown(oneshot::Sender<()>),
    Running(oneshot::Sender<BTreeSet<InstanceKey>>),
    /// What prune leaves alone.
    InUse(oneshot::Sender<prune::InUse>),
    /// Whether a stack has a worktree again, for a teardown about to run a process.
    HasWorktree {
        project: String,
        stack: String,
        reply: oneshot::Sender<bool>,
    },
    Prune {
        orphans: Vec<Orphan>,
        reply: oneshot::Sender<Vec<(Orphan, prune::Detached)>>,
    },
    /// Like `Prune`, for a torn-down stack: detaches nothing, and replies `None`, when
    /// the stack has a worktree again.
    DetachStack {
        project: String,
        stack: String,
        orphans: Vec<Orphan>,
        reply: oneshot::Sender<Option<Vec<(Orphan, prune::Detached)>>>,
    },
    Internal(actor::Internal),
}

/// Why there is nothing to attach to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    /// The address or the name matches no process of the app.
    NoProcess(String),
    /// Not running, without a pty, or an address that names no app.
    Refused(String),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::NoProcess(message) | AttachError::Refused(message) => f.write_str(message),
        }
    }
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
        self.ready(key, None, None, true).await
    }

    /// Like [`Handle::ensure_ready`], for a proxied request for `path`, which goes to
    /// the process serving it: the first request since the instance started opens
    /// it. `referrer` is the instance its `Origin` or `Referer` names.
    pub async fn request(
        &self,
        key: InstanceKey,
        path: &str,
        referrer: Option<InstanceKey>,
    ) -> Result<Upstream, BootFailure> {
        self.ready(key, Some(path.to_string()), Some(referrer), true)
            .await
    }

    /// Like [`Handle::request`], for a request that may only reach an instance that
    /// is starting or ready. It neither starts nor opens the instance; one that
    /// isn't running answers with [`BootFailure::is_not_running`].
    pub async fn request_running(
        &self,
        key: InstanceKey,
        path: &str,
    ) -> Result<Upstream, BootFailure> {
        self.ready(key, Some(path.to_string()), None, false).await
    }

    async fn ready(
        &self,
        key: InstanceKey,
        path: Option<String>,
        request: Option<Option<InstanceKey>>,
        start: bool,
    ) -> Result<Upstream, BootFailure> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::EnsureReady {
            key,
            path,
            reply,
            request,
            start,
        });
        rx.await
            .unwrap_or_else(|_| Err(BootFailure::new("carte is shutting down", "")))
    }

    pub fn activity(&self, key: InstanceKey, activity: Activity) {
        self.send(Msg::Activity(key, activity));
    }

    /// Keeps the running instance an address names from idling out until
    /// [`Handle::release`].
    pub async fn hold(&self, target: Address) -> Result<(InstanceKey, u64), String> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Hold { target, reply });
        rx.await
            .unwrap_or_else(|_| Err("carte is shutting down".into()))
    }

    pub fn release(&self, key: InstanceKey, id: u64) {
        self.send(Msg::Release(key, id));
    }

    /// The pty of a running process to attach to: the one the address or `process`
    /// names, else one paused at a debugger prompt, else the HTTP process.
    pub async fn attach(
        &self,
        target: Address,
        process: Option<String>,
    ) -> Result<(Attached, Arc<terminal::Terminal>), AttachError> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Attach {
            target,
            process,
            reply,
        });
        rx.await
            .unwrap_or_else(|_| Err(AttachError::Refused("carte is shutting down".into())))
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

    /// Adds an event to the log, for things outside the supervisor.
    pub fn event(&self, kind: EventKind, message: impl Into<String>, subjects: Vec<EventSubject>) {
        self.send(Msg::Event {
            kind,
            message: message.into(),
            subjects,
        });
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

    /// Instances that are starting or ready, or still have processes running.
    pub async fn running(&self) -> BTreeSet<InstanceKey> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Running(reply));
        rx.await.unwrap_or_default()
    }

    /// Running instances, and stacks whose teardown is pending or running.
    pub async fn in_use(&self) -> prune::InUse {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::InUse(reply));
        rx.await.unwrap_or_default()
    }

    /// Takes leftovers out of the way, skipping any a running instance owns or of a
    /// stack being torn down, without an instance starting in between.
    pub async fn prune(&self, orphans: Vec<Orphan>) -> Vec<(Orphan, prune::Detached)> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Prune { orphans, reply });
        rx.await.unwrap_or_default()
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
