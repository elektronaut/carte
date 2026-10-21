use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carte_api::attach::Attached;
use carte_api::{
    Action, Address, ControlRequest, ControlResponse, Event, EventKind, EventSubject, LogLine,
    LogsRequest, ProcessState, ShareRequest, ShareResponse, Snapshot, TunnelState, TunnelStatus,
};
use carte_core::address::{self, Target};
use carte_core::dirs::DirRoots;
use carte_core::env::{self, Env, EnvRequest, SystemRunner};
use carte_core::lifecycle::{self, Capacity, EvictionReason, Targets, Usage};
use carte_core::model::{
    App, Checkout, InstanceKey, Policy, Preset, ProcessSpec, ProjectState, Restart, Scope, Stack,
    World,
};
use carte_core::warming::{Edge, Warming};
use carte_core::{naming, procfile};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::events::{self, EventFilter, EventLog};
use super::groups::GroupRecord;
use super::runner::{self, LogLabel, ProcLog};
use super::teardown::{self, StackId, Teardowns};
use super::terminal::Terminal;
use super::{
    Activity, AttachError, BootFailure, Handle, LogFilter, Msg, Options, Upstream, share, snapshot,
};
use crate::instance_dirs;
use crate::paths::Paths;
use crate::process::{IoMode, SpawnSpec, Spawner};
use crate::prune;
use crate::urls::StackUrls;

pub(crate) enum Internal {
    EnvReady {
        key: InstanceKey,
        boot: u64,
        /// The process build it answers, for one started on its own.
        build: Option<u64>,
        result: Result<Vec<(usize, SpawnSpec)>, String>,
        provider: Option<(PathBuf, Env)>,
    },
    Proc {
        key: InstanceKey,
        run: u64,
        idx: usize,
        event: runner::Event,
    },
    Ready {
        key: InstanceKey,
        run: u64,
        idx: usize,
        result: Result<(), String>,
    },
    RestartProcess {
        key: InstanceKey,
        run: u64,
        idx: usize,
    },
    Tick,
    TeardownCheck(Vec<(StackId, teardown::Status)>),
    TeardownEnded(StackId),
    Event {
        kind: EventKind,
        message: String,
        subjects: Vec<EventSubject>,
        exit_code: Option<i32>,
    },
}

/// Where an instance is in its lifecycle. The snapshot maps it to an
/// [`InstanceState`](carte_api::InstanceState), adding the states it derives from
/// the processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lifecycle {
    Stopped,
    Starting,
    Ready,
    Stopping,
    Crashed,
}

pub(super) struct Instance {
    pub key: InstanceKey,
    pub state: Lifecycle,
    boot: u64,
    pub checkout: Option<Checkout>,
    pub procs: Vec<Proc>,
    pub route_port: Option<u16>,
    waiters: Vec<Waiter>,
    pub in_flight: u32,
    pub connections: u32,
    /// Holds from `carte exec`, by id.
    pub execs: BTreeSet<u64>,
    pub last_activity: Instant,
    pub last_error: Option<String>,
    pub env_changed: bool,
    restart_pending: bool,
    /// Why the pending restart starts it, for its `started` event.
    restart_why: Option<Why>,
    forced: BTreeSet<String>,
    /// Disabled processes the pending restart starts once.
    restart_forced: BTreeSet<String>,
    /// When the latest start began.
    pub started_at: Instant,
    /// It had a proxied request since it started.
    opened: bool,
    /// The app whose open warmed it.
    warmed_after: Option<String>,
    /// Its HTTP process crashed too often: requests get this until it's started by hand.
    gave_up: Option<BootFailure>,
    /// Why the latest start crashed, or the restart after a stop couldn't begin.
    failed: Option<BootFailure>,
}

/// A request waiting for an instance, or one of its processes, to serve it.
struct Waiter {
    /// The request's path; `None` waits for the HTTP process.
    path: Option<String>,
    reply: oneshot::Sender<Result<Upstream, BootFailure>>,
}

/// Why an instance starts, for its `started` event.
#[derive(Clone)]
enum Why {
    Request,
    Manual,
    /// A restart, which already has its `restarted` event.
    Restart,
    /// An instance that depends on it started.
    Dependency(InstanceKey),
    Share,
    /// It usually opens after this instance, which just opened.
    Warm(InstanceKey),
}

pub(super) struct Proc {
    pub spec: ProcessSpec,
    pub port: u16,
    pub state: ProcessState,
    pub pid: Option<u32>,
    stop: Option<oneshot::Sender<Duration>>,
    pub log: Arc<Mutex<ProcLog>>,
    pub restarts: u32,
    pub memory_bytes: u64,
    pub cpu_percent: f32,
    cpu_sample: Option<(Instant, Duration)>,
    run: u64,
    /// When the latest run started, in milliseconds since the Unix epoch.
    pub started_ms: Option<u64>,
    manual_stop: bool,
    restart: bool,
    /// A checkout-scope process another instance runs; taken over when that stops.
    deferred: bool,
    /// Its readiness check passed in the latest run.
    ready: bool,
    /// Part of the start under way, waiting for its turn.
    pub queued: bool,
    /// What a queued process runs, once its environment is built.
    spawn: Option<SpawnSpec>,
    /// The pending environment build that starts it on its own.
    building: Option<u64>,
    /// When it crashed within the last minute.
    crashes: Vec<Instant>,
    /// It crashed and waits to be restarted at this time.
    pub restart_at: Option<Instant>,
    /// It crashed too often and stays down until started again.
    pub gave_up: bool,
    /// It didn't become ready after a restart and is being stopped; its exit counts
    /// as a crash.
    stalled: bool,
    /// Readiness timeouts after a restart since it last passed its readiness check.
    stalls: u32,
    /// Disabled or stopped by hand: requests for its paths go to the HTTP process.
    off: bool,
    /// The latest run's pty, while it runs.
    terminal: Option<Arc<Terminal>>,
    /// The debugger prompt it waits at.
    pub paused: Option<String>,
    /// Whether it waits at one, for the latest run's readiness check.
    held: watch::Sender<bool>,
}

impl Proc {
    fn is_running(&self) -> bool {
        matches!(
            self.state,
            ProcessState::Starting | ProcessState::Running | ProcessState::Stopping
        )
    }

    /// Running and ready, and not being stopped: requests can go to it.
    fn serving(&self) -> bool {
        matches!(self.state, ProcessState::Starting | ProcessState::Running) && self.ready
    }

    /// Holds back what starts after it: still queued, or running and not yet done.
    /// The HTTP process and services are done once ready, other processes once
    /// started, and tasks once they have exited 0.
    pub(super) fn blocks(&self) -> bool {
        if self.queued {
            return true;
        }
        if !self.is_running() {
            return false;
        }
        if self.spec.is_task() {
            true
        } else if self.spec.is_checked() {
            !self.ready
        } else {
            self.state != ProcessState::Running
        }
    }
}

impl Instance {
    fn new(key: InstanceKey) -> Self {
        Instance {
            key,
            state: Lifecycle::Stopped,
            boot: 0,
            checkout: None,
            procs: Vec::new(),
            route_port: None,
            waiters: Vec::new(),
            in_flight: 0,
            connections: 0,
            execs: BTreeSet::new(),
            last_activity: Instant::now(),
            last_error: None,
            env_changed: false,
            restart_pending: false,
            restart_why: None,
            forced: BTreeSet::new(),
            restart_forced: BTreeSet::new(),
            started_at: Instant::now(),
            opened: false,
            warmed_after: None,
            gave_up: None,
            failed: None,
        }
    }

    fn is_active(&self) -> bool {
        matches!(self.state, Lifecycle::Starting | Lifecycle::Ready)
    }

    /// Ready, with its HTTP process up: not waiting to restart or to pass its
    /// readiness check again after a crash.
    fn web_serving(&self) -> bool {
        self.state == Lifecycle::Ready
            && (self.route_port.is_some() || self.http().is_none_or(|p| p.is_running() && p.ready))
    }

    /// Ready, but its HTTP process or a process serving paths, other than one
    /// carte gave up on, waits to restart or to pass its readiness check again.
    pub(super) fn restarting(&self) -> bool {
        self.state == Lifecycle::Ready
            && (!self.web_serving()
                || self
                    .procs
                    .iter()
                    .any(|p| p.spec.serves_paths() && !p.off && !p.gave_up && !p.serving()))
    }

    fn http(&self) -> Option<&Proc> {
        self.procs.iter().find(|p| p.spec.http)
    }

    fn upstream(&self) -> Option<Upstream> {
        self.route_port
            .or_else(|| self.http().map(|p| p.port))
            .map(|port| Upstream {
                port,
                boot: self.boot,
            })
    }

    pub(super) fn any_running(&self) -> bool {
        self.procs.iter().any(Proc::is_running)
    }

    fn fail_waiters(&mut self, failure: BootFailure) {
        for waiter in self.waiters.drain(..) {
            let _ = waiter.reply.send(Err(failure.clone()));
        }
    }

    /// The process that serves `path` instead of the HTTP process, unless it's off.
    fn path_proc(&self, path: &str) -> Option<&Proc> {
        self.procs
            .iter()
            .filter(|p| !p.off)
            .filter_map(|p| p.spec.path_match(path).map(|len| (len, p)))
            .max_by_key(|(len, _)| *len)
            .map(|(_, p)| p)
    }

    /// Where a request for `path` goes once the instance is ready; `None` while
    /// the process serving it starts or restarts.
    fn answer(&self, path: Option<&str>) -> Option<Result<Upstream, BootFailure>> {
        if self.state != Lifecycle::Ready {
            return None;
        }
        if let Some(proc) = path.and_then(|path| self.path_proc(path)) {
            if proc.gave_up {
                let name = &proc.spec.name;
                let label = self.label();
                let mut failure = BootFailure::new(
                    format!("{label}: {name} crashed"),
                    format!(
                        "carte gave up restarting {name}, which serves this path. Run `carte restart {label}/{name}` to start it again."
                    ),
                );
                failure.log = proc.log.lock().unwrap().tail(40);
                return Some(Err(failure));
            }
            return proc.serving().then_some(Ok(Upstream {
                port: proc.port,
                boot: self.boot,
            }));
        }
        self.web_serving().then(|| {
            self.upstream()
                .ok_or_else(|| BootFailure::no_web_process(&self.key.app))
        })
    }

    /// Answers the waiters whose process can take their request now.
    fn release_waiters(&mut self) {
        let mut waiting = Vec::new();
        for waiter in std::mem::take(&mut self.waiters) {
            match self.answer(waiter.path.as_deref()) {
                Some(result) => {
                    let _ = waiter.reply.send(result);
                }
                None => waiting.push(waiter),
            }
        }
        self.waiters = waiting;
    }

    fn label(&self) -> String {
        format!("{}@{}", self.key.app, self.key.stack.slug())
    }

    /// Stops what is running, services last: they get their signal once everything
    /// else has exited. `mark` shows the processes as stopping.
    fn signal_stop(&mut self, grace: Duration, mark: bool) {
        for proc in &mut self.procs {
            proc.queued = false;
            proc.spawn = None;
            proc.restart_at = None;
        }
        let others = self
            .procs
            .iter()
            .any(|p| !p.spec.is_service() && p.is_running());
        for proc in &mut self.procs {
            if others && proc.spec.is_service() {
                continue;
            }
            if let Some(stop) = proc.stop.take() {
                if mark {
                    proc.state = ProcessState::Stopping;
                }
                proc.manual_stop = true;
                let _ = stop.send(grace);
            }
        }
    }
}

/// Temporary policies from `pin`/`disable`, per instance and process (`None` = all).
pub(super) type Overrides = BTreeMap<(InstanceKey, Option<String>), Policy>;

pub(super) fn effective_policy(
    overrides: &Overrides,
    key: &InstanceKey,
    spec: &ProcessSpec,
) -> (Policy, bool) {
    let by_process = overrides.get(&(key.clone(), Some(spec.name.clone())));
    let by_instance = overrides.get(&(key.clone(), None));
    match by_process.or(by_instance) {
        Some(policy) => (*policy, true),
        None => (spec.policy, false),
    }
}

struct Actor {
    world: Arc<World>,
    world_tx: watch::Sender<Arc<World>>,
    paths: Paths,
    options: Options,
    spawner: Arc<Spawner>,
    instances: BTreeMap<InstanceKey, Instance>,
    overrides: Overrides,
    providers: HashMap<PathBuf, Env>,
    daemon_env: Arc<BTreeMap<String, String>>,
    groups: GroupRecord,
    tx: mpsc::UnboundedSender<Msg>,
    snapshot_tx: watch::Sender<Arc<Snapshot>>,
    log_tx: broadcast::Sender<LogLine>,
    /// Markers added to logs, oldest first.
    markers: VecDeque<LogLine>,
    next_id: u64,
    ticks: u64,
    dirty: bool,
    shutdown: Option<oneshot::Sender<()>>,
    shares: share::Shares,
    shared_tx: watch::Sender<Arc<share::SharedRoutes>>,
    /// Instances a share pinned, so unsharing unpins only those.
    share_pins: BTreeSet<InstanceKey>,
    tunnel: Option<TunnelStatus>,
    events: EventLog,
    teardowns: Teardowns,
    warming: Warming,
    /// `warming` changed since it was last written.
    warming_changed: bool,
}

