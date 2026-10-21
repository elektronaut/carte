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
use carte_core::model::{InstanceKey, ProjectState, Stack, World};
use carte_core::prune::{self, Known, KnownProject, Listing, Orphan, OrphanKind, Plan, Skip};
use carte_core::{config, discovery, include};

use crate::paths::Paths;

/// What the supervisor uses: instances that run, and stacks being torn down, as
/// (project, stack), with the reason to leave them alone.
#[derive(Debug, Clone, Default)]
pub struct InUse {
    pub running: BTreeSet<InstanceKey>,
    pub tearing_down: BTreeMap<(String, String), String>,
}

/// The leftovers on disk now, judged against a fresh load of the configuration.
pub fn plan(paths: &Paths, in_use: InUse) -> Plan {
    let world = discovery::load_world(&paths.config_dir);
    let (known, global) = known(paths, &world, in_use);
    let roots = paths.dir_roots();
    let listing = listing(&roots, &paths.run_dir());
    let mut plan = prune::plan(&roots, &paths.run_dir(), &listing, &known);
    plan.skipped.splice(0..0, global);
    plan
}

/// What `carte prune` lists.
pub fn list(paths: &Paths, in_use: InUse) -> PruneList {
    let plan = plan(paths, in_use);
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
    in_use: InUse,
    asked: &[PathBuf],
) -> (Vec<(Orphan, u64)>, Vec<PruneKept>) {
    let mut orphans: BTreeMap<PathBuf, Orphan> = plan(paths, in_use)
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
/// `apps` and of every app with a folder in it. Only run directories whose owner
/// marker names this carte count.
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
        let run = roots.instance(&key).run;
        if owns_run_dir(&roots, &run) {
            orphans.push(orphan(OrphanKind::RunDir, Some(app), run));
        }
        orphans.push(orphan(OrphanKind::Pidfile, Some(app), paths.pidfile(&key)));
    }
    orphans
}

#[derive(Debug)]
pub enum Detached {
    /// A running instance owns it now.
    Running,
    /// Left alone, for this reason.
    Kept(String),
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
            if orphan.kind == OrphanKind::RunDir && !owns_run_dir(roots, &orphan.path) {
                let reason = "its owner marker doesn't name this carte, or a socket in it answers";
                return (orphan, Detached::Kept(reason.into()));
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
            Detached::Kept(reason) => {
                kept.push(PruneKept {
                    path: orphan.path,
                    reason,
                });
                continue;
            }
            Detached::Failed(message) => Some(message),
            Detached::Done(None) => None,
            Detached::Done(Some(trash)) => remove_trash(&trash)
                .err()
                .filter(|err| err.kind() != io::ErrorKind::NotFound)
                .map(|err| match orphan.kind {
                    OrphanKind::Trash => format!("couldn't delete it: {err}"),
                    _ => format!("moved to {} but couldn't delete it: {err}", trash.display()),
                }),
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

/// Deletes a folder prune moved leftovers into, making the directories in it
/// writable when a read-only one is in the way.
fn remove_trash(trash: &Path) -> io::Result<()> {
    match fs::remove_dir_all(trash) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            make_writable(trash);
            fs::remove_dir_all(trash)
        }
        result => result,
    }
}

/// Gives the owner full access to `dir` and every directory below it, never
/// through a symlink.
fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(meta) = fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mode = meta.permissions().mode() | 0o700;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode));
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        make_writable(&entry.path());
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

/// The main checkouts of a project's git repos.
pub fn project_repos(state: &ProjectState) -> BTreeSet<PathBuf> {
    let apps = state.project.apps.iter().filter_map(|app| app.repo.clone());
    state.repo_worktrees.keys().cloned().chain(apps).collect()
}

/// The worktrees git still lists for `repos` although their directory is missing,
/// such as on a volume that isn't mounted; why when git can't tell.
pub fn missing_worktrees(repos: &BTreeSet<PathBuf>) -> Result<BTreeSet<PathBuf>, String> {
    let mut missing = BTreeSet::new();
    for repo in repos {
        if !repo.is_dir() {
            return Err(format!("its repo {} is missing", repo.display()));
        }
        missing.extend(discovery::prunable_worktrees(repo).map_err(|err| err.to_string())?);
    }
    Ok(missing)
}

/// The project's stacks without a worktree whose record names one of `missing`.
fn recorded_in(
    roots: &dirs::DirRoots,
    state: &ProjectState,
    missing: &BTreeSet<PathBuf>,
) -> Vec<String> {
    let project = &state.project.name;
    let Ok(entries) = fs::read_dir(roots.data.join(project)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|stack| stack != "main" && !state.has_stack(stack))
        .filter(|stack| {
            crate::instance_dirs::read_stack_record(&roots.stack_dir(project, stack))
                .ok()
                .flatten()
                .is_some_and(|record| record.paths().any(|path| missing.contains(path)))
        })
        .collect()
}

