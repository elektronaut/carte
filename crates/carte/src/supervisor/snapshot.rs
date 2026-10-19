//! The read-only view of everything, for `carte ls` and the TUI.

use std::collections::BTreeMap;
use std::time::Instant;

use carte_api::{
    AppInfo, BootProgress, Event, Hold, InstanceState, InstanceStatus, Policy as ApiPolicy,
    ProcessState, ProcessStatus, ProjectStatus, Snapshot, StackStatus, TunnelStatus, Warm,
};
use carte_core::lifecycle;
use carte_core::model::{
    App, Checkout, InstanceKey, Policy, ProjectState, Readiness, Stack, World,
};
use carte_core::naming;
use carte_core::warming::Warming;

use super::actor::{Instance, Overrides, Proc, effective_policy};
use super::share::{self, Shares};

pub(super) struct Input<'a> {
    pub world: &'a World,
    pub instances: &'a BTreeMap<InstanceKey, Instance>,
    pub overrides: &'a Overrides,
    pub shares: &'a Shares,
    pub tunnel: Option<&'a TunnelStatus>,
    /// The latest event about each instance.
    pub latest: &'a BTreeMap<InstanceKey, Event>,
    pub warming: &'a Warming,
    pub now: Instant,
}

pub(super) fn build(input: &Input) -> Snapshot {
    let Input {
        world,
        instances,
        shares,
        tunnel,
        now,
        ..
    } = *input;
    let active: Vec<InstanceKey> = instances
        .values()
        .filter(|i| matches!(i.state, InstanceState::Starting | InstanceState::Ready))
        .map(|i| i.key.clone())
        .collect();
    let running = active.len() as u32;
    let holders = lifecycle::holders(world, &active);
    let projects = world
        .projects
        .iter()
        .map(|state| {
            let mut stacks = vec![stack_status(input, state, Stack::Main, &holders)];
            for slug in state.stacks() {
                stacks.push(stack_status(input, state, Stack::Branch(slug), &holders));
            }
            ProjectStatus {
                name: state.project.name.clone(),
                folder: state.project.folder.clone(),
                apps: state
                    .project
                    .apps
                    .iter()
                    .map(|app| AppInfo {
                        name: app.name.clone(),
                        short: app.short.clone(),
                        path: app.path.clone(),
                        runnable: true,
                    })
                    .collect(),
                stacks,
            }
        })
        .collect();
    Snapshot {
        projects,
        problems: world
            .problems
            .iter()
            .map(|p| carte_api::Problem {
                project: p.project.clone(),
                app: p.app.clone(),
                message: p.message.clone(),
            })
            .collect(),
        running_instances: running,
        memory_bytes: instances
            .values()
            .flat_map(|i| &i.procs)
            .map(|p| p.memory_bytes)
            .sum(),
        shares: match &world.machine.share {
            Some(config) => shares
                .iter()
                .map(|(entry, share)| share::status(world, config, entry, share, now))
                .collect(),
            None => Vec::new(),
        },
        tunnel: tunnel.cloned(),
    }
}

/// The apps an open of `app` in `stack` would warm.
fn warms(warming: &Warming, state: &ProjectState, app: &App, stack: &Stack) -> Vec<Warm> {
    warming
        .warms(&state.project.name, &app.name)
        .into_iter()
        .filter(|edge| {
            state.project.app(&edge.app).is_some_and(|target| {
                state.checkout(target, stack).is_some() && !target.settings.never_warm
            })
        })
        .map(|edge| Warm {
            app: edge.app,
            hits: edge.hits as u32,
            opens: edge.opens as u32,
        })
        .collect()
}

