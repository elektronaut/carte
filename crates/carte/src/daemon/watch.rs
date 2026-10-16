//! Watches configuration, worktrees and a few files in each checkout, and tells the
//! supervisor what changed. Only specific directories are watched, never whole repos.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use carte_core::discovery;
use carte_core::model::World;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::supervisor::Handle;

const TOOL_FILES: [&str; 3] = ["mise.toml", ".mise.toml", ".tool-versions"];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    Reload,
    Restart(PathBuf),
    EnvChanged(PathBuf),
}

/// What to watch for a world: the config directory recursively, and per app the
/// git directory (new worktrees), each checkout's root (tool files, removal) and
/// its `tmp` (restart.txt).
pub fn watch_set(config_dir: &Path, world: &World) -> BTreeSet<(PathBuf, bool)> {
    let mut set = BTreeSet::from([(config_dir.to_path_buf(), true)]);
    for state in &world.projects {
        for app in &state.project.apps {
            if app.path.as_os_str().is_empty() {
                continue;
            }
            let git = app.path.join(".git");
            set.insert((git.clone(), false));
            if git.join("worktrees").is_dir() {
                set.insert((git.join("worktrees"), false));
            }
            let mut checkouts = vec![app.path.clone()];
            checkouts.extend(
                state
                    .worktrees
                    .get(&app.name)
                    .into_iter()
                    .flatten()
                    .map(|wt| wt.path.clone()),
            );
            for checkout in checkouts {
                if checkout.join("tmp").is_dir() {
                    set.insert((checkout.join("tmp"), false));
                }
                set.insert((checkout, false));
            }
        }
    }
    set
}

/// What a changed path means, given the checkouts in the world.
pub fn classify(path: &Path, config_dir: &Path, checkouts: &BTreeSet<PathBuf>) -> Option<Change> {
    if path.starts_with(config_dir) {
        return Some(Change::Reload);
    }
    if path.components().any(|c| c.as_os_str() == ".git") {
        return Some(Change::Reload);
    }
    if checkouts.contains(path) {
        return Some(Change::Reload);
    }
    let name = path.file_name()?.to_str()?;
    let parent = path.parent()?;
    if name == "restart.txt" && parent.file_name()? == "tmp" {
        let checkout = parent.parent()?;
        return checkouts
            .contains(checkout)
            .then(|| Change::Restart(checkout.to_path_buf()));
    }
    if TOOL_FILES.contains(&name) && checkouts.contains(parent) {
        return Some(Change::EnvChanged(parent.to_path_buf()));
    }
    None
}

fn checkouts(world: &World) -> BTreeSet<PathBuf> {
    world
        .projects
        .iter()
        .flat_map(|state| {
            state
                .project
                .apps
                .iter()
                .map(|app| app.path.clone())
                .chain(state.worktrees.values().flatten().map(|wt| wt.path.clone()))
        })
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// Runs until the supervisor goes away. Rescans everything every `rescan` as a
/// backstop for missed events.
pub async fn run(config_dir: PathBuf, supervisor: Handle, rescan: Duration) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher = match RecommendedWatcher::new(
        move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event
                && !matches!(event.kind, EventKind::Access(_))
            {
                for path in event.paths {
                    let _ = tx.send(path);
                }
            }
        },
        notify::Config::default(),
    ) {
        Ok(watcher) => watcher,
        Err(err) => {
            warn!("file watching is off: {err}");
            return;
        }
    };

    let mut watched = BTreeSet::new();
    let mut world = supervisor.world();
    let mut known = checkouts(&world);
    update_watches(&mut watcher, &mut watched, watch_set(&config_dir, &world));
    let mut interval = tokio::time::interval(rescan);
    interval.tick().await;

    loop {
        let mut changes = BTreeSet::new();
        tokio::select! {
            Some(path) = rx.recv() => {
                changes.extend(classify(&path, &config_dir, &known));
                tokio::time::sleep(Duration::from_millis(300)).await;
                while let Ok(path) = rx.try_recv() {
                    changes.extend(classify(&path, &config_dir, &known));
                }
            }
            _ = interval.tick() => {
                changes.insert(Change::Reload);
            }
        }
        for change in &changes {
            debug!(?change, "file change");
            match change {
                Change::Reload => {}
                Change::Restart(path) => supervisor.restart_checkout(path.clone()),
                Change::EnvChanged(path) => supervisor.env_changed(path.clone()),
            }
        }
        if changes.contains(&Change::Reload) {
            let dir = config_dir.clone();
            let Ok(fresh) = tokio::task::spawn_blocking(move || discovery::load_world(&dir)).await
            else {
                continue;
            };
            if fresh != *world {
                world = std::sync::Arc::new(fresh.clone());
                supervisor.reload(fresh);
                known = checkouts(&world);
                update_watches(&mut watcher, &mut watched, watch_set(&config_dir, &world));
            }
        }
    }
}

fn update_watches(
    watcher: &mut RecommendedWatcher,
    watched: &mut BTreeSet<(PathBuf, bool)>,
    wanted: BTreeSet<(PathBuf, bool)>,
) {
    for (path, _) in watched.difference(&wanted) {
        let _ = watcher.unwatch(path);
    }
    for (path, recursive) in wanted.difference(watched) {
        let mode = if *recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        if let Err(err) = watcher.watch(path, mode) {
            debug!("not watching {}: {err}", path.display());
        }
    }
    *watched = wanted;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkouts() -> BTreeSet<PathBuf> {
        BTreeSet::from([
            PathBuf::from("/work/acme-web"),
            PathBuf::from("/work/acme-web-feature-x"),
        ])
    }

    #[test]
    fn classifies_changes() {
        let config = Path::new("/config");
        let c = checkouts();
        assert_eq!(
            classify(Path::new("/config/projects/acme.toml"), config, &c),
            Some(Change::Reload)
        );
        assert_eq!(
            classify(Path::new("/work/acme-web/.git/worktrees/x"), config, &c),
            Some(Change::Reload)
        );
        assert_eq!(
            classify(Path::new("/work/acme-web-feature-x"), config, &c),
            Some(Change::Reload)
        );
        assert_eq!(
            classify(
                Path::new("/work/acme-web-feature-x/tmp/restart.txt"),
                config,
                &c
            ),
            Some(Change::Restart(PathBuf::from("/work/acme-web-feature-x")))
        );
        assert_eq!(
            classify(Path::new("/work/acme-web/.tool-versions"), config, &c),
            Some(Change::EnvChanged(PathBuf::from("/work/acme-web")))
        );
        assert_eq!(
            classify(Path::new("/work/acme-web/app/models/x.rb"), config, &c),
            None
        );
        assert_eq!(
            classify(Path::new("/work/acme-web/tmp/cache"), config, &c),
            None
        );
    }
}
