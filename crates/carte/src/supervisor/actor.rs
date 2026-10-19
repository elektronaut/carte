use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carte_api::{
    Action, Address, ControlRequest, ControlResponse, Event, EventKind, EventSubject,
    InstanceState, LogLine, LogsRequest, ProcessState, ShareRequest, ShareResponse, Snapshot,
    TunnelState, TunnelStatus,
};
use carte_core::address::{self, Target};
use carte_core::dirs::DirRoots;
use carte_core::env::{self, Env, EnvRequest, SystemRunner};
use carte_core::lifecycle::{self, Capacity, EvictionReason, Targets, Usage};
use carte_core::model::{
    App, Checkout, InstanceKey, Policy, ProcessSpec, ProjectState, Restart, Scope, Stack, World,
};
use carte_core::warming::{Edge, Warming};
use carte_core::{naming, procfile};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::events::{self, EventFilter, EventLog};
use super::groups::GroupRecord;
use super::runner::{self, LogLabel, ProcLog};
use super::teardown::{self, StackId, Teardowns};
use super::{Activity, BootFailure, Handle, LogFilter, Msg, Options, Upstream, share, snapshot};
use crate::instance_dirs;
use crate::paths::Paths;
use crate::process::{IoMode, SpawnSpec, Spawner};
use crate::prune;
use crate::urls::StackUrls;

