//! What `carte prune` removes: state folders, run directories and pidfiles that
//! belong to no project, app or stack that exists, and to no running instance.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::dirs::DirRoots;
use crate::model::{InstanceKey, Stack};

/// The name prefix of the folders prune moves leftovers into before deleting them.
pub const TRASH_PREFIX: &str = ".carte-prune-";

fn is_trash(name: &str) -> bool {
    name.starts_with(TRASH_PREFIX)
}

/// The stacks of a project folder and the apps in each, without trash.
fn stacks_in(
    stacks: &BTreeMap<String, BTreeSet<String>>,
) -> impl Iterator<Item = (&String, impl Iterator<Item = &String>)> {
    stacks
        .iter()
        .filter(|(stack, _)| !is_trash(stack))
        .map(|(stack, apps)| (stack, apps.iter().filter(|app| !is_trash(app))))
}

/// What exists now, to tell leftovers from directories in use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Known {
    /// Every loaded project, by name.
    pub projects: BTreeMap<String, KnownProject>,
    /// Projects whose directories are left alone, with the reason.
    pub skipped: BTreeMap<String, String>,
    /// Every project file was listed, so a project that isn't loaded or skipped is gone.
    pub complete: bool,
    /// Instances that are starting, running or stopping.
    pub running: BTreeSet<InstanceKey>,
    /// Stacks whose teardown is pending or running, as (project, stack), with the
    /// reason to leave them alone.
    pub tearing_down: BTreeMap<(String, String), String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownProject {
    /// Full app names.
    pub apps: BTreeSet<String>,
    /// Stack names; `main` is implied.
    pub stacks: BTreeSet<String>,
}

impl KnownProject {
    fn has_stack(&self, stack: &str) -> bool {
        stack == "main" || self.stacks.contains(stack)
    }
}

/// What the roots hold: directories only, symlinks left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// Project folder, stack folder, app folders in the data root.
    pub data: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    /// Directory names in the run root.
    pub run: BTreeSet<String>,
    /// Those whose owner marker names this carte's data root.
    pub run_owned: BTreeSet<String>,
    /// Pidfile names in each project's folder of the pidfile directory.
    pub pidfiles: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OrphanKind {
    /// A project folder whose project file is gone.
    Project,
    /// A stack folder of a stack with no worktree.
    Stack,
    /// An app folder of an app no longer in its project.
    App,
    RunDir,
    Pidfile,
    /// A folder an earlier prune moved leftovers into but couldn't delete.
    Trash,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Orphan {
    /// `None` for a run directory nothing on disk names.
    pub project: Option<String>,
    pub stack: Option<String>,
    pub kind: OrphanKind,
    pub app: Option<String>,
    pub path: PathBuf,
}

impl Orphan {
    /// Whether it belongs to a running instance, or holds one's directories.
    pub fn is_running(&self, running: &BTreeSet<InstanceKey>, roots: &DirRoots) -> bool {
        running.iter().any(|key| match self.kind {
            OrphanKind::RunDir => roots.instance(key).run == self.path,
            _ => {
                self.project.as_deref() == Some(key.project.as_str())
                    && self
                        .stack
                        .as_deref()
                        .is_none_or(|stack| stack == key.stack.slug())
                    && self
                        .app
                        .as_deref()
                        .is_none_or(|app| app == key.app.as_str())
            }
        })
    }
}

/// Something prune leaves alone, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub project: Option<String>,
    pub stack: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Sorted by project, stack and kind; unattributed run directories last.
    pub orphans: Vec<Orphan>,
    /// Stack folders among the orphans, as (project, stack): their teardown never
    /// ran or failed, so their external resources may remain.
    pub stacks: Vec<(String, String)>,
    pub skipped: Vec<Skip>,
}

/// `app@stack.pid`, as `(app, stack)`.
fn parse_pidfile(name: &str) -> Option<(&str, &str)> {
    let (app, stack) = name.strip_suffix(".pid")?.rsplit_once('@')?;
    (!app.is_empty() && !stack.is_empty()).then_some((app, stack))
}

fn key(project: &str, stack: &str, app: &str) -> InstanceKey {
    InstanceKey {
        project: project.into(),
        app: app.into(),
        stack: if stack == "main" {
            Stack::Main
        } else {
            Stack::Branch(stack.into())
        },
    }
}

