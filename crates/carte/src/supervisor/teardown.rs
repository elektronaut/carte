//! Teardown: once a stack's worktrees are gone, runs each app's teardown processes
//! in its main checkout, then removes the stack's folders.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carte_api::{EventKind, EventSubject, LogLine};
use carte_core::dirs::StackRecord;
use carte_core::model::{Checkout, InstanceKey, MachineConfig, ProjectState, Stack};
use carte_core::{duration, procfile, teardown};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{info, warn};

use super::actor::{EnvInput, Internal};
use super::runner::{self, LogLabel, ProcLog};
use super::{Msg, events};
use crate::instance_dirs;
use crate::paths::Paths;
use crate::process::{SpawnSpec, Spawner};
use crate::prune;

/// A project and a stack slug.
pub(super) type StackId = (String, String);

#[derive(Default)]
pub(super) struct Teardowns {
    /// Stacks that had a worktree while this daemon ran; only those are torn down.
    pub seen: BTreeSet<StackId>,
    /// Removed stacks waiting out the delay, and since when.
    pub pending: BTreeMap<StackId, Instant>,
    /// Pending teardowns waiting until carte can tell the stack is still removed.
    pub paused: BTreeSet<StackId>,
    /// Teardowns under way.
    pub running: BTreeSet<StackId>,
    /// A check of the removed stacks is under way.
    pub checking: bool,
    /// When every seen stack was last checked.
    pub checked_all: Option<Instant>,
    /// Discovery changed since that check.
    pub stale: bool,
}

/// A stack that was seen and has no worktree now.
pub(super) struct Candidate {
    pub id: StackId,
    /// The project's stacks with a worktree; `None` when its discovery failed.
    pub stacks: Option<BTreeSet<String>>,
    /// Why the project's worktrees aren't known for sure.
    pub troubled: Option<String>,
    pub stack_dir: PathBuf,
    /// Each app's main checkout, by app name.
    pub repos: BTreeMap<String, PathBuf>,
}

pub(crate) enum Status {
    /// No record: no instance ever ran in the stack, so there's nothing to tear down.
    Unrecorded,
    Kept,
    /// Its project has a problem or its record can't be read, so carte can't tell.
    Unknown(String),
    Removed(StackRecord),
}

/// Whether each candidate counts as removed, from its record, its worktree
/// directories and its branch.
pub(super) fn check(candidates: Vec<Candidate>) -> Vec<(StackId, Status)> {
    candidates
        .into_iter()
        .map(|candidate| {
            let record = match instance_dirs::read_stack_record(&candidate.stack_dir) {
                Ok(Some(record)) => record,
                Ok(None) => return (candidate.id, Status::Unrecorded),
                Err(err) => {
                    return (
                        candidate.id,
                        Status::Unknown(format!("couldn't read its stack.toml: {err}")),
                    );
                }
            };
            if let Some(reason) = candidate.troubled {
                return (candidate.id, Status::Unknown(reason));
            }
            let removed = teardown::is_removed(
                &candidate.id.1,
                candidate.stacks.as_ref(),
                &record,
                Path::exists,
                |app, branch| {
                    candidate
                        .repos
                        .get(app)
                        .is_none_or(|repo| branch_exists(repo, branch))
                },
            );
            let status = if removed {
                Status::Removed(record)
            } else {
                Status::Kept
            };
            (candidate.id, status)
        })
        .collect()
}

/// Whether the repo has the branch; true when git can't tell.
fn branch_exists(repo: &Path, branch: &str) -> bool {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    !matches!(status, Ok(status) if status.code() == Some(1))
}

/// One stack's teardown, with what it needs from the supervisor.
pub(super) struct Job {
    pub id: StackId,
    pub state: ProjectState,
    pub machine: MachineConfig,
    /// The stack's branch, from its record.
    pub branch: String,
    pub daemon_env: Arc<BTreeMap<String, String>>,
    pub paths: Paths,
    pub ca_bundle: PathBuf,
    pub ca_cert: PathBuf,
    /// How long a teardown process may run.
    pub timeout: Duration,
    pub grace: Duration,
    pub spawner: Arc<Spawner>,
    pub log_tx: broadcast::Sender<LogLine>,
    pub log_cap: usize,
    pub tx: mpsc::UnboundedSender<Msg>,
}

