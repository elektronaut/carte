//! The supervisor owns every instance and process. It runs as one task; everything
//! else talks to it through a [`Handle`].

mod actor;
pub mod groups;
pub mod runner;
mod snapshot;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use carte_api::{Address, ControlRequest, ControlResponse, LogLine, Snapshot};
use carte_core::model::{InstanceKey, World};
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

impl BootFailure {
    fn new(title: impl Into<String>, detail: impl Into<String>) -> Self {
        BootFailure {
            title: title.into(),
            detail: detail.into(),
            log: Vec::new(),
            transient: false,
        }
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
    Control {
        request: ControlRequest,
        reply: oneshot::Sender<Result<ControlResponse, String>>,
    },
    StopAll {
        idle_only: bool,
        reply: oneshot::Sender<ControlResponse>,
    },
    Logs {
        target: Address,
        tail: usize,
        search: Option<String>,
        reply: oneshot::Sender<Result<(Vec<LogLine>, LogFilter), String>>,
    },
    Reload(World),
    Shutdown(oneshot::Sender<()>),
    Internal(actor::Internal),
}

/// Which log lines a follower wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFilter {
    pub instances: Vec<(String, String)>,
    pub process: Option<String>,
    pub search: Option<String>,
}

impl LogFilter {
    pub fn matches(&self, line: &LogLine) -> bool {
        self.instances
            .iter()
            .any(|(app, stack)| *app == line.app && *stack == line.stack)
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
    logs: broadcast::Sender<LogLine>,
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

    pub async fn control(&self, request: ControlRequest) -> Result<ControlResponse, String> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Control { request, reply });
        rx.await
            .unwrap_or_else(|_| Err("carte is shutting down".into()))
    }

    pub async fn stop_all(&self, idle_only: bool) -> ControlResponse {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::StopAll { idle_only, reply });
        rx.await.unwrap_or(ControlResponse {
            affected: vec![],
            messages: vec![],
        })
    }

    /// Buffered lines for a target, oldest first, and the filter for following it.
    pub async fn logs(
        &self,
        target: Address,
        tail: usize,
        search: Option<String>,
    ) -> Result<(Vec<LogLine>, LogFilter), String> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Logs {
            target,
            tail,
            search,
            reply,
        });
        rx.await
            .unwrap_or_else(|_| Err("carte is shutting down".into()))
    }

    pub fn follow_logs(&self) -> broadcast::Receiver<LogLine> {
        self.logs.subscribe()
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