/// The leftovers in `listing`: in a project that is gone, a stack that has no
/// worktree, or an app that isn't in its project. Skipped projects and running
/// instances are never touched. A run directory goes only when its owner marker
/// names this carte, and one no folder or instance names only when nothing was
/// skipped. Folders an earlier prune couldn't delete always go.
pub fn plan(roots: &DirRoots, pidfiles: &Path, listing: &Listing, known: &Known) -> Plan {
    let mut plan = Plan::default();
    let gone = |project: &str| {
        known.complete
            && !known.projects.contains_key(project)
            && !known.skipped.contains_key(project)
    };
    let mut candidates = trash(roots, listing);
    let mut owners: BTreeMap<PathBuf, (InstanceKey, bool)> = BTreeMap::new();

    for (project, stacks) in listing.data.iter().filter(|(p, _)| !is_trash(p)) {
        let skipped = known.skipped.contains_key(project);
        let loaded = known.projects.get(project);
        for (stack, apps) in stacks_in(stacks) {
            for app in apps {
                let key = key(project, stack, app);
                let orphaned = !skipped
                    && (gone(project)
                        || loaded.is_some_and(|p| !p.has_stack(stack) || !p.apps.contains(app)));
                owners.insert(roots.instance(&key).run, (key, orphaned));
            }
        }
        if skipped {
            continue;
        }
        let data = roots.data.join(project);
        let Some(loaded) = loaded else {
            if gone(project) {
                candidates.push(Orphan {
                    project: Some(project.clone()),
                    stack: None,
                    kind: OrphanKind::Project,
                    app: None,
                    path: data,
                });
            }
            continue;
        };
        for (stack, apps) in stacks_in(stacks) {
            if !loaded.has_stack(stack) {
                candidates.push(Orphan {
                    project: Some(project.clone()),
                    stack: Some(stack.clone()),
                    kind: OrphanKind::Stack,
                    app: None,
                    path: data.join(stack),
                });
                continue;
            }
            for app in apps.filter(|app| !loaded.apps.contains(*app)) {
                candidates.push(Orphan {
                    project: Some(project.clone()),
                    stack: Some(stack.clone()),
                    kind: OrphanKind::App,
                    app: Some(app.clone()),
                    path: data.join(stack).join(app),
                });
            }
        }
    }

    for (project, loaded) in &known.projects {
        let stacks = std::iter::once("main").chain(loaded.stacks.iter().map(String::as_str));
        for stack in stacks {
            for app in &loaded.apps {
                let key = key(project, stack, app);
                owners.insert(roots.instance(&key).run, (key, false));
            }
        }
    }
    for key in &known.running {
        owners.insert(roots.instance(key).run, (key.clone(), false));
    }

    for name in listing.run.iter().filter(|name| !is_trash(name)) {
        let path = roots.run.join(name);
        match owners.get(&path) {
            Some((key, true)) if listing.run_owned.contains(name) => candidates.push(Orphan {
                project: Some(key.project.clone()),
                stack: Some(key.stack.slug().to_string()),
                kind: OrphanKind::RunDir,
                app: Some(key.app.clone()),
                path,
            }),
            Some(_) => {}
            None if known.complete
                && known.skipped.is_empty()
                && listing.run_owned.contains(name) =>
            {
                candidates.push(Orphan {
                    project: None,
                    stack: None,
                    kind: OrphanKind::RunDir,
                    app: None,
                    path,
                })
            }
            None => {}
        }
    }

    for (project, files) in &listing.pidfiles {
        if known.skipped.contains_key(project) {
            continue;
        }
        let loaded = known.projects.get(project);
        if loaded.is_none() && !gone(project) {
            continue;
        }
        for file in files {
            let Some((app, stack)) = parse_pidfile(file) else {
                continue;
            };
            if loaded.is_some_and(|p| p.has_stack(stack) && p.apps.contains(app)) {
                continue;
            }
            candidates.push(Orphan {
                project: Some(project.clone()),
                stack: Some(stack.to_string()),
                kind: OrphanKind::Pidfile,
                app: Some(app.to_string()),
                path: pidfiles.join(project).join(file),
            });
        }
    }

    let mut running_stacks = BTreeSet::new();
    let mut torn_down = BTreeMap::new();
    for orphan in candidates {
        if let (Some(project), Some(stack)) = (&orphan.project, &orphan.stack)
            && let Some(reason) = known.tearing_down.get(&(project.clone(), stack.clone()))
        {
            torn_down.insert((project.clone(), stack.clone()), reason.clone());
            continue;
        }
        if orphan.is_running(&known.running, roots) {
            if let Some(project) = &orphan.project {
                running_stacks.insert((project.clone(), orphan.stack.clone()));
            }
            continue;
        }
        plan.orphans.push(orphan);
    }
    plan.orphans
        .sort_by(|a, b| (a.project.is_none(), a).cmp(&(b.project.is_none(), b)));

    for orphan in &plan.orphans {
        let (Some(project), OrphanKind::Project | OrphanKind::Stack) =
            (&orphan.project, orphan.kind)
        else {
            continue;
        };
        let stacks: Vec<&String> = match &orphan.stack {
            Some(stack) => vec![stack],
            None => listing.data[project].keys().collect(),
        };
        plan.stacks.extend(
            stacks
                .into_iter()
                .filter(|stack| !stack.starts_with('.'))
                .map(|stack| (project.clone(), stack.clone())),
        );
    }

    plan.skipped = known
        .skipped
        .iter()
        .filter(|(project, _)| {
            listing.data.contains_key(*project) || listing.pidfiles.contains_key(*project)
        })
        .map(|(project, reason)| Skip {
            project: Some(project.clone()),
            stack: None,
            reason: reason.clone(),
        })
        .chain(running_stacks.into_iter().map(|(project, stack)| Skip {
            project: Some(project),
            stack,
            reason: "running".into(),
        }))
        .chain(
            torn_down
                .into_iter()
                .map(|((project, stack), reason)| Skip {
                    project: Some(project),
                    stack: Some(stack),
                    reason,
                }),
        )
        .collect();
    plan
}