impl Job {
    /// Waits for the stack's instances to stop, runs the teardown processes, and
    /// removes the stack's folders when every one succeeded.
    pub(super) async fn run(self) {
        let ok = self.run_processes().await;
        let (project, stack) = &self.id;
        let subject = events::stack(project, stack);
        if !ok {
            warn!("teardown of {project}@{stack} failed; its folders are kept");
            self.event(
                EventKind::TeardownFailed,
                "kept its folders; `carte prune` removes them".into(),
                subject,
                None,
            );
        } else {
            match self.remove_folders().await {
                Ok(false) => {
                    info!("{project}@{stack} has a worktree again; its folders are kept");
                    self.event(
                        EventKind::TeardownCancelled,
                        "it has a worktree again; kept its folders".into(),
                        subject,
                        None,
                    );
                }
                Ok(true) => {
                    info!("tore down {project}@{stack}");
                    self.event(
                        EventKind::TeardownFinished,
                        "removed its folders".into(),
                        subject,
                        None,
                    );
                }
                Err(message) => {
                    warn!("teardown of {project}@{stack}: {message}");
                    self.event(EventKind::TeardownFailed, message, subject, None);
                }
            }
        }
        let _ = self
            .tx
            .send(Msg::Internal(Internal::TeardownEnded(self.id.clone())));
    }

    fn key(&self, app: &str) -> InstanceKey {
        InstanceKey {
            project: self.id.0.clone(),
            app: app.to_string(),
            stack: Stack::Branch(self.id.1.clone()),
        }
    }

    fn event(
        &self,
        kind: EventKind,
        message: String,
        subject: EventSubject,
        exit_code: Option<i32>,
    ) {
        let _ = self.tx.send(Msg::Internal(Internal::Event {
            kind,
            message,
            subjects: vec![subject],
            exit_code,
        }));
    }

