//! Watches configuration, worktrees and a few files in each checkout, and tells the
//! supervisor what changed. Only specific directories are watched, never whole repos.

use std::collections::{BTreeMap, BTreeSet};
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

/// What to watch for a world: the config directory recursively, each repo's git
/// directory (new worktrees), the directories new apps appear in, and per app each
/// checkout's root (tool and process files, removal) and its `tmp` (restart.txt).
pub fn watch_set(config_dir: &Path, world: &World) -> BTreeSet<(PathBuf, bool)> {
    let mut set = BTreeSet::from([(config_dir.to_path_buf(), true)]);
    for dir in &world.scan_dirs {
        set.insert((dir.clone(), false));
    }
    for state in &world.projects {
        for (repo, worktrees) in &state.repo_worktrees {
            let git = repo.join(".git");
            set.insert((git.clone(), false));
            if git.join("worktrees").is_dir() {
                set.insert((git.join("worktrees"), false));
            }
            set.insert((repo.clone(), false));
            for path in worktrees.values() {
                set.insert((path.clone(), false));
            }
        }
        for dir in &state.scan_dirs {
            set.insert((dir.clone(), false));
        }
        for app in &state.project.apps {
            if app.path.as_os_str().is_empty() {
                continue;
            }
            let mut checkouts: Vec<PathBuf> = (!app.branch_only)
                .then(|| app.path.clone())
                .into_iter()
                .collect();
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

/// What a changed path means. File events carry real paths, so `config_dir`, the
/// checkouts' keys and the scan directories are canonical; the checkouts' values are
/// the paths the supervisor knows them by.
pub fn classify(
    path: &Path,
    config_dirs: &[PathBuf],
    checkouts: &BTreeMap<PathBuf, PathBuf>,
    scan_dirs: &BTreeSet<PathBuf>,
) -> Option<Change> {
    if config_dirs.iter().any(|dir| path.starts_with(dir)) {
        return Some(Change::Reload);
    }
    if path
        .parent()
        .is_some_and(|parent| scan_dirs.contains(parent))
    {
        return Some(Change::Reload);
    }
    if path.components().any(|c| c.as_os_str() == ".git") {
        return Some(Change::Reload);
    }
    if checkouts.contains_key(path) {
        return Some(Change::Reload);
    }
    let name = path.file_name()?.to_str()?;
    let parent = path.parent()?;
    if matches!(
        name,
        "Procfile.dev" | "Procfile" | "config.ru" | "carte.toml"
    ) && checkouts.contains_key(parent)
    {
        return Some(Change::Reload);
    }
    if name == "restart.txt" && parent.file_name()? == "tmp" {
        let checkout = checkouts.get(parent.parent()?)?;
        return Some(Change::Restart(checkout.clone()));
    }
    if TOOL_FILES.contains(&name) {
        return checkouts
            .get(parent)
            .map(|checkout| Change::EnvChanged(checkout.clone()));
    }
    None
}

/// Every checkout, keyed by its configured path and by its canonical path.
fn checkouts(world: &World) -> BTreeMap<PathBuf, PathBuf> {
    let paths = world
        .projects
        .iter()
        .flat_map(|state| {
            state
                .project
                .apps
                .iter()
                .filter(|app| !app.branch_only)
                .map(|app| app.path.clone())
                .chain(state.worktrees.values().flatten().map(|wt| wt.path.clone()))
                .chain(
                    state
                        .repo_worktrees
                        .iter()
                        .flat_map(|(repo, wts)| std::iter::once(repo).chain(wts.values()))
                        .cloned(),
                )
        })
        .filter(|p| !p.as_os_str().is_empty());
    keyed(paths)
}

/// Every directory new apps appear in, by its configured and its canonical path.
fn scan_dirs(world: &World) -> BTreeSet<PathBuf> {
    world
        .projects
        .iter()
        .flat_map(|state| state.scan_dirs.iter())
        .chain(&world.scan_dirs)
        .flat_map(|dir| [canonical(dir), dir.clone()])
        .collect()
}

fn keyed(paths: impl Iterator<Item = PathBuf>) -> BTreeMap<PathBuf, PathBuf> {
    paths
        .flat_map(|p| [(canonical(&p), p.clone()), (p.clone(), p)])
        .collect()
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Starts watching right away and returns the loop that handles changes until the
/// supervisor goes away, rescanning everything every `rescan` as a backstop for
/// missed events. `None` when file watching isn't available.
pub fn run(
    config_dir: PathBuf,
    supervisor: Handle,
    rescan: Duration,
) -> Option<impl Future<Output = ()>> {
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
            return None;
        }
    };

    let mut watched = BTreeSet::new();
    let mut world = supervisor.world();
    let config_dirs = vec![canonical(&config_dir), config_dir.clone()];
    let mut known = checkouts(&world);
    let mut scans = scan_dirs(&world);
    update_watches(&mut watcher, &mut watched, watch_set(&config_dir, &world));
    Some(async move {
        let mut interval = tokio::time::interval(rescan);
        interval.tick().await;

        loop {
            let mut changes = BTreeSet::new();
            tokio::select! {
                Some(path) = rx.recv() => {
                    changes.extend(classify(&path, &config_dirs, &known, &scans));
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    while let Ok(path) = rx.try_recv() {
                        changes.extend(classify(&path, &config_dirs, &known, &scans));
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
                let Ok(fresh) =
                    tokio::task::spawn_blocking(move || discovery::load_world(&dir)).await
                else {
                    continue;
                };
                if fresh != *world {
                    world = std::sync::Arc::new(fresh.clone());
                    supervisor.reload(fresh);
                    known = checkouts(&world);
                    scans = scan_dirs(&world);
                    update_watches(&mut watcher, &mut watched, watch_set(&config_dir, &world));
                }
            }
        }
    })
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

    fn checkouts() -> BTreeMap<PathBuf, PathBuf> {
        ["/work/acme-web", "/work/acme-web-feature-x"]
            .into_iter()
            .map(|p| (PathBuf::from(p), PathBuf::from(p)))
            .collect()
    }

    fn scans() -> BTreeSet<PathBuf> {
        BTreeSet::from([PathBuf::from("/work/mono/apps")])
    }

    #[test]
    fn a_changed_carte_toml_reloads() {
        assert_eq!(
            classify(
                Path::new("/work/acme-web-feature-x/carte.toml"),
                &[PathBuf::from("/config")],
                &checkouts(),
                &scans()
            ),
            Some(Change::Reload)
        );
    }

    #[test]
    fn new_directories_in_scan_dirs_reload() {
        assert_eq!(
            classify(
                Path::new("/work/mono/apps/new"),
                &[PathBuf::from("/config")],
                &checkouts(),
                &scans()
            ),
            Some(Change::Reload)
        );
    }

    #[test]
    fn classifies_changes() {
        let config = &[PathBuf::from("/config")];
        let c = checkouts();
        assert_eq!(
            classify(
                Path::new("/config/projects/acme.toml"),
                config,
                &c,
                &scans()
            ),
            Some(Change::Reload)
        );
        assert_eq!(
            classify(
                Path::new("/work/acme-web/.git/worktrees/x"),
                config,
                &c,
                &scans()
            ),
            Some(Change::Reload)
        );
        assert_eq!(
            classify(Path::new("/work/acme-web-feature-x"), config, &c, &scans()),
            Some(Change::Reload)
        );
        assert_eq!(
            classify(
                Path::new("/work/acme-web-feature-x/tmp/restart.txt"),
                config,
                &c,
                &scans()
            ),
            Some(Change::Restart(PathBuf::from("/work/acme-web-feature-x")))
        );
        assert_eq!(
            classify(
                Path::new("/work/acme-web/.tool-versions"),
                config,
                &c,
                &scans()
            ),
            Some(Change::EnvChanged(PathBuf::from("/work/acme-web")))
        );
        assert_eq!(
            classify(
                Path::new("/work/acme-web-feature-x/Procfile.dev"),
                config,
                &c,
                &scans()
            ),
            Some(Change::Reload)
        );
        assert_eq!(
            classify(
                Path::new("/work/acme-web/app/Procfile"),
                config,
                &c,
                &scans()
            ),
            None
        );
        assert_eq!(
            classify(
                Path::new("/work/acme-web/app/models/x.rb"),
                config,
                &c,
                &scans()
            ),
            None
        );
        assert_eq!(
            classify(Path::new("/work/acme-web/tmp/cache"), config, &c, &scans()),
            None
        );
    }

    #[test]
    fn events_on_real_paths_map_back_to_configured_ones() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(real.join("work/app/tmp")).unwrap();
        std::os::unix::fs::symlink(real.join("work"), real.join("link")).unwrap();
        let configured = real.join("link/app");
        let map = keyed([configured.clone()].into_iter());
        let config = &[real.join("config")];
        for event in [
            real.join("work/app/tmp/restart.txt"),
            configured.join("tmp/restart.txt"),
        ] {
            assert_eq!(
                classify(&event, config, &map, &scans()),
                Some(Change::Restart(configured.clone())),
                "{}",
                event.display()
            );
        }
    }
}