pub(super) fn start(world: World, paths: Paths, spawner: Spawner, options: Options) -> Handle {
    let (tx, rx) = mpsc::unbounded_channel();
    let world = Arc::new(world);
    let (world_tx, world_rx) = watch::channel(world.clone());
    let (log_tx, _) = broadcast::channel(4096);
    let (event_tx, _) = broadcast::channel(256);
    let tunnel = world.machine.share.as_ref().map(|config| TunnelStatus {
        provider: config.provider.clone(),
        single_host: config.single_host(),
        state: TunnelState::Idle,
        message: None,
    });
    let warming = std::fs::read_to_string(paths.warming())
        .ok()
        .and_then(|text| Warming::from_json(&text))
        .unwrap_or_default();
    let initial = Arc::new(snapshot::build(&snapshot::Input {
        world: &world,
        instances: &BTreeMap::new(),
        overrides: &BTreeMap::new(),
        shares: &BTreeMap::new(),
        tunnel: tunnel.as_ref(),
        latest: &BTreeMap::new(),
        warming: &warming,
        now: Instant::now(),
    }));
    let (snapshot_tx, snapshot_rx) = watch::channel(initial);
    let (shared_tx, shared_rx) = watch::channel(Arc::new(share::SharedRoutes::new()));
    let _ = std::fs::create_dir_all(paths.run_dir());
    let left_behind = teardown::left_behind(&world, &paths.dir_roots());
    let actor = Actor {
        world,
        world_tx,
        groups: GroupRecord::new(paths.run_dir().join("groups")),
        paths,
        options,
        spawner: Arc::new(spawner),
        instances: BTreeMap::new(),
        overrides: BTreeMap::new(),
        providers: HashMap::new(),
        daemon_env: Arc::new(super::base_env()),
        tx: tx.clone(),
        snapshot_tx,
        log_tx: log_tx.clone(),
        markers: VecDeque::new(),
        next_id: 0,
        ticks: 0,
        dirty: false,
        shutdown: None,
        shares: BTreeMap::new(),
        shared_tx,
        share_pins: BTreeSet::new(),
        tunnel,
        events: EventLog::new(event_tx.clone()),
        teardowns: Teardowns {
            seen: left_behind,
            ..Teardowns::default()
        },
        warming,
        warming_changed: false,
    };
    let tick_tx = tx.clone();
    let tick = actor.options.tick;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if tick_tx.send(Msg::Internal(Internal::Tick)).is_err() {
                break;
            }
        }
    });
    tokio::spawn(actor.run(rx));
    Handle {
        tx,
        snapshot: snapshot_rx,
        world: world_rx,
        shared: shared_rx,
        logs: log_tx,
        events: event_tx,
    }
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            self.handle(msg);
            if self.dirty {
                self.dirty = false;
                self.publish();
            }
            if self.shutdown.is_some() && !self.instances.values().any(Instance::any_running) {
                if let Some(reply) = self.shutdown.take() {
                    let _ = reply.send(());
                }
                break;
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::EnsureReady {
                key,
                path,
                reply,
                request,
                start,
            } => {
                if !start && !self.instances.get(&key).is_some_and(|i| i.is_active()) {
                    let _ = reply.send(Err(BootFailure::not_running(&key.app)));
                    return;
                }
                self.ensure_ready(key.clone(), path, reply);
                if let Some(referrer) = request {
                    self.requested(&key, referrer.as_ref());
                }
            }
            Msg::Activity(key, boot, activity) => self.activity(&key, boot, activity),
            Msg::Hold { target, reply } => {
                let _ = reply.send(self.hold(&target));
            }
            Msg::Release(key, id) => self.release(&key, id),
            Msg::Attach {
                target,
                process,
                reply,
            } => {
                let _ = reply.send(self.attach(&target, process.as_deref()));
            }
            Msg::Control { request, reply } => {
                let _ = reply.send(self.control(request));
            }
            Msg::StopAll { idle_only, reply } => {
                let _ = reply.send(self.stop_all(idle_only));
            }
            Msg::Logs { request, reply } => {
                let _ = reply.send(self.logs(&request));
            }
            Msg::Mark { target, reply } => {
                let _ = reply.send(self.mark(&target));
            }
            Msg::Events {
                target,
                tail,
                reply,
            } => {
                let _ = reply.send(self.recent_events(target.as_ref(), tail));
            }
            Msg::Reload(world) => self.reload(world),
            Msg::RestartCheckout(path) => self.restart_checkout(&path),
            Msg::Share { request, reply } => {
                let _ = reply.send(self.share(request));
            }
            Msg::Unshare { target, reply } => {
                let _ = reply.send(self.unshare(&target));
            }
            Msg::ExternalRequest(key, request) => {
                if let Some(share) = self.shares.get_mut(&(key.project, key.stack)) {
                    share.requests.push_back(request);
                    if share.requests.len() > 50 {
                        share.requests.pop_front();
                    }
                    self.dirty = true;
                }
            }
            Msg::Event {
                kind,
                message,
                subjects,
            } => self.events.push(kind, message, subjects),
            Msg::Tunnel(status) => {
                let before = self.tunnel.as_ref().map(|t| t.state);
                if before != Some(status.state) {
                    let message = status.message.clone().unwrap_or_default();
                    match status.state {
                        TunnelState::Connected => {
                            self.events
                                .push(EventKind::TunnelConnected, &status.provider, vec![])
                        }
                        TunnelState::Failed => {
                            self.events.push(EventKind::TunnelFailed, message, vec![])
                        }
                        TunnelState::Idle | TunnelState::Starting | TunnelState::Unknown => {}
                    }
                }
                self.tunnel = Some(status);
                self.dirty = true;
            }
            Msg::EnvChanged(path) => {
                self.providers
                    .retain(|checkout, _| !checkout.starts_with(&path));
                for inst in self.instances.values_mut() {
                    if inst.is_active()
                        && inst
                            .checkout
                            .as_ref()
                            .is_some_and(|c| c.path().starts_with(&path))
                    {
                        inst.env_changed = true;
                        self.dirty = true;
                    }
                }
            }
            Msg::Shutdown(reply) => {
                self.save_warming();
                self.shutdown = Some(reply);
                for (project, stack) in self.teardowns.running.clone() {
                    self.event(
                        EventKind::TeardownFailed,
                        "carte stopped while it ran; it runs again once carte starts",
                        vec![events::stack(&project, &stack)],
                    );
                }
                let keys: Vec<_> = self.instances.keys().cloned().collect();
                for key in keys {
                    self.stop(&key, None);
                }
            }
            Msg::Running(reply) => {
                let _ = reply.send(self.running());
            }
            Msg::InUse(reply) => {
                let _ = reply.send(prune::InUse {
                    running: self.running(),
                    tearing_down: self.tearing_down(),
                });
            }
            Msg::HasWorktree {
                project,
                stack,
                reply,
            } => {
                let _ = reply.send(self.has_worktree(&project, &stack));
            }
            Msg::Prune { orphans, reply } => {
                let tearing_down = self.tearing_down();
                let (kept, orphans): (Vec<_>, Vec<_>) = orphans
                    .into_iter()
                    .partition(|orphan| torn_down_reason(&tearing_down, orphan).is_some());
                let mut detached = prune::detach(orphans, &self.running(), &self.paths.dir_roots());
                for orphan in kept {
                    let reason = torn_down_reason(&tearing_down, &orphan)
                        .unwrap_or_default()
                        .to_string();
                    detached.push((orphan, prune::Detached::Kept(reason)));
                }
                let _ = reply.send(detached);
                self.forget_gone_apps();
            }
            Msg::DetachStack {
                project,
                stack,
                orphans,
                reply,
            } => {
                let present = self.has_worktree(&project, &stack);
                let detached = (!present)
                    .then(|| prune::detach(orphans, &self.running(), &self.paths.dir_roots()));
                let _ = reply.send(detached);
            }
            Msg::Internal(internal) => self.internal(internal),
        }
    }

    /// Whether a stack has a worktree now.
    fn has_worktree(&self, project: &str, stack: &str) -> bool {
        self.world
            .project(project)
            .is_some_and(|state| state.has_stack(stack))
    }

    /// Stacks whose teardown is pending or running, with the reason prune leaves
    /// them alone.
    fn tearing_down(&self) -> BTreeMap<StackId, String> {
        let pending = self
            .teardowns
            .pending
            .keys()
            .map(|id| (id.clone(), "its teardown is pending".to_string()));
        let running = self
            .teardowns
            .running
            .iter()
            .map(|id| (id.clone(), "its teardown is running".to_string()));
        pending.chain(running).collect()
    }

    /// Instances that are starting or ready, or still have processes running.
    fn running(&self) -> BTreeSet<InstanceKey> {
        self.instances
            .values()
            .filter(|inst| inst.is_active() || inst.any_running())
            .map(|inst| inst.key.clone())
            .collect()
    }

    fn internal(&mut self, internal: Internal) {
        match internal {
            Internal::EnvReady {
                key,
                boot,
                build,
                result,
                provider,
            } => {
                if let Some((path, env)) = provider {
                    self.providers.insert(path, env);
                }
                self.env_ready(&key, boot, build, result);
            }
            Internal::Proc {
                key,
                run,
                idx,
                event,
            } => self.proc_event(&key, run, idx, event),
            Internal::Ready {
                key,
                run,
                idx,
                result,
            } => self.ready(&key, run, idx, result),
            Internal::RestartProcess { key, run, idx } => self.restart_process(&key, run, idx),
            Internal::Tick => self.tick(),
            Internal::TeardownCheck(results) => self.teardown_checked(results),
            Internal::TeardownEnded(id) => {
                self.teardowns.running.remove(&id);
            }
            Internal::Event {
                kind,
                message,
                subjects,
                exit_code,
            } => {
                self.events
                    .push_event(kind, message, subjects, |event| event.exit_code = exit_code);
                self.dirty = true;
            }
        }
    }

    fn publish(&mut self) {
        let snapshot = snapshot::build(&snapshot::Input {
            world: &self.world,
            instances: &self.instances,
            overrides: &self.overrides,
            shares: &self.shares,
            tunnel: self.tunnel.as_ref(),
            latest: self.events.latest(),
            warming: &self.warming,
            now: Instant::now(),
        });
        self.snapshot_tx.send_replace(Arc::new(snapshot));
    }

    fn event(&mut self, kind: EventKind, message: impl Into<String>, subjects: Vec<EventSubject>) {
        self.events.push(kind, message, subjects);
        self.dirty = true;
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn lookup(&self, key: &InstanceKey) -> Result<(&ProjectState, &App), BootFailure> {
        let state = self.world.project(&key.project).ok_or_else(|| {
            BootFailure::new(
                "Unknown project",
                format!("No project named {}", key.project),
            )
        })?;
        let app = state.project.app(&key.app).ok_or_else(|| {
            BootFailure::new(
                "Unknown app",
                format!("No app named {} in {}", key.app, key.project),
            )
        })?;
        Ok((state, app))
    }

    // Starting

    fn ensure_ready(
        &mut self,
        key: InstanceKey,
        path: Option<String>,
        reply: oneshot::Sender<Result<Upstream, BootFailure>>,
    ) {
        if let Some(inst) = self.instances.get_mut(&key) {
            match inst.state {
                Lifecycle::Starting | Lifecycle::Ready => {
                    match inst.answer(path.as_deref()) {
                        Some(result) => {
                            let _ = reply.send(result);
                        }
                        None => {
                            inst.last_activity = Instant::now();
                            inst.waiters.push(Waiter { path, reply });
                        }
                    }
                    return;
                }
                Lifecycle::Stopping => {
                    inst.restart_pending = true;
                    inst.restart_why.get_or_insert(Why::Request);
                    inst.waiters.push(Waiter { path, reply });
                    return;
                }
                Lifecycle::Stopped | Lifecycle::Crashed => {}
            }
        }
        match self.start(&key, None, Why::Request) {
            Ok(()) => {
                let inst = self.instances.get_mut(&key).expect("started instance");
                match inst.answer(path.as_deref()) {
                    Some(result) => {
                        let _ = reply.send(result);
                    }
                    None => inst.waiters.push(Waiter { path, reply }),
                }
            }
            Err(failure) => {
                let _ = reply.send(Err(failure));
            }
        }
    }

    /// Answers once the start a control just began is ready, or with why it isn't
    /// ready, without starting anything.
    fn await_start(
        &mut self,
        key: &InstanceKey,
        reply: oneshot::Sender<Result<Upstream, BootFailure>>,
    ) {
        let Some(inst) = self.instances.get_mut(key) else {
            let _ = reply.send(Err(BootFailure::not_running(&key.app)));
            return;
        };
        match inst.state {
            Lifecycle::Starting | Lifecycle::Ready => match inst.answer(None) {
                Some(result) => {
                    let _ = reply.send(result);
                }
                None => inst.waiters.push(Waiter { path: None, reply }),
            },
            Lifecycle::Stopping if inst.restart_pending => {
                inst.waiters.push(Waiter { path: None, reply });
            }
            Lifecycle::Crashed => {
                let label = inst.label();
                let failure = inst
                    .gave_up
                    .clone()
                    .or_else(|| inst.failed.clone())
                    .unwrap_or_else(|| {
                        BootFailure::new(
                            format!("{label} crashed"),
                            inst.last_error.clone().unwrap_or_default(),
                        )
                    });
                let _ = reply.send(Err(failure));
            }
            Lifecycle::Stopping | Lifecycle::Stopped => {
                let failure = inst
                    .failed
                    .clone()
                    .filter(|_| inst.state == Lifecycle::Stopped)
                    .unwrap_or_else(|| {
                        let mut failure = BootFailure::new(
                            format!("{} was stopped", inst.label()),
                            "It was stopped before it was ready.",
                        );
                        failure.transient = true;
                        failure
                    });
                let _ = reply.send(Err(failure));
            }
        }
    }

    /// Starts an instance and its dependencies. `force` starts a disabled process once.
    /// A crashed or stopping instance whose processes still run is restarted once they
    /// have exited, services last. An instance carte gave up on starts only by hand.
    fn start(
        &mut self,
        key: &InstanceKey,
        force: Option<&str>,
        why: Why,
    ) -> Result<(), BootFailure> {
        if self.shutdown.is_some() {
            return Err(BootFailure::new(
                "carte is stopping",
                "carte is shutting down and starts nothing new.",
            ));
        }
        if self
            .teardowns
            .running
            .contains(&(key.project.clone(), key.stack.slug().to_string()))
        {
            let mut failure = BootFailure::new(
                format!("{}@{} is being torn down", key.app, key.stack.slug()),
                "Its stack's teardown is still running; it starts once that has ended.",
            );
            failure.transient = true;
            return Err(failure);
        }
        if let Some(inst) = self.instances.get_mut(key) {
            if matches!(why, Why::Manual | Why::Restart) {
                inst.gave_up = None;
            } else if let Some(failure) = &inst.gave_up {
                return Err(failure.clone());
            }
        }
        if let Some(inst) = self.instances.get_mut(key)
            && matches!(inst.state, Lifecycle::Crashed | Lifecycle::Stopping)
            && inst.any_running()
        {
            inst.restart_pending = true;
            inst.restart_why.get_or_insert(why);
            if let Some(name) = force {
                inst.restart_forced.insert(name.to_string());
            }
            if inst.state == Lifecycle::Crashed {
                inst.state = Lifecycle::Stopping;
                inst.signal_stop(self.options.grace, true);
            }
            self.dirty = true;
            return Ok(());
        }
        let running = self.instances.values().filter(|i| i.is_active()).count();
        if !lifecycle::under_ceiling(running, self.world.machine.limits.hard_ceiling) {
            return Err(BootFailure::new(
                "Too many instances",
                format!(
                    "{running} instances are running, the hard ceiling. This usually means a loop of services calling each other; raise limits.hard_ceiling if it doesn't."
                ),
            ));
        }
        let (state, app) = self.lookup(key)?;
        let app = state.app_at(app, &key.stack);
        let checkout = state.checkout(app, &key.stack).ok_or_else(|| {
            BootFailure::new(
                format!("{} isn't in this stack", app.name),
                state.not_in_stack(app, &key.stack),
            )
        })?;
        let deps = lifecycle::dependencies(&self.world, key);
        let route_port = app.settings.port;
        let list = procfile::load(checkout.path(), &app.settings)
            .map_err(|err| BootFailure::new(format!("{} can't start", app.name), err.to_string()));
        let boot = self.next_id();
        let overrides = &self.overrides;
        let inst = self
            .instances
            .entry(key.clone())
            .or_insert_with(|| Instance::new(key.clone()));
        let list = match list {
            Ok(list) => list,
            Err(failure) => {
                inst.state = Lifecycle::Crashed;
                inst.last_error = Some(failure.detail.clone());
                inst.failed = Some(failure.clone());
                let message = failure.detail.clone();
                self.event(EventKind::Crashed, message, vec![events::instance(key)]);
                return Err(failure);
            }
        };
        if let Some(name) = force {
            inst.forced.insert(name.to_string());
        }

        let grace = self.options.grace;
        let old_logs: HashMap<String, (Arc<Mutex<ProcLog>>, u32)> = inst
            .procs
            .drain(..)
            .map(|mut p| {
                if let Some(stop) = p.stop.take() {
                    let _ = stop.send(grace);
                }
                (p.spec.name, (p.log, p.restarts))
            })
            .collect();
        let log_cap = self.world.machine.limits.log_buffer_bytes;
        let mut procs = Vec::new();
        for spec in list.processes {
            let port = runner::free_port().map_err(|err| {
                BootFailure::new("No free port", format!("Couldn't find a free port: {err}"))
            })?;
            let (log, restarts) = match old_logs.get(&spec.name) {
                Some((log, restarts)) => (log.clone(), restarts + 1),
                None => (Arc::new(Mutex::new(ProcLog::new(log_cap))), 0),
            };
            procs.push(Proc {
                spec,
                port,
                state: ProcessState::Stopped,
                pid: None,
                stop: None,
                log,
                restarts,
                run: 0,
                started_ms: None,
                manual_stop: false,
                restart: false,
                deferred: false,
                ready: false,
                queued: false,
                spawn: None,
                building: None,
                crashes: Vec::new(),
                restart_at: None,
                gave_up: false,
                stalled: false,
                stalls: 0,
                off: false,
                memory_bytes: 0,
                cpu_percent: 0.0,
                cpu_sample: None,
                terminal: None,
                paused: None,
                held: watch::channel(false).0,
            });
        }
        inst.procs = procs;
        inst.checkout = Some(checkout.clone());
        inst.boot = boot;
        inst.last_error = None;
        inst.failed = None;
        inst.env_changed = false;
        inst.last_activity = Instant::now();
        inst.started_at = Instant::now();
        inst.restart_pending = false;
        inst.restart_why = None;
        if !matches!(why, Why::Restart) {
            inst.opened = false;
            inst.warmed_after = None;
        }
        self.dirty = true;

        if let Some(port) = route_port {
            inst.route_port = Some(port);
            inst.state = Lifecycle::Ready;
            self.started_event(key, &why);
            return Ok(());
        }
        inst.route_port = None;

        let mut to_run = Vec::new();
        let mut off = Vec::new();
        for (idx, proc) in inst.procs.iter().enumerate() {
            let (policy, _) = effective_policy(overrides, key, &proc.spec);
            if policy == Policy::Disabled && !inst.forced.contains(&proc.spec.name) {
                off.push(idx);
                if proc.spec.http {
                    inst.state = Lifecycle::Stopped;
                    return Err(BootFailure::new(
                        format!("{} is disabled", inst.label()),
                        format!(
                            "Its {} process is disabled. Run `carte enable {}` or `carte up {}/{}` to start it once.",
                            proc.spec.name,
                            inst.label(),
                            inst.label(),
                            proc.spec.name
                        ),
                    ));
                }
                continue;
            }
            to_run.push(idx);
        }
        let shared: Vec<usize> = to_run
            .iter()
            .copied()
            .filter(|&idx| inst.procs[idx].spec.scope == Scope::Checkout)
            .collect();
        let checkout_path = checkout.path().clone();
        to_run.retain(|idx| {
            !shared.contains(idx)
                || !self_runs_elsewhere(
                    &self.instances,
                    key,
                    &checkout_path,
                    &self.instances[key].procs[*idx].spec.name,
                )
        });
        let inst = self.instances.get_mut(key).expect("instance");
        for &idx in &shared {
            inst.procs[idx].deferred = !to_run.contains(&idx);
        }
        for idx in off {
            inst.procs[idx].off = true;
        }
        for &idx in &to_run {
            inst.procs[idx].queued = true;
        }
        inst.state = Lifecycle::Starting;
        self.started_event(key, &why);
        self.build_env(key, boot, to_run, None);

        for dep_key in deps {
            if !self
                .instances
                .get(&dep_key)
                .is_some_and(Instance::is_active)
            {
                let _ = self.start(&dep_key, None, Why::Dependency(key.clone()));
            }
        }
        Ok(())
    }

    fn started_event(&mut self, key: &InstanceKey, why: &Why) {
        let mut subjects = vec![events::instance(key)];
        let message = match why {
            Why::Request => "on request".to_string(),
            Why::Manual => "by hand".to_string(),
            Why::Restart => return,
            Why::Dependency(dependent) => {
                subjects.push(events::instance(dependent));
                format!("for {}", events::instance(dependent).label())
            }
            Why::Share => "to keep a pinned share running".to_string(),
            Why::Warm(opened) => {
                subjects.push(events::instance(opened));
                format!("warmed after {} opened", events::instance(opened).label())
            }
        };
        self.event(EventKind::Started, message, subjects);
    }

    /// Starts one process of a running instance, unless it runs or is being started.
    fn start_process(&mut self, key: &InstanceKey, name: &str) -> Result<(), String> {
        let build = self.next_id();
        let inst = self
            .instances
            .get_mut(key)
            .ok_or_else(|| format!("{}@{} isn't running", key.app, key.stack.slug()))?;
        let idx = inst
            .procs
            .iter()
            .position(|p| p.spec.name == name)
            .ok_or_else(|| format!("{} has no process named {name}", inst.label()))?;
        let proc = &mut inst.procs[idx];
        proc.restart_at = None;
        proc.off = false;
        if std::mem::take(&mut proc.gave_up) {
            proc.crashes.clear();
            proc.stalls = 0;
        }
        if proc.is_running() || proc.building.is_some() {
            return Ok(());
        }
        proc.building = Some(build);
        inst.forced.insert(name.to_string());
        let boot = inst.boot;
        self.build_env(key, boot, vec![idx], Some(build));
        Ok(())
    }

    fn build_env(&mut self, key: &InstanceKey, boot: u64, indices: Vec<usize>, build: Option<u64>) {
        let Ok((state, app)) = self.lookup(key) else {
            return;
        };
        let app = state.app_at(app, &key.stack);
        let inst = &self.instances[key];
        let Some(checkout) = inst.checkout.clone() else {
            return;
        };
        let input = EnvInput {
            state: state.clone(),
            app: app.clone(),
            key: key.clone(),
            checkout,
            machine: self.world.machine.clone(),
            procs: indices
                .iter()
                .map(|&idx| (idx, inst.procs[idx].spec.clone(), inst.procs[idx].port))
                .collect(),
            processes: inst
                .procs
                .iter()
                .map(|p| (p.spec.clone(), p.port))
                .collect(),
            cached_provider: self
                .providers
                .get(inst.checkout.as_ref().unwrap().path())
                .cloned(),
            daemon_env: self.daemon_env.clone(),
            pidfile: self.paths.pidfile(key),
            dir_roots: self.paths.dir_roots(),
            ca_bundle: self.options.ca_bundle.clone(),
            ca_cert: self.options.ca_cert.clone(),
            requires: state.project.requires.clone(),
            shared: share::shared_apps(&self.shares, &key.project, &key.stack),
            ptys_free: crate::pty::capacity()
                .map(|c| {
                    c.available(self.world.machine.limits.pty_reserve as usize) >= indices.len()
                })
                .unwrap_or(true),
            branch: None,
            prepare: true,
        };
        let tx = self.tx.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let (result, provider) = input.build();
            let _ = tx.send(Msg::Internal(Internal::EnvReady {
                key,
                boot,
                build,
                result,
                provider,
            }));
        });
    }

    /// Hands a built environment to the queued processes, or starts the process
    /// built on its own unless it was stopped, started or built again meanwhile.
    fn env_ready(
        &mut self,
        key: &InstanceKey,
        boot: u64,
        build: Option<u64>,
        result: Result<Vec<(usize, SpawnSpec)>, String>,
    ) {
        let Some(inst) = self.instances.get(key) else {
            return;
        };
        if inst.boot != boot || !inst.is_active() {
            return;
        }
        let specs = match result {
            Ok(specs) => specs,
            Err(message) => {
                match build {
                    Some(build) if inst.state == Lifecycle::Ready => {
                        self.build_failed(key, build, message)
                    }
                    _ => self.crash(key, None, None, message),
                }
                return;
            }
        };
        for (idx, spec) in specs {
            let proc = &mut self.instances.get_mut(key).expect("instance").procs[idx];
            let own = build.is_some_and(|build| proc.building == Some(build));
            if own {
                proc.building = None;
            }
            if proc.queued {
                proc.spawn = Some(spec);
            } else if own && !proc.is_running() && !proc.off {
                self.spawn_proc(key, idx, spec);
            }
        }
        self.advance(key);
    }

    /// A process's own env build failed on a ready instance: it shows as exited with
    /// the error, and restarts as after a crash while the rest of the instance runs on.
    fn build_failed(&mut self, key: &InstanceKey, build: u64, message: String) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        let Some(idx) = inst.procs.iter().position(|p| p.building == Some(build)) else {
            return;
        };
        let proc = &mut inst.procs[idx];
        proc.building = None;
        if proc.is_running() || proc.off {
            return;
        }
        proc.state = ProcessState::Exited {
            code: None,
            signal: None,
        };
        proc.log
            .lock()
            .unwrap()
            .push(format!("carte: {message}\n").as_bytes());
        let name = proc.spec.name.clone();
        let restarts =
            proc.spec.http || proc.spec.serves_paths() || proc.spec.restart == Restart::OnFailure;
        let message = format!("{name} couldn't start: {message}");
        inst.last_error = Some(message.clone());
        self.event(
            EventKind::Crashed,
            message,
            vec![events::process(key, &name)],
        );
        if restarts {
            self.schedule_restart(key, idx);
        }
    }

    fn spawn_proc(&mut self, key: &InstanceKey, idx: usize, spec: SpawnSpec) {
        let run = self.next_id();
        let boot_timeout = self.options.boot_timeout;
        let inst = self.instances.get_mut(key).expect("instance");
        let proc = &mut inst.procs[idx];
        let (stop_tx, stop_rx) = oneshot::channel();
        proc.stop = Some(stop_tx);
        proc.state = ProcessState::Starting;
        proc.run = run;
        proc.started_ms = Some(runner::now_ms());
        proc.manual_stop = false;
        proc.ready = false;
        proc.stalled = false;
        proc.paused = None;
        let (held, paused) = watch::channel(false);
        proc.held = held;
        let label = LogLabel {
            project: key.project.clone(),
            app: key.app.clone(),
            stack: key.stack.slug().to_string(),
            process: proc.spec.name.clone(),
        };
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        tokio::spawn(runner::run(
            self.spawner.clone(),
            spec,
            proc.log.clone(),
            label,
            self.log_tx.clone(),
            stop_rx,
            event_tx,
        ));
        let tx = self.tx.clone();
        let event_key = key.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let msg = Msg::Internal(Internal::Proc {
                    key: event_key.clone(),
                    run,
                    idx,
                    event,
                });
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        if proc.spec.is_task() {
            let tx = self.tx.clone();
            let ready_key = key.clone();
            tokio::spawn(async move {
                runner::held_sleep(boot_timeout, paused).await;
                let _ = tx.send(Msg::Internal(Internal::Ready {
                    key: ready_key,
                    run,
                    idx,
                    result: Err(format!(
                        "did not finish within {}",
                        carte_core::duration::format(boot_timeout)
                    )),
                }));
            });
        }
        self.dirty = true;
    }

    /// Launches the queued processes whose turn has come: services first, then tasks,
    /// then the rest, each tier once the one before is done, and within a tier after
    /// the processes named in `after`. The instance is ready once nothing blocks.
    fn advance(&mut self, key: &InstanceKey) {
        let Some(inst) = self.instances.get(key) else {
            return;
        };
        if inst.state != Lifecycle::Starting {
            return;
        }
        let launch: Vec<usize> = inst
            .procs
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.spawn.is_some()
                    && !inst.procs.iter().any(|other| {
                        other.blocks()
                            && (other.spec.tier() < p.spec.tier()
                                || p.spec.after.contains(&other.spec.name))
                    })
            })
            .map(|(idx, _)| idx)
            .collect();
        for idx in launch {
            let proc = &mut self.instances.get_mut(key).expect("instance").procs[idx];
            proc.queued = false;
            let spec = proc.spawn.take().expect("queued process");
            self.spawn_proc(key, idx, spec);
        }
        let inst = self.instances.get_mut(key).expect("instance");
        if inst.procs.iter().any(Proc::blocks) {
            return;
        }
        inst.state = Lifecycle::Ready;
        inst.last_activity = Instant::now();
        inst.release_waiters();
        self.ready_event(key);
    }

    fn ready_event(&mut self, key: &InstanceKey) {
        let Some(inst) = self.instances.get(key) else {
            return;
        };
        let boot = inst.started_at.elapsed();
        self.events.push_event(
            EventKind::Ready,
            format!("in {:.1}s", boot.as_secs_f64()),
            vec![events::instance(key)],
            |event| event.boot_ms = Some(boot.as_millis() as u64),
        );
        self.dirty = true;
    }

    /// Waits for a started process to pass its readiness check. It starts once the
    /// process has started, so the process is never ready before it is running.
    fn check_ready(&self, key: &InstanceKey, run: u64, idx: usize) {
        let proc = &self.instances[key].procs[idx];
        let tx = self.tx.clone();
        let port = proc.port;
        let readiness = proc.spec.ready.clone();
        let boot_timeout = self.options.boot_timeout;
        let key = key.clone();
        let paused = proc.held.subscribe();
        tokio::spawn(async move {
            let result = runner::wait_ready(port, &readiness, boot_timeout, paused).await;
            let _ = tx.send(Msg::Internal(Internal::Ready {
                key,
                run,
                idx,
                result,
            }));
        });
    }

    fn proc_event(&mut self, key: &InstanceKey, run: u64, idx: usize, event: runner::Event) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        let Some(proc) = inst.procs.get_mut(idx) else {
            return;
        };
        if proc.run != run {
            return;
        }
        self.dirty = true;
        match event {
            runner::Event::Started { pid, terminal } => {
                if proc.state == ProcessState::Starting {
                    proc.state = ProcessState::Running;
                }
                proc.pid = Some(pid);
                proc.terminal = terminal;
                if proc.spec.is_checked() {
                    self.check_ready(key, run, idx);
                }
                self.groups.add(pid);
                self.advance(key);
            }
            runner::Event::Paused(prompt) => {
                let attached = proc.terminal.as_ref().is_some_and(|t| t.clients() > 0);
                let name = proc.spec.name.clone();
                proc.paused = prompt.clone();
                proc.held.send_replace(prompt.is_some());
                if let Some(prompt) = prompt.filter(|_| !attached) {
                    self.event(
                        EventKind::Paused,
                        format!("at {prompt}"),
                        vec![events::process(key, &name)],
                    );
                }
                self.active(key);
            }
            runner::Event::SpawnFailed(message) => {
                proc.state = ProcessState::Exited {
                    code: None,
                    signal: None,
                };
                proc.stop = None;
                proc.terminal = None;
                let was_paused = proc.paused.take().is_some();
                proc.held.send_replace(false);
                let name = proc.spec.name.clone();
                proc.log
                    .lock()
                    .unwrap()
                    .push(format!("carte: {message}\n").as_bytes());
                if was_paused {
                    self.active(key);
                }
                self.after_exit(
                    key,
                    idx,
                    false,
                    None,
                    format!("{name} couldn't start: {message}"),
                );
            }
            runner::Event::Exited { code, signal } => {
                if let Some(pid) = proc.pid.take() {
                    self.groups.remove(pid);
                }
                proc.stop = None;
                proc.terminal = None;
                let was_paused = proc.paused.take().is_some();
                proc.held.send_replace(false);
                let manual = proc.manual_stop || inst.state == Lifecycle::Stopping;
                proc.state = if manual {
                    ProcessState::Stopped
                } else if proc.spec.is_task() && code == Some(0) {
                    ProcessState::Done
                } else {
                    ProcessState::Exited { code, signal }
                };
                let how = match (code, signal) {
                    (Some(code), _) => format!("exited with code {code}"),
                    (None, Some(signal)) => format!("was killed by signal {signal}"),
                    (None, None) => "exited".to_string(),
                };
                let message = format!("{} {how}", proc.spec.name);
                if proc.spec.preset == Some(Preset::ViteRuby)
                    && let Some(checkout) = &inst.checkout
                {
                    let dir = match &proc.spec.cwd {
                        Some(cwd) => checkout.path().join(cwd),
                        None => checkout.path().clone(),
                    };
                    forget_vite_dev_server(&dir, proc.port);
                }
                if was_paused {
                    self.active(key);
                }
                self.after_exit(key, idx, manual, code, message);
            }
        }
    }

    fn after_exit(
        &mut self,
        key: &InstanceKey,
        idx: usize,
        manual: bool,
        code: Option<i32>,
        message: String,
    ) {
        let grace = self.options.grace;
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        let proc = &inst.procs[idx];
        let name = proc.spec.name.clone();
        let name = name.as_str();
        let always_restarted = proc.spec.http || proc.spec.serves_paths();
        let boot_step = proc.spec.kind.is_some();
        let restarts =
            always_restarted || (proc.spec.restart == Restart::OnFailure && code != Some(0));
        let done = proc.state == ProcessState::Done;
        let restarting = proc.restart;
        match inst.state {
            Lifecycle::Starting | Lifecycle::Ready if done => self.advance(key),
            Lifecycle::Starting if always_restarted && !manual => {
                self.crash(key, code, Some(idx), message);
            }
            Lifecycle::Starting if boot_step && !manual => {
                self.crash(key, code, Some(idx), message);
            }
            Lifecycle::Starting if boot_step && !restarting => {
                let message = format!("{name} was stopped while {} was starting", inst.label());
                self.crash(key, None, Some(idx), message);
            }
            Lifecycle::Starting | Lifecycle::Ready => {
                let crashed = !manual && code != Some(0);
                if crashed {
                    inst.last_error = Some(message.clone());
                }
                let (restart, stalled) = inst
                    .procs
                    .iter_mut()
                    .find(|p| p.spec.name == name)
                    .map(|p| {
                        (
                            std::mem::take(&mut p.restart),
                            std::mem::take(&mut p.stalled),
                        )
                    })
                    .unwrap_or_default();
                if !manual {
                    let kind = if crashed {
                        EventKind::Crashed
                    } else {
                        EventKind::Exited
                    };
                    self.events.push_event(
                        kind,
                        message,
                        vec![events::process(key, name)],
                        |event| event.exit_code = code,
                    );
                    self.dirty = true;
                }
                let ready = inst.state == Lifecycle::Ready;
                if restart {
                    let _ = self.start_process(key, name);
                } else if restarts && ready && (!manual || stalled) {
                    self.schedule_restart(key, idx);
                }
                self.advance(key);
            }
            Lifecycle::Stopping => {
                inst.signal_stop(grace, true);
                if !inst.any_running() {
                    self.finish_stop(key);
                }
            }
            Lifecycle::Crashed => inst.signal_stop(grace, false),
            Lifecycle::Stopped => {}
        }
    }

    /// Restarts a crashed process after a wait that doubles with each crash: 1, 2, 4
    /// and 8 times the first; gives up at the fifth crash within a minute or the fifth
    /// readiness timeout in a row, and on the HTTP process stops the instance.
    fn schedule_restart(&mut self, key: &InstanceKey, idx: usize) {
        let backoff = self.options.restart_backoff;
        let window = self.options.crash_window;
        let Some(proc) = self
            .instances
            .get_mut(key)
            .and_then(|inst| inst.procs.get_mut(idx))
        else {
            return;
        };
        let now = Instant::now();
        proc.crashes.retain(|at| now.duration_since(*at) < window);
        proc.crashes.push(now);
        let crashes = proc.crashes.len() as u32;
        let name = proc.spec.name.clone();
        let message = if proc.stalls >= 5 {
            Some(format!(
                "{name} didn't become ready {} times in a row; it stays down",
                proc.stalls
            ))
        } else if crashes >= 5 {
            Some(format!(
                "{name} crashed {crashes} times within a minute; it stays down"
            ))
        } else {
            None
        };
        if let Some(message) = message {
            proc.gave_up = true;
            let http = proc.spec.http;
            self.event(
                EventKind::GaveUp,
                message.clone(),
                vec![events::process(key, &name)],
            );
            if http {
                self.give_up(key, idx, message);
            } else if let Some(inst) = self.instances.get_mut(key) {
                inst.release_waiters();
            }
            return;
        }
        let delay = backoff * 2u32.pow(crashes - 1);
        proc.restart_at = Some(now + delay);
        let run = proc.run;
        self.dirty = true;
        let tx = self.tx.clone();
        let key = key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(Msg::Internal(Internal::RestartProcess { key, run, idx }));
        });
    }

    /// Restarts a crashed process once its wait is over, unless it was started or
    /// stopped meanwhile; a checkout-scope process another instance started
    /// meanwhile is left to that instance.
    fn restart_process(&mut self, key: &InstanceKey, run: u64, idx: usize) {
        let Some(inst) = self.instances.get(key) else {
            return;
        };
        if inst.state != Lifecycle::Ready {
            return;
        }
        let Some(proc) = inst.procs.get(idx) else {
            return;
        };
        if proc.run != run || proc.restart_at.is_none() || proc.is_running() {
            return;
        }
        let checkout = inst.checkout.as_ref().map(Checkout::path);
        let elsewhere = proc.spec.scope == Scope::Checkout
            && self.instances.values().any(|other| {
                other.key != *key
                    && other.checkout.as_ref().map(Checkout::path) == checkout
                    && other
                        .procs
                        .iter()
                        .any(|p| p.spec.name == proc.spec.name && p.is_running())
            });
        let proc = &mut self.instances.get_mut(key).expect("instance").procs[idx];
        proc.restart_at = None;
        if elsewhere {
            proc.deferred = true;
            self.dirty = true;
            return;
        }
        proc.restarts += 1;
        let name = proc.spec.name.clone();
        self.event(
            EventKind::Restarted,
            "after a crash",
            vec![events::process(key, &name)],
        );
        let _ = self.start_process(key, &name);
    }

    /// A readiness check passed or failed, or a task ran out of time.
    fn ready(&mut self, key: &InstanceKey, run: u64, idx: usize, result: Result<(), String>) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        let starting = inst.state == Lifecycle::Starting;
        let is_ready = inst.state == Lifecycle::Ready;
        let Some(proc) = inst.procs.get_mut(idx) else {
            return;
        };
        if proc.run != run || !proc.is_running() {
            return;
        }
        match result {
            Ok(()) => {
                proc.ready = true;
                self.dirty = true;
                proc.stalls = 0;
                if !starting {
                    inst.release_waiters();
                }
                self.advance(key);
            }
            Err(message) if starting => {
                let message = format!("{} {message}", proc.spec.name);
                self.crash(key, None, Some(idx), message);
            }
            Err(message)
                if (proc.spec.http || proc.spec.serves_paths())
                    && is_ready
                    && !proc.manual_stop =>
            {
                let name = proc.spec.name.clone();
                let message = format!("{name} {message}");
                if let Some(stop) = proc.stop.take() {
                    proc.state = ProcessState::Stopping;
                    proc.manual_stop = true;
                    proc.stalled = true;
                    proc.stalls += 1;
                    let _ = stop.send(self.options.grace);
                }
                inst.last_error = Some(message.clone());
                self.event(
                    EventKind::Crashed,
                    message,
                    vec![events::process(key, &name)],
                );
            }
            Err(_) => {}
        }
    }

    // Stopping

    /// Fails the instance; `failed` is the process whose log explains it.
    fn crash(
        &mut self,
        key: &InstanceKey,
        code: Option<i32>,
        failed: Option<usize>,
        message: String,
    ) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        let log = failed
            .and_then(|idx| inst.procs.get(idx))
            .or(inst.http())
            .or(inst.procs.first())
            .map(|p| p.log.lock().unwrap().tail(40))
            .unwrap_or_default();
        inst.state = Lifecycle::Crashed;
        inst.last_error = Some(message.clone());
        let mut failure = BootFailure::new(format!("{} crashed", inst.label()), message.clone());
        failure.log = log;
        inst.failed = Some(failure.clone());
        inst.fail_waiters(failure);
        inst.signal_stop(self.options.grace, false);
        if let Some(proc) = failed.and_then(|idx| inst.procs.get_mut(idx)) {
            proc.manual_stop = false;
        }
        self.events.push_event(
            EventKind::Crashed,
            message,
            vec![events::instance(key)],
            |event| event.exit_code = code,
        );
        self.dirty = true;
    }

    /// Stops an instance whose HTTP process kept crashing; until it's started by hand,
    /// requests get the error page with the process's log.
    fn give_up(&mut self, key: &InstanceKey, idx: usize, message: String) {
        let grace = self.options.grace;
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        let label = inst.label();
        let mut failure = BootFailure::new(
            format!("{label} crashed"),
            format!("{message}. Run `carte restart {label}` to start it again."),
        );
        failure.log = inst.procs[idx].log.lock().unwrap().tail(40);
        inst.state = Lifecycle::Crashed;
        inst.last_error = Some(message);
        inst.fail_waiters(failure.clone());
        inst.gave_up = Some(failure);
        inst.signal_stop(grace, false);
        self.dirty = true;
    }

    /// Forgets that carte gave up on an instance, so the next request starts it.
    fn forget_give_up(&mut self, key: &InstanceKey, why: &str) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        if inst.gave_up.take().is_none() {
            return;
        }
        for proc in &mut inst.procs {
            proc.gave_up = false;
        }
        if inst.state == Lifecycle::Crashed && !inst.any_running() {
            inst.state = Lifecycle::Stopped;
            self.event(EventKind::Stopped, why, vec![events::instance(key)]);
        }
        self.dirty = true;
    }

    /// Stops an instance. `why` names the reason for its `stopped` or, with a restart
    /// pending, `restarted` event; `None` records nothing.
    fn stop(&mut self, key: &InstanceKey, why: Option<&str>) {
        let grace = self.options.grace;
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        if !inst.is_active() && !inst.any_running() {
            return;
        }
        if let Some(why) = why {
            let kind = if inst.restart_pending {
                inst.restart_why = Some(Why::Restart);
                EventKind::Restarted
            } else {
                EventKind::Stopped
            };
            self.events.push(kind, why, vec![events::instance(key)]);
        }
        let inst = self.instances.get_mut(key).expect("instance");
        let detail = match inst.state {
            Lifecycle::Starting if !inst.restart_pending => Some("It was stopped while starting."),
            Lifecycle::Ready if !inst.restart_pending => {
                Some("It was stopped while one of its processes restarted.")
            }
            _ => None,
        };
        if let Some(detail) = detail {
            let mut failure = BootFailure::new(format!("{} was stopped", inst.label()), detail);
            failure.transient = true;
            inst.fail_waiters(failure);
        }
        inst.state = Lifecycle::Stopping;
        inst.signal_stop(grace, true);
        if !inst.any_running() {
            self.finish_stop(key);
        }
        self.dirty = true;
    }

    /// Drops the restart pending once an instance has stopped, failing the requests
    /// that wait for it, so a stop by hand keeps it stopped.
    fn cancel_restart(&mut self, key: &InstanceKey) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        if !std::mem::take(&mut inst.restart_pending) {
            return;
        }
        inst.restart_why = None;
        inst.restart_forced.clear();
        let mut failure = BootFailure::new(
            format!("{} was stopped", inst.label()),
            "It was stopped before it could start again.",
        );
        failure.transient = true;
        inst.fail_waiters(failure);
    }

    /// Stops one process; `why` as for [`Actor::stop`].
    fn stop_process(
        &mut self,
        key: &InstanceKey,
        name: &str,
        why: Option<&str>,
    ) -> Result<(), String> {
        let grace = self.options.grace;
        let inst = self
            .instances
            .get_mut(key)
            .ok_or_else(|| format!("{}@{} isn't running", key.app, key.stack.slug()))?;
        let proc = inst
            .procs
            .iter_mut()
            .find(|p| p.spec.name == name)
            .ok_or_else(|| format!("{} has no process named {name}", key.app))?;
        if proc.spec.http {
            self.stop(key, why);
            return Ok(());
        }
        let waiting = proc.restart_at.take().is_some();
        let building = proc.building.take().is_some();
        let restarting = proc.restart;
        if !restarting {
            proc.off = true;
            proc.queued = false;
            proc.spawn = None;
        }
        let mut event = None;
        if inst.state == Lifecycle::Starting
            && proc.spec.kind.is_some()
            && proc.restart
            && proc.is_running()
        {
            proc.queued = true;
        }
        if let Some(stop) = proc.stop.take() {
            proc.state = ProcessState::Stopping;
            proc.manual_stop = true;
            let _ = stop.send(grace);
            let kind = if proc.restart {
                EventKind::Restarted
            } else {
                EventKind::Stopped
            };
            event = why.map(|why| (kind, why));
        } else if (waiting || building) && !restarting {
            event = why.map(|why| (EventKind::Stopped, why));
        }
        inst.forced.remove(name);
        inst.release_waiters();
        if let Some((kind, why)) = event {
            self.events
                .push(kind, why, vec![events::process(key, name)]);
        }
        if !restarting {
            self.advance(key);
        }
        self.dirty = true;
        Ok(())
    }

    fn finish_stop(&mut self, key: &InstanceKey) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        inst.state = Lifecycle::Stopped;
        inst.in_flight = 0;
        inst.connections = 0;
        if !inst.restart_pending {
            inst.execs.clear();
        }
        inst.forced.clear();
        inst.failed = None;
        let restart_forced = std::mem::take(&mut inst.restart_forced);
        self.dirty = true;
        if inst.restart_pending {
            inst.restart_pending = false;
            inst.forced = restart_forced;
            let why = inst.restart_why.take().unwrap_or(Why::Request);
            let waiters = std::mem::take(&mut inst.waiters);
            let result = self.start(key, None, why);
            let inst = self.instances.get_mut(key).expect("instance");
            for waiter in waiters {
                match &result {
                    Ok(()) => inst.waiters.push(waiter),
                    Err(failure) => {
                        let _ = waiter.reply.send(Err(failure.clone()));
                    }
                }
            }
            if let Err(failure) = result {
                inst.failed = Some(failure);
            }
        }
    }

    // Activity, idling and limits

    fn activity(&mut self, key: &InstanceKey, boot: Option<u64>, activity: Activity) {
        let current = |inst: &&mut Instance| boot.is_none_or(|boot| inst.boot == boot);
        let Some(inst) = self.instances.get_mut(key).filter(current) else {
            return;
        };
        match activity {
            Activity::RequestStarted => inst.in_flight += 1,
            Activity::RequestEnded => inst.in_flight = inst.in_flight.saturating_sub(1),
            Activity::ConnectionOpened => inst.connections += 1,
            Activity::ConnectionClosed => inst.connections = inst.connections.saturating_sub(1),
        }
        inst.last_activity = Instant::now();
    }

    /// Starts the idle timer over, as when a hold like a pause ends.
    fn active(&mut self, key: &InstanceKey) {
        if let Some(inst) = self.instances.get_mut(key) {
            inst.last_activity = Instant::now();
        }
    }

    fn hold(&mut self, target: &Address) -> Result<(InstanceKey, u64), String> {
        let resolved = self.resolve(target)?;
        let key = resolved
            .instance()
            .ok_or_else(|| format!("{:?} names no app", target.address))?;
        if !self.instances.get(&key).is_some_and(Instance::is_active) {
            return Err(format!("{}@{} isn't running", key.app, key.stack.slug()));
        }
        let id = self.next_id();
        let inst = self.instances.get_mut(&key).expect("instance");
        inst.execs.insert(id);
        inst.last_activity = Instant::now();
        self.dirty = true;
        Ok((key, id))
    }

    fn release(&mut self, key: &InstanceKey, id: u64) {
        if let Some(inst) = self.instances.get_mut(key)
            && inst.execs.remove(&id)
        {
            inst.last_activity = Instant::now();
            self.dirty = true;
        }
    }

    /// The pty of a running process to attach to: the one named, else one paused at
    /// a debugger prompt, else the HTTP process, else the only process.
    fn attach(
        &self,
        target: &Address,
        process: Option<&str>,
    ) -> Result<(Attached, Arc<Terminal>), AttachError> {
        let refused = AttachError::Refused;
        let resolved = self.resolve(target).map_err(refused)?;
        let key = resolved
            .instance()
            .ok_or_else(|| refused(format!("{:?} names no app", target.address)))?;
        let label = format!("{}@{}", key.app, key.stack.slug());
        let process = match (resolved.process.as_deref(), process) {
            (Some(a), Some(b)) if a != b => {
                return Err(refused(format!(
                    "{:?} already names process {a}",
                    target.address
                )));
            }
            (a, b) => a.or(b),
        };
        let inst = self
            .instances
            .get(&key)
            .filter(|inst| inst.any_running())
            .ok_or_else(|| refused(format!("{label} isn't running")))?;
        let paused = inst
            .procs
            .iter()
            .find(|p| p.paused.is_some() && p.terminal.is_some());
        let proc = match process {
            Some(name) => inst
                .procs
                .iter()
                .find(|p| p.spec.name == name)
                .ok_or_else(|| {
                    AttachError::NoProcess(format!("{label} has no process named {name}"))
                })?,
            None => match (paused, inst.http(), inst.procs.as_slice()) {
                (Some(paused), _, _) => paused,
                (None, Some(http), _) => http,
                (None, None, [only]) => only,
                (None, None, procs) => {
                    let names: Vec<&str> = procs.iter().map(|p| p.spec.name.as_str()).collect();
                    return Err(refused(format!(
                        "{label} has no HTTP process; name one: {}",
                        names.join(", ")
                    )));
                }
            },
        };
        let name = &proc.spec.name;
        let terminal = match &proc.terminal {
            Some(terminal) => terminal.clone(),
            None if !proc.is_running() => {
                return Err(refused(format!("{label}/{name} isn't running")));
            }
            None if !proc.spec.pty => {
                return Err(refused(format!(
                    "{label}/{name} runs without a pty (pty = false), so there is nothing to attach to"
                )));
            }
            None => {
                return Err(refused(format!(
                    "{label}/{name} runs without a pty: ptys were running out when it started"
                )));
            }
        };
        Ok((
            Attached {
                instance: label,
                process: name.clone(),
            },
            terminal,
        ))
    }

    pub(super) fn usages(&self) -> Vec<Usage> {
        let mut usages = Vec::new();
        let active: Vec<InstanceKey> = self
            .instances
            .values()
            .filter(|i| i.is_active())
            .map(|i| i.key.clone())
            .collect();
        let holders = lifecycle::holders(&self.world, &active);
        for state in &self.world.projects {
            let running: Vec<InstanceKey> = active
                .iter()
                .filter(|k| k.project == state.project.name)
                .cloned()
                .collect();
            for key in &running {
                let inst = &self.instances[key];
                let pinned = inst.procs.iter().any(|p| {
                    p.is_running()
                        && effective_policy(&self.overrides, key, &p.spec).0 == Policy::Pinned
                });
                usages.push(Usage {
                    key: key.clone(),
                    pinned,
                    starting: inst.state == Lifecycle::Starting,
                    requests_in_flight: inst.in_flight,
                    waiting: inst.waiters.len() as u32,
                    connections: inst.connections,
                    execs: inst.execs.len() as u32,
                    held_by: holders.get(key).cloned().unwrap_or_default(),
                    paused: inst.procs.iter().filter(|p| p.paused.is_some()).count() as u32,
                    last_activity: inst.last_activity,
                    memory_bytes: inst.procs.iter().map(|p| p.memory_bytes).sum(),
                    ptys: inst
                        .procs
                        .iter()
                        .filter(|p| p.is_running() && p.spec.pty)
                        .count() as u32,
                });
            }
        }
        usages
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.ticks += 1;
        if self.ticks.is_multiple_of(2) {
            self.sample_metrics(now);
        }
        self.enforce_log_budget();
        self.adopt_shared_processes();
        self.expire_shares(now);
        let usages = self.usages();
        let mut to_stop = BTreeMap::new();
        for usage in &usages {
            let Some(state) = self.world.project(&usage.key.project) else {
                continue;
            };
            let timeout = state
                .project
                .idle_timeout
                .unwrap_or(self.world.machine.idle_timeout);
            if self.instances[&usage.key].state == Lifecycle::Ready
                && lifecycle::idle_deadline(usage, timeout).is_some_and(|deadline| now >= deadline)
            {
                let why = format!("idle for {}", carte_core::duration::format(timeout));
                to_stop.insert(usage.key.clone(), why);
                let inst = &self.instances[&usage.key];
                if let Some(from) = inst.warmed_after.as_ref().filter(|_| !inst.opened) {
                    self.warming_changed |=
                        self.warming
                            .missed(&usage.key.project, from, &usage.key.app);
                }
            }
        }
        let limits = &self.world.machine.limits;
        if let Ok(ptys) = crate::pty::capacity() {
            let capacity = Capacity {
                total_ram_bytes: total_ram(),
                ptys_in_use: ptys.in_use as u32,
                pty_max: ptys.max as u32,
            };
            let targets = Targets {
                memory: limits.memory_target,
                instances: limits.instance_target,
                pty_reserve: limits.pty_reserve,
            };
            for (key, reason) in lifecycle::evictions(&usages, targets, capacity) {
                let why = match reason {
                    EvictionReason::Memory => "evicted: memory is over its target",
                    EvictionReason::InstanceCount => "evicted: too many instances are running",
                    EvictionReason::Ptys => "evicted: ptys are running out",
                };
                to_stop.entry(key).or_insert_with(|| why.to_string());
            }
        }
        for (key, why) in to_stop {
            self.stop(&key, Some(&why));
        }
        if self.instances.values().any(|i| i.state == Lifecycle::Ready) {
            self.dirty = true;
        }
        self.check_teardowns();
        self.save_warming();
    }

    // Learned warming

    /// Notes a proxied request: what it came from, and whether it opens the instance,
    /// which starts the apps that usually open next.
    fn requested(&mut self, key: &InstanceKey, referrer: Option<&InstanceKey>) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        if !inst.is_active() {
            return;
        }
        let first = !std::mem::replace(&mut inst.opened, true);
        let stack = key.stack.slug().to_string();
        if let Some(from) = referrer.filter(|r| r.project == key.project && r.stack == key.stack) {
            self.warming_changed |=
                self.warming
                    .referred(&key.project, &stack, &key.app, &from.app);
        }
        if first {
            let warm = self
                .warming
                .opened(&key.project, &stack, &key.app, Instant::now());
            self.warming_changed = true;
            self.warm(key, warm);
        }
    }

    /// Starts the apps an open of `opened` warms, in its stack, while there's room.
    fn warm(&mut self, opened: &InstanceKey, edges: Vec<Edge>) {
        for edge in edges {
            let target = InstanceKey {
                project: opened.project.clone(),
                app: edge.app,
                stack: opened.stack.clone(),
            };
            let Some(state) = self.world.project(&target.project) else {
                return;
            };
            let warmable = state.project.app(&target.app).is_some_and(|app| {
                state.checkout(app, &target.stack).is_some() && !app.settings.never_warm
            });
            let running = self
                .instances
                .get(&target)
                .is_some_and(|inst| inst.is_active() || inst.any_running());
            if !warmable || running {
                continue;
            }
            if !self.room_to_warm() {
                return;
            }
            if self.start(&target, None, Why::Warm(opened.clone())).is_ok()
                && let Some(inst) = self.instances.get_mut(&target)
            {
                inst.warmed_after = Some(opened.app.clone());
            }
        }
    }

    fn room_to_warm(&self) -> bool {
        let limits = &self.world.machine.limits;
        let targets = Targets {
            memory: limits.memory_target,
            instances: limits.instance_target,
            pty_reserve: limits.pty_reserve,
        };
        lifecycle::room_to_warm(&self.usages(), targets, total_ram())
    }

    /// Forgets apps and projects that are gone.
    fn forget_gone_apps(&mut self) {
        let world = self.world.clone();
        self.warming_changed |= self.warming.retain(|project, app| {
            world
                .project(project)
                .is_some_and(|state| state.project.app(app).is_some())
        });
    }

    fn save_warming(&mut self) {
        if !std::mem::take(&mut self.warming_changed) {
            return;
        }
        let path = self.paths.warming();
        let partial = path.with_extension("json.partial");
        if std::fs::write(&partial, self.warming.to_json()).is_ok() {
            let _ = std::fs::rename(&partial, &path);
        }
    }

    // Teardown

    /// Notes the stacks that have a worktree, cancels pending teardowns of those that
    /// came back, and checks off the actor's thread whether the others are removed:
    /// every seen stack when discovery changed or every 30s, and in between only those
    /// whose delay has passed.
    fn check_teardowns(&mut self) {
        let mut present = BTreeSet::new();
        for state in &self.world.projects {
            for stack in state.stacks() {
                present.insert((state.project.name.clone(), stack));
            }
        }
        for id in &present {
            self.teardowns.paused.remove(id);
            if self.teardowns.pending.remove(id).is_some() {
                self.event(
                    EventKind::TeardownCancelled,
                    "it has a worktree again",
                    vec![events::stack(&id.0, &id.1)],
                );
            }
        }
        self.teardowns.seen.extend(present.iter().cloned());
        if self.teardowns.checking {
            return;
        }
        let now = Instant::now();
        let all = self.teardowns.stale
            || self
                .teardowns
                .checked_all
                .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(30));
        let troubled = prune::troubled(&self.world);
        let roots = self.paths.dir_roots();
        let candidates: Vec<teardown::Candidate> = self
            .teardowns
            .seen
            .iter()
            .filter(|id| !present.contains(*id) && !self.teardowns.running.contains(*id))
            .filter(|id| {
                all || (self.teardown_due(id, now) && !self.teardowns.paused.contains(*id))
            })
            .map(|(project, stack)| {
                let state = self.world.project(project);
                let troubled = match state {
                    None => Some("its project isn't loaded".to_string()),
                    Some(_) => troubled
                        .get(project)
                        .map(|reason| format!("project {project} has a problem: {reason}")),
                };
                let state = state.filter(|_| troubled.is_none());
                let repos = state.map(prune::project_repos).unwrap_or_default();
                teardown::Candidate {
                    id: (project.clone(), stack.clone()),
                    stacks: state.map(ProjectState::stacks),
                    troubled,
                    repos,
                    stack_dir: roots.stack_dir(project, stack),
                }
            })
            .collect();
        if all {
            self.teardowns.stale = false;
            self.teardowns.checked_all = Some(now);
        }
        if candidates.is_empty() {
            return;
        }
        self.teardowns.checking = true;
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let results = teardown::check(candidates);
            let _ = tx.send(Msg::Internal(Internal::TeardownCheck(results)));
        });
    }

    fn teardown_delay(&self, project: &str) -> Duration {
        self.world
            .project(project)
            .and_then(|state| state.project.teardown_delay)
            .unwrap_or(self.world.machine.teardown_delay)
    }

    /// Whether a stack's teardown is pending and its delay has passed.
    fn teardown_due(&self, id: &StackId, now: Instant) -> bool {
        self.teardowns
            .pending
            .get(id)
            .is_some_and(|since| now.duration_since(*since) >= self.teardown_delay(&id.0))
    }

    /// Schedules the teardown of stacks that are now removed, cancels it for those
    /// that no longer are, and starts the ones whose delay has passed.
    fn teardown_checked(&mut self, results: Vec<(StackId, teardown::Status)>) {
        self.teardowns.checking = false;
        let now = Instant::now();
        for (id, status) in results {
            if !self.teardowns.seen.contains(&id)
                || self.teardowns.running.contains(&id)
                || self.has_worktree(&id.0, &id.1)
            {
                continue;
            }
            let record = match status {
                teardown::Status::Unrecorded => {
                    self.teardowns.seen.remove(&id);
                    self.teardowns.pending.remove(&id);
                    self.teardowns.paused.remove(&id);
                    continue;
                }
                teardown::Status::Kept => {
                    self.teardowns.paused.remove(&id);
                    if self.teardowns.pending.remove(&id).is_some() {
                        self.event(
                            EventKind::TeardownCancelled,
                            "its worktree is back",
                            vec![events::stack(&id.0, &id.1)],
                        );
                    }
                    continue;
                }
                teardown::Status::Unknown(reason) => {
                    if self.teardowns.pending.contains_key(&id)
                        && self.teardowns.paused.insert(id.clone())
                    {
                        self.event(
                            EventKind::TeardownPaused,
                            format!("waits until carte can tell its worktrees are gone: {reason}"),
                            vec![events::stack(&id.0, &id.1)],
                        );
                    }
                    continue;
                }
                teardown::Status::Removed(record) => {
                    self.teardowns.paused.remove(&id);
                    record
                }
            };
            if self.world.project(&id.0).is_none() {
                continue;
            }
            let delay = self.teardown_delay(&id.0);
            match self.teardowns.pending.get(&id) {
                None => {
                    self.teardowns.pending.insert(id.clone(), now);
                    self.event(
                        EventKind::TeardownScheduled,
                        format!(
                            "its worktrees are gone; runs in {}",
                            carte_core::duration::format(delay)
                        ),
                        vec![events::stack(&id.0, &id.1)],
                    );
                }
                Some(since) if now.duration_since(*since) >= delay => {
                    self.start_teardown(id, record.branch.unwrap_or_default());
                }
                Some(_) => {}
            }
        }
    }

    /// Stops what still runs in the stack and hands the rest to a teardown job.
    fn start_teardown(&mut self, id: StackId, branch: String) {
        let Some(state) = self.world.project(&id.0).cloned() else {
            return;
        };
        self.teardowns.pending.remove(&id);
        self.teardowns.paused.remove(&id);
        self.teardowns.seen.remove(&id);
        self.teardowns.running.insert(id.clone());
        let keys: Vec<InstanceKey> = self
            .instances
            .keys()
            .filter(|key| key.project == id.0 && key.stack.slug() == id.1)
            .cloned()
            .collect();
        for key in keys {
            self.stop(&key, Some("its stack was removed"));
        }
        let job = teardown::Job {
            id,
            state,
            machine: self.world.machine.clone(),
            branch,
            daemon_env: self.daemon_env.clone(),
            paths: self.paths.clone(),
            ca_bundle: self.options.ca_bundle.clone(),
            ca_cert: self.options.ca_cert.clone(),
            timeout: self.options.boot_timeout,
            grace: self.options.grace,
            spawner: self.spawner.clone(),
            log_tx: self.log_tx.clone(),
            log_cap: self.world.machine.limits.log_buffer_bytes,
            tx: self.tx.clone(),
        };
        tokio::spawn(job.run());
    }

    /// Starts deferred checkout-scope processes whose running copy went away.
    fn adopt_shared_processes(&mut self) {
        let mut adopt = Vec::new();
        for inst in self.instances.values() {
            if inst.state != Lifecycle::Ready {
                continue;
            }
            let Some(checkout) = &inst.checkout else {
                continue;
            };
            for proc in inst.procs.iter().filter(|p| p.deferred && !p.is_running()) {
                let taken = adopt
                    .iter()
                    .any(|(_, name, path)| *name == proc.spec.name && path == checkout.path());
                if !taken
                    && !self_runs_elsewhere(
                        &self.instances,
                        &inst.key,
                        checkout.path(),
                        &proc.spec.name,
                    )
                {
                    adopt.push((
                        inst.key.clone(),
                        proc.spec.name.clone(),
                        checkout.path().clone(),
                    ));
                }
            }
        }
        for (key, name, _) in adopt {
            if let Some(proc) = self
                .instances
                .get_mut(&key)
                .and_then(|i| i.procs.iter_mut().find(|p| p.spec.name == name))
            {
                proc.deferred = false;
            }
            let _ = self.start_process(&key, &name);
        }
    }

    fn sample_metrics(&mut self, now: Instant) {
        for inst in self.instances.values_mut() {
            for proc in &mut inst.procs {
                let Some(pid) = proc.pid.filter(|_| proc.is_running()) else {
                    proc.memory_bytes = 0;
                    proc.cpu_percent = 0.0;
                    proc.cpu_sample = None;
                    continue;
                };
                let Some(usage) = crate::metrics::group_usage(pid) else {
                    continue;
                };
                proc.memory_bytes = usage.memory_bytes;
                if let Some((at, cpu)) = proc.cpu_sample {
                    proc.cpu_percent = crate::metrics::cpu_percent(cpu, usage.cpu_time, now - at);
                }
                proc.cpu_sample = Some((now, usage.cpu_time));
            }
        }
        self.dirty = true;
    }

    /// Drops the logs of stopped instances, least recently used first, while all logs
    /// together are over the budget. Running processes are only held to their own cap.
    fn enforce_log_budget(&mut self) {
        let budget = self.world.machine.limits.log_budget_bytes;
        let mut total: usize = self
            .instances
            .values()
            .flat_map(|i| &i.procs)
            .map(|p| p.log.lock().unwrap().bytes())
            .sum();
        if total <= budget {
            return;
        }
        let mut stopped: Vec<(Instant, InstanceKey)> = self
            .instances
            .values()
            .filter(|i| !i.is_active() && !i.any_running())
            .map(|i| (i.last_activity, i.key.clone()))
            .collect();
        stopped.sort();
        let cap = self.world.machine.limits.log_buffer_bytes;
        for (_, key) in stopped {
            if total <= budget {
                break;
            }
            for proc in &mut self.instances.get_mut(&key).expect("instance").procs {
                let mut log = proc.log.lock().unwrap();
                total -= log.bytes();
                *log = ProcLog::new(cap);
            }
        }
    }

    // Control

    fn control(
        &mut self,
        request: ControlRequest,
    ) -> Result<(ControlResponse, Vec<super::Started>), String> {
        let target = self.resolve(&request.target)?;
        let keys = self.target_keys(&target, request.action)?;
        let mut response = ControlResponse {
            affected: Vec::new(),
            messages: Vec::new(),
            failures: Vec::new(),
        };
        let mut applied = Vec::new();
        let process = target.process.as_deref();
        for key in keys {
            let label = match process {
                Some(p) => format!("{}@{}/{p}", key.app, key.stack.slug()),
                None => format!("{}@{}", key.app, key.stack.slug()),
            };
            let result = self.apply(&key, process, request.action);
            match result {
                Ok(Some(message)) => {
                    response.affected.push(label);
                    response.messages.push(message);
                    applied.push(key);
                }
                Ok(None) => {
                    response.affected.push(label);
                    applied.push(key);
                }
                Err(message) => response.messages.push(format!("{label}: {message}")),
            }
        }
        if response.affected.is_empty() && !response.messages.is_empty() {
            return Err(response.messages.join("\n"));
        }
        self.dirty = true;
        let wait = request.wait && matches!(request.action, Action::Up | Action::Restart);
        let started = applied
            .into_iter()
            .map(|key| {
                let (reply, outcome) = oneshot::channel();
                if wait {
                    self.await_start(&key, reply);
                }
                (key, outcome)
            })
            .collect();
        Ok((response, started))
    }

    fn apply(
        &mut self,
        key: &InstanceKey,
        process: Option<&str>,
        action: Action,
    ) -> Result<Option<String>, String> {
        let active = self.instances.get(key).is_some_and(Instance::is_active);
        if let Some(name) = process
            && matches!(
                action,
                Action::Pin { .. }
                    | Action::Unpin { .. }
                    | Action::Disable { .. }
                    | Action::Enable { .. }
            )
            && self
                .specs(key)
                .is_some_and(|specs| !specs.iter().any(|s| s.name == name))
        {
            return Err(format!(
                "{}@{} has no process named {name}",
                key.app,
                key.stack.slug()
            ));
        }
        let edit = self.policy_edit(key, process, &action)?;
        let by_hand = Some("by hand");
        match action {
            Action::Up => match process {
                Some(name) if active => self.start_process(key, name).map(|_| None),
                Some(name) => self
                    .start(key, Some(name), Why::Manual)
                    .map(|_| None)
                    .map_err(|f| f.detail),
                None if active => Ok(None),
                None => self
                    .start(key, None, Why::Manual)
                    .map(|_| None)
                    .map_err(|f| f.detail),
            },
            Action::Down => {
                let http = process.is_none_or(|name| {
                    self.specs(key)
                        .is_some_and(|specs| specs.iter().any(|s| s.http && s.name == name))
                });
                if http {
                    self.forget_give_up(key, "by hand");
                    self.cancel_restart(key);
                }
                match process {
                    Some(name) => self.stop_process(key, name, by_hand).map(|_| None),
                    None => {
                        self.stop(key, by_hand);
                        Ok(None)
                    }
                }
            }
            Action::Restart => match process {
                Some(name) if active => {
                    let inst = self.instances.get_mut(key).expect("active instance");
                    match inst.procs.iter_mut().find(|p| p.spec.name == name) {
                        Some(proc) if proc.is_running() && !proc.spec.http => proc.restart = true,
                        Some(proc) if proc.spec.http => inst.restart_pending = true,
                        _ => return self.start_process(key, name).map(|_| None),
                    }
                    self.stop_process(key, name, by_hand).map(|_| None)
                }
                Some(name) => self
                    .start(key, Some(name), Why::Manual)
                    .map(|_| None)
                    .map_err(|f| f.detail),
                None if active => {
                    if let Some(inst) = self.instances.get_mut(key) {
                        inst.restart_pending = true;
                    }
                    self.stop(key, by_hand);
                    Ok(None)
                }
                None => self
                    .start(key, None, Why::Manual)
                    .map(|_| None)
                    .map_err(|f| f.detail),
            },
            Action::Pin { save } => {
                self.set_override(key, process, Policy::Pinned);
                self.policy_event(key, process, "pinned", save);
                if !active {
                    self.start(key, process, Why::Manual)
                        .map_err(|f| f.detail)?;
                } else if let Some(name) = process {
                    self.start_process(key, name)?;
                }
                self.save_policy(edit, key, process, Some(Policy::Pinned))
            }
            Action::Unpin { save } => {
                self.set_override(key, process, Policy::Auto);
                self.policy_event(key, process, "unpinned", save);
                self.save_policy(edit, key, process, None)
            }
            Action::Disable { save } => {
                self.set_override(key, process, Policy::Disabled);
                self.policy_event(key, process, "disabled", save);
                let http = process.is_none_or(|name| {
                    self.specs(key)
                        .is_some_and(|specs| specs.iter().any(|s| s.http && s.name == name))
                });
                if http {
                    self.cancel_restart(key);
                }
                match process {
                    Some(name) if active => {
                        let _ = self.stop_process(key, name, Some("disabled"));
                    }
                    Some(_) => {}
                    None => self.stop(key, Some("disabled")),
                }
                self.save_policy(edit, key, process, Some(Policy::Disabled))
            }
            Action::Enable { save } => {
                self.set_override(key, process, Policy::Auto);
                self.policy_event(key, process, "enabled", save);
                self.save_policy(edit, key, process, None)
            }
        }
    }

    fn policy_event(&mut self, key: &InstanceKey, process: Option<&str>, what: &str, save: bool) {
        let subject = match process {
            Some(name) => events::process(key, name),
            None => events::instance(key),
        };
        let message = if save {
            format!("{what}, saved to the project file")
        } else {
            format!("{what} until the daemon restarts")
        };
        self.event(EventKind::PolicyChanged, message, vec![subject]);
    }

    /// The project file as a saving policy action leaves it, made before the action
    /// changes anything, so a save that can't be made changes nothing.
    fn policy_edit(
        &self,
        key: &InstanceKey,
        process: Option<&str>,
        action: &Action,
    ) -> Result<Option<(PathBuf, String)>, String> {
        let policy = match action {
            Action::Pin { save: true } => Some(Policy::Pinned),
            Action::Disable { save: true } => Some(Policy::Disabled),
            Action::Unpin { save: true } | Action::Enable { save: true } => None,
            _ => return Ok(None),
        };
        let state = self
            .world
            .project(&key.project)
            .ok_or_else(|| format!("no project named {}", key.project))?;
        let path = carte_core::config_edit::policy_file(&state.project)
            .map_err(|err| err.to_string())?
            .to_path_buf();
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("reading {}: {err}", path.display()))?;
        let edited = carte_core::config_edit::set_policy(&text, &key.app, process, policy)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        Ok(Some((path, edited)))
    }

    /// Writes the policy to the project file, applies it to every running instance
    /// of the app and drops the temporary overrides it replaces.
    fn save_policy(
        &mut self,
        edit: Option<(PathBuf, String)>,
        key: &InstanceKey,
        process: Option<&str>,
        policy: Option<Policy>,
    ) -> Result<Option<String>, String> {
        let Some((path, edited)) = edit else {
            return Ok(None);
        };
        let app_policy = self
            .world
            .project(&key.project)
            .and_then(|state| state.project.app(&key.app))
            .and_then(|a| a.settings.policy)
            .unwrap_or_default();
        std::fs::write(&path, edited)
            .map_err(|err| format!("writing {}: {err}", path.display()))?;

        let configured = match (policy, process) {
            (Some(policy), _) => policy,
            (None, Some(_)) => app_policy,
            (None, None) => Policy::Auto,
        };
        for inst in self
            .instances
            .values_mut()
            .filter(|i| i.key.project == key.project && i.key.app == key.app)
        {
            for proc in &mut inst.procs {
                if process.is_none_or(|name| name == proc.spec.name) {
                    proc.spec.policy = configured;
                }
            }
        }
        self.overrides.retain(|(k, p), _| {
            !(k.project == key.project && k.app == key.app && p.as_deref() == process)
        });

        let tx = self.tx.clone();
        let config_dir = self.paths.config_dir.clone();
        tokio::task::spawn_blocking(move || {
            let world = carte_core::discovery::load_world(&config_dir);
            let _ = tx.send(Msg::Reload(world));
        });
        self.dirty = true;
        Ok(Some(format!("saved to {}", path.display())))
    }

    // Sharing

    fn share(&mut self, request: ShareRequest) -> Result<ShareResponse, String> {
        let Some(config) = self.world.machine.share.clone() else {
            return Err(format!(
                "sharing isn't set up: add a [share] section with your domain to {}",
                self.paths.machine_config().display()
            ));
        };
        let target = self.resolve(&request.target)?;
        let stack = target.stack.clone().unwrap_or(Stack::Main);
        let state = self
            .world
            .project(&target.project)
            .ok_or_else(|| format!("no project named {}", target.project))?;
        let mut apps: BTreeSet<String> = target.app.iter().cloned().collect();
        for name in &request.apps {
            let app = state
                .project
                .app(name)
                .or_else(|| state.project.app_by_short(name))
                .ok_or_else(|| format!("{} has no app named {name}", target.project))?;
            apps.insert(app.name.clone());
        }
        if apps.is_empty() {
            return Err("name the apps to share: an app address, or --apps".into());
        }
        let entry = (target.project.clone(), stack.clone());
        let new_credentials = request.auth.then(share::new_credentials).transpose()?;
        let before = share::routes(&self.shares);
        let mut replaced = Vec::new();
        if config.single_host() {
            let one_at_a_time = format!("{} shares one app at a time", config.provider);
            if apps.len() > 1 {
                return Err(one_at_a_time);
            }
            let others: Vec<String> = share::routes(&self.shares)
                .into_keys()
                .filter(|k| {
                    !(k.project == target.project && k.stack == stack && apps.contains(&k.app))
                })
                .map(|k| format!("{}@{}", k.app, k.stack.slug()))
                .collect();
            if !others.is_empty() && request.replace {
                self.shares.retain(|(project, stack_of), share| {
                    project == &target.project
                        && stack_of == &stack
                        && share.apps.iter().all(|a| apps.contains(a))
                });
                replaced = others;
            } else if !others.is_empty() {
                return Err(format!(
                    "{one_at_a_time}, and {} is shared: unshare it first, or share with --replace",
                    others.join(", ")
                ));
            }
        }
        for name in &apps {
            let app = state.project.app(name).expect("resolved app");
            naming::public_url(&config, state, app, &stack).map_err(|e| e.to_string())?;
        }

        let now = Instant::now();
        let share = self
            .shares
            .entry(entry.clone())
            .or_insert_with(|| share::Share {
                apps: BTreeSet::new(),
                credentials: None,
                pin: false,
                started: now,
                expires: None,
                requests: Default::default(),
            });
        share.apps.extend(apps);
        share.pin |= request.pin;
        if let Some(secs) = request.ttl_secs {
            share.expires = Some(now + Duration::from_secs(secs));
        }
        if share.credentials.is_none() {
            share.credentials = new_credentials;
        }
        let credentials = share.credentials.clone();
        let mut messages = Vec::new();
        if !replaced.is_empty() {
            messages.push(format!("stopped sharing {}", replaced.join(", ")));
        }
        if state
            .project
            .apps
            .iter()
            .any(|a| a.settings.subdomains && share.apps.contains(&a.name))
        {
            messages.push(
                "app subdomains aren't reachable when shared: the certificate covers one level"
                    .to_string(),
            );
        }
        self.apply_shares(&before, EventKind::Unshared);
        let status = share::status(&self.world, &config, &entry, &self.shares[&entry], now);
        Ok(ShareResponse {
            share: status,
            credentials,
            messages,
        })
    }

    fn unshare(&mut self, target: &Address) -> Result<Vec<String>, String> {
        let resolved = self.resolve(target)?;
        let entry = (
            resolved.project.clone(),
            resolved.stack.clone().unwrap_or(Stack::Main),
        );
        let before = share::routes(&self.shares);
        let Some(share) = self.shares.remove(&entry) else {
            return Err(format!(
                "nothing in {}@{} is shared",
                entry.0,
                entry.1.slug()
            ));
        };
        self.apply_shares(&before, EventKind::Unshared);
        Ok(share.apps.into_iter().collect())
    }

    fn expire_shares(&mut self, now: Instant) {
        let before = share::routes(&self.shares);
        self.shares
            .retain(|_, share| share.expires.is_none_or(|at| at > now));
        if before.len() != share::routes(&self.shares).len() {
            self.apply_shares(&before, EventKind::ShareExpired);
        }
    }

    /// Publishes the shared routes, restarts running instances whose public URL
    /// appeared or went away, and keeps pinned shares running. Apps no longer shared
    /// get an `ended` event.
    fn apply_shares(&mut self, before: &share::SharedRoutes, ended: EventKind) {
        let after = share::routes(&self.shares);
        self.shared_tx.send_replace(Arc::new(after.clone()));
        let removed: Vec<InstanceKey> = before
            .keys()
            .filter(|k| !after.contains_key(*k))
            .cloned()
            .collect();
        let added: Vec<InstanceKey> = after
            .keys()
            .filter(|k| !before.contains_key(*k))
            .cloned()
            .collect();
        for key in &removed {
            self.events.push(ended, "", vec![events::instance(key)]);
        }
        for key in &added {
            let url = self.world.machine.share.as_ref().and_then(|config| {
                let state = self.world.project(&key.project)?;
                let app = state.project.app(&key.app)?;
                naming::public_url(config, state, app, &key.stack).ok()
            });
            self.events.push(
                EventKind::Shared,
                url.unwrap_or_default(),
                vec![events::instance(key)],
            );
        }
        let changed = removed
            .iter()
            .map(|k| (k, "share ended"))
            .chain(added.iter().map(|k| (k, "share started")));
        for (key, why) in changed {
            if self.instances.get(key).is_some_and(Instance::is_active) {
                if let Some(inst) = self.instances.get_mut(key) {
                    inst.restart_pending = true;
                }
                self.stop(key, Some(why));
            }
        }

        let pinned: BTreeSet<InstanceKey> = self
            .shares
            .iter()
            .filter(|(_, share)| share.pin)
            .flat_map(|((project, stack), share)| {
                share.apps.iter().map(move |app| InstanceKey {
                    project: project.clone(),
                    app: app.clone(),
                    stack: stack.clone(),
                })
            })
            .collect();
        for key in self
            .share_pins
            .difference(&pinned)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.overrides.remove(&(key.clone(), None));
            self.share_pins.remove(&key);
        }
        for key in pinned {
            if self.share_pins.insert(key.clone()) {
                self.overrides.insert((key.clone(), None), Policy::Pinned);
            }
            if !self.instances.get(&key).is_some_and(Instance::is_active) {
                let _ = self.start(&key, None, Why::Share);
            }
        }
        self.dirty = true;
    }

    fn set_override(&mut self, key: &InstanceKey, process: Option<&str>, policy: Policy) {
        let entry = (key.clone(), process.map(str::to_string));
        let configured = self.specs(key).and_then(|specs| {
            let spec = match process {
                Some(name) => specs.into_iter().find(|s| s.name == name),
                None => specs.into_iter().find(|s| s.http),
            }?;
            Some(spec.policy)
        });
        if configured == Some(policy) || (configured.is_none() && policy == Policy::Auto) {
            self.overrides.remove(&entry);
        } else {
            self.overrides.insert(entry, policy);
        }
        self.dirty = true;
    }

    /// An instance's processes: what it started with, else the list read at load.
    fn specs(&self, key: &InstanceKey) -> Option<Vec<&ProcessSpec>> {
        if let Some(inst) = self.instances.get(key).filter(|i| !i.procs.is_empty()) {
            return Some(inst.procs.iter().map(|p| &p.spec).collect());
        }
        let (state, app) = self.lookup(key).ok()?;
        let list = state.process_list(app, &key.stack)?.as_ref().ok()?;
        Some(list.processes.iter().collect())
    }

    fn resolve(&self, address: &Address) -> Result<Target, String> {
        let parsed: address::Address = address
            .address
            .parse()
            .map_err(|e: address::AddressError| e.to_string())?;
        let cwd = address.cwd.as_deref();
        match &address.project {
            Some(project) => address::resolve_in_project(&self.world, project, &parsed, cwd),
            None => address::resolve(&self.world, &parsed, cwd),
        }
        .map_err(|e| e.to_string())
    }

    fn target_keys(&self, target: &Target, action: Action) -> Result<Vec<InstanceKey>, String> {
        let project = &target.project;
        if let Some(app) = &target.app {
            return Ok(vec![InstanceKey {
                project: project.clone(),
                app: app.clone(),
                stack: target.stack.clone().unwrap_or(Stack::Main),
            }]);
        }
        let state = self
            .world
            .project(project)
            .ok_or_else(|| format!("no project named {project}"))?;
        match (&target.stack, action) {
            (Some(Stack::Branch(slug)), Action::Up | Action::Restart) => {
                let mut apps = BTreeSet::new();
                for (app, worktrees) in &state.worktrees {
                    if worktrees.iter().any(|wt| &wt.slug == slug) {
                        apps.insert(app.clone());
                        apps.extend(lifecycle::dependency_closure(&state.project, app));
                    }
                }
                Ok(apps
                    .into_iter()
                    .map(|app| InstanceKey {
                        project: project.clone(),
                        app,
                        stack: Stack::Branch(slug.clone()),
                    })
                    .collect())
            }
            (Some(Stack::Main), Action::Up | Action::Restart) => Ok(state
                .project
                .apps
                .iter()
                .map(|app| InstanceKey {
                    project: project.clone(),
                    app: app.name.clone(),
                    stack: Stack::Main,
                })
                .collect()),
            (_, Action::Up) => Err(format!(
                "name an app or a stack: this would start every app in {project}"
            )),
            (stack, _) => Ok(self
                .instances
                .values()
                .filter(|i| {
                    i.key.project == *project
                        && (i.is_active()
                            || i.any_running()
                            || (action == Action::Down && i.gave_up.is_some()))
                        && stack.as_ref().is_none_or(|s| *s == i.key.stack)
                })
                .map(|i| i.key.clone())
                .collect()),
        }
    }

    fn stop_all(&mut self, idle_only: bool) -> ControlResponse {
        let busy: BTreeSet<InstanceKey> = if idle_only {
            self.usages()
                .into_iter()
                .filter(Usage::is_busy)
                .map(|u| u.key)
                .collect()
        } else {
            BTreeSet::new()
        };
        let keys: Vec<InstanceKey> = self
            .instances
            .values()
            .filter(|i| {
                (i.is_active()
                    || (!idle_only
                        && (i.gave_up.is_some()
                            || (i.state == Lifecycle::Stopping && i.restart_pending))))
                    && !busy.contains(&i.key)
            })
            .map(|i| i.key.clone())
            .collect();
        let why = if idle_only {
            "stop-all --idle"
        } else {
            "stop-all"
        };
        for key in &keys {
            if !idle_only {
                self.forget_give_up(key, why);
                self.cancel_restart(key);
            }
            self.stop(key, Some(why));
        }
        ControlResponse {
            affected: keys
                .iter()
                .map(|k| format!("{}@{}", k.app, k.stack.slug()))
                .collect(),
            messages: Vec::new(),
            failures: Vec::new(),
        }
    }

    // Logs and config

    fn logs(&self, request: &LogsRequest) -> Result<(Vec<LogLine>, LogFilter), String> {
        let resolved = self.resolve(&request.target)?;
        let stack = match (&resolved.app, &resolved.stack) {
            (_, Some(stack)) => Some(stack.clone()),
            (Some(_), None) => Some(Stack::Main),
            (None, None) => None,
        };
        let filter = LogFilter {
            project: resolved.project.clone(),
            stack: stack.as_ref().map(|s| s.slug().to_string()),
            app: resolved.app.clone(),
            process: resolved.process.clone(),
            search: request.search.clone(),
        };
        let mut lines = Vec::new();
        let mut earliest_start: Option<u64> = None;
        let keys = self.instances.keys().filter(|k| {
            k.project == resolved.project
                && stack.as_ref().is_none_or(|s| *s == k.stack)
                && resolved.app.as_ref().is_none_or(|a| *a == k.app)
        });
        for key in keys {
            let inst = &self.instances[key];
            for proc in &inst.procs {
                if resolved
                    .process
                    .as_ref()
                    .is_some_and(|p| *p != proc.spec.name)
                {
                    continue;
                }
                let since = match (request.since_start, proc.started_ms) {
                    (false, _) => request.since_ms.unwrap_or(0),
                    (true, Some(started)) => {
                        earliest_start = Some(earliest_start.map_or(started, |e| e.min(started)));
                        request.since_ms.unwrap_or(0).max(started)
                    }
                    (true, None) => continue,
                };
                let log = proc.log.lock().unwrap();
                for (at_ms, text) in log.lines().filter(|(at_ms, _)| *at_ms >= since) {
                    lines.push(LogLine {
                        project: key.project.clone(),
                        app: key.app.clone(),
                        stack: key.stack.slug().to_string(),
                        process: proc.spec.name.clone(),
                        at_ms: *at_ms,
                        text: text.clone(),
                        marker: false,
                    });
                }
            }
        }
        let markers_since = match (request.since_start, earliest_start) {
            (false, _) => request.since_ms.unwrap_or(0),
            (true, Some(started)) => request.since_ms.unwrap_or(0).max(started),
            (true, None) => u64::MAX,
        };
        lines.extend(
            self.markers
                .iter()
                .filter(|marker| marker.at_ms >= markers_since)
                .cloned(),
        );
        lines.retain(|line| filter.matches(line));
        lines.sort_by_key(|line| line.at_ms);
        if let Some(query) = &request.rank {
            let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
            let order = crate::rank::rank(&texts, query, request.top.unwrap_or(20));
            let ranked = order.into_iter().map(|i| lines[i].clone()).collect();
            return Ok((ranked, filter));
        }
        let narrowed = request.since_ms.is_some() || request.since_start;
        let tail = request
            .tail
            .unwrap_or(if narrowed { usize::MAX } else { 100 });
        let skip = lines.len().saturating_sub(tail);
        Ok((lines.split_off(skip), filter))
    }

    fn mark(&mut self, target: &Address) -> Result<LogLine, String> {
        let resolved = self.resolve(target)?;
        let stack = match (&resolved.app, &resolved.stack) {
            (_, Some(stack)) => Some(stack.slug().to_string()),
            (Some(_), None) => Some(Stack::Main.slug().to_string()),
            (None, None) => None,
        };
        let marker = LogLine {
            project: resolved.project.clone(),
            app: resolved.app.clone().unwrap_or_default(),
            stack: stack.unwrap_or_default(),
            process: resolved.process.clone().unwrap_or_default(),
            at_ms: runner::now_ms(),
            text: String::new(),
            marker: true,
        };
        self.markers.push_back(marker.clone());
        if self.markers.len() > 1000 {
            self.markers.pop_front();
        }
        let _ = self.log_tx.send(marker.clone());
        Ok(marker)
    }

    fn recent_events(
        &self,
        target: Option<&Address>,
        tail: usize,
    ) -> Result<(Vec<Event>, EventFilter), String> {
        let filter = match target {
            None => EventFilter::default(),
            Some(target) => {
                let resolved = self.resolve(target)?;
                let stack = match (&resolved.app, &resolved.stack) {
                    (_, Some(stack)) => Some(stack.slug().to_string()),
                    (Some(_), None) => Some("main".to_string()),
                    (None, None) => None,
                };
                EventFilter {
                    project: Some(resolved.project),
                    stack,
                    app: resolved.app,
                    process: resolved.process,
                }
            }
        };
        Ok((self.events.recent(&filter, tail), filter))
    }

    fn restart_checkout(&mut self, path: &Path) {
        let given_up: Vec<InstanceKey> = self
            .instances
            .values()
            .filter(|i| {
                i.gave_up.is_some() && i.checkout.as_ref().is_some_and(|c| c.path() == path)
            })
            .map(|i| i.key.clone())
            .collect();
        for key in given_up {
            self.forget_give_up(&key, "tmp/restart.txt");
        }
        let keys: Vec<InstanceKey> = self
            .instances
            .values()
            .filter(|i| i.is_active() && i.checkout.as_ref().is_some_and(|c| c.path() == path))
            .map(|i| i.key.clone())
            .collect();
        for key in keys {
            if let Some(inst) = self.instances.get_mut(&key) {
                inst.restart_pending = true;
            }
            self.stop(&key, Some("tmp/restart.txt"));
        }
    }

    /// Takes a new view of config and disk. Instances whose app is gone, or whose
    /// checkout moved (a removed or added worktree), are stopped, and carte forgets
    /// giving up on those whose configuration or checkout changed; the next request
    /// starts them from the right place. A branch switch inside a worktree isn't a move.
    fn reload(&mut self, world: World) {
        if *self.world == world {
            return;
        }
        for (kind, message, subjects) in world_changes(&self.world, &world) {
            self.events.push(kind, message, subjects);
        }
        let old = std::mem::replace(&mut self.world, Arc::new(world));
        self.world_tx.send_replace(self.world.clone());
        self.providers.clear();
        let changed: Vec<InstanceKey> = self
            .instances
            .values()
            .filter(|inst| {
                inst.gave_up.is_some()
                    && start_inputs(&old, &inst.key) != start_inputs(&self.world, &inst.key)
            })
            .map(|inst| inst.key.clone())
            .collect();
        for key in changed {
            self.forget_give_up(&key, "its configuration changed");
        }
        let gone: Vec<(InstanceKey, &'static str)> = self
            .instances
            .values()
            .filter(|inst| inst.is_active() || inst.any_running())
            .filter_map(|inst| {
                let why = match self.world.project(&inst.key.project) {
                    None => "its project is gone",
                    Some(state) => match state.project.app(&inst.key.app) {
                        None => "its app is gone",
                        Some(app) => match state.checkout(app, &inst.key.stack) {
                            None => "its app is gone from the stack",
                            Some(current)
                                if inst
                                    .checkout
                                    .as_ref()
                                    .is_some_and(|c| c.path() != current.path()) =>
                            {
                                "its checkout moved"
                            }
                            Some(_) => return None,
                        },
                    },
                };
                Some((inst.key.clone(), why))
            })
            .collect();
        for (key, why) in gone {
            self.stop(&key, Some(why));
        }
        self.forget_gone_apps();
        self.teardowns.stale = true;
        self.check_teardowns();
        self.dirty = true;
    }
}