/// The folders earlier prunes moved leftovers into but couldn't delete, which sit
/// where those leftovers were.
fn trash(roots: &DirRoots, listing: &Listing) -> Vec<Orphan> {
    let mut paths = Vec::new();
    for (project, stacks) in &listing.data {
        let project_dir = roots.data.join(project);
        if is_trash(project) {
            paths.push(project_dir);
            continue;
        }
        for (stack, apps) in stacks {
            if is_trash(stack) {
                paths.push(project_dir.join(stack));
                continue;
            }
            for app in apps.iter().filter(|app| is_trash(app)) {
                paths.push(project_dir.join(stack).join(app));
            }
        }
    }
    for name in listing.run.iter().filter(|name| is_trash(name)) {
        paths.push(roots.run.join(name));
    }
    paths
        .into_iter()
        .map(|path| Orphan {
            project: None,
            stack: None,
            kind: OrphanKind::Trash,
            app: None,
            path,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> DirRoots {
        DirRoots {
            data: "/s/data".into(),
            run: "/r".into(),
        }
    }

    fn known(projects: &[(&str, &[&str], &[&str])]) -> Known {
        Known {
            projects: projects
                .iter()
                .map(|(name, apps, stacks)| {
                    let project = KnownProject {
                        apps: apps.iter().map(|a| a.to_string()).collect(),
                        stacks: stacks.iter().map(|s| s.to_string()).collect(),
                    };
                    (name.to_string(), project)
                })
                .collect(),
            complete: true,
            ..Known::default()
        }
    }

    fn listing(folders: &[(&str, &str, &str)]) -> Listing {
        let mut listing = Listing::default();
        for (project, stack, app) in folders {
            listing
                .data
                .entry(project.to_string())
                .or_default()
                .entry(stack.to_string())
                .or_default()
                .insert(app.to_string());
        }
        listing
    }

    fn run_name(project: &str, stack: &str, app: &str) -> String {
        let dirs = roots().instance(&key(project, stack, app));
        dirs.run.file_name().unwrap().to_string_lossy().into_owned()
    }

    fn paths(plan: &Plan) -> Vec<String> {
        plan.orphans
            .iter()
            .map(|o| o.path.display().to_string())
            .collect()
    }

    #[test]
    fn finds_stacks_apps_and_projects_that_are_gone() {
        let listing = listing(&[
            ("nova", "main", "nova-web"),
            ("nova", "main", "nova-old"),
            ("nova", "feature-x", "nova-web"),
            ("nova", "feature-y", "nova-web"),
            ("gone", "main", "gone-web"),
            ("gone", "z", "gone-web"),
        ]);
        let known = known(&[("nova", &["nova-web"], &["feature-y"])]);
        let plan = plan(&roots(), Path::new("/s/run"), &listing, &known);
        assert_eq!(
            paths(&plan),
            [
                "/s/data/gone",
                "/s/data/nova/feature-x",
                "/s/data/nova/main/nova-old",
            ]
        );
        assert_eq!(plan.orphans[0].kind, OrphanKind::Project);
        assert_eq!(plan.orphans[1].kind, OrphanKind::Stack);
        assert_eq!(plan.orphans[2].kind, OrphanKind::App);
        assert_eq!(plan.orphans[2].app.as_deref(), Some("nova-old"));
        assert_eq!(
            plan.stacks,
            [
                ("gone".into(), "main".into()),
                ("gone".into(), "z".into()),
                ("nova".into(), "feature-x".into()),
            ]
        );
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn main_stays_while_the_project_exists() {
        let listing = listing(&[("nova", "main", "nova-web")]);
        let known = known(&[("nova", &["nova-web"], &[])]);
        let plan = plan(&roots(), Path::new("/s/run"), &listing, &known);
        assert!(plan.orphans.is_empty());
    }

    #[test]
    fn skipped_projects_and_unlisted_project_files_are_left_alone() {
        let listing = listing(&[("broken", "x", "broken-web"), ("unknown", "main", "u")]);
        let mut known = known(&[]);
        known
            .skipped
            .insert("broken".into(), "git worktree list failed".into());
        let mut listing = listing;
        listing.run.insert(run_name("broken", "x", "broken-web"));
        listing.run.insert("aaaaaaaa".into());
        listing.run_owned.insert("aaaaaaaa".into());
        let mut skipped = plan(&roots(), Path::new("/s/run"), &listing, &known);
        assert_eq!(paths(&skipped), ["/s/data/unknown"]);
        assert_eq!(
            skipped.skipped.remove(0),
            Skip {
                project: Some("broken".into()),
                stack: None,
                reason: "git worktree list failed".into(),
            }
        );

        known.complete = false;
        let incomplete = plan(&roots(), Path::new("/s/run"), &listing, &known);
        assert!(incomplete.orphans.is_empty());
    }

    #[test]
    fn run_dirs_go_with_their_owner() {
        let mut listing = listing(&[("nova", "feature-x", "nova-web")]);
        let kept = run_name("nova", "main", "nova-web");
        let orphaned = run_name("nova", "feature-x", "nova-web");
        listing
            .run
            .extend([kept, orphaned.clone(), "aaaaaaaa".into(), "bbbbbbbb".into()]);
        listing
            .run_owned
            .extend([orphaned.clone(), "aaaaaaaa".into()]);
        let known = known(&[("nova", &["nova-web"], &[])]);
        let plan = plan(&roots(), Path::new("/s/run"), &listing, &known);
        assert_eq!(
            paths(&plan),
            [
                "/s/data/nova/feature-x".to_string(),
                format!("/r/{orphaned}"),
                "/r/aaaaaaaa".into(),
            ]
        );
        assert_eq!(plan.orphans[1].kind, OrphanKind::RunDir);
        assert_eq!(plan.orphans[1].stack.as_deref(), Some("feature-x"));
        assert_eq!(plan.orphans[2].project, None);
    }

    #[test]
    fn run_dirs_another_carte_owns_stay() {
        let mut listing = listing(&[("nova", "main", "nova-old")]);
        listing.run.insert(run_name("nova", "main", "nova-old"));
        let known = known(&[("nova", &["nova-web"], &[])]);
        let plan = plan(&roots(), Path::new("/s/run"), &listing, &known);
        assert_eq!(paths(&plan), ["/s/data/nova/main/nova-old"]);
    }

    #[test]
    fn trash_of_earlier_prunes_is_listed_on_its_own() {
        let mut listing = listing(&[
            ("nova", "main", "nova-web"),
            ("nova", "main", ".carte-prune-abc"),
            ("nova", ".carte-prune-def", "nova-web"),
            (".carte-prune-ghi", "gone", "gone-web"),
        ]);
        listing.run.insert(".carte-prune-abc".into());
        let mut known = known(&[("nova", &["nova-web"], &[])]);
        known.skipped.insert("other".into(), "broken".into());
        let plan = plan(&roots(), Path::new("/s/run"), &listing, &known);
        assert_eq!(
            paths(&plan),
            [
                "/r/.carte-prune-abc",
                "/s/data/.carte-prune-ghi",
                "/s/data/nova/.carte-prune-def",
                "/s/data/nova/main/.carte-prune-abc",
            ]
        );
        assert!(plan.orphans.iter().all(|o| o.kind == OrphanKind::Trash));
        assert!(plan.stacks.is_empty());
    }

    #[test]
    fn pidfiles_of_instances_that_no_longer_exist() {
        let mut listing = Listing::default();
        listing.pidfiles.insert(
            "nova".into(),
            [
                "nova-web@main.pid",
                "nova-web@feature-x.pid",
                "nova-old@main.pid",
                "notes.txt",
            ]
            .map(String::from)
            .into(),
        );
        listing
            .pidfiles
            .insert("gone".into(), ["gone-web@main.pid".to_string()].into());
        let known = known(&[("nova", &["nova-web"], &[])]);
        let plan = plan(&roots(), Path::new("/s/run"), &listing, &known);
        assert_eq!(
            paths(&plan),
            [
                "/s/run/gone/gone-web@main.pid",
                "/s/run/nova/nova-web@feature-x.pid",
                "/s/run/nova/nova-old@main.pid",
            ]
        );
        assert!(plan.stacks.is_empty());
    }

    #[test]
    fn running_instances_are_never_touched() {
        let mut listing = listing(&[
            ("nova", "feature-x", "nova-web"),
            ("nova", "feature-y", "nova-web"),
            ("gone", "main", "gone-web"),
        ]);
        let running_run = run_name("nova", "feature-x", "nova-web");
        listing.run.insert(running_run);
        listing
            .pidfiles
            .insert("nova".into(), ["nova-web@feature-x.pid".to_string()].into());
        let mut known = known(&[("nova", &["nova-web"], &[])]);
        known.running.insert(key("nova", "feature-x", "nova-web"));
        known.running.insert(key("gone", "main", "gone-web"));
        let plan = plan(&roots(), Path::new("/s/run"), &listing, &known);
        assert_eq!(paths(&plan), ["/s/data/nova/feature-y"]);
        assert_eq!(
            plan.skipped,
            [
                Skip {
                    project: Some("gone".into()),
                    stack: None,
                    reason: "running".into(),
                },
                Skip {
                    project: Some("nova".into()),
                    stack: Some("feature-x".into()),
                    reason: "running".into(),
                },
            ]
        );
    }

    #[test]
    fn stacks_being_torn_down_are_never_touched() {
        let mut listing = listing(&[
            ("nova", "feature-x", "nova-web"),
            ("nova", "feature-y", "nova-web"),
        ]);
        listing
            .pidfiles
            .insert("nova".into(), ["nova-web@feature-x.pid".to_string()].into());
        let mut known = known(&[("nova", &["nova-web"], &[])]);
        known.tearing_down.insert(
            ("nova".into(), "feature-x".into()),
            "its teardown is pending".into(),
        );
        let plan = plan(&roots(), Path::new("/s/run"), &listing, &known);
        assert_eq!(paths(&plan), ["/s/data/nova/feature-y"]);
        assert_eq!(
            plan.skipped,
            [Skip {
                project: Some("nova".into()),
                stack: Some("feature-x".into()),
                reason: "its teardown is pending".into(),
            }]
        );
    }

    #[test]
    fn pidfile_names() {
        assert_eq!(parse_pidfile("web@main.pid"), Some(("web", "main")));
        assert_eq!(parse_pidfile("a@b@x.pid"), Some(("a@b", "x")));
        assert_eq!(parse_pidfile("web.pid"), None);
        assert_eq!(parse_pidfile("@main.pid"), None);
        assert_eq!(parse_pidfile("web@main"), None);
    }
}
