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
    /// Running instances that depend on this one.
    pub held_by: Vec<InstanceKey>,
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
    let memory_target = match targets.memory {
        MemoryTarget::PercentOfRam(pct) => capacity.total_ram_bytes / 100 * u64::from(pct),
        MemoryTarget::Bytes(bytes) => bytes,
    };
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
            project: "make".into(),
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
            held_by: vec![],
            last_activity: now - Duration::from_secs(age_secs),
            memory_bytes: memory_mb * 1024 * 1024,
            ptys: 3,
        }
    }

    fn project(deps: &[(&str, &[&str])]) -> Project {
        Project {
            folder: None,
            name: "make".into(),
            config_path: PathBuf::new(),
            strip_prefix: Some("make-".into()),
            env_provider: None,
            env: vec![],
            requires: vec![],
            scheme: Scheme::Https,
            idle_timeout: None,
            apps: deps
                .iter()
                .map(|(name, depends_on)| App {
                    name: (*name).into(),
                    short: name.trim_start_matches("make-").into(),
                    path: PathBuf::from("/work").join(name),
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
        let u = usage("make-billing", 60, 500, now);
        assert_eq!(
            idle_deadline(&u, Duration::from_secs(900)),
            Some(now + Duration::from_secs(840))
        );
    }

    #[test]
    fn connections_dependents_and_pins_keep_an_instance_alive() {
        let now = Instant::now();
        let mut connected = usage("make-frontend", 3600, 500, now);
        connected.connections = 2;
        let mut held = usage("make-billing", 3600, 500, now);
        held.held_by = vec![key("make-frontend")];
        let mut pinned = usage("make-api", 3600, 500, now);
        pinned.pinned = true;
        for u in [connected, held, pinned] {
            assert_eq!(idle_deadline(&u, Duration::from_secs(900)), None);
        }
    }

    #[test]
    fn nothing_is_evicted_under_the_targets() {
        let now = Instant::now();
        let usages = vec![
            usage("make-a", 10, 4000, now),
            usage("make-b", 5, 4000, now),
        ];
        assert!(evictions(&usages, targets(), capacity()).is_empty());
    }

    #[test]
    fn evicts_least_recently_used_until_under_memory_target() {
        let now = Instant::now();
        let usages = vec![
            usage("make-new", 1, 6000, now),
            usage("make-old", 500, 6000, now),
            usage("make-mid", 100, 6000, now),
        ];
        let evicted = evictions(&usages, targets(), capacity());
        assert_eq!(evicted, vec![(key("make-old"), EvictionReason::Memory)]);
    }

    #[test]
    fn busy_instances_are_never_evicted_even_over_target() {
        let now = Instant::now();
        let mut a = usage("make-a", 500, 12000, now);
        a.requests_in_flight = 1;
        let mut b = usage("make-b", 400, 12000, now);
        b.held_by = vec![key("make-a")];
        assert!(evictions(&[a, b], targets(), capacity()).is_empty());
    }

    #[test]
    fn instance_target_and_pty_reserve_trigger_eviction() {
        let now = Instant::now();
        let usages = vec![usage("make-a", 30, 100, now), usage("make-b", 20, 100, now)];
        let by_count = Targets {
            instances: Some(1),
            ..targets()
        };
        assert_eq!(
            evictions(&usages, by_count, capacity()),
            vec![(key("make-a"), EvictionReason::InstanceCount)]
        );
        let low_ptys = Capacity {
            ptys_in_use: 449,
            ..capacity()
        };
        assert_eq!(
            evictions(&usages, targets(), low_ptys),
            vec![(key("make-a"), EvictionReason::Ptys)]
        );
    }

    #[test]
    fn ceiling() {
        assert!(under_ceiling(49, 50));
        assert!(!under_ceiling(50, 50));
    }

    #[test]
    fn dependency_closure_is_transitive_and_survives_cycles() {
        let p = project(&[
            ("make-frontend", &["make-billing"]),
            ("make-billing", &["make-api", "make-frontend"]),
            ("make-api", &["make-ghost"]),
        ]);
        let closure = dependency_closure(&p, "make-frontend");
        assert_eq!(
            closure.into_iter().collect::<Vec<_>>(),
            vec!["make-api", "make-billing", "make-ghost"]
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
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn holders_are_running_dependents_in_the_same_stack() {
        let p = project(&[
            ("make-frontend", &["make-billing"]),
            ("make-billing", &["make-api"]),
            ("make-api", &[]),
        ]);
        let mut other_stack = key("make-frontend");
        other_stack.stack = Stack::Main;
        let running = vec![key("make-frontend"), key("make-api"), other_stack];
        let held = holders(&world(vec![p]), &running);
        assert_eq!(
            held.get(&key("make-api")),
            Some(&vec![key("make-frontend")])
        );
        assert_eq!(held.len(), 1);
    }

    #[test]
    fn apps_of_other_projects_run_in_their_main_stack() {
        let mut make = project(&[("make-frontend", &["make-billing"]), ("make-billing", &[])]);
        make.apps[1].settings.depends_on_elsewhere = vec![crate::model::AppRef {
            project: "auth".into(),
            app: "auth".into(),
        }];
        let mut auth = project(&[("auth", &[])]);
        auth.name = "auth".into();
        auth.apps[0].settings.depends_on_elsewhere = vec![crate::model::AppRef {
            project: "make".into(),
            app: "make-frontend".into(),
        }];
        let world = world(vec![make, auth]);
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
            dependencies(&world, &key("make-frontend")),
            BTreeSet::from([
                key("make-billing"),
                auth_main.clone(),
                main("make-frontend"),
                main("make-billing")
            ]),
            "a cycle through another project comes back in main"
        );
        let held = holders(&world, &[key("make-frontend"), auth_main.clone()]);
        assert_eq!(held.get(&auth_main), Some(&vec![key("make-frontend")]));
    }
}