/// What an instance starts from: its app's configuration in its stack, the
/// project-wide env, `requires`, env provider and scheme, the machine's
/// configuration, its checkout and the process list read there.
type StartInputs<'a> = (
    &'a App,
    (
        &'a [carte_core::model::EnvTemplate],
        &'a [String],
        Option<carte_core::model::EnvProviderKind>,
        carte_core::model::Scheme,
    ),
    &'a carte_core::model::MachineConfig,
    Option<Checkout>,
    Option<&'a Result<procfile::ProcessList, String>>,
);

fn start_inputs<'a>(world: &'a World, key: &InstanceKey) -> Option<StartInputs<'a>> {
    let state = world.project(&key.project)?;
    let app = state.project.app(&key.app)?;
    let project = &state.project;
    Some((
        state.app_at(app, &key.stack),
        (
            &project.env,
            &project.requires,
            project.env_provider,
            project.scheme,
        ),
        &world.machine,
        state.checkout(app, &key.stack),
        state.process_list(app, &key.stack),
    ))
}

/// Events for what changed between two views of config and disk: config files,
/// worktrees and new problems.
fn world_changes(old: &World, new: &World) -> Vec<(EventKind, String, Vec<EventSubject>)> {
    let mut changes = Vec::new();
    if old.machine != new.machine {
        changes.push((
            EventKind::ConfigReloaded,
            "machine config".to_string(),
            vec![],
        ));
    }
    for state in &new.projects {
        let name = &state.project.name;
        let Some(before) = old.project(name) else {
            changes.push((
                EventKind::ConfigReloaded,
                "project added".to_string(),
                vec![events::project(name)],
            ));
            continue;
        };
        if before.project != state.project {
            changes.push((
                EventKind::ConfigReloaded,
                "project file".to_string(),
                vec![events::project(name)],
            ));
        }
        let worktrees = |state: &ProjectState| -> BTreeSet<(String, String, PathBuf)> {
            state
                .worktrees
                .iter()
                .flat_map(|(app, wts)| {
                    wts.iter()
                        .map(|wt| (app.clone(), wt.slug.clone(), wt.path.clone()))
                })
                .collect()
        };
        let (was, is) = (worktrees(before), worktrees(state));
        let subject = |(app, slug, _): &(String, String, PathBuf)| EventSubject {
            project: name.clone(),
            stack: Some(slug.clone()),
            app: Some(app.clone()),
            process: None,
        };
        for added in is.difference(&was) {
            changes.push((
                EventKind::WorktreeAdded,
                added.2.display().to_string(),
                vec![subject(added)],
            ));
        }
        for removed in was.difference(&is) {
            changes.push((
                EventKind::WorktreeRemoved,
                removed.2.display().to_string(),
                vec![subject(removed)],
            ));
        }
    }
    for state in &old.projects {
        if new.project(&state.project.name).is_none() {
            changes.push((
                EventKind::ConfigReloaded,
                "project removed".to_string(),
                vec![events::project(&state.project.name)],
            ));
        }
    }
    for problem in new.problems.iter().filter(|p| !old.problems.contains(p)) {
        let message = match &problem.app {
            Some(app) => format!("{app}: {}", problem.message),
            None => problem.message.clone(),
        };
        let subjects = problem.project.iter().map(|p| events::project(p)).collect();
        changes.push((EventKind::ConfigError, message, subjects));
    }
    changes
}

