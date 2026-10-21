//! Finds and removes leftover instance directories and pidfiles for `carte prune`.
//! Only files: nothing here runs a command.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use carte_api::{
    PruneCandidate, PruneKept, PruneKind, PruneList, PruneResult, PruneSkip, PruneStack,
};
use carte_core::dirs::{self, DirRoots};
use carte_core::discovery;
use carte_core::model::{InstanceKey, Stack, World};
use carte_core::prune::{self, Known, KnownProject, Listing, Orphan, OrphanKind, Plan, Skip};

use crate::paths::Paths;

/// The leftovers on disk now, judged against a fresh load of the configuration.
pub fn plan(paths: &Paths, running: BTreeSet<InstanceKey>) -> Plan {
    let world = discovery::load_world(&paths.config_dir);
    let (known, global) = known(paths, &world, running);
    let roots = paths.dir_roots();
    let listing = listing(&roots, &paths.run_dir());
    let mut plan = prune::plan(&roots, &paths.run_dir(), &listing, &known);
    plan.skipped.splice(0..0, global);
    plan
}

/// What `carte prune` lists.
pub fn list(paths: &Paths, running: BTreeSet<InstanceKey>) -> PruneList {
    let plan = plan(paths, running);
    let candidates: Vec<PruneCandidate> = plan
        .orphans
        .iter()
        .map(|orphan| candidate(orphan, size(&orphan.path)))
        .collect();
    PruneList {
        total_bytes: candidates.iter().map(|c| c.bytes).sum(),
        candidates,
        stacks: plan
            .stacks
            .into_iter()
            .map(|(project, stack)| PruneStack { project, stack })
            .collect(),
        skipped: plan
            .skipped
            .into_iter()
            .map(|skip| PruneSkip {
                project: skip.project,
                stack: skip.stack,
                reason: skip.reason,
            })
            .collect(),
    }
}

/// The asked-for paths that are still leftovers, with their sizes, and the rest.
pub fn select(
    paths: &Paths,
    running: BTreeSet<InstanceKey>,
    asked: &[PathBuf],
) -> (Vec<(Orphan, u64)>, Vec<PruneKept>) {
    let mut orphans: BTreeMap<PathBuf, Orphan> = plan(paths, running)
        .orphans
        .into_iter()
        .map(|orphan| (orphan.path.clone(), orphan))
        .collect();
    let mut selected = Vec::new();
    let mut kept = Vec::new();
    for path in asked {
        match orphans.remove(path) {
            Some(orphan) => {
                let bytes = size(&orphan.path);
                selected.push((orphan, bytes));
            }
            None => kept.push(PruneKept {
                path: path.clone(),
                reason: "no longer a leftover".into(),
            }),
        }
    }
    (selected, kept)
}

/// A stack's folder, and the run directories and pidfiles of its instances: those of
/// `apps` and of every app with a folder in it.
pub fn stack_leftovers(
    paths: &Paths,
    project: &str,
    stack: &str,
    apps: impl IntoIterator<Item = String>,
) -> Vec<Orphan> {
    let roots = paths.dir_roots();
    let stack_dir = roots.stack_dir(project, stack);
    let mut apps: BTreeSet<String> = apps.into_iter().collect();
    apps.extend(dirs_in(&stack_dir));
    let orphan = |kind, app: Option<&String>, path| Orphan {
        project: Some(project.to_string()),
        stack: Some(stack.to_string()),
        kind,
        app: app.cloned(),
        path,
    };
    let mut orphans = vec![orphan(OrphanKind::Stack, None, stack_dir.clone())];
    for app in &apps {
        let key = InstanceKey {
            project: project.to_string(),
            app: app.clone(),
            stack: Stack::Branch(stack.to_string()),
        };
        orphans.push(orphan(
            OrphanKind::RunDir,
            Some(app),
            roots.instance(&key).run,
        ));
        orphans.push(orphan(OrphanKind::Pidfile, Some(app), paths.pidfile(&key)));
    }
    orphans
}

#[derive(Debug)]
pub enum Detached {
    /// A running instance owns it now.
    Running,
    /// Out of the way; a directory waits in this folder to be deleted.
    Done(Option<PathBuf>),
    Failed(String),
}

/// Takes leftovers out of the way, unless a running instance owns them: removes
/// pidfiles, and moves directories into a fresh folder beside them, so an instance
/// that starts afterwards never sees half a directory. Fast enough for the supervisor
/// to call between messages.
pub fn detach(
    orphans: Vec<Orphan>,
    running: &BTreeSet<InstanceKey>,
    roots: &DirRoots,
) -> Vec<(Orphan, Detached)> {
    orphans
        .into_iter()
        .map(|orphan| {
            if orphan.is_running(running, roots) {
                return (orphan, Detached::Running);
            }
            let detached = match detach_one(&orphan) {
                Ok(trash) => Detached::Done(trash),
                Err(err) => Detached::Failed(err.to_string()),
            };
            (orphan, detached)
        })
        .collect()
}