fn stack_status(
    input: &Input,
    state: &ProjectState,
    stack: Stack,
    holders: &BTreeMap<InstanceKey, Vec<InstanceKey>>,
) -> StackStatus {
    let Input {
        world,
        instances,
        overrides,
        shares,
        latest,
        now,
        ..
    } = *input;
    let project = &state.project;
    let instances = project
        .apps
        .iter()
        .filter_map(|app| {
            let key = InstanceKey {
                project: project.name.clone(),
                app: app.name.clone(),
                stack: stack.clone(),
            };
            let checkout = state.checkout(app, &stack);
            if checkout.is_none() && !instances.contains_key(&key) {
                return None;
            }
            let checkout = checkout.unwrap_or_else(|| Checkout::Main {
                path: app.path.clone(),
            });
            let url = naming::instance_url(&world.machine, project, app, &stack);
            let public_url = world.machine.share.as_ref().and_then(|config| {
                share::shared_apps(shares, &project.name, &stack)
                    .contains(&app.name)
                    .then(|| naming::public_url(config, state, app, &stack).ok())
                    .flatten()
            });
            let warms = warms(input.warming, state, app, &stack);
            let Some(inst) = instances.get(&key) else {
                let processes = stopped_processes(input, state, app, &stack, &key, &[]);
                return Some(InstanceStatus {
                    app: app.name.clone(),
                    stack: stack.slug().to_string(),
                    checkout: checkout.slug().to_string(),
                    checkout_path: checkout.path().clone(),
                    url,
                    public_url,
                    state: InstanceState::Stopped,
                    processes,
                    holds: Vec::new(),
                    idle_in_secs: None,
                    memory_bytes: 0,
                    cpu_percent: 0.0,
                    last_error: None,
                    env_changed: false,
                    last_event: latest.get(&key).cloned(),
                    boot: None,
                    warms,
                });
            };
            let mut holds = Vec::new();
            let pinned = inst
                .procs
                .iter()
                .any(|p| effective_policy(overrides, &key, &p.spec).0 == Policy::Pinned);
            if pinned {
                holds.push(Hold::Pinned);
            }
            if inst.in_flight > 0 {
                holds.push(Hold::Requests {
                    count: inst.in_flight,
                });
            }
            if inst.connections > 0 {
                holds.push(Hold::Connections {
                    count: inst.connections,
                });
            }
            if !inst.execs.is_empty() {
                holds.push(Hold::Exec {
                    count: inst.execs.len() as u32,
                });
            }
            for dependent in holders.get(&key).into_iter().flatten() {
                let app = if dependent.stack == key.stack {
                    dependent.app.clone()
                } else {
                    format!("{}@{}", dependent.app, dependent.stack.slug())
                };
                holds.push(Hold::Dependent { app });
            }
            let timeout = project.idle_timeout.unwrap_or(world.machine.idle_timeout);
            let idle_in_secs =
                (inst.state == InstanceState::Ready && holds.is_empty()).then(|| {
                    (inst.last_activity + timeout)
                        .saturating_duration_since(now)
                        .as_secs()
                });
            let checkout = inst.checkout.clone().unwrap_or(checkout);
            let processes = if inst.state == InstanceState::Stopped && !inst.any_running() {
                stopped_processes(input, state, app, &stack, &key, &inst.procs)
            } else {
                inst.procs
                    .iter()
                    .map(|p| {
                        let (policy, temporary) = effective_policy(overrides, &key, &p.spec);
                        ProcessStatus {
                            name: p.spec.name.clone(),
                            state: p.state,
                            policy: api_policy(policy),
                            temporary,
                            pid: p.pid,
                            port: Some(p.port),
                            http: p.spec.http,
                            restarts: p.restarts,
                            gave_up: p.gave_up,
                            restart_in_secs: p
                                .restart_at
                                .filter(|_| inst.state == InstanceState::Ready)
                                .map(|at| {
                                    at.saturating_duration_since(now).as_secs_f64().ceil() as u64
                                }),
                            memory_bytes: p.memory_bytes,
                            cpu_percent: p.cpu_percent,
                        }
                    })
                    .collect()
            };
            Some(InstanceStatus {
                app: app.name.clone(),
                stack: stack.slug().to_string(),
                checkout: checkout.slug().to_string(),
                checkout_path: checkout.path().clone(),
                url,
                public_url,
                state: inst.state,
                processes,
                holds,
                idle_in_secs,
                memory_bytes: inst.procs.iter().map(|p| p.memory_bytes).sum(),
                cpu_percent: inst.procs.iter().map(|p| p.cpu_percent).sum(),
                last_error: inst.last_error.clone(),
                env_changed: inst.env_changed,
                last_event: latest.get(&key).cloned(),
                boot: (inst.state == InstanceState::Starting).then(|| BootProgress {
                    elapsed_ms: now.saturating_duration_since(inst.started_at).as_millis() as u64,
                    waiting_for: waiting_for(&inst.procs),
                }),
                warms,
            })
        })
        .collect();
    StackStatus {
        name: stack.slug().to_string(),
        branch: match &stack {
            Stack::Main => None,
            Stack::Branch(slug) => naming::stack_branch(state, slug).map(str::to_string),
        },
        instances,
    }
}

/// The processes an instance would start, from the process list read at load,
/// keeping ports and restart counts from an earlier run.
fn stopped_processes(
    input: &Input,
    state: &ProjectState,
    app: &App,
    stack: &Stack,
    key: &InstanceKey,
    previous: &[Proc],
) -> Vec<ProcessStatus> {
    let Some(Ok(list)) = state.process_list(app, stack) else {
        return Vec::new();
    };
    list.processes
        .iter()
        .map(|spec| {
            let (policy, temporary) = effective_policy(input.overrides, key, spec);
            let earlier = previous.iter().find(|p| p.spec.name == spec.name);
            ProcessStatus {
                name: spec.name.clone(),
                state: ProcessState::Stopped,
                policy: api_policy(policy),
                temporary,
                pid: None,
                port: earlier.map(|p| p.port),
                http: spec.http,
                restarts: earlier.map_or(0, |p| p.restarts),
                memory_bytes: 0,
                cpu_percent: 0.0,
                gave_up: false,
                restart_in_secs: None,
            }
        })
        .collect()
}

/// What a starting instance waits for, in words.
fn waiting_for(procs: &[Proc]) -> String {
    if procs.iter().all(|p| p.state == ProcessState::Stopped) {
        return "its environment".to_string();
    }
    let waits: Vec<&Proc> = procs.iter().filter(|p| p.blocks() && !p.queued).collect();
    let checked = waits
        .iter()
        .find(|p| p.state != ProcessState::Starting && (p.spec.http || p.spec.is_service()));
    if let Some(proc) = checked {
        let name = &proc.spec.name;
        return match &proc.spec.ready {
            Readiness::Http(path) => format!("{name} to answer {path} on port {}", proc.port),
            Readiness::Port => format!("{name} to listen on port {}", proc.port),
        };
    }
    let tasks: Vec<&str> = waits
        .iter()
        .filter(|p| p.spec.is_task() && p.state != ProcessState::Starting)
        .map(|p| p.spec.name.as_str())
        .collect();
    if !tasks.is_empty() {
        return format!("{} to finish", tasks.join(", "));
    }
    let starting: Vec<&str> = waits
        .iter()
        .filter(|p| p.state == ProcessState::Starting)
        .map(|p| p.spec.name.as_str())
        .collect();
    format!("{} to start", starting.join(", "))
}

fn api_policy(policy: Policy) -> ApiPolicy {
    match policy {
        Policy::Pinned => ApiPolicy::Pinned,
        Policy::Auto => ApiPolicy::Auto,
        Policy::Disabled => ApiPolicy::Disabled,
    }
}
