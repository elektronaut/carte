//! The read-only view of everything, for `carte ls` and the TUI.

use std::collections::BTreeMap;
use std::time::Instant;

use carte_api::{
    AppInfo, Event, Hold, InstanceState, InstanceStatus, Policy as ApiPolicy, ProcessState,
    ProcessStatus, ProjectStatus, Snapshot, StackStatus, TunnelStatus,
};
use carte_core::lifecycle;
use carte_core::model::{App, InstanceKey, Policy, ProjectState, Stack, World};
use carte_core::naming;

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
        .map(|app| {
            let key = InstanceKey {
                project: project.name.clone(),
                app: app.name.clone(),
                stack: stack.clone(),
            };
            let checkout = state.checkout(app, &stack);
            let url = naming::instance_url(&world.machine, project, app, &stack);
            let public_url = world.machine.share.as_ref().and_then(|config| {
                share::shared_apps(shares, &project.name, &stack)
                    .contains(&app.name)
                    .then(|| naming::public_url(config, state, app, &stack).ok())
                    .flatten()
            });
            let Some(inst) = instances.get(&key) else {
                let processes = stopped_processes(input, state, app, &stack, &key, &[]);
                return InstanceStatus {
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
                };
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
                            memory_bytes: p.memory_bytes,
                            cpu_percent: p.cpu_percent,
                        }
                    })
                    .collect()
            };
            InstanceStatus {
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
            }
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
            }
        })
        .collect()
}

fn api_policy(policy: Policy) -> ApiPolicy {
    match policy {
        Policy::Pinned => ApiPolicy::Pinned,
        Policy::Auto => ApiPolicy::Auto,
        Policy::Disabled => ApiPolicy::Disabled,
    }
}
