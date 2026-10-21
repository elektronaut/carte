//! Watches configuration, worktrees and a few files in each checkout, and tells the
//! supervisor what changed. Only specific directories are watched, never whole repos.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use carte_core::discovery;
use carte_core::model::World;
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::supervisor::Handle;

/// Files in a checkout's root that change the env its processes get.
fn is_tool_file(name: &str) -> bool {
    carte_core::env::is_mise_config_name(name)
        || matches!(
            name,
            ".node-version" | ".nvmrc" | ".ruby-version" | ".python-version"
        )
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    Reload,
    Rewatch,
    Restart(PathBuf),
    EnvChanged(PathBuf),
}

/// What to watch for a world: the config directory recursively, each repo's git
/// directory and each worktree's folder in it (worktrees being added or removed),
/// the directories new apps appear in, and per app each checkout's root (tool and
/// process files, removal) and its `tmp` (restart.txt).
pub fn watch_set(config_dir: &Path, world: &World) -> BTreeSet<(PathBuf, bool)> {
    let mut set = BTreeSet::from([(config_dir.to_path_buf(), true)]);
    for dir in &world.scan_dirs {
        set.insert((dir.clone(), false));
    }
    for state in &world.projects {
        for (repo, worktrees) in &state.repo_worktrees {
            let git = repo.join(".git");
            set.insert((git.clone(), false));
            if let Ok(entries) = std::fs::read_dir(git.join("worktrees")) {
                set.insert((git.join("worktrees"), false));
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        set.insert((entry.path(), false));
                    }
                }
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
        "Procfile.dev" | "Procfile" | "config.ru" | "package.json" | "carte.toml"
    ) && checkouts.contains_key(parent)
    {
        return Some(Change::Reload);
    }
    if name == "tmp" && checkouts.contains_key(parent) {
        return Some(Change::Rewatch);
    }
    if name == "restart.txt" && parent.file_name()? == "tmp" {
        let checkout = checkouts.get(parent.parent()?)?;
        return Some(Change::Restart(checkout.clone()));
    }
    if is_tool_file(name) {
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

/// The last seen modification time of each checkout's `tmp/restart.txt`, `None`
/// while it doesn't exist.
#[derive(Debug, Default)]
struct RestartFiles(BTreeMap<PathBuf, Option<SystemTime>>);

impl RestartFiles {
    /// Records the files of new checkouts and of checkouts whose file was missing,
    /// and forgets checkouts that are gone.
    fn arm<'a>(&mut self, checkouts: impl Iterator<Item = &'a PathBuf>) {
        let checkouts: BTreeSet<&PathBuf> = checkouts.collect();
        self.0.retain(|path, _| checkouts.contains(path));
        for checkout in checkouts {
            let mtime = self.0.entry(checkout.clone()).or_default();
            if mtime.is_none() {
                *mtime = restart_mtime(checkout);
            }
        }
    }

    /// Whether a change to the checkout's `tmp/restart.txt` restarts it: only a
    /// newer modification time on a file that already existed does, so creating it
    /// (as Puma's `tmp_restart` plugin does on boot) doesn't.
    fn touched(&mut self, checkout: &Path) -> bool {
        let current = restart_mtime(checkout);
        let previous = self.0.insert(checkout.to_path_buf(), current).flatten();
        matches!((previous, current), (Some(previous), Some(current)) if current > previous)
    }
}

fn restart_mtime(checkout: &Path) -> Option<SystemTime> {
    std::fs::metadata(checkout.join("tmp/restart.txt"))
        .and_then(|m| m.modified())
        .ok()
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
        move |event| {
            for change in changed_paths(event) {
                let _ = tx.send(change);
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
    let mut restart_files = RestartFiles::default();
    update_watches(&mut watcher, &mut watched, watch_set(&config_dir, &world));
    restart_files.arm(known.values());
    Some(async move {
        let mut interval = tokio::time::interval(rescan);
        interval.tick().await;

        loop {
            let mut changes = BTreeSet::new();
            tokio::select! {
                Some((path, gone)) = rx.recv() => {
                    if gone {
                        forget(&mut watcher, &mut watched, &path);
                    }
                    changes.extend(classify(&path, &config_dirs, &known, &scans));
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    while let Ok((path, gone)) = rx.try_recv() {
                        if gone {
                            forget(&mut watcher, &mut watched, &path);
                        }
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
                    Change::Rewatch => {
                        update_watches(&mut watcher, &mut watched, watch_set(&config_dir, &world));
                        restart_files.arm(known.values());
                    }
                    Change::Restart(path) => {
                        if restart_files.touched(path) {
                            supervisor.restart_checkout(path.clone());
                        }
                    }
                    Change::EnvChanged(path) => supervisor.env_changed(path.clone()),
                }
            }
            // Scans again after adding watches, so a change made before a new
            // watch took effect is still seen.
            let mut reload = changes.contains(&Change::Reload);
            while reload {
                let dir = config_dir.clone();
                let Ok(fresh) =
                    tokio::task::spawn_blocking(move || discovery::load_world(&dir)).await
                else {
                    break;
                };
                let changed = fresh != *world;
                if changed {
                    world = std::sync::Arc::new(fresh.clone());
                    supervisor.reload(fresh);
                    known = checkouts(&world);
                    scans = scan_dirs(&world);
                }
                reload = update_watches(&mut watcher, &mut watched, watch_set(&config_dir, &world));
                if changed || reload {
                    restart_files.arm(known.values());
                }
            }
        }
    })
}

/// The paths an event is about, each with whether a watch on it went away: inotify
/// drops the watch of a deleted path and follows a renamed one to where it went.
fn changed_paths(event: notify::Result<notify::Event>) -> Vec<(PathBuf, bool)> {
    let Ok(event) = event else {
        return Vec::new();
    };
    let gone = match event.kind {
        EventKind::Access(_) => return Vec::new(),
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => true,
        _ => false,
    };
    event.paths.into_iter().map(|path| (path, gone)).collect()
}

/// Stops watching a path that was deleted or moved away, so the next update
/// watches whatever is at the path then.
fn forget(watcher: &mut RecommendedWatcher, watched: &mut BTreeSet<(PathBuf, bool)>, path: &Path) {
    let before = watched.len();
    watched.retain(|(watched, _)| watched != path);
    if watched.len() != before {
        let _ = watcher.unwatch(path);
    }
}

/// Returns whether it watches anything new. A path that doesn't exist, such as a
/// checkout that isn't cloned yet, or whose watch fails isn't recorded, so the next
/// update tries it again.
fn update_watches(
    watcher: &mut RecommendedWatcher,
    watched: &mut BTreeSet<(PathBuf, bool)>,
    wanted: BTreeSet<(PathBuf, bool)>,
) -> bool {
    static LIMIT_REACHED: std::sync::Once = std::sync::Once::new();
    let gone: Vec<_> = watched
        .iter()
        .filter(|entry| !wanted.contains(*entry) || !entry.0.exists())
        .cloned()
        .collect();
    for entry in gone {
        let _ = watcher.unwatch(&entry.0);
        watched.remove(&entry);
    }
    let mut added = false;
    for (path, recursive) in wanted {
        if watched.contains(&(path.clone(), recursive)) {
            continue;
        }
        if !path.exists() {
            debug!("not watching {}: it doesn't exist", path.display());
            continue;
        }
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        match watcher.watch(&path, mode) {
            Ok(()) => {
                watched.insert((path, recursive));
                added = true;
            }
            Err(err) if matches!(err.kind, notify::ErrorKind::MaxFilesWatch) => {
                LIMIT_REACHED.call_once(|| {
                    warn!(
                        "file watching is off for {} and other paths: the system's limit on file watches is reached (on Linux, raise fs.inotify.max_user_watches)",
                        path.display()
                    );
                });
            }
            Err(err) => debug!("not watching {}: {err}", path.display()),
        }
    }
    added
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
        for name in ["mise.local.toml", "mise.dev.toml", ".mise.toml"] {
            assert_eq!(
                classify(
                    &Path::new("/work/acme-web").join(name),
                    config,
                    &c,
                    &scans()
                ),
                Some(Change::EnvChanged(PathBuf::from("/work/acme-web"))),
                "{name}"
            );
        }
        assert_eq!(
            classify(
                Path::new("/work/acme-web/.node-version"),
                config,
                &c,
                &scans()
            ),
            Some(Change::EnvChanged(PathBuf::from("/work/acme-web")))
        );
        assert_eq!(
            classify(
                Path::new("/work/acme-web-feature-x/package.json"),
                config,
                &c,
                &scans()
            ),
            Some(Change::Reload)
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
        assert_eq!(
            classify(Path::new("/work/acme-web/tmp"), config, &c, &scans()),
            Some(Change::Rewatch)
        );
        assert_eq!(
            classify(Path::new("/work/acme-web/app/tmp"), config, &c, &scans()),
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

    fn watcher() -> (
        RecommendedWatcher,
        std::sync::mpsc::Receiver<(PathBuf, bool)>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = RecommendedWatcher::new(
            move |event| {
                for change in changed_paths(event) {
                    let _ = tx.send(change);
                }
            },
            notify::Config::default(),
        )
        .unwrap();
        (watcher, rx)
    }

    fn wait_for(rx: &std::sync::mpsc::Receiver<(PathBuf, bool)>, want: &Path, gone: bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok((path, was_gone)) if path == want && was_gone == gone => return,
                Ok(_) => {}
                Err(err) => panic!("no event for {}: {err}", want.display()),
            }
        }
    }

    #[test]
    fn a_directory_that_appears_later_is_watched_on_the_next_update() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().canonicalize().unwrap().join("acme-api");
        let (mut watcher, rx) = watcher();
        let mut watched = BTreeSet::new();
        let wanted = BTreeSet::from([(app.clone(), false)]);
        assert!(!update_watches(&mut watcher, &mut watched, wanted.clone()));

        std::fs::create_dir(&app).unwrap();
        assert!(update_watches(&mut watcher, &mut watched, wanted.clone()));
        assert!(!update_watches(&mut watcher, &mut watched, wanted));

        let file = app.join(".ruby-version");
        std::fs::write(&file, "3.4.1\n").unwrap();
        wait_for(&rx, &file, false);
    }

    #[test]
    fn a_checkout_deleted_and_cloned_again_is_watched_again() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().canonicalize().unwrap().join("acme-api");
        std::fs::create_dir(&app).unwrap();
        let (mut watcher, rx) = watcher();
        let mut watched = BTreeSet::new();
        let wanted = BTreeSet::from([(app.clone(), false)]);
        assert!(update_watches(&mut watcher, &mut watched, wanted.clone()));

        std::fs::remove_dir(&app).unwrap();
        wait_for(&rx, &app, true);
        forget(&mut watcher, &mut watched, &app);
        std::fs::create_dir(&app).unwrap();
        assert!(update_watches(&mut watcher, &mut watched, wanted.clone()));

        let file = app.join(".ruby-version");
        std::fs::write(&file, "3.4.1\n").unwrap();
        wait_for(&rx, &file, false);
    }

    #[test]
    fn a_checkout_deleted_between_updates_is_watched_again() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().canonicalize().unwrap().join("acme-api");
        std::fs::create_dir(&app).unwrap();
        let (mut watcher, _rx) = watcher();
        let mut watched = BTreeSet::new();
        let wanted = BTreeSet::from([(app.clone(), false)]);
        assert!(update_watches(&mut watcher, &mut watched, wanted.clone()));

        std::fs::remove_dir(&app).unwrap();
        assert!(!update_watches(&mut watcher, &mut watched, wanted.clone()));
        assert!(watched.is_empty());
        std::fs::create_dir(&app).unwrap();
        assert!(update_watches(&mut watcher, &mut watched, wanted));
    }

    fn set_mtime(path: &Path, secs: u64) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn checkout_with_tmp() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("app");
        std::fs::create_dir_all(checkout.join("tmp")).unwrap();
        let file = checkout.join("tmp/restart.txt");
        (dir, checkout, file)
    }

    #[test]
    fn creating_restart_txt_does_not_restart() {
        let (_dir, checkout, file) = checkout_with_tmp();
        let mut files = RestartFiles::default();
        files.arm([checkout.clone()].iter());
        std::fs::write(&file, "").unwrap();
        assert!(!files.touched(&checkout));
    }

    #[test]
    fn touching_an_existing_restart_txt_restarts() {
        let (_dir, checkout, file) = checkout_with_tmp();
        std::fs::write(&file, "").unwrap();
        set_mtime(&file, 1_000);
        let mut files = RestartFiles::default();
        files.arm([checkout.clone()].iter());
        set_mtime(&file, 2_000);
        assert!(files.touched(&checkout));
        assert!(!files.touched(&checkout), "same mtime");
        set_mtime(&file, 1_500);
        assert!(!files.touched(&checkout), "older mtime");
    }

    #[test]
    fn creating_restart_txt_after_deleting_it_does_not_restart() {
        let (_dir, checkout, file) = checkout_with_tmp();
        std::fs::write(&file, "").unwrap();
        set_mtime(&file, 1_000);
        let mut files = RestartFiles::default();
        files.arm([checkout.clone()].iter());
        std::fs::remove_file(&file).unwrap();
        assert!(!files.touched(&checkout));
        std::fs::write(&file, "").unwrap();
        set_mtime(&file, 2_000);
        assert!(!files.touched(&checkout));
    }

    #[test]
    fn touching_restart_txt_after_creating_it_restarts() {
        let (_dir, checkout, file) = checkout_with_tmp();
        let mut files = RestartFiles::default();
        files.arm([checkout.clone()].iter());
        std::fs::write(&file, "").unwrap();
        set_mtime(&file, 1_000);
        assert!(!files.touched(&checkout));
        set_mtime(&file, 2_000);
        assert!(files.touched(&checkout));
    }

    #[test]
    fn arming_again_keeps_known_mtimes_and_picks_up_new_files() {
        let (_dir, checkout, file) = checkout_with_tmp();
        let mut files = RestartFiles::default();
        files.arm([checkout.clone()].iter());
        std::fs::write(&file, "").unwrap();
        set_mtime(&file, 1_000);
        files.arm([checkout.clone()].iter());
        set_mtime(&file, 2_000);
        files.arm([checkout.clone()].iter());
        assert!(files.touched(&checkout));
    }
}
