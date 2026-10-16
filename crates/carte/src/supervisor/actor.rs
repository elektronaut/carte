use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carte_api::{
    Action, Address, ControlRequest, ControlResponse, InstanceState, LogLine, ProcessState,
    Snapshot,
};
use carte_core::address::{self, Target};
use carte_core::env::{self, Env, EnvRequest, SystemRunner};
use carte_core::lifecycle::{self, Capacity, Targets, Usage};
use carte_core::model::{
    App, Checkout, InstanceKey, Policy, ProcessSpec, ProjectState, Scope, Stack, World,
};
use carte_core::{naming, procfile};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::groups::GroupRecord;
use super::runner::{self, LogLabel, ProcLog};
use super::{Activity, BootFailure, Handle, LogFilter, Msg, Options, Upstream, snapshot};
use crate::paths::Paths;
use crate::process::{IoMode, SpawnSpec, Spawner};
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
        result: Result<(), String>,
    },
    Tick,
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
    pub last_activity: Instant,
    pub last_error: Option<String>,
    restart_pending: bool,
    forced: BTreeSet<String>,
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
    manual_stop: bool,
    restart: bool,
}

impl Proc {
    fn is_running(&self) -> bool {
        matches!(
            self.state,
            ProcessState::Starting | ProcessState::Running | ProcessState::Stopping
        )
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
            last_activity: Instant::now(),
            last_error: None,
            restart_pending: false,
            forced: BTreeSet::new(),
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

    fn any_running(&self) -> bool {
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
    next_id: u64,
    ticks: u64,
    dirty: bool,
    shutdown: Option<oneshot::Sender<()>>,
}

pub(super) fn start(world: World, paths: Paths, spawner: Spawner, options: Options) -> Handle {
    let (tx, rx) = mpsc::unbounded_channel();
    let world = Arc::new(world);
    let (world_tx, world_rx) = watch::channel(world.clone());
    let (log_tx, _) = broadcast::channel(4096);
    let initial = Arc::new(snapshot::build(
        &world,
        &BTreeMap::new(),
        &BTreeMap::new(),
        Instant::now(),
    ));
    let (snapshot_tx, snapshot_rx) = watch::channel(initial);
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
        next_id: 0,
        ticks: 0,
        dirty: false,
        shutdown: None,
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
        logs: log_tx,
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
            Msg::EnsureReady { key, reply } => self.ensure_ready(key, reply),
            Msg::Activity(key, activity) => self.activity(&key, activity),
            Msg::Control { request, reply } => {
                let _ = reply.send(self.control(request));
            }
            Msg::StopAll { idle_only, reply } => {
                let _ = reply.send(self.stop_all(idle_only));
            }
            Msg::Logs {
                target,
                tail,
                search,
                reply,
            } => {
                let _ = reply.send(self.logs(&target, tail, search));
            }
            Msg::Reload(world) => self.reload(world),
            Msg::Shutdown(reply) => {
                self.shutdown = Some(reply);
                let keys: Vec<_> = self.instances.keys().cloned().collect();
                for key in keys {
                    self.stop(&key);
                }
            }
            Msg::Internal(internal) => self.internal(internal),
        }
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
            Internal::Ready { key, run, result } => self.ready(&key, run, result),
            Internal::Tick => self.tick(),
        }
    }

    fn publish(&mut self) {
        let snapshot = snapshot::build(
            &self.world,
            &self.instances,
            &self.overrides,
            Instant::now(),
        );
        self.snapshot_tx.send_replace(Arc::new(snapshot));
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
                    let _ = reply.send(inst.upstream().ok_or_else(|| {
                        BootFailure::new(
                            "No web process",
                            format!("{} has no HTTP process", key.app),
                        )
                    }));
                    return;
                }
                InstanceState::Starting => {
                    inst.waiters.push(reply);
                    return;
                }
                InstanceState::Stopping => {
                    inst.restart_pending = true;
                    inst.waiters.push(reply);
                    return;
                }
                InstanceState::Stopped | InstanceState::Crashed => {}
            }
        }
        match self.start(&key, None) {
            Ok(()) => {
                let inst = self.instances.get_mut(&key).expect("started instance");
                if inst.state == InstanceState::Ready {
                    let _ = reply.send(inst.upstream().ok_or_else(|| {
                        BootFailure::new(
                            "No web process",
                            format!("{} has no HTTP process", key.app),
                        )
                    }));
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
    fn start(&mut self, key: &InstanceKey, force: Option<&str>) -> Result<(), BootFailure> {
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
        let checkout = state.checkout(app, &key.stack);
        let deps = lifecycle::dependency_closure(&state.project, &app.name);
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
                self.dirty = true;
                return Err(failure);
            }
        };
        if let Some(name) = force {
            inst.forced.insert(name.to_string());
        }

        let old_logs: HashMap<String, (Arc<Mutex<ProcLog>>, u32)> = inst
            .procs
            .drain(..)
            .map(|p| (p.spec.name, (p.log, p.restarts)))
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
                manual_stop: false,
                restart: false,
                memory_bytes: 0,
                cpu_percent: 0.0,
                cpu_sample: None,
            });
        }
        inst.procs = procs;
        inst.checkout = Some(checkout.clone());
        inst.boot = boot;
        inst.last_error = None;
        inst.last_activity = Instant::now();
        inst.restart_pending = false;
        self.dirty = true;

        if let Some(port) = route_port {
            inst.route_port = Some(port);
            inst.state = InstanceState::Ready;
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
        inst.state = InstanceState::Starting;
        self.build_env(key, boot, to_run);

        for dep in deps {
            let dep_key = InstanceKey {
                project: key.project.clone(),
                app: dep,
                stack: key.stack.clone(),
            };
            if !self
                .instances
                .get(&dep_key)
                .is_some_and(Instance::is_active)
            {
                let _ = self.start(&dep_key, None);
            }
        }
        Ok(())
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
            cached_provider: self
                .providers
                .get(inst.checkout.as_ref().unwrap().path())
                .cloned(),
            daemon_env: self.daemon_env.clone(),
            pidfile: self.paths.pidfile(key),
            ca_bundle: self.options.ca_bundle.clone(),
            ca_cert: self.options.ca_cert.clone(),
            requires: state.project.requires.clone(),
            ptys_free: crate::pty::capacity()
                .map(|c| {
                    c.available(self.world.machine.limits.pty_reserve as usize) >= indices.len()
                })
                .unwrap_or(true),
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
                self.crash(key, message);
                return;
            }
        };
        for (idx, spec) in specs {
            self.spawn_proc(key, idx, spec);
        }
        self.check_started(key);
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
        proc.manual_stop = false;
        let label = LogLabel {
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
        if proc.spec.http {
            let tx = self.tx.clone();
            let port = proc.port;
            let readiness = proc.spec.ready.clone();
            let ready_key = key.clone();
            tokio::spawn(async move {
                let result = runner::wait_ready(port, &readiness, boot_timeout).await;
                let _ = tx.send(Msg::Internal(Internal::Ready {
                    key: ready_key,
                    run,
                    result,
                }));
            });
        }
        self.dirty = true;
    }

    /// An instance without an HTTP process is ready once its processes have started.
    fn check_started(&mut self, key: &InstanceKey) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        if inst.state != InstanceState::Starting || inst.http().is_some() {
            return;
        }
        if inst.procs.iter().all(|p| p.state != ProcessState::Starting) {
            inst.state = InstanceState::Ready;
            for waiter in inst.waiters.drain(..) {
                let _ = waiter.send(Err(BootFailure::new(
                    "No web process",
                    format!("{} has no HTTP process", key.app),
                )));
            }
            self.dirty = true;
        }
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
                self.check_started(key);
            }
            runner::Event::SpawnFailed(message) => {
                proc.state = ProcessState::Exited {
                    code: None,
                    signal: None,
                };
                proc.stop = None;
                let name = proc.spec.name.clone();
                let http = proc.spec.http;
                proc.log
                    .lock()
                    .unwrap()
                    .push(format!("carte: {message}\n").as_bytes());
                self.after_exit(
                    key,
                    &name,
                    http,
                    false,
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
                } else {
                    ProcessState::Exited { code, signal }
                };
                let how = match (code, signal) {
                    (Some(code), _) => format!("exited with code {code}"),
                    (None, Some(signal)) => format!("was killed by signal {signal}"),
                    (None, None) => "exited".to_string(),
                };
                let name = proc.spec.name.clone();
                let http = proc.spec.http;
                let message = format!("{name} {how}");
                self.after_exit(key, &name, http, manual, message);
            }
        }
    }

    fn after_exit(
        &mut self,
        key: &InstanceKey,
        name: &str,
        http: bool,
        manual: bool,
        message: String,
    ) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        match inst.state {
            InstanceState::Starting | InstanceState::Ready if http && !manual => {
                self.crash(key, message);
            }
            InstanceState::Starting | InstanceState::Ready => {
                if !manual {
                    inst.last_error = Some(message);
                }
                let restart = inst
                    .procs
                    .iter_mut()
                    .find(|p| p.spec.name == name)
                    .is_some_and(|p| std::mem::take(&mut p.restart));
                if restart {
                    let _ = self.start_process(key, name);
                }
                self.check_started(key);
            }
            InstanceState::Stopping if !inst.any_running() => self.finish_stop(key),
            _ => {}
        }
    }

    fn ready(&mut self, key: &InstanceKey, run: u64, result: Result<(), String>) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        let Some(http) = inst.procs.iter().find(|p| p.spec.http) else {
            return;
        };
        if http.run != run || !http.is_running() {
            return;
        }
        match result {
            Ok(()) if inst.state == InstanceState::Starting => {
                inst.state = InstanceState::Ready;
                inst.last_activity = Instant::now();
                let upstream = inst.upstream().expect("http process");
                for waiter in inst.waiters.drain(..) {
                    let _ = waiter.send(Ok(upstream));
                }
                self.dirty = true;
            }
            Ok(()) => {}
            Err(message) => {
                let name = http.spec.name.clone();
                self.crash(key, format!("{name} {message}"));
            }
        }
    }

    // Stopping

    fn crash(&mut self, key: &InstanceKey, message: String) {
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        let log = inst
            .http()
            .or(inst.procs.first())
            .map(|p| p.log.lock().unwrap().tail(40))
            .unwrap_or_default();
        inst.state = InstanceState::Crashed;
        inst.last_error = Some(message.clone());
        let mut failure = BootFailure::new(format!("{} crashed", inst.label()), message);
        failure.log = log;
        inst.fail_waiters(failure);
        let grace = self.options.grace;
        for proc in &mut inst.procs {
            if let Some(stop) = proc.stop.take() {
                proc.manual_stop = true;
                let _ = stop.send(grace);
            }
        }
        self.dirty = true;
    }

    fn stop(&mut self, key: &InstanceKey) {
        let grace = self.options.grace;
        let Some(inst) = self.instances.get_mut(key) else {
            return;
        };
        if !inst.is_active() && !inst.any_running() {
            return;
        }
        if inst.state == InstanceState::Starting {
            let mut failure = BootFailure::new(
                format!("{} was stopped", inst.label()),
                "It was stopped while starting.",
            );
            failure.transient = true;
            inst.fail_waiters(failure);
        }
        inst.state = InstanceState::Stopping;
        for proc in &mut inst.procs {
            if let Some(stop) = proc.stop.take() {
                proc.state = ProcessState::Stopping;
                proc.manual_stop = true;
                let _ = stop.send(grace);
            }
        }
        if !inst.any_running() {
            self.finish_stop(key);
        }
        self.dirty = true;
    }

    fn stop_process(&mut self, key: &InstanceKey, name: &str) -> Result<(), String> {
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
            self.stop(key);
            return Ok(());
        }
        if let Some(stop) = proc.stop.take() {
            proc.state = ProcessState::Stopping;
            proc.manual_stop = true;
            let _ = stop.send(grace);
        }
        inst.forced.remove(name);
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
        inst.forced.clear();
        self.dirty = true;
        if inst.restart_pending {
            inst.restart_pending = false;
            let waiters = std::mem::take(&mut inst.waiters);
            let result = self.start(key, None);
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

    pub(super) fn usages(&self) -> Vec<Usage> {
        let mut usages = Vec::new();
        for state in &self.world.projects {
            let running: Vec<InstanceKey> = self
                .instances
                .values()
                .filter(|i| i.key.project == state.project.name && i.is_active())
                .map(|i| i.key.clone())
                .collect();
            let holders = lifecycle::holders(&state.project, &running);
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
        let usages = self.usages();
        let mut to_stop = Vec::new();
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
                to_stop.push(usage.key.clone());
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
            for (key, _reason) in lifecycle::evictions(&usages, targets, capacity) {
                to_stop.push(key);
            }
        }
        for key in to_stop {
            self.stop(&key);
        }
        if self
            .instances
            .values()
            .any(|i| i.state == InstanceState::Ready)
        {
            self.dirty = true;
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

    fn control(&mut self, request: ControlRequest) -> Result<ControlResponse, String> {
        let target = self.resolve(&request.target)?;
        let keys = self.target_keys(&target, request.action)?;
        let mut response = ControlResponse {
            affected: Vec::new(),
            messages: Vec::new(),
        };
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
                }
                Ok(None) => response.affected.push(label),
                Err(message) => response.messages.push(format!("{label}: {message}")),
            }
        }
        if response.affected.is_empty() && !response.messages.is_empty() {
            return Err(response.messages.join("\n"));
        }
        self.dirty = true;
        Ok(response)
    }

    fn apply(
        &mut self,
        key: &InstanceKey,
        process: Option<&str>,
        action: Action,
    ) -> Result<Option<String>, String> {
        let active = self.instances.get(key).is_some_and(Instance::is_active);
        let save_note = |save: bool| {
            save.then(|| {
                "--save isn't supported yet; the change lasts until the daemon restarts".to_string()
            })
        };
        match action {
            Action::Up => match process {
                Some(name) if active => self.start_process(key, name).map(|_| None),
                Some(name) => self
                    .start(key, Some(name))
                    .map(|_| None)
                    .map_err(|f| f.detail),
                None if active => Ok(None),
                None => self.start(key, None).map(|_| None).map_err(|f| f.detail),
            },
            Action::Down => match process {
                Some(name) => self.stop_process(key, name).map(|_| None),
                None => {
                    self.stop(key);
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
                    self.stop_process(key, name).map(|_| None)
                }
                Some(name) => self
                    .start(key, Some(name))
                    .map(|_| None)
                    .map_err(|f| f.detail),
                None if active => {
                    if let Some(inst) = self.instances.get_mut(key) {
                        inst.restart_pending = true;
                    }
                    self.stop(key);
                    Ok(None)
                }
                None => self.start(key, None).map(|_| None).map_err(|f| f.detail),
            },
            Action::Pin { save } => {
                self.set_override(key, process, Policy::Pinned);
                if !active {
                    self.start(key, process).map_err(|f| f.detail)?;
                } else if let Some(name) = process {
                    self.start_process(key, name)?;
                }
                Ok(save_note(save))
            }
            Action::Unpin { save } => {
                self.set_override(key, process, Policy::Auto);
                Ok(save_note(save))
            }
            Action::Disable { save } => {
                self.set_override(key, process, Policy::Disabled);
                match process {
                    Some(name) if active => {
                        let _ = self.stop_process(key, name);
                    }
                    Some(_) => {}
                    None => self.stop(key),
                }
                Ok(save_note(save))
            }
            Action::Enable { save } => {
                self.set_override(key, process, Policy::Auto);
                Ok(save_note(save))
            }
        }
    }

    fn set_override(&mut self, key: &InstanceKey, process: Option<&str>, policy: Policy) {
        let entry = (key.clone(), process.map(str::to_string));
        let configured = self.instances.get(key).and_then(|inst| {
            let spec = match process {
                Some(name) => inst.procs.iter().find(|p| p.spec.name == name),
                None => inst.http(),
            }?;
            Some(spec.spec.policy)
        });
        if configured == Some(policy) || (configured.is_none() && policy == Policy::Auto) {
            self.overrides.remove(&entry);
        } else {
            self.overrides.insert(entry, policy);
        }
        self.dirty = true;
    }

    fn resolve(&self, address: &Address) -> Result<Target, String> {
        let parsed: address::Address = address
            .address
            .parse()
            .map_err(|e: address::AddressError| e.to_string())?;
        address::resolve(&self.world, &parsed, address.cwd.as_deref()).map_err(|e| e.to_string())
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
        for key in &keys {
            self.stop(key);
        }
        ControlResponse {
            affected: keys
                .iter()
                .map(|k| format!("{}@{}", k.app, k.stack.slug()))
                .collect(),
            messages: Vec::new(),
        }
    }

    // Logs and config

    fn logs(
        &self,
        target: &Address,
        tail: usize,
        search: Option<String>,
    ) -> Result<(Vec<LogLine>, LogFilter), String> {
        let resolved = self.resolve(target)?;
        let keys: Vec<InstanceKey> = match &resolved.app {
            Some(app) => vec![InstanceKey {
                project: resolved.project.clone(),
                app: app.clone(),
                stack: resolved.stack.clone().unwrap_or(Stack::Main),
            }],
            None => self
                .instances
                .keys()
                .filter(|k| {
                    k.project == resolved.project
                        && resolved.stack.as_ref().is_none_or(|s| *s == k.stack)
                })
                .cloned()
                .collect(),
        };
        let filter = LogFilter {
            instances: keys
                .iter()
                .map(|k| (k.app.clone(), k.stack.slug().to_string()))
                .collect(),
            process: resolved.process.clone(),
            search: search.clone(),
        };
        let mut lines = Vec::new();
        for key in &keys {
            let Some(inst) = self.instances.get(key) else {
                continue;
            };
            for proc in &inst.procs {
                if resolved
                    .process
                    .as_ref()
                    .is_some_and(|p| *p != proc.spec.name)
                {
                    continue;
                }
                let log = proc.log.lock().unwrap();
                for (at_ms, text) in log.lines() {
                    lines.push(LogLine {
                        app: key.app.clone(),
                        stack: key.stack.slug().to_string(),
                        process: proc.spec.name.clone(),
                        at_ms: *at_ms,
                        text: text.clone(),
                    });
                }
            }
        }
        lines.retain(|line| filter.matches(line));
        lines.sort_by_key(|line| line.at_ms);
        let skip = lines.len().saturating_sub(tail);
        Ok((lines.split_off(skip), filter))
    }

    fn reload(&mut self, world: World) {
        self.world = Arc::new(world);
        self.world_tx.send_replace(self.world.clone());
        self.providers.clear();
        let gone: Vec<InstanceKey> = self
            .instances
            .keys()
            .filter(|key| {
                self.world
                    .project(&key.project)
                    .and_then(|p| p.project.app(&key.app))
                    .is_none()
            })
            .cloned()
            .collect();
        for key in gone {
            self.stop(&key);
        }
        self.dirty = true;
    }
}

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
            && inst
                .procs
                .iter()
                .any(|p| p.spec.name == name && p.is_running())
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

