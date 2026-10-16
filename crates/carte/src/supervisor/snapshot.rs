//! The read-only view of everything, for `carte ls` and the TUI.

use std::collections::BTreeMap;
use std::time::Instant;

use carte_api::{
    AppInfo, Hold, InstanceState, InstanceStatus, Policy as ApiPolicy, ProcessStatus,
    ProjectStatus, Snapshot, StackStatus,
};
use carte_core::lifecycle;
use carte_core::model::{InstanceKey, Policy, ProjectState, Stack, World};
use carte_core::naming;

use super::actor::{Instance, Overrides, effective_policy};

pub(super) fn build(
    world: &World,
    instances: &BTreeMap<InstanceKey, Instance>,
    overrides: &Overrides,
    now: Instant,
) -> Snapshot {
    let mut running = 0;
    let projects = world
        .projects
        .iter()
        .map(|state| {
            let active: Vec<InstanceKey> = instances
                .values()
                .filter(|i| {
                    i.key.project == state.project.name
                        && matches!(i.state, InstanceState::Starting | InstanceState::Ready)
                })
                .map(|i| i.key.clone())
                .collect();
            running += active.len() as u32;
            let holders = lifecycle::holders(&state.project, &active);
            let mut stacks = vec![stack_status(
                world,
                state,
                Stack::Main,
                instances,
                overrides,
                &holders,
                now,
            )];
            for slug in state.stacks() {
                stacks.push(stack_status(
                    world,
                    state,
                    Stack::Branch(slug),
                    instances,
                    overrides,
                    &holders,
                    now,
                ));
            }
            ProjectStatus {
                name: state.project.name.clone(),
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
    }
}

fn stack_status(
    world: &World,
    state: &ProjectState,
    stack: Stack,
    instances: &BTreeMap<InstanceKey, Instance>,
    overrides: &Overrides,
    holders: &BTreeMap<InstanceKey, Vec<InstanceKey>>,
    now: Instant,
) -> StackStatus {
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
            let Some(inst) = instances.get(&key) else {
                return InstanceStatus {
                    app: app.name.clone(),
                    stack: stack.slug().to_string(),
                    checkout: checkout.slug().to_string(),
                    checkout_path: checkout.path().clone(),
                    url,
                    public_url: None,
                    state: InstanceState::Stopped,
                    processes: Vec::new(),
                    holds: Vec::new(),
                    idle_in_secs: None,
                    memory_bytes: 0,
                    cpu_percent: 0.0,
                    last_error: None,
                    env_changed: false,
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
                holds.push(Hold::Dependent {
                    app: dependent.app.clone(),
                });
            }
            let timeout = project.idle_timeout.unwrap_or(world.machine.idle_timeout);
            let idle_in_secs =
                (inst.state == InstanceState::Ready && holds.is_empty()).then(|| {
                    (inst.last_activity + timeout)
                        .saturating_duration_since(now)
                        .as_secs()
                });
            let checkout = inst.checkout.clone().unwrap_or(checkout);
            InstanceStatus {
                app: app.name.clone(),
                stack: stack.slug().to_string(),
                checkout: checkout.slug().to_string(),
                checkout_path: checkout.path().clone(),
                url,
                public_url: None,
                state: inst.state,
                processes: inst
                    .procs
                    .iter()
                    .map(|p| {
                        let (policy, temporary) = effective_policy(overrides, &key, &p.spec);
                        ProcessStatus {
                            name: p.spec.name.clone(),
                            state: p.state,
                            policy: match policy {
                                Policy::Pinned => ApiPolicy::Pinned,
                                Policy::Auto => ApiPolicy::Auto,
                                Policy::Disabled => ApiPolicy::Disabled,
                            },
                            temporary,
                            pid: p.pid,
                            port: Some(p.port),
                            http: p.spec.http,
                            restarts: p.restarts,
                            memory_bytes: p.memory_bytes,
                            cpu_percent: p.cpu_percent,
                        }
                    })
                    .collect(),
                holds,
                idle_in_secs,
                memory_bytes: inst.procs.iter().map(|p| p.memory_bytes).sum(),
                cpu_percent: inst.procs.iter().map(|p| p.cpu_percent).sum(),
                last_error: inst.last_error.clone(),
                env_changed: inst.env_changed,
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