/// Why prune leaves an orphan alone, when it belongs to a stack being torn down.
fn torn_down_reason<'a>(
    tearing_down: &'a BTreeMap<StackId, String>,
    orphan: &carte_core::prune::Orphan,
) -> Option<&'a str> {
    let id = (orphan.project.clone()?, orphan.stack.clone()?);
    tearing_down.get(&id).map(String::as_str)
}

/// Another instance from the checkout runs the process, even while stopping it, or
/// is active and ran it until it exited on its own without leaving it to another
/// instance: it restarts the process or leaves it down.
fn self_runs_elsewhere(
    instances: &BTreeMap<InstanceKey, Instance>,
    key: &InstanceKey,
    checkout: &Path,
    name: &str,
) -> bool {
    instances.values().any(|inst| {
        inst.key != *key
            && inst.checkout.as_ref().is_some_and(|c| c.path() == checkout)
            && inst.procs.iter().any(|p| {
                p.spec.name == name
                    && (p.is_running()
                        || inst.is_active()
                            && !p.deferred
                            && matches!(p.state, ProcessState::Exited { .. }))
            })
    })
}

#[cfg(target_os = "macos")]
fn total_ram() -> u64 {
    let mut size: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let name = c"hw.memsize";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut size as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 { size } else { u64::MAX }
}