    async fn running(&self) -> Option<BTreeSet<InstanceKey>> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Msg::Running(reply)).ok()?;
        rx.await.ok()
    }

    async fn run_processes(&self) -> bool {
        let deadline = Instant::now() + self.grace * 2 + Duration::from_secs(5);
        loop {
            let Some(running) = self.running().await else {
                return false;
            };
            if !running
                .iter()
                .any(|key| key.project == self.id.0 && key.stack.slug() == self.id.1)
            {
                break;
            }
            if Instant::now() >= deadline {
                self.event(
                    EventKind::TeardownFailed,
                    "its instances didn't stop".into(),
                    events::stack(&self.id.0, &self.id.1),
                    None,
                );
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let inputs = self.env_inputs();
        let built = tokio::task::spawn_blocking(move || {
            inputs
                .into_iter()
                .map(|(app, input)| {
                    let names: Vec<String> = match &input {
                        Ok(input) => input.procs.iter().map(|(_, p, _)| p.name.clone()).collect(),
                        Err(_) => Vec::new(),
                    };
                    let specs = input.and_then(|input| input.build().0).map(|specs| {
                        specs
                            .into_iter()
                            .map(|(idx, spec)| (names[idx].clone(), spec))
                            .collect::<Vec<_>>()
                    });
                    (app, specs)
                })
                .collect::<Vec<_>>()
        })
        .await;
        let Ok(built) = built else {
            return false;
        };

        let mut ok = true;
        for (app, specs) in built {
            let key = self.key(&app);
            let specs = match specs {
                Ok(specs) => specs,
                Err(message) => {
                    warn!("teardown of {}@{}: {app}: {message}", self.id.0, self.id.1);
                    self.event(
                        EventKind::TeardownFailed,
                        message,
                        events::instance(&key),
                        None,
                    );
                    ok = false;
                    continue;
                }
            };
            for (name, spec) in specs {
                let started = Instant::now();
                let subject = events::process(&key, &name);
                match self.run_one(&key, &name, spec).await {
                    Ok(()) => self.event(
                        EventKind::TeardownFinished,
                        format!("in {:.1}s", started.elapsed().as_secs_f64()),
                        subject,
                        Some(0),
                    ),
                    Err((message, code)) => {
                        self.event(EventKind::TeardownFailed, message, subject, code);
                        ok = false;
                    }
                }
            }
        }
        ok
    }

    /// What renders each app's teardown env, for the apps that have teardown
    /// processes, or why an app's processes couldn't be read.
    fn env_inputs(&self) -> Vec<(String, Result<EnvInput, String>)> {
        let mut inputs = Vec::new();
        for app in &self.state.project.apps {
            let specs = match procfile::teardowns(&app.path, &app.settings) {
                Ok(specs) if specs.is_empty() => continue,
                Ok(specs) => specs,
                Err(err) => {
                    inputs.push((app.name.clone(), Err(err.to_string())));
                    continue;
                }
            };
            let mut procs = Vec::new();
            for (idx, spec) in specs.into_iter().enumerate() {
                match runner::free_port() {
                    Ok(port) => procs.push((idx, spec, port)),
                    Err(err) => {
                        inputs.push((
                            app.name.clone(),
                            Err(format!("Couldn't find a free port: {err}")),
                        ));
                        procs.clear();
                        break;
                    }
                }
            }
            if procs.is_empty() {
                continue;
            }
            let key = self.key(&app.name);
            inputs.push((
                app.name.clone(),
                Ok(EnvInput {
                    state: self.state.clone(),
                    app: app.clone(),
                    checkout: Checkout::Main {
                        path: app.path.clone(),
                    },
                    machine: self.machine.clone(),
                    procs,
                    processes: Vec::new(),
                    cached_provider: None,
                    daemon_env: self.daemon_env.clone(),
                    pidfile: self.paths.pidfile(&key),
                    dir_roots: self.paths.dir_roots(),
                    ca_bundle: self.ca_bundle.clone(),
                    ca_cert: self.ca_cert.clone(),
                    requires: self.state.project.requires.clone(),
                    shared: BTreeSet::new(),
                    ptys_free: false,
                    branch: Some(self.branch.clone()),
                    prepare: false,
                    key,
                }),
            ));
        }
        inputs
    }

    /// Runs one teardown process to its end, or stops it after the timeout.
    async fn run_one(
        &self,
        key: &InstanceKey,
        name: &str,
        spec: SpawnSpec,
    ) -> Result<(), (String, Option<i32>)> {
        let log = Arc::new(Mutex::new(ProcLog::new(self.log_cap)));
        let label = LogLabel {
            project: key.project.clone(),
            app: key.app.clone(),
            stack: key.stack.slug().to_string(),
            process: name.to_string(),
        };
        let (stop_tx, stop_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        tokio::spawn(runner::run(
            self.spawner.clone(),
            spec,
            log.clone(),
            label,
            self.log_tx.clone(),
            stop_rx,
            event_tx,
        ));
        let mut stop = Some(stop_tx);
        let timeout = tokio::time::sleep(self.timeout);
        tokio::pin!(timeout);
        let result = loop {
            tokio::select! {
                event = event_rx.recv() => match event {
                    Some(runner::Event::Started { .. }) => {}
                    Some(runner::Event::SpawnFailed(message)) => break Err((message, None)),
                    Some(runner::Event::Exited { code, signal }) => {
                        break match (stop.is_none(), code, signal) {
                            (true, code, _) => Err((
                                format!("did not finish within {}", duration::format(self.timeout)),
                                code,
                            )),
                            (false, Some(0), _) => Ok(()),
                            (false, Some(code), _) => Err((format!("exited with code {code}"), Some(code))),
                            (false, None, signal) => Err((
                                format!("killed by signal {}", signal.unwrap_or_default()),
                                None,
                            )),
                        };
                    }
                    None => break Err(("lost track of the process".into(), None)),
                },
                _ = &mut timeout, if stop.is_some() => {
                    if let Some(stop) = stop.take() {
                        let _ = stop.send(self.grace);
                    }
                }
            }
        };
        let tail = log.lock().unwrap().tail(20);
        let who = format!("{}@{}/{name}", key.app, key.stack.slug());
        match &result {
            Ok(()) => info!("teardown {who} finished\n{}", tail.join("\n")),
            Err((message, _)) => warn!("teardown {who} failed: {message}\n{}", tail.join("\n")),
        }
        result.map_err(|(message, code)| match tail.last() {
            Some(last) => (format!("{message}: {last}"), code),
            None => (message, code),
        })
    }

    /// Removes the stack's folder and its instances' run directories and pidfiles,
    /// through the supervisor so no instance starts in between. False when the stack
    /// has a worktree again, and nothing was removed.
    async fn remove_folders(&self) -> Result<bool, String> {
        let apps = self.state.project.apps.iter().map(|a| a.name.clone());
        let orphans = prune::stack_leftovers(&self.paths, &self.id.0, &self.id.1, apps);
        let count = orphans.len();
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::DetachStack {
                project: self.id.0.clone(),
                stack: self.id.1.clone(),
                orphans,
                reply,
            })
            .map_err(|_| "carte is shutting down".to_string())?;
        let Some(detached) = rx.await.map_err(|_| "carte is shutting down".to_string())? else {
            return Ok(false);
        };
        if detached.len() != count {
            return Err("carte is shutting down".into());
        }
        let result = tokio::task::spawn_blocking(move || {
            prune::finish(detached, &BTreeMap::new(), Vec::new())
        })
        .await
        .map_err(|err| err.to_string())?;
        let problems: Vec<String> = result
            .kept
            .iter()
            .chain(&result.errors)
            .map(|kept| format!("{}: {}", kept.path.display(), kept.reason))
            .collect();
        if problems.is_empty() {
            Ok(true)
        } else {
            Err(format!("couldn't remove {}", problems.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn candidate(stack_dir: &Path, troubled: Option<&str>) -> Candidate {
        Candidate {
            id: ("acme".into(), "feature-x".into()),
            stacks: troubled.is_none().then(BTreeSet::new),
            troubled: troubled.map(str::to_string),
            stack_dir: stack_dir.to_path_buf(),
            repos: BTreeMap::new(),
        }
    }

    fn record(stack_dir: &Path, worktree: &Path) {
        instance_dirs::write_stack_record(
            stack_dir,
            &StackRecord {
                branch: Some("feature-x".into()),
                worktrees: BTreeMap::from([("web".to_string(), worktree.to_path_buf())]),
            },
        )
        .unwrap();
    }

    #[test]
    fn a_troubled_project_or_unreadable_record_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let stack_dir = tmp.path().join("stack");
        let gone = tmp.path().join("gone");
        record(&stack_dir, &gone);

        let results = check(vec![
            candidate(&stack_dir, None),
            candidate(&stack_dir, Some("git worktree list failed")),
        ]);
        assert!(matches!(results[0].1, Status::Removed(_)));
        assert!(
            matches!(&results[1].1, Status::Unknown(reason) if reason == "git worktree list failed")
        );

        fs::write(stack_dir.join("stack.toml"), "not = [toml").unwrap();
        let results = check(vec![candidate(&stack_dir, None)]);
        assert!(
            matches!(&results[0].1, Status::Unknown(reason) if reason.starts_with("couldn't read its stack.toml")),
        );
    }

    #[test]
    fn a_worktree_that_still_exists_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let stack_dir = tmp.path().join("stack");
        record(&stack_dir, tmp.path());
        let results = check(vec![candidate(&stack_dir, None)]);
        assert!(matches!(results[0].1, Status::Kept));
    }
}