fn detach_one(orphan: &Orphan) -> io::Result<Option<PathBuf>> {
    let detached = detach_path(orphan)?;
    if orphan.kind == OrphanKind::RunDir {
        let _ = fs::remove_file(dirs::owner_marker(&orphan.path));
    }
    Ok(detached)
}

fn detach_path(orphan: &Orphan) -> io::Result<Option<PathBuf>> {
    let meta = match fs::symlink_metadata(&orphan.path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let parent = orphan.path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    if orphan.kind == OrphanKind::Pidfile {
        if !meta.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        fs::remove_file(&orphan.path)?;
        return Ok(None);
    }
    if !meta.is_dir() {
        return Err(io::Error::other("not a directory"));
    }
    if orphan.kind == OrphanKind::Trash {
        return Ok(Some(orphan.path.clone()));
    }
    let name = orphan.path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let trash = tempfile::Builder::new()
        .prefix(prune::TRASH_PREFIX)
        .tempdir_in(parent)?
        .keep();
    if let Err(err) = fs::rename(&orphan.path, trash.join(name)) {
        let _ = fs::remove_dir(&trash);
        return Err(err);
    }
    Ok(Some(trash))
}

/// Deletes what `detach` moved aside and reports the outcome.
pub fn finish(
    detached: Vec<(Orphan, Detached)>,
    sizes: &BTreeMap<PathBuf, u64>,
    mut kept: Vec<PruneKept>,
) -> PruneResult {
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    for (orphan, outcome) in detached {
        let failure = match outcome {
            Detached::Running => {
                kept.push(PruneKept {
                    path: orphan.path,
                    reason: "running".into(),
                });
                continue;
            }
            Detached::Failed(message) => Some(message),
            Detached::Done(None) => None,
            Detached::Done(Some(trash)) => fs::remove_dir_all(&trash)
                .err()
                .filter(|err| err.kind() != io::ErrorKind::NotFound)
                .map(|err| format!("moved to {} but couldn't delete it: {err}", trash.display())),
        };
        match failure {
            Some(reason) => errors.push(PruneKept {
                path: orphan.path,
                reason,
            }),
            None => {
                let bytes = sizes.get(&orphan.path).copied().unwrap_or(0);
                removed.push(candidate(&orphan, bytes));
            }
        }
    }
    PruneResult {
        removed_bytes: removed.iter().map(|c| c.bytes).sum(),
        removed,
        kept,
        errors,
    }
}

fn candidate(orphan: &Orphan, bytes: u64) -> PruneCandidate {
    PruneCandidate {
        kind: match orphan.kind {
            OrphanKind::Project => PruneKind::Project,
            OrphanKind::Stack => PruneKind::Stack,
            OrphanKind::App => PruneKind::App,
            OrphanKind::RunDir => PruneKind::RunDir,
            OrphanKind::Pidfile => PruneKind::Pidfile,
            OrphanKind::Trash => PruneKind::Trash,
        },
        project: orphan.project.clone(),
        stack: orphan.stack.clone(),
        app: orphan.app.clone(),
        path: orphan.path.clone(),
        bytes,
    }
}

/// What exists: loaded projects, and the projects to leave alone because their
/// configuration or discovery has a problem. The second value says why every project
/// is left alone, when the project files couldn't be listed.
fn known(paths: &Paths, world: &World, running: BTreeSet<InstanceKey>) -> (Known, Vec<Skip>) {
    let mut known = Known {
        running,
        ..Known::default()
    };
    for state in &world.projects {
        known.projects.insert(
            state.project.name.clone(),
            KnownProject {
                apps: state.project.apps.iter().map(|a| a.name.clone()).collect(),
                stacks: state.stacks(),
            },
        );
    }
    known.skipped = troubled(world);
    let mut global = Vec::new();
    match discovery::project_files(&paths.projects_dir()) {
        Ok(files) => {
            known.complete = true;
            for file in files {
                if !known.projects.contains_key(&file.name) {
                    known
                        .skipped
                        .entry(file.name)
                        .or_insert_with(|| "its project file didn't load".into());
                }
            }
        }
        Err(err) => global.push(Skip {
            project: None,
            stack: None,
            reason: format!(
                "couldn't list {}, so no project counts as gone: {err}",
                paths.projects_dir().display()
            ),
        }),
    }
    (known, global)
}

/// Projects whose configuration or discovery has a problem, with the first one: their
/// worktrees aren't known for sure, so nothing of theirs counts as gone. A problem
/// with a single stack, such as a worktree named main, leaves the others known.
pub fn troubled(world: &World) -> BTreeMap<String, String> {
    let mut troubled = BTreeMap::new();
    for problem in &world.problems {
        let Some(project) = &problem.project else {
            continue;
        };
        if problem.stack.is_some() {
            continue;
        }
        let reason = match &problem.app {
            Some(app) => format!("{app}: {}", problem.message),
            None => problem.message.clone(),
        };
        troubled.entry(project.clone()).or_insert(reason);
    }
    troubled
}

/// The directories in the data and run roots and the pidfiles, never through a symlink.
fn listing(roots: &DirRoots, pidfiles: &Path) -> Listing {
    let mut listing = Listing::default();
    for project in dirs_in(&roots.data) {
        let stacks = dirs_in(&roots.data.join(&project))
            .into_iter()
            .map(|stack| {
                let apps = dirs_in(&roots.data.join(&project).join(&stack));
                (stack, apps)
            })
            .collect();
        listing.data.insert(project, stacks);
    }
    listing.run = dirs_in(&roots.run);
    let owner = roots.data.display().to_string();
    listing.run_owned = listing
        .run
        .iter()
        .filter(|name| {
            fs::read_to_string(dirs::owner_marker(&roots.run.join(name)))
                .is_ok_and(|text| text.trim_end() == owner)
        })
        .cloned()
        .collect();
    for project in dirs_in(pidfiles) {
        let files = entries(&pidfiles.join(&project), fs::FileType::is_file);
        if !files.is_empty() {
            listing.pidfiles.insert(project, files);
        }
    }
    listing
}

fn dirs_in(dir: &Path) -> BTreeSet<String> {
    entries(dir, fs::FileType::is_dir)
}

/// Names of the entries of `dir` of a type, read without following symlinks; empty
/// when it can't be read.
fn entries(dir: &Path, wanted: fn(&fs::FileType) -> bool) -> BTreeSet<String> {
    let Ok(read) = fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    read.filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| wanted(&t)))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