#[cfg(not(target_os = "macos"))]
fn total_ram() -> u64 {
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages > 0 && page > 0 {
        pages as u64 * page as u64
    } else {
        u64::MAX
    }
}

pub(super) type BuiltEnv = (
    Result<Vec<(usize, SpawnSpec)>, String>,
    Option<(PathBuf, Env)>,
);

/// Everything needed to render the env off the actor's thread.
pub(super) struct EnvInput {
    pub state: ProjectState,
    pub app: App,
    pub key: InstanceKey,
    pub checkout: Checkout,
    pub machine: carte_core::model::MachineConfig,
    pub procs: Vec<(usize, ProcessSpec, u16)>,
    /// Every process of the instance with its port, for their `provides`.
    pub processes: Vec<(ProcessSpec, u16)>,
    pub cached_provider: Option<Env>,
    pub daemon_env: Arc<BTreeMap<String, String>>,
    pub pidfile: PathBuf,
    pub dir_roots: DirRoots,
    pub ca_bundle: PathBuf,
    pub ca_cert: PathBuf,
    pub requires: Vec<String>,
    pub shared: BTreeSet<String>,
    pub ptys_free: bool,
    /// `CARTE_BRANCH` and `{{branch}}`; the checkout's branch when `None`.
    pub branch: Option<String>,
    /// Creates the instance's directories and records its stack's worktrees.
    pub prepare: bool,
}

