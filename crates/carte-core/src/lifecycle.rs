use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::model::{InstanceKey, MemoryTarget, Project, Stack, World};

/// What the supervisor knows about one running (or starting) instance when deciding
/// whether it may idle out or be evicted.
#[derive(Debug, Clone)]
pub struct Usage {
    pub key: InstanceKey,
    pub pinned: bool,
    pub requests_in_flight: u32,
    /// Open upgraded connections (WebSocket, SSE).
    pub connections: u32,
    /// Commands run with `carte exec` that are still running.
    pub execs: u32,
    /// Running instances that depend on this one.
    pub held_by: Vec<InstanceKey>,
    /// Processes waiting at a debugger prompt.
    pub paused: u32,
    pub last_activity: Instant,
    pub memory_bytes: u64,
    pub ptys: u32,
}

impl Usage {
    /// Busy instances are never stopped for idleness or evicted.
    pub fn is_busy(&self) -> bool {
        self.pinned
            || self.requests_in_flight > 0
            || self.connections > 0
            || self.execs > 0
            || self.paused > 0
            || !self.held_by.is_empty()
    }
}

/// When an instance stops for idleness, or `None` while something keeps it alive.
pub fn idle_deadline(usage: &Usage, timeout: Duration) -> Option<Instant> {
    (!usage.is_busy()).then(|| usage.last_activity + timeout)
}