/// Bytes in the files under `path`, not following symlinks.
fn size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path)
        .map(|read| {
            read.filter_map(Result::ok)
                .map(|entry| size(&entry.path()))
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn problems_with_one_stack_leave_the_project_untroubled() {
        let problem = |project: &str, app: Option<&str>, stack: Option<&str>, message: &str| {
            carte_core::model::Problem {
                project: Some(project.into()),
                app: app.map(str::to_string),
                stack: stack.map(str::to_string),
                message: message.into(),
            }
        };
        let world = World {
            problems: vec![
                problem("acme", Some("acme-web"), Some("main"), "named main"),
                problem("acme", Some("acme-web"), Some("x"), "same name"),
                problem("nova", Some("nova-api"), Some("x"), "named like an app"),
                problem("nova", Some("nova-api"), None, "git worktree list failed"),
                problem("wiki", None, None, "include entry matches nothing"),
            ],
            ..World::default()
        };
        assert_eq!(
            troubled(&world),
            BTreeMap::from([
                (
                    "nova".to_string(),
                    "nova-api: git worktree list failed".to_string()
                ),
                (
                    "wiki".to_string(),
                    "include entry matches nothing".to_string()
                ),
            ])
        );
    }

    #[test]
    fn lists_directories_without_following_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = DirRoots {
            data: tmp.path().join("data"),
            run: tmp.path().join("run"),
        };
        let outside = tmp.path().join("outside");
        fs::create_dir_all(outside.join("app")).unwrap();
        fs::write(outside.join("app/big"), vec![0; 4096]).unwrap();
        fs::create_dir_all(roots.data.join("nova/x/web")).unwrap();
        fs::write(roots.data.join("nova/x/stack.toml"), "").unwrap();
        fs::write(roots.data.join("nova/x/web/file"), "12345").unwrap();
        symlink(&outside, roots.data.join("nova/x/linked")).unwrap();
        symlink(&outside, roots.data.join("elsewhere")).unwrap();
        fs::create_dir_all(roots.run.join("abcdefgh")).unwrap();
        fs::create_dir_all(roots.run.join("ijklmnop")).unwrap();
        fs::create_dir_all(roots.run.join("qrstuvwx")).unwrap();
        let owner = roots.data.display().to_string();
        fs::write(dirs::owner_marker(&roots.run.join("abcdefgh")), &owner).unwrap();
        fs::write(
            dirs::owner_marker(&roots.run.join("ijklmnop")),
            "/elsewhere",
        )
        .unwrap();
        let pidfiles = tmp.path().join("pids");
        fs::create_dir_all(pidfiles.join("nova")).unwrap();
        fs::write(pidfiles.join("nova/web@x.pid"), "1").unwrap();

        let listing = listing(&roots, &pidfiles);
        assert_eq!(
            listing.data,
            BTreeMap::from([(
                "nova".to_string(),
                BTreeMap::from([("x".to_string(), BTreeSet::from(["web".to_string()]))])
            )])
        );
        assert_eq!(
            listing.run,
            BTreeSet::from(["abcdefgh", "ijklmnop", "qrstuvwx"].map(String::from))
        );
        assert_eq!(listing.run_owned, BTreeSet::from(["abcdefgh".to_string()]));
        assert_eq!(
            listing.pidfiles["nova"],
            BTreeSet::from(["web@x.pid".into()])
        );
        let link_size = fs::symlink_metadata(roots.data.join("nova/x/linked"))
            .unwrap()
            .len();
        assert_eq!(size(&roots.data.join("nova/x")), 5 + link_size);
    }

    #[test]
    fn detaching_moves_directories_aside_and_removes_pidfiles() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = DirRoots {
            data: tmp.path().join("data"),
            run: tmp.path().join("run"),
        };
        let stack = roots.data.join("nova/x");
        fs::create_dir_all(stack.join("web")).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), "").unwrap();
        symlink(&outside, stack.join("web/link")).unwrap();
        let pidfile = tmp.path().join("pids/nova/web@x.pid");
        fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
        fs::write(&pidfile, "1").unwrap();
        let orphan = |kind, path: &Path| Orphan {
            project: Some("nova".into()),
            stack: Some("x".into()),
            kind,
            app: None,
            path: path.to_path_buf(),
        };
        let running_key = InstanceKey {
            project: "nova".into(),
            app: "web".into(),
            stack: carte_core::model::Stack::Branch("x".into()),
        };

        let running = detach(
            vec![orphan(OrphanKind::Stack, &stack)],
            &BTreeSet::from([running_key]),
            &roots,
        );
        assert!(matches!(running[0].1, Detached::Running));
        assert!(stack.is_dir());

        let detached = detach(
            vec![
                orphan(OrphanKind::Stack, &stack),
                orphan(OrphanKind::Pidfile, &pidfile),
            ],
            &BTreeSet::new(),
            &roots,
        );
        assert!(!stack.exists());
        assert!(!pidfile.exists());
        assert!(pidfile.parent().unwrap().is_dir());
        let result = finish(detached, &BTreeMap::new(), Vec::new());
        assert_eq!(result.removed.len(), 2, "{result:?}");
        assert!(result.errors.is_empty());
        assert_eq!(fs::read_dir(roots.data.join("nova")).unwrap().count(), 0);
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn trash_an_earlier_prune_left_is_found_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = DirRoots {
            data: tmp.path().join("data"),
            run: tmp.path().join("run"),
        };
        fs::create_dir_all(roots.run.join(".carte-prune-abc/x")).unwrap();
        fs::write(roots.run.join(".carte-prune-abc/x/file"), "12345").unwrap();
        let pidfiles = tmp.path().join("pids");
        let known = Known {
            complete: true,
            ..Known::default()
        };
        let plan = prune::plan(&roots, &pidfiles, &listing(&roots, &pidfiles), &known);
        assert_eq!(plan.orphans.len(), 1, "{plan:?}");
        assert_eq!(plan.orphans[0].kind, OrphanKind::Trash);
        assert_eq!(plan.orphans[0].path, roots.run.join(".carte-prune-abc"));

        let detached = detach(plan.orphans, &BTreeSet::new(), &roots);
        let result = finish(detached, &BTreeMap::new(), Vec::new());
        assert_eq!(result.removed.len(), 1, "{result:?}");
        assert_eq!(result.removed[0].kind, PruneKind::Trash);
        assert_eq!(fs::read_dir(&roots.run).unwrap().count(), 0);
    }

    #[test]
    fn detaching_a_run_directory_removes_its_owner_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = DirRoots {
            data: tmp.path().join("data"),
            run: tmp.path().join("run"),
        };
        let run = roots.run.join("abcdefgh");
        fs::create_dir_all(&run).unwrap();
        fs::write(dirs::owner_marker(&run), "x").unwrap();
        let orphan = Orphan {
            project: None,
            stack: None,
            kind: OrphanKind::RunDir,
            app: None,
            path: run.clone(),
        };
        let detached = detach(vec![orphan], &BTreeSet::new(), &roots);
        let result = finish(detached, &BTreeMap::new(), Vec::new());
        assert_eq!(result.removed.len(), 1, "{result:?}");
        assert_eq!(fs::read_dir(&roots.run).unwrap().count(), 0);
    }
}