type BuiltEnv = (
    Result<Vec<(usize, SpawnSpec)>, String>,
    Option<(PathBuf, Env)>,
);

/// Everything needed to render the env off the actor's thread.
struct EnvInput {
    state: ProjectState,
    app: App,
    key: InstanceKey,
    checkout: Checkout,
    machine: carte_core::model::MachineConfig,
    procs: Vec<(usize, ProcessSpec, u16)>,
    cached_provider: Option<Env>,
    daemon_env: Arc<BTreeMap<String, String>>,
    pidfile: PathBuf,
    ca_bundle: PathBuf,
    ca_cert: PathBuf,
    requires: Vec<String>,
    ptys_free: bool,
}

impl EnvInput {
    fn build(self) -> BuiltEnv {
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
        let branch = match &self.checkout {
            Checkout::Worktree(wt) => wt.branch.clone(),
            Checkout::Main { path } => current_branch(path),
        };
        if let Some(dir) = self.pidfile.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let shared = BTreeSet::new();
        let urls = StackUrls {
            machine: &self.machine,
            state: &self.state,
            stack: &self.key.stack,
            shared: &shared,
        };
        let tld_length = naming::tld_length(
            &self.machine.tld,
            &self.state.project,
            &self.app,
            &self.key.stack,
        ) as u32;
        let share_domain = self.machine.share.as_ref().map(|s| s.domain.as_str());
        let mut out = Vec::new();
        for (idx, spec, port) in &self.procs {
            let request = EnvRequest {
                project: &self.state.project,
                app: &self.app,
                process: spec,
                checkout: &self.checkout,
                stack: &self.key.stack,
                branch: &branch,
                port: *port,
                urls: &urls,
                tld: &self.machine.tld,
                share_domain,
                tld_length,
                ca_bundle: &self.ca_bundle,
                ca_cert: &self.ca_cert,
                pidfile: &self.pidfile,
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
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
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