/// The machine's current headroom, as seen by the supervisor.
#[derive(Debug, Clone, Copy)]
pub struct Capacity {
    pub total_ram_bytes: u64,
    pub ptys_in_use: u32,
    pub pty_max: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Targets {
    pub memory: MemoryTarget,
    pub instances: Option<u32>,
    pub pty_reserve: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionReason {
    Memory,
    InstanceCount,
    Ptys,
}

/// Picks least recently used idle instances to stop until every target is met, or no
/// candidate is left. Targets never block a start: a chain of busy instances may run
/// over them.
pub fn evictions(
    usages: &[Usage],
    targets: Targets,
    capacity: Capacity,
) -> Vec<(InstanceKey, EvictionReason)> {
    let memory_target = memory_target_bytes(targets.memory, capacity.total_ram_bytes);
    let pty_budget = capacity.pty_max.saturating_sub(targets.pty_reserve);

    let mut memory: u64 = usages.iter().map(|u| u.memory_bytes).sum();
    let mut count = usages.len() as u32;
    let mut ptys = capacity.ptys_in_use;

    let mut candidates: Vec<&Usage> = usages.iter().filter(|u| !u.is_busy()).collect();
    candidates.sort_by_key(|u| u.last_activity);

    let mut out = Vec::new();
    for candidate in candidates {
        let reason = if memory > memory_target {
            EvictionReason::Memory
        } else if targets.instances.is_some_and(|max| count > max) {
            EvictionReason::InstanceCount
        } else if ptys > pty_budget {
            EvictionReason::Ptys
        } else {
            break;
        };
        memory = memory.saturating_sub(candidate.memory_bytes);
        count -= 1;
        ptys = ptys.saturating_sub(candidate.ptys);
        out.push((candidate.key.clone(), reason));
    }
    out
}

fn memory_target_bytes(target: MemoryTarget, total_ram_bytes: u64) -> u64 {
    match target {
        MemoryTarget::PercentOfRam(pct) => total_ram_bytes / 100 * u64::from(pct),
        MemoryTarget::Bytes(bytes) => bytes,
    }
}

/// Whether an instance may start ahead of need: memory is within its target and one
/// more instance stays within the instance target.
pub fn room_to_warm(usages: &[Usage], targets: Targets, total_ram_bytes: u64) -> bool {
    let memory: u64 = usages.iter().map(|u| u.memory_bytes).sum();
    memory <= memory_target_bytes(targets.memory, total_ram_bytes)
        && targets
            .instances
            .is_none_or(|max| (usages.len() as u32) < max)
}

/// Whether one more instance may start under the hard ceiling for runaway loops.
pub fn under_ceiling(running: usize, hard_ceiling: u32) -> bool {
    running < hard_ceiling as usize
}

/// Every app an app depends on, directly or through other apps, excluding itself.
/// Unknown names are ignored here; config validation reports them.
pub fn dependency_closure(project: &Project, app: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![app.to_string()];
    while let Some(name) = queue.pop() {
        let Some(app) = project.app(&name) else {
            continue;
        };
        for dep in &app.settings.depends_on {
            if dep != &name && seen.insert(dep.clone()) {
                queue.push(dep.clone());
            }
        }
    }
    seen.remove(app);
    seen
}

/// Every instance an instance depends on, directly or through others. Apps of the
/// same project run in the dependent's stack; apps of other projects in their main
/// stack.
pub fn dependencies(world: &World, key: &InstanceKey) -> BTreeSet<InstanceKey> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![key.clone()];
    while let Some(current) = queue.pop() {
        let Some(app) = world
            .project(&current.project)
            .and_then(|state| state.project.app(&current.app))
        else {
            continue;
        };
        let here = app.settings.depends_on.iter().map(|dep| InstanceKey {
            project: current.project.clone(),
            app: dep.clone(),
            stack: current.stack.clone(),
        });
        let elsewhere = app
            .settings
            .depends_on_elsewhere
            .iter()
            .map(|dep| InstanceKey {
                project: dep.project.clone(),
                app: dep.app.clone(),
                stack: Stack::Main,
            });
        for dep in here.chain(elsewhere) {
            if seen.insert(dep.clone()) {
                queue.push(dep);
            }
        }
    }
    seen.remove(key);
    seen
}

/// For each running instance, the running instances that depend on it, directly or
/// transitively.
pub fn holders(world: &World, running: &[InstanceKey]) -> BTreeMap<InstanceKey, Vec<InstanceKey>> {
    let mut out: BTreeMap<InstanceKey, Vec<InstanceKey>> = BTreeMap::new();
    for dependent in running {
        for dep in dependencies(world, dependent) {
            if running.contains(&dep) {
                out.entry(dep).or_default().push(dependent.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::{App, AppSettings, Scheme, Stack};

    fn key(app: &str) -> InstanceKey {
        InstanceKey {
            project: "nova".into(),
            app: app.into(),
            stack: Stack::Branch("feature-x".into()),
        }
    }

    fn usage(app: &str, age_secs: u64, memory_mb: u64, now: Instant) -> Usage {
        Usage {
            key: key(app),
            pinned: false,
            requests_in_flight: 0,
            connections: 0,
            execs: 0,
            held_by: vec![],
            paused: 0,
            last_activity: now - Duration::from_secs(age_secs),
            memory_bytes: memory_mb * 1024 * 1024,
            ptys: 3,
        }
    }

    fn project(deps: &[(&str, &[&str])]) -> Project {
        Project {
            folder: None,
            name: "nova".into(),
            config_path: PathBuf::new(),
            strip_prefix: Some("nova-".into()),
            env_provider: None,
            env: vec![],
            requires: vec![],
            scheme: Scheme::Https,
            idle_timeout: None,
            teardown_delay: None,
            apps: deps
                .iter()
                .map(|(name, depends_on)| App {
                    name: (*name).into(),
                    short: name.trim_start_matches("nova-").into(),
                    path: PathBuf::from("/work").join(name),
                    repo: None,
                    branch_only: false,
                    settings: AppSettings {
                        depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
                        ..Default::default()
                    },
                })
                .collect(),
        }
    }

    const GB: u64 = 1024 * 1024 * 1024;

    fn capacity() -> Capacity {
        Capacity {
            total_ram_bytes: 32 * GB,
            ptys_in_use: 100,
            pty_max: 511,
        }
    }

    fn targets() -> Targets {
        Targets {
            memory: MemoryTarget::PercentOfRam(50),
            instances: None,
            pty_reserve: 64,
        }
    }

    #[test]
    fn idle_deadline_follows_last_activity() {
        let now = Instant::now();
        let u = usage("nova-billing", 60, 500, now);
        assert_eq!(
            idle_deadline(&u, Duration::from_secs(900)),
            Some(now + Duration::from_secs(840))
        );
    }

    #[test]
    fn connections_execs_dependents_and_pins_keep_an_instance_alive() {
        let now = Instant::now();
        let mut connected = usage("nova-frontend", 3600, 500, now);
        connected.connections = 2;
        let mut exec = usage("nova-auth", 3600, 500, now);
        exec.execs = 1;
        let mut held = usage("nova-billing", 3600, 500, now);
        held.held_by = vec![key("nova-frontend")];
        let mut pinned = usage("nova-api", 3600, 500, now);
        pinned.pinned = true;
        for u in [connected, exec, held, pinned] {
            assert_eq!(idle_deadline(&u, Duration::from_secs(900)), None);
        }
    }

    #[test]
    fn nothing_is_evicted_under_the_targets() {
        let now = Instant::now();
        let usages = vec![
            usage("nova-a", 10, 4000, now),
            usage("nova-b", 5, 4000, now),
        ];
        assert!(evictions(&usages, targets(), capacity()).is_empty());
    }

    #[test]
    fn evicts_least_recently_used_until_under_memory_target() {
        let now = Instant::now();
        let usages = vec![
            usage("nova-new", 1, 6000, now),
            usage("nova-old", 500, 6000, now),
            usage("nova-mid", 100, 6000, now),
        ];
        let evicted = evictions(&usages, targets(), capacity());
        assert_eq!(evicted, vec![(key("nova-old"), EvictionReason::Memory)]);
    }

    #[test]
    fn busy_instances_are_never_evicted_even_over_target() {
        let now = Instant::now();
        let mut a = usage("nova-a", 500, 12000, now);
        a.requests_in_flight = 1;
        let mut b = usage("nova-b", 400, 12000, now);
        b.held_by = vec![key("nova-a")];
        assert!(evictions(&[a, b], targets(), capacity()).is_empty());
    }

    #[test]
    fn instance_target_and_pty_reserve_trigger_eviction() {
        let now = Instant::now();
        let usages = vec![usage("nova-a", 30, 100, now), usage("nova-b", 20, 100, now)];
        let by_count = Targets {
            instances: Some(1),
            ..targets()
        };
        assert_eq!(
            evictions(&usages, by_count, capacity()),
            vec![(key("nova-a"), EvictionReason::InstanceCount)]
        );
        let low_ptys = Capacity {
            ptys_in_use: 449,
            ..capacity()
        };
        assert_eq!(
            evictions(&usages, targets(), low_ptys),
            vec![(key("nova-a"), EvictionReason::Ptys)]
        );
    }

    #[test]
    fn warming_needs_room_under_both_targets() {
        let now = Instant::now();
        let usages = vec![usage("a", 10, 4096, now), usage("b", 20, 4096, now)];
        assert!(room_to_warm(&usages, targets(), 32 * GB));
        let low_memory = Targets {
            memory: MemoryTarget::Bytes(6 * GB),
            ..targets()
        };
        assert!(!room_to_warm(&usages, low_memory, 32 * GB));
        let few = Targets {
            instances: Some(2),
            ..targets()
        };
        assert!(!room_to_warm(&usages, few, 32 * GB));
        let three = Targets {
            instances: Some(3),
            ..targets()
        };
        assert!(room_to_warm(&usages, three, 32 * GB));
    }

    #[test]
    fn ceiling() {
        assert!(under_ceiling(49, 50));
        assert!(!under_ceiling(50, 50));
    }

    #[test]
    fn dependency_closure_is_transitive_and_survives_cycles() {
        let p = project(&[
            ("nova-frontend", &["nova-billing"]),
            ("nova-billing", &["nova-api", "nova-frontend"]),
            ("nova-api", &["nova-ghost"]),
        ]);
        let closure = dependency_closure(&p, "nova-frontend");
        assert_eq!(
            closure.into_iter().collect::<Vec<_>>(),
            vec!["nova-api", "nova-billing", "nova-ghost"]
        );
    }

    fn world(projects: Vec<Project>) -> World {
        World {
            projects: projects
                .into_iter()
                .map(|project| crate::model::ProjectState {
                    project,
                    worktrees: Default::default(),
                    processes: Default::default(),
                    repo_worktrees: Default::default(),
                    scan_dirs: Default::default(),
                    checkout_apps: Default::default(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn holders_are_running_dependents_in_the_same_stack() {
        let p = project(&[
            ("nova-frontend", &["nova-billing"]),
            ("nova-billing", &["nova-api"]),
            ("nova-api", &[]),
        ]);
        let mut other_stack = key("nova-frontend");
        other_stack.stack = Stack::Main;
        let running = vec![key("nova-frontend"), key("nova-api"), other_stack];
        let held = holders(&world(vec![p]), &running);
        assert_eq!(
            held.get(&key("nova-api")),
            Some(&vec![key("nova-frontend")])
        );
        assert_eq!(held.len(), 1);
    }

    #[test]
    fn apps_of_other_projects_run_in_their_main_stack() {
        let mut nova = project(&[("nova-frontend", &["nova-billing"]), ("nova-billing", &[])]);
        nova.apps[1].settings.depends_on_elsewhere = vec![crate::model::AppRef {
            project: "auth".into(),
            app: "auth".into(),
        }];
        let mut auth = project(&[("auth", &[])]);
        auth.name = "auth".into();
        auth.apps[0].settings.depends_on_elsewhere = vec![crate::model::AppRef {
            project: "nova".into(),
            app: "nova-frontend".into(),
        }];
        let world = world(vec![nova, auth]);
        let auth_main = InstanceKey {
            project: "auth".into(),
            app: "auth".into(),
            stack: Stack::Main,
        };
        let main = |app: &str| InstanceKey {
            stack: Stack::Main,
            ..key(app)
        };
        assert_eq!(
            dependencies(&world, &key("nova-frontend")),
            BTreeSet::from([
                key("nova-billing"),
                auth_main.clone(),
                main("nova-frontend"),
                main("nova-billing")
            ]),
            "a cycle through another project comes back in main"
        );
        let held = holders(&world, &[key("nova-frontend"), auth_main.clone()]);
        assert_eq!(held.get(&auth_main), Some(&vec![key("nova-frontend")]));
    }
}