impl EnvInput {
    pub(super) fn build(self) -> BuiltEnv {
        let checkout_path = self.checkout.path().clone();
        if let Err(message) = check_requires(&self.requires) {
            return (Err(message), None);
        }
        let runner = SystemRunner::from_env();
        let (provider, fresh) = match self.cached_provider {
            Some(env) => (env, None),
            None => {
                let kind =
                    env::detect_provider(self.state.project.env_provider, &checkout_path, &runner);
                match env::load_provider(kind, &checkout_path, &self.daemon_env, &runner) {
                    Ok(env) => (env.clone(), Some((checkout_path.clone(), env))),
                    Err(err) => return (Err(err.to_string()), None),
                }
            }
        };
        let files = match env::read_env_files(&checkout_path, &self.app.settings.env_files) {
            Ok(files) => files,
            Err(err) => return (Err(err.to_string()), fresh),
        };
        let branch = match (&self.branch, &self.checkout) {
            (Some(branch), _) => branch.clone(),
            (None, Checkout::Worktree(wt)) => wt.branch.clone().unwrap_or_default(),
            (None, Checkout::Main { path }) => current_branch(path),
        };
        let dirs = if self.prepare {
            if let Some(dir) = self.pidfile.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match instance_dirs::prepare(&self.dir_roots, &self.key, &self.state) {
                Ok(dirs) => dirs,
                Err(err) => {
                    return (
                        Err(format!("couldn't create the instance's directories: {err}")),
                        fresh,
                    );
                }
            }
        } else {
            self.dir_roots.instance(&self.key)
        };
        let urls = StackUrls {
            machine: &self.machine,
            state: &self.state,
            stack: &self.key.stack,
            shared: &self.shared,
        };
        let tld_length = naming::tld_length(
            &self.machine.tld,
            &self.state.project,
            &self.app,
            &self.key.stack,
        ) as u32;
        let share_domain = self.machine.share.as_ref().map(|s| s.domain.as_str());
        let vite_one_host = carte_core::node::vite_takes_one_host(self.checkout.path());
        let mut out = Vec::new();
        for (idx, spec, port) in &self.procs {
            let request = EnvRequest {
                project: &self.state.project,
                app: &self.app,
                process: spec,
                processes: &self.processes,
                checkout: &self.checkout,
                stack: &self.key.stack,
                branch: &branch,
                port: *port,
                urls: &urls,
                tld: &self.machine.tld,
                share_domain,
                vite_one_host,
                tld_length,
                ca_bundle: &self.ca_bundle,
                ca_cert: &self.ca_cert,
                pidfile: &self.pidfile,
                dirs: &dirs,
            };
            let env = match env::render_env(&request, provider.clone(), &files) {
                Ok(env) => env,
                Err(err) => return (Err(err.to_string()), fresh),
            };
            let cwd = match &spec.cwd {
                Some(dir) => checkout_path.join(dir),
                None => checkout_path.clone(),
            };
            let mut spawn = SpawnSpec::new(spec.command.clone(), cwd);
            spawn.env = env
                .to_vars()
                .into_iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect();
            spawn.io = if spec.pty && self.ptys_free {
                IoMode::default()
            } else {
                IoMode::Pipes
            };
            out.push((*idx, spawn));
        }
        (Ok(out), fresh)
    }
}