/// What exists: loaded projects, and the projects to leave alone because their
/// configuration or discovery has a problem. The second value says why every project
/// is left alone, when the project files couldn't be listed.
fn known(paths: &Paths, world: &World, in_use: InUse) -> (Known, Vec<Skip>) {
    let mut known = Known {
        running: in_use.running,
        held: in_use.tearing_down,
        ..Known::default()
    };
    let roots = paths.dir_roots();
    for state in &world.projects {
        if let Ok(missing) = missing_worktrees(&project_repos(state)) {
            for stack in recorded_in(&roots, state, &missing) {
                known
                    .held
                    .entry((state.project.name.clone(), stack))
                    .or_insert_with(|| "worktree missing, git still lists it".into());
            }
        }
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
            let mut names = BTreeSet::new();
            for file in files {
                let first = names.insert(file.name.clone());
                if may_be_template(&file.path) {
                    let expanded = config::load_project_file(&file.path)
                        .ok()
                        .filter(|_| first)
                        .map(|t| include::expand_projects(t.root.as_deref(), &t.projects))
                        .filter(|(_, _, problems)| problems.is_empty());
                    match expanded {
                        Some((dirs, _, _)) => {
                            known.present.extend(
                                dirs.iter()
                                    .filter_map(|(dir, _)| dir.file_name()?.to_str())
                                    .filter(|name| !known.projects.contains_key(*name))
                                    .map(str::to_string),
                            );
                            known.present.insert(file.name);
                        }
                        None => {
                            known.complete = false;
                            global.push(Skip {
                                project: None,
                                stack: None,
                                reason: format!(
                                    "the template {} has a problem, so no project counts as gone: the projects it makes may be missing",
                                    file.path.display()
                                ),
                            });
                        }
                    }
                    continue;
                }
                if known.projects.contains_key(&file.name) {
                    continue;
                }
                known
                    .skipped
                    .entry(file.name)
                    .or_insert_with(|| "its project file didn't load".into());
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

/// Whether a project file is a template, or can't be read well enough to tell.
fn may_be_template(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .is_none_or(|doc| doc.contains_key("projects"))
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
    listing.run_owned = listing
        .run
        .iter()
        .filter(|name| owns_run_dir(roots, &roots.run.join(name)))
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

/// Whether a run directory is provably this carte's: its owner marker names this
/// carte's data root in the current format, or in the older one, from when cartes
/// shared run directories, and no socket in it answers.
fn owns_run_dir(roots: &DirRoots, run_dir: &Path) -> bool {
    let Ok(text) = fs::read_to_string(dirs::owner_marker(run_dir)) else {
        return false;
    };
    let owner = dirs::Owner::parse(&text);
    owner.is(&roots.data) && (owner.current || !serves(run_dir, 4))
}

/// Whether a Unix socket in `dir`, or in its folders up to `depth` deep, takes a
/// connection, or part of it can't be read. Symlinks aren't followed.
fn serves(dir: &Path, depth: usize) -> bool {
    use std::os::unix::fs::FileTypeExt;

    let Ok(read) = fs::read_dir(dir) else {
        return true;
    };
    for entry in read {
        let Ok(entry) = entry else {
            return true;
        };
        let Ok(kind) = entry.file_type() else {
            return true;
        };
        if kind.is_socket() {
            match std::os::unix::net::UnixStream::connect(entry.path()) {
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) => {}
                _ => return true,
            }
        } else if kind.is_dir() && (depth == 0 || serves(&entry.path(), depth - 1)) {
            return true;
        }
    }
    false
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
    fn stack_leftovers_take_only_run_directories_this_carte_owns() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let roots = paths.dir_roots();
        let run = |app: &str| {
            roots
                .instance(&InstanceKey {
                    project: "acme".into(),
                    app: app.into(),
                    stack: Stack::Branch("x".into()),
                })
                .run
        };
        for app in ["acme-web", "acme-api", "acme-docs"] {
            fs::create_dir_all(run(app)).unwrap();
        }
        let owner = roots.data.display().to_string();
        fs::write(dirs::owner_marker(&run("acme-web")), &owner).unwrap();
        fs::write(dirs::owner_marker(&run("acme-api")), "/elsewhere/data").unwrap();

        let apps = ["acme-web", "acme-api", "acme-docs"].map(String::from);
        let run_dirs: Vec<PathBuf> = stack_leftovers(&paths, "acme", "x", apps)
            .into_iter()
            .filter(|o| matches!(o.kind, OrphanKind::RunDir))
            .map(|o| o.path)
            .collect();
        assert_eq!(run_dirs, [run("acme-web")]);
    }

    fn leftovers(paths: &Paths) -> Vec<String> {
        let mut gone: Vec<String> = super::plan(paths, InUse::default())
            .orphans
            .into_iter()
            .filter_map(|o| o.project)
            .collect();
        gone.dedup();
        gone
    }

    fn runnable_repo(dir: &Path) {
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join("Procfile"), "web: x\n").unwrap();
    }

    #[test]
    fn a_template_making_a_project_of_its_own_name_is_still_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let paths = Paths::under(&root);
        fs::create_dir_all(paths.projects_dir()).unwrap();
        runnable_repo(&root.join("sites/sites"));
        let data = paths.dir_roots().data;
        for project in ["sites", "blog"] {
            fs::create_dir_all(data.join(project).join("main/web")).unwrap();
        }
        fs::write(
            paths.projects_dir().join("sites.toml"),
            format!(
                "projects = [\"{0}/sites/*\", \"{0}/ext/*\"]\n",
                root.display()
            ),
        )
        .unwrap();
        assert!(leftovers(&paths).is_empty(), "{:?}", leftovers(&paths));
    }

    #[test]
    fn a_template_skipped_as_a_duplicate_keeps_every_project() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let paths = Paths::under(&root);
        fs::create_dir_all(paths.projects_dir().join("clients")).unwrap();
        runnable_repo(&root.join("shop"));
        runnable_repo(&root.join("clients/blog"));
        let data = paths.dir_roots().data;
        for project in ["shop", "blog"] {
            fs::create_dir_all(data.join(project).join("main/web")).unwrap();
        }
        fs::write(
            paths.projects_dir().join("shop.toml"),
            format!("root = \"{}/shop\"\n", root.display()),
        )
        .unwrap();
        fs::write(
            paths.projects_dir().join("clients/shop.toml"),
            format!("projects = \"{}/clients/*\"\n", root.display()),
        )
        .unwrap();
        assert!(leftovers(&paths).is_empty(), "{:?}", leftovers(&paths));
    }

    #[test]
    fn a_matched_directory_with_nothing_to_run_isnt_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let paths = Paths::under(&root);
        fs::create_dir_all(paths.projects_dir()).unwrap();
        fs::create_dir_all(root.join("sites/wiki")).unwrap();
        fs::write(root.join("sites/wiki/Procfile"), "web: x\n").unwrap();
        fs::create_dir_all(root.join("sites/blog/.git")).unwrap();
        let data = paths.dir_roots().data;
        for project in ["wiki", "blog", "old"] {
            fs::create_dir_all(data.join(project).join("main").join(project)).unwrap();
        }
        fs::write(
            paths.projects_dir().join("sites.toml"),
            format!("projects = \"{}/sites/*\"\n", root.display()),
        )
        .unwrap();
        assert_eq!(leftovers(&paths), ["old"]);

        let roots = paths.dir_roots();
        let run = roots.run.join("abcdefgh");
        fs::create_dir_all(&run).unwrap();
        fs::write(dirs::owner_marker(&run), roots.data.display().to_string()).unwrap();
        let plan = super::plan(&paths, InUse::default());
        assert!(
            plan.orphans.iter().any(|o| o.path == run),
            "a matched directory doesn't hold up unclaimed run directories: {:?}",
            plan.skipped
        );
    }

    #[test]
    fn a_problem_of_a_project_named_like_its_template_isnt_the_templates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let paths = Paths::under(&root);
        fs::create_dir_all(paths.projects_dir()).unwrap();
        runnable_repo(&root.join("sites/sites"));
        let data = paths.dir_roots().data;
        for project in ["sites", "old"] {
            fs::create_dir_all(data.join(project).join("main/web")).unwrap();
        }
        fs::write(
            paths.projects_dir().join("sites.toml"),
            format!(
                "projects = \"{}/sites/*\"\n[apps.sites]\naliases = [\"a.b\"]\n",
                root.display()
            ),
        )
        .unwrap();
        let plan = super::plan(&paths, InUse::default());
        assert!(
            plan.skipped.iter().all(|s| s.project.is_some()),
            "{:?}",
            plan.skipped
        );
        assert_eq!(leftovers(&paths), ["old"]);
    }

    #[test]
    fn a_template_with_a_problem_keeps_every_project() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        fs::create_dir_all(paths.projects_dir()).unwrap();
        let data = paths.dir_roots().data;
        for project in ["acme", "nova"] {
            fs::create_dir_all(data.join(project).join("main/web")).unwrap();
        }
        fs::write(paths.projects_dir().join("shop.toml"), "root = 3\n").unwrap();
        let plan = super::plan(&paths, InUse::default());
        let gone: Vec<_> = plan
            .orphans
            .iter()
            .filter_map(|o| o.project.as_deref())
            .collect();
        assert_eq!(
            gone,
            ["acme", "nova"],
            "a broken project file isn't a template"
        );

        for text in [
            "projects = \"*\"\nroot = 3\n",
            "projects = [\n",
            "root = \"/x\"\nprojects = \"*\"\n",
        ] {
            fs::write(paths.projects_dir().join("sites.toml"), text).unwrap();
            let plan = super::plan(&paths, InUse::default());
            assert!(plan.orphans.is_empty(), "{text}: {:?}", plan.orphans);
            assert!(
                plan.skipped[0].reason.contains("sites.toml has a problem"),
                "{text}: {:?}",
                plan.skipped
            );
        }
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
    fn trash_with_a_read_only_directory_is_removed() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let roots = DirRoots {
            data: tmp.path().join("data"),
            run: tmp.path().join("run"),
        };
        let locked = roots.run.join(".carte-prune-abc/x/locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("file"), "12345").unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o500)).unwrap();
        symlink(&outside, roots.run.join(".carte-prune-abc/x/link")).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        fs::set_permissions(locked.parent().unwrap(), fs::Permissions::from_mode(0o500)).unwrap();
        let orphan = Orphan {
            project: None,
            stack: None,
            kind: OrphanKind::Trash,
            app: None,
            path: roots.run.join(".carte-prune-abc"),
        };

        let detached = detach(vec![orphan], &BTreeSet::new(), &roots);
        let result = finish(detached, &BTreeMap::new(), Vec::new());
        assert_eq!(result.removed.len(), 1, "{result:?}");
        assert_eq!(fs::read_dir(&roots.run).unwrap().count(), 0);
        let mode = fs::metadata(&outside).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o500);
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn old_format_run_directories_go_only_when_no_socket_answers() {
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, socket};

        let tmp = tempfile::tempdir().unwrap();
        let roots = DirRoots {
            data: tmp.path().join("data"),
            run: tmp.path().join("run"),
        };
        let pidfiles = tmp.path().join("pids");
        let owner = roots.data.display().to_string();
        let dir = |name: &str, marker: &str| {
            let run = roots.run.join(name);
            fs::create_dir_all(run.join("sub")).unwrap();
            fs::write(dirs::owner_marker(&run), marker).unwrap();
            run
        };
        let in_use = dir("aaaaaaaa", &owner);
        let _listener = std::os::unix::net::UnixListener::bind(in_use.join("sub/s")).unwrap();
        let unused = dir("bbbbbbbb", &owner);
        let bound = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .unwrap();
        bind(
            std::os::fd::AsRawFd::as_raw_fd(&bound),
            &UnixAddr::new(&unused.join("s")).unwrap(),
        )
        .unwrap();
        drop(bound);
        let current = dir("cccccccc", &dirs::owner_marker_text(&roots.data));
        let _current_listener = std::os::unix::net::UnixListener::bind(current.join("s")).unwrap();

        let listing = listing(&roots, &pidfiles);
        assert_eq!(
            listing.run_owned,
            BTreeSet::from(["bbbbbbbb", "cccccccc"].map(String::from))
        );
        let known = Known {
            complete: true,
            ..Known::default()
        };
        let plan = prune::plan(&roots, &pidfiles, &listing, &known);
        let planned: Vec<&Path> = plan.orphans.iter().map(|o| o.path.as_path()).collect();
        assert_eq!(planned, [unused.as_path(), current.as_path()]);

        let mut orphans = plan.orphans;
        orphans.push(Orphan {
            project: None,
            stack: None,
            kind: OrphanKind::RunDir,
            app: None,
            path: in_use.clone(),
        });
        let detached = detach(orphans, &BTreeSet::new(), &roots);
        let result = finish(detached, &BTreeMap::new(), Vec::new());
        assert_eq!(result.removed.len(), 2, "{result:?}");
        assert!(
            in_use.is_dir(),
            "a run directory in use is kept at detach too"
        );
        assert!(!unused.exists() && !current.exists());
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
        fs::write(
            dirs::owner_marker(&run),
            dirs::owner_marker_text(&roots.data),
        )
        .unwrap();
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