pub(crate) enum Internal {
    EnvReady {
        key: InstanceKey,
        boot: u64,
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

pub(super) struct Instance {
    pub key: InstanceKey,
    pub state: InstanceState,
    boot: u64,
    pub checkout: Option<Checkout>,
    pub procs: Vec<Proc>,
    pub route_port: Option<u16>,
    waiters: Vec<oneshot::Sender<Result<Upstream, BootFailure>>>,
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
    /// When it crashed within the last minute.
    crashes: Vec<Instant>,
    /// It crashed and waits to be restarted at this time.
    pub restart_at: Option<Instant>,
    /// It crashed too often and stays down until started again.
    pub gave_up: bool,
}

impl Proc {
    fn is_running(&self) -> bool {
        matches!(
            self.state,
            ProcessState::Starting | ProcessState::Running | ProcessState::Stopping
        )
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
        } else if self.spec.http || self.spec.is_service() {
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
            state: InstanceState::Stopped,
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
        }
    }

    fn is_active(&self) -> bool {
        matches!(self.state, InstanceState::Starting | InstanceState::Ready)
    }

    fn http(&self) -> Option<&Proc> {
        self.procs.iter().find(|p| p.spec.http)
    }

    fn upstream(&self) -> Option<Upstream> {
        self.route_port
            .or_else(|| self.http().map(|p| p.port))
            .map(|port| Upstream { port })
    }

    pub(super) fn any_running(&self) -> bool {
        self.procs.iter().any(Proc::is_running)
    }

    fn fail_waiters(&mut self, failure: BootFailure) {
        for waiter in self.waiters.drain(..) {
            let _ = waiter.send(Err(failure.clone()));
        }
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
        teardowns: Teardowns::default(),
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
                reply,
                request,
            } => {
                self.ensure_ready(key.clone(), reply);
                if let Some(referrer) = request {
                    self.requested(&key, referrer.as_ref());
                }
            }
            Msg::Activity(key, activity) => self.activity(&key, activity),
            Msg::Hold { target, reply } => {
                let _ = reply.send(self.hold(&target));
            }
            Msg::Release(key, id) => self.release(&key, id),
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
                        TunnelState::Idle | TunnelState::Starting => {}
                    }
                }
                self.tunnel = Some(status);
                self.dirty = true;
            }
            Msg::EnvChanged(path) => {
                self.providers.remove(&path);
                for inst in self.instances.values_mut() {
                    if inst.is_active() && inst.checkout.as_ref().is_some_and(|c| *c.path() == path)
                    {
                        inst.env_changed = true;
                        self.dirty = true;
                    }
                }
            }
            Msg::Shutdown(reply) => {
                self.save_warming();
                self.shutdown = Some(reply);
                let keys: Vec<_> = self.instances.keys().cloned().collect();
                for key in keys {
                    self.stop(&key, None);
                }
            }
            Msg::Running(reply) => {
                let _ = reply.send(self.running());
            }
            Msg::Prune { orphans, reply } => {
                let running = self.running();
                let _ = reply.send(prune::detach(orphans, &running, &self.paths.dir_roots()));
                self.forget_gone_apps();
            }
            Msg::DetachStack {
                project,
                stack,
                orphans,
                reply,
            } => {
                let present = self
                    .world
                    .project(&project)
                    .is_some_and(|state| state.stacks().contains(&stack));
                let detached = (!present)
                    .then(|| prune::detach(orphans, &self.running(), &self.paths.dir_roots()));
                let _ = reply.send(detached);
            }
            Msg::Internal(internal) => self.internal(internal),
        }
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
                result,
                provider,
            } => {
                if let Some((path, env)) = provider {
                    self.providers.insert(path, env);
                }
                self.env_ready(&key, boot, result);
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
        reply: oneshot::Sender<Result<Upstream, BootFailure>>,
    ) {
        if let Some(inst) = self.instances.get_mut(&key) {
            match inst.state {
                InstanceState::Ready => {
                    let _ = reply.send(
                        inst.upstream()
                            .ok_or_else(|| BootFailure::no_web_process(&key.app)),
                    );
                    return;
                }
                InstanceState::Starting => {
                    inst.waiters.push(reply);
                    return;
                }
                InstanceState::Stopping => {
                    inst.restart_pending = true;
                    inst.restart_why.get_or_insert(Why::Request);
                    inst.waiters.push(reply);
                    return;
                }
                InstanceState::Stopped | InstanceState::Crashed => {}
            }
        }
        match self.start(&key, None, Why::Request) {
            Ok(()) => {
                let inst = self.instances.get_mut(&key).expect("started instance");
                if inst.state == InstanceState::Ready {
                    let _ = reply.send(
                        inst.upstream()
                            .ok_or_else(|| BootFailure::no_web_process(&key.app)),
                    );
                } else {
                    inst.waiters.push(reply);
                }
            }
            Err(failure) => {
                let _ = reply.send(Err(failure));
            }
        }
    }

    /// Starts an instance and its dependencies. `force` starts a disabled process once.
    /// A crashed or stopping instance whose processes still run is restarted once they
    /// have exited, services last.
    fn start(
        &mut self,
        key: &InstanceKey,
        force: Option<&str>,
        why: Why,
    ) -> Result<(), BootFailure> {
        if let Some(inst) = self.instances.get_mut(key)
            && matches!(inst.state, InstanceState::Crashed | InstanceState::Stopping)
            && inst.any_running()
        {
            inst.restart_pending = true;
            inst.restart_why.get_or_insert(why);
            if let Some(name) = force {
                inst.restart_forced.insert(name.to_string());
            }
            if inst.state == InstanceState::Crashed {
                inst.state = InstanceState::Stopping;
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
                inst.state = InstanceState::Crashed;
                inst.last_error = Some(failure.detail.clone());
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
                crashes: Vec::new(),
                restart_at: None,
                gave_up: false,
                memory_bytes: 0,
                cpu_percent: 0.0,
                cpu_sample: None,
            });
        }
        inst.procs = procs;
        inst.checkout = Some(checkout.clone());
        inst.boot = boot;
        inst.last_error = None;
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
            inst.state = InstanceState::Ready;
            self.started_event(key, &why);
            return Ok(());
        }
        inst.route_port = None;

        let mut to_run = Vec::new();
        for (idx, proc) in inst.procs.iter().enumerate() {
            let (policy, _) = effective_policy(overrides, key, &proc.spec);
            if policy == Policy::Disabled && !inst.forced.contains(&proc.spec.name) {
                if proc.spec.http {
                    inst.state = InstanceState::Stopped;
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
        for &idx in &to_run {
            inst.procs[idx].queued = true;
        }
        inst.state = InstanceState::Starting;
        self.started_event(key, &why);
        self.build_env(key, boot, to_run);

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

    /// Starts one process of a running instance.
    fn start_process(&mut self, key: &InstanceKey, name: &str) -> Result<(), String> {
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
        if std::mem::take(&mut proc.gave_up) {
            proc.crashes.clear();
        }
        if inst.procs[idx].is_running() {
            return Ok(());
        }
        inst.forced.insert(name.to_string());
        let boot = inst.boot;
        self.build_env(key, boot, vec![idx]);
        Ok(())
    }

    fn build_env(&mut self, key: &InstanceKey, boot: u64, indices: Vec<usize>) {
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
                result,
                provider,
            }));
        });
    }

    fn env_ready(
        &mut self,
        key: &InstanceKey,
        boot: u64,
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
                self.crash(key, None, None, message);
                return;
            }
        };
        for (idx, spec) in specs {
            let proc = &mut self.instances.get_mut(key).expect("instance").procs[idx];
            if proc.queued {
                proc.spawn = Some(spec);
            } else {
                self.spawn_proc(key, idx, spec);
            }
        }
        self.advance(key);
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
        if proc.spec.http || proc.spec.is_service() {
            let tx = self.tx.clone();
            let port = proc.port;
            let readiness = proc.spec.ready.clone();
            let ready_key = key.clone();
            tokio::spawn(async move {
                let result = runner::wait_ready(port, &readiness, boot_timeout).await;
                let _ = tx.send(Msg::Internal(Internal::Ready {
                    key: ready_key,
                    run,
                    idx,
                    result,
                }));
            });
        } else if proc.spec.is_task() {
            let tx = self.tx.clone();
            let ready_key = key.clone();
            tokio::spawn(async move {
                tokio::time::sleep(boot_timeout).await;
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
        if inst.state != InstanceState::Starting {
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
        inst.state = InstanceState::Ready;
        inst.last_activity = Instant::now();
        let result = match inst.http() {
            Some(_) => Ok(inst.upstream().expect("http process")),
            None => Err(BootFailure::no_web_process(&key.app)),
        };
        for waiter in inst.waiters.drain(..) {
            let _ = waiter.send(result.clone());
        }
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
            runner::Event::Started { pid } => {
                if proc.state == ProcessState::Starting {
                    proc.state = ProcessState::Running;
                }
                proc.pid = Some(pid);
                self.groups.add(pid);
                self.advance(key);
            }
            runner::Event::SpawnFailed(message) => {
                proc.state = ProcessState::Exited {
                    code: None,
                    signal: None,
                };
                proc.stop = None;
                let name = proc.spec.name.clone();
                proc.log
                    .lock()
                    .unwrap()
                    .push(format!("carte: {message}\n").as_bytes());
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
                let manual = proc.manual_stop || inst.state == InstanceState::Stopping;
                proc.state = if manual && inst.state != InstanceState::Crashed {
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
        let http = proc.spec.http;
        let boot_step = proc.spec.kind.is_some();
        let on_failure = proc.spec.restart == Restart::OnFailure && code != Some(0);
        let done = proc.state == ProcessState::Done;
        let restarting = proc.restart;
        match inst.state {
            InstanceState::Starting | InstanceState::Ready if done => self.advance(key),
            InstanceState::Starting | InstanceState::Ready if http && !manual => {
                self.crash(key, code, Some(idx), message);
            }
            InstanceState::Starting if boot_step && !manual => {
                self.crash(key, code, Some(idx), message);
            }
            InstanceState::Starting if boot_step && !restarting => {
                let message = format!("{name} was stopped while {} was starting", inst.label());
                self.crash(key, None, Some(idx), message);
            }
            InstanceState::Starting | InstanceState::Ready => {
                let crashed = !manual && code != Some(0);
                if crashed {
                    inst.last_error = Some(message.clone());
                }
                let restart = inst
                    .procs
                    .iter_mut()
                    .find(|p| p.spec.name == name)
                    .is_some_and(|p| std::mem::take(&mut p.restart));
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
                let ready = inst.state == InstanceState::Ready;
                if restart {
                    let _ = self.start_process(key, name);
                } else if on_failure && ready && !manual {
                    self.schedule_restart(key, idx);
                }
                self.advance(key);
            }
            InstanceState::Stopping => {
                inst.signal_stop(grace, true);
                if !inst.any_running() {
                    self.finish_stop(key);
                }
            }
            InstanceState::Crashed => inst.signal_stop(grace, false),
            InstanceState::Stopped => {}
        }
    }

    /// Restarts a crashed process after a wait that doubles with each crash: 1, 2, 4
    /// and 8 times the first; gives up at the fifth crash within a minute.
    fn schedule_restart(&mut self, key: &InstanceKey, idx: usize) {
        let backoff = self.options.restart_backoff;
        let Some(proc) = self
            .instances
            .get_mut(key)
            .and_then(|inst| inst.procs.get_mut(idx))
        else {
            return;
        };
        let now = Instant::now();
        proc.crashes
            .retain(|at| now.duration_since(*at) < Duration::from_secs(60));
        proc.crashes.push(now);
        let crashes = proc.crashes.len() as u32;
        let name = proc.spec.name.clone();
        if crashes >= 5 {
            proc.gave_up = true;
            self.event(
                EventKind::Crashed,
                format!("{name} crashed {crashes} times within a minute; it stays down"),
                vec![events::process(key, &name)],
            );
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
        if inst.state != InstanceState::Ready {
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
                    && other.is_active()
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
        let starting = inst.state == InstanceState::Starting;
        let Some(proc) = inst.procs.get_mut(idx) else {
            return;
        };
        if proc.run != run || !proc.is_running() {
            return;
        }
        match result {
            Ok(()) => {
                proc.ready = true;
                self.advance(key);
            }
            Err(message) if starting || proc.spec.http => {
                let message = format!("{} {message}", proc.spec.name);
                self.crash(key, None, Some(idx), message);
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
        inst.state = InstanceState::Crashed;
        inst.last_error = Some(message.clone());
        let mut failure = BootFailure::new(format!("{} crashed", inst.label()), message.clone());
        failure.log = log;
        inst.fail_waiters(failure);
        inst.signal_stop(self.options.grace, false);
        self.events.push_event(
            EventKind::Crashed,
            message,
            vec![events::instance(key)],
            |event| event.exit_code = code,
        );
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
        if inst.state == InstanceState::Starting {
            let mut failure = BootFailure::new(
                format!("{} was stopped", inst.label()),
                "It was stopped while starting.",
            );
            failure.transient = true;
            inst.fail_waiters(failure);
        }
        inst.state = InstanceState::Stopping;
        inst.signal_stop(grace, true);
        if !inst.any_running() {
            self.finish_stop(key);
        }
        self.dirty = true;
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
        proc.restart_at = None;
        let mut event = None;
        if inst.state == InstanceState::Starting
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
        }
        inst.forced.remove(name);
        if let Some((kind, why)) = event {
            self.events
                .push(kind, why, vec![events::process(key, name)]);
        }
        self.dirty = true;
        Ok(())
    }

    fn finish_stop(&mut self, key: &InstanceKey) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        inst.state = InstanceState::Stopped;
        inst.in_flight = 0;
        inst.connections = 0;
        if !inst.restart_pending {
            inst.execs.clear();
        }
        inst.forced.clear();
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
                        let _ = waiter.send(Err(failure.clone()));
                    }
                }
            }
        }
    }

    // Activity, idling and limits

    fn activity(&mut self, key: &InstanceKey, activity: Activity) {
        let Some(inst) = self.instances.get_mut(key) else {
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
                    requests_in_flight: inst.in_flight,
                    connections: inst.connections,
                    execs: inst.execs.len() as u32,
                    held_by: holders.get(key).cloned().unwrap_or_default(),
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
            if self.instances[&usage.key].state == InstanceState::Ready
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
        if self
            .instances
            .values()
            .any(|i| i.state == InstanceState::Ready)
        {
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
                teardown::Candidate {
                    id: (project.clone(), stack.clone()),
                    stacks: state.map(ProjectState::stacks),
                    troubled,
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
            if !self.teardowns.seen.contains(&id) || self.teardowns.running.contains(&id) {
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
            if inst.state != InstanceState::Ready {
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
    ) -> Result<(ControlResponse, Vec<InstanceKey>), String> {
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
        Ok((response, applied))
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
            Action::Down => match process {
                Some(name) => self.stop_process(key, name, by_hand).map(|_| None),
                None => {
                    self.stop(key, by_hand);
                    Ok(None)
                }
            },
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
                self.maybe_save(save, key, process, Some(Policy::Pinned))
            }
            Action::Unpin { save } => {
                self.set_override(key, process, Policy::Auto);
                self.policy_event(key, process, "unpinned", save);
                self.maybe_save(save, key, process, None)
            }
            Action::Disable { save } => {
                self.set_override(key, process, Policy::Disabled);
                self.policy_event(key, process, "disabled", save);
                match process {
                    Some(name) if active => {
                        let _ = self.stop_process(key, name, Some("disabled"));
                    }
                    Some(_) => {}
                    None => self.stop(key, Some("disabled")),
                }
                self.maybe_save(save, key, process, Some(Policy::Disabled))
            }
            Action::Enable { save } => {
                self.set_override(key, process, Policy::Auto);
                self.policy_event(key, process, "enabled", save);
                self.maybe_save(save, key, process, None)
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

    /// Writes the policy to the project file, applies it to every running instance
    /// of the app and drops the temporary overrides it replaces.
    fn maybe_save(
        &mut self,
        save: bool,
        key: &InstanceKey,
        process: Option<&str>,
        policy: Option<Policy>,
    ) -> Result<Option<String>, String> {
        if !save {
            return Ok(None);
        }
        let state = self
            .world
            .project(&key.project)
            .ok_or_else(|| format!("no project named {}", key.project))?;
        let path = state.project.config_path.clone();
        let app_policy = state
            .project
            .app(&key.app)
            .and_then(|a| a.settings.policy)
            .unwrap_or_default();
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("reading {}: {err}", path.display()))?;
        let edited = carte_core::config_edit::set_policy(&text, &key.app, process, policy)
            .map_err(|err| err.to_string())?;
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
        let entry = (target.project.clone(), stack.clone());
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
        share.expires = request.ttl_secs.map(|secs| now + Duration::from_secs(secs));
        if request.auth && share.credentials.is_none() {
            share.credentials = Some(share::new_credentials());
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
                        && (i.is_active() || i.any_running())
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
            .filter(|i| i.is_active() && !busy.contains(&i.key))
            .map(|i| i.key.clone())
            .collect();
        let why = if idle_only {
            "stop-all --idle"
        } else {
            "stop-all"
        };
        for key in &keys {
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
    /// checkout moved (a removed or added worktree), are stopped; the next request
    /// starts them from the right place. A branch switch inside a worktree isn't a move.
    fn reload(&mut self, world: World) {
        if *self.world == world {
            return;
        }
        for (kind, message, subjects) in world_changes(&self.world, &world) {
            self.events.push(kind, message, subjects);
        }
        self.world = Arc::new(world);
        self.world_tx.send_replace(self.world.clone());
        self.providers.clear();
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

/// Another active instance from the checkout runs the process, or ran it until it
/// exited on its own without leaving it to another instance: it restarts the process
/// or leaves it down.
fn self_runs_elsewhere(
    instances: &BTreeMap<InstanceKey, Instance>,
    key: &InstanceKey,
    checkout: &Path,
    name: &str,
) -> bool {
    instances.values().any(|inst| {
        inst.key != *key
            && inst.is_active()
            && inst.checkout.as_ref().is_some_and(|c| c.path() == checkout)
            && inst.procs.iter().any(|p| {
                p.spec.name == name
                    && (p.is_running()
                        || !p.deferred && matches!(p.state, ProcessState::Exited { .. }))
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
                    addrs.any(|addr| std::net::TcpStream::connect_timeout(&addr, timeout).is_ok())
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