fn current_branch(path: &Path) -> String {
    std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Removes vite_ruby's record of a dev server on `port` from `dir/tmp`. Rails
/// trusts the record while its port matches, and the dev server only removes it
/// when it exits by itself, not when it's killed.
fn forget_vite_dev_server(dir: &Path, port: u16) {
    let Ok(entries) = std::fs::read_dir(dir.join("tmp")) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("vite-ruby-") && name.ends_with(".json")) {
            continue;
        }
        let recorded = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|meta| meta.get("port")?.as_u64());
        if recorded == Some(u64::from(port)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Checks a project's `requires`: `tcp://host:port` or `unix:///path`.
fn check_requires(requires: &[String]) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    let timeout = Duration::from_millis(500);
    for entry in requires {
        let reachable = if let Some(path) = entry.strip_prefix("unix://") {
            std::os::unix::net::UnixStream::connect(path).is_ok()
        } else if let Some(address) = entry.strip_prefix("tcp://") {
            address
                .to_socket_addrs()
                .map(|mut addrs| {
                    addrs.any(|addr| {
                        std::net::TcpStream::connect_timeout(&addr, timeout).is_ok_and(|stream| {
                            !crate::loopback::connected_to_itself(
                                &stream,
                                stream.local_addr(),
                                stream.peer_addr(),
                            )
                        })
                    })
                })
                .unwrap_or(false)
        } else {
            return Err(format!(
                "requires entry {entry:?} must start with tcp:// or unix://"
            ));
        };
        if !reachable {
            return Err(format!(
                "{entry} isn't reachable; the project requires it to be running"
            ));
        }
    }
    Ok(())
}
