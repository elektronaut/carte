//! Finding app checkouts and their worktrees on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::config::{self, DiscoveredApp, ProjectFile};
use crate::model::{AppRef, Problem, ProjectState, Worktree, World};
use crate::naming;
use crate::slug::branch_slug;

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("could not run git in {}: {source}", path.display())]
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("git worktree list failed in {}: {stderr}", path.display())]
    Git { path: PathBuf, stderr: String },
}

/// Expands `~` at the start of a path to `$HOME`.
pub fn expand_home(path: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if path == "~" {
        return home().unwrap_or_else(|| PathBuf::from(path));
    }
    match (path.strip_prefix("~/"), home()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// Whether a directory is a primary git checkout: it has a `.git` directory, or a
/// `.git` file that does not point into another repo's `worktrees/`.
pub fn is_primary_checkout(dir: &Path) -> bool {
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return true;
    }
    match std::fs::read_to_string(&dot_git) {
        Ok(contents) => !contents
            .trim()
            .strip_prefix("gitdir:")
            .is_some_and(|gitdir| gitdir.trim().contains("/worktrees/")),
        Err(_) => false,
    }
}

/// Expands `repos` entries (paths or globs, `~` allowed) into primary checkouts,
/// sorted and deduplicated. Also returns messages for entries that found nothing.
pub fn expand_repos(patterns: &[String]) -> (Vec<PathBuf>, Vec<String>) {
    let mut dirs = Vec::new();
    let mut problems = Vec::new();
    for pattern in patterns {
        let expanded = expand_home(pattern);
        let expanded = expanded.to_string_lossy();
        let paths = match glob::glob(&expanded) {
            Ok(paths) => paths.filter_map(Result::ok),
            Err(err) => {
                problems.push(format!(
                    "repos entry {pattern:?} is not a valid glob: {err}"
                ));
                continue;
            }
        };
        let found: Vec<PathBuf> = paths
            .filter(|p| p.is_dir() && is_primary_checkout(p))
            .collect();
        if found.is_empty() {
            problems.push(format!(
                "repos entry {pattern:?} matches no git repositories"
            ));
        }
        dirs.extend(found);
    }
    dirs.sort();
    dirs.dedup();
    (dirs, problems)
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    /// The branch name without `refs/heads/`.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    pub prunable: bool,
}

/// Parses `git worktree list --porcelain` output. The first entry is the main worktree.
pub fn parse_worktree_list(output: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in output.lines() {
        if line.is_empty() {
            entries.extend(current.take());
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            entries.extend(current.take());
            current = Some(WorktreeEntry {
                path: PathBuf::from(value),
                ..Default::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        match key {
            "HEAD" => entry.head = Some(value.to_string()),
            "branch" => {
                entry.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_string(),
                )
            }
            "bare" => entry.bare = true,
            "detached" => entry.detached = true,
            "locked" => entry.locked = true,
            "prunable" => entry.prunable = true,
            _ => {}
        }
    }
    entries.extend(current);
    entries
}

/// The linked worktrees that can run: not the main worktree, not bare, on a branch,
/// and not prunable (their directory is gone).
pub fn linked_worktrees(entries: &[WorktreeEntry]) -> Vec<Worktree> {
    entries
        .iter()
        .skip(1)
        .filter(|e| !e.bare && !e.detached && !e.prunable)
        .filter_map(|e| {
            let branch = e.branch.clone()?;
            Some(Worktree {
                slug: branch_slug(&branch),
                branch,
                path: e.path.clone(),
            })
        })
        .collect()
}

/// Runs `git worktree list --porcelain` in a main checkout.
pub fn list_worktrees(main_checkout: &Path) -> Result<Vec<Worktree>, DiscoveryError> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(main_checkout)
        .output()
        .map_err(|source| DiscoveryError::Spawn {
            path: main_checkout.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(DiscoveryError::Git {
            path: main_checkout.to_path_buf(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(linked_worktrees(&parse_worktree_list(&stdout)))
}

/// Expands a project's repos, reads each app's `carte.toml`, assembles the project
/// and lists each app's worktrees.
pub fn discover_project(file: ProjectFile) -> (ProjectState, Vec<Problem>) {
    let project_name = file.name.clone();
    let problem = |app: Option<&str>, message: String| Problem {
        project: Some(project_name.clone()),
        app: app.map(str::to_string),
        message,
    };
    let (dirs, repo_problems) = expand_repos(&file.repos);
    let mut problems: Vec<Problem> = repo_problems
        .into_iter()
        .map(|m| problem(None, m))
        .collect();

    let discovered = dirs
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let repo_config = config::load_repo_config(&path).unwrap_or_else(|err| {
                problems.push(problem(Some(&name), err.to_string()));
                None
            });
            Some(DiscoveredApp {
                name,
                path,
                repo_config,
            })
        })
        .collect();
    let (project, assembly_problems) = config::assemble_project(file, discovered);
    problems.extend(assembly_problems);

    let mut worktrees = BTreeMap::new();
    for app in project
        .apps
        .iter()
        .filter(|a| !a.path.as_os_str().is_empty())
    {
        let wts = match list_worktrees(&app.path) {
            Ok(wts) => wts,
            Err(err) => {
                problems.push(problem(Some(&app.name), err.to_string()));
                continue;
            }
        };
        for (i, wt) in wts.iter().enumerate() {
            if let Some(other) = wts[..i].iter().find(|o| o.slug == wt.slug) {
                problems.push(problem(
                    Some(&app.name),
                    format!(
                        "branches {:?} and {:?} have the same name in hostnames ({})",
                        other.branch, wt.branch, wt.slug
                    ),
                ));
            }
        }
        if !wts.is_empty() {
            worktrees.insert(app.name.clone(), wts);
        }
    }
    let state = ProjectState { project, worktrees };
    problems.extend(naming::collisions(&state));
    (state, problems)
}

/// Loads `config.toml`, every project file in `projects/` and what they find on disk.
/// Nothing here fails: problems are collected in the world.
pub fn load_world(config_dir: &Path) -> World {
    let mut world = World::default();
    match config::load_machine_config(&config_dir.join("config.toml")) {
        Ok(machine) => world.machine = machine,
        Err(err) => world.problems.push(Problem {
            project: None,
            app: None,
            message: format!("{err}; using defaults"),
        }),
    }

    let projects_dir = config_dir.join("projects");
    let files = match project_files(&projects_dir) {
        Ok(files) => files,
        Err(err) => {
            world.problems.push(Problem {
                project: None,
                app: None,
                message: format!("{}: {err}", projects_dir.display()),
            });
            Vec::new()
        }
    };

    for ProjectFileEntry { name, folder, path } in files {
        if let Some(other) = world.project(&name) {
            world.problems.push(Problem {
                project: Some(name.clone()),
                app: None,
                message: format!(
                    "{} is skipped: {} already defines a project named {name}, and names are hostnames, so they must be unique across folders",
                    path.display(),
                    other.project.config_path.display()
                ),
            });
            continue;
        }
        match config::load_project_file(&path) {
            Ok(file) => {
                let (mut state, problems) = discover_project(file);
                state.project.folder = folder;
                world.projects.push(state);
                world.problems.extend(problems);
            }
            Err(err) => world.problems.push(Problem {
                project: path.file_stem().map(|s| s.to_string_lossy().into_owned()),
                app: None,
                message: err.to_string(),
            }),
        }
    }
    resolve_elsewhere(&mut world);
    world
}

pub struct ProjectFileEntry {
    pub name: String,
    /// The subdirectory of `projects/` the file is in.
    pub folder: Option<String>,
    pub path: PathBuf,
}

/// The project files in `projects/` and its subdirectories one level down, top level
/// first, each level sorted by name. A missing directory has none.
pub fn project_files(projects_dir: &Path) -> std::io::Result<Vec<ProjectFileEntry>> {
    let mut top = Vec::new();
    let mut nested = Vec::new();
    let entries = match std::fs::read_dir(projects_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let toml_files = |dir: &Path| -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "toml") && p.is_file())
            .collect();
        files.sort();
        files
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    let entry = |path: PathBuf, folder: Option<String>| ProjectFileEntry {
        name: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        folder,
        path,
    };
    for path in toml_files(projects_dir) {
        top.push(entry(path, None));
    }
    for dir in dirs {
        let Some(folder) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if folder.starts_with('.') {
            continue;
        }
        for path in toml_files(&dir) {
            nested.push(entry(path, Some(folder.clone())));
        }
    }
    top.extend(nested);
    Ok(top)
}

/// Resolves the `depends_on` names that aren't apps of their own project: another
/// project (its collapsed app, or its only app) or `app.project`, like the hostname.
fn resolve_elsewhere(world: &mut World) {
    let index: Vec<(String, Vec<(String, String)>)> = world
        .projects
        .iter()
        .map(|state| {
            let apps = state
                .project
                .apps
                .iter()
                .map(|a| (a.name.clone(), a.short.clone()))
                .collect();
            (state.project.name.clone(), apps)
        })
        .collect();
    for state in &mut world.projects {
        let project = state.project.name.clone();
        for app in &mut state.project.apps {
            for dep in std::mem::take(&mut app.settings.unresolved_deps) {
                match find_elsewhere(&index, &project, &dep) {
                    Ok(found) => {
                        if !app.settings.depends_on_elsewhere.contains(&found) {
                            app.settings.depends_on_elsewhere.push(found);
                        }
                    }
                    Err(message) => world.problems.push(Problem {
                        project: Some(project.clone()),
                        app: Some(app.name.clone()),
                        message,
                    }),
                }
            }
        }
    }
}

fn find_elsewhere(
    index: &[(String, Vec<(String, String)>)],
    own: &str,
    dep: &str,
) -> Result<AppRef, String> {
    let unknown = || format!("depends_on refers to unknown app {dep:?}");
    let (app, project) = if index.iter().any(|(name, _)| name == dep) {
        (None, dep)
    } else {
        match dep.split_once('.') {
            Some((app, project)) => (Some(app), project),
            None => return Err(unknown()),
        }
    };
    if project == own {
        return Err(unknown());
    }
    let (_, apps) = index
        .iter()
        .find(|(name, _)| name == project)
        .ok_or_else(unknown)?;
    let found = match app {
        Some(app) => apps
            .iter()
            .find(|(name, _)| name == app)
            .or_else(|| apps.iter().find(|(_, short)| short == app)),
        None => apps
            .iter()
            .find(|(_, short)| short == project)
            .or(if apps.len() == 1 { apps.first() } else { None }),
    };
    match (found, app) {
        (Some((name, _)), _) => Ok(AppRef {
            project: project.to_string(),
            app: name.clone(),
        }),
        (None, None) => Err(format!(
            "depends_on names project {project}, which has several apps: name one, like {}.{project}",
            apps.first().map_or("app", |(_, short)| short.as_str())
        )),
        (None, Some(_)) => Err(unknown()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_apps_of_other_projects() {
        let index = vec![
            (
                "marketplace".to_string(),
                vec![("marketplace".to_string(), "marketplace".to_string())],
            ),
            (
                "nova".to_string(),
                vec![
                    ("nova-billing".to_string(), "invoices".to_string()),
                    ("nova-frontend".to_string(), "frontend".to_string()),
                ],
            ),
            (
                "wiki".to_string(),
                vec![("wiki".to_string(), "wiki".to_string())],
            ),
        ];
        let found = |dep| find_elsewhere(&index, "wiki", dep);
        let app_ref = |project: &str, app: &str| AppRef {
            project: project.into(),
            app: app.into(),
        };
        assert_eq!(
            found("marketplace"),
            Ok(app_ref("marketplace", "marketplace"))
        );
        assert_eq!(found("invoices.nova"), Ok(app_ref("nova", "nova-billing")));
        assert_eq!(
            found("nova-frontend.nova"),
            Ok(app_ref("nova", "nova-frontend"))
        );
        assert_eq!(
            found("nova"),
            Err("depends_on names project nova, which has several apps: name one, like invoices.nova".into())
        );
        assert!(found("nope").is_err());
        assert!(found("api.nova").is_err());
        assert!(found("wiki").is_err(), "not its own project");
    }

    const PORCELAIN: &str = "\
worktree /work/app
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /work/app-feature
HEAD 2222222222222222222222222222222222222222
branch refs/heads/Feature/New_Thing

worktree /work/app-detached
HEAD 3333333333333333333333333333333333333333
detached

worktree /work/app-locked
HEAD 4444444444444444444444444444444444444444
branch refs/heads/locked-branch
locked reason: on a usb stick

worktree /work/app-gone
HEAD 5555555555555555555555555555555555555555
branch refs/heads/gone
prunable gitdir file points to non-existent location

";

    #[test]
    fn parses_porcelain_output() {
        let entries = parse_worktree_list(PORCELAIN);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].branch.as_deref(), Some("Feature/New_Thing"));
        assert!(entries[2].detached);
        assert_eq!(entries[2].branch, None);
        assert!(entries[3].locked);
        assert!(entries[4].prunable);
        assert_eq!(
            entries[0].head.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
    }

    #[test]
    fn keeps_runnable_linked_worktrees() {
        let worktrees = linked_worktrees(&parse_worktree_list(PORCELAIN));
        assert_eq!(
            worktrees,
            vec![
                Worktree {
                    branch: "Feature/New_Thing".into(),
                    slug: "feature-new-thing".into(),
                    path: "/work/app-feature".into(),
                },
                Worktree {
                    branch: "locked-branch".into(),
                    slug: "locked-branch".into(),
                    path: "/work/app-locked".into(),
                },
            ]
        );
    }

    #[test]
    fn skips_bare_main_repo() {
        let output =
            "worktree /work/app.git\nbare\n\nworktree /work/app-x\nHEAD abc\nbranch refs/heads/x\n";
        let entries = parse_worktree_list(output);
        assert!(entries[0].bare);
        let worktrees = linked_worktrees(&entries);
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].branch, "x");
    }

    #[test]
    fn expands_home() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        assert_eq!(expand_home("~/work"), home.join("work"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("/abs/~x"), PathBuf::from("/abs/~x"));
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    fn repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
    }

    #[test]
    fn folders_group_projects_and_names_stay_unique() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for name in ["wiki", "marketplace", "blog"] {
            repo(&root.join("work").join(name));
        }
        let projects = root.join("config/projects");
        std::fs::create_dir_all(projects.join("agency")).unwrap();
        std::fs::create_dir_all(projects.join(".git")).unwrap();
        let file = |path: &str, repo: &str| {
            std::fs::write(
                projects.join(path),
                format!("repos = \"{}/work/{repo}\"\n", root.display()),
            )
            .unwrap();
        };
        file("agency/wiki.toml", "wiki");
        file("agency/marketplace.toml", "marketplace");
        file("blog.toml", "blog");
        file("wiki.toml", "wiki");
        file(".git/stray.toml", "blog");

        let world = load_world(&root.join("config"));
        let folders: Vec<_> = world
            .projects
            .iter()
            .map(|p| (p.project.name.as_str(), p.project.folder.as_deref()))
            .collect();
        assert_eq!(
            folders,
            [
                ("blog", None),
                ("wiki", None),
                ("marketplace", Some("agency"))
            ]
        );
        assert_eq!(world.problems.len(), 1, "{:?}", world.problems);
        assert!(
            world.problems[0]
                .message
                .contains("already defines a project named wiki")
        );
    }

    #[test]
    fn discovers_repos_and_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let work = root.join("work");
        repo(&work.join("nova-frontend"));
        repo(&work.join("nova-billing"));
        std::fs::create_dir_all(work.join("nova-notes")).unwrap();
        git(
            &work.join("nova-frontend"),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/x",
                "../nova-frontend-feature-x",
            ],
        );
        git(
            &work.join("nova-frontend"),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                "../nova-frontend-detached",
            ],
        );
        git(
            &work.join("nova-billing"),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "Feature/X",
                "../nova-billing-x",
            ],
        );
        std::fs::write(
            work.join("nova-billing/carte.toml"),
            "name = \"invoices\"\n[processes.web]\ncommand = \"bin/web\"\n",
        )
        .unwrap();

        let config_dir = root.join("config");
        std::fs::create_dir_all(config_dir.join("projects")).unwrap();
        std::fs::write(
            config_dir.join("projects/nova.toml"),
            format!(
                "repos = \"{}/nova-*\"\nstrip_prefix = \"nova-\"\n[apps.nova-frontend]\ndepends_on = [\"invoices\"]\n",
                work.display()
            ),
        )
        .unwrap();
        std::fs::write(
            config_dir.join("projects/xy.toml"),
            "repos = \"/nowhere\"\n",
        )
        .unwrap();
        std::fs::write(config_dir.join("projects/broken.toml"), "repos = 3\n").unwrap();

        let world = load_world(&config_dir);
        assert_eq!(world.projects.len(), 1);
        let state = world.project("nova").unwrap();
        let apps: Vec<_> = state
            .project
            .apps
            .iter()
            .map(|a| (a.name.as_str(), a.short.as_str()))
            .collect();
        assert_eq!(
            apps,
            vec![("nova-billing", "invoices"), ("nova-frontend", "frontend")]
        );
        let frontend = state.project.app("nova-frontend").unwrap();
        assert_eq!(frontend.settings.depends_on, vec!["nova-billing"]);
        let billing = state.project.app("nova-billing").unwrap();
        assert_eq!(
            billing.settings.processes.entries["web"].command.as_deref(),
            Some("bin/web")
        );
        assert_eq!(
            state.worktrees["nova-frontend"],
            vec![Worktree {
                branch: "feature/x".into(),
                slug: "feature-x".into(),
                path: work.join("nova-frontend-feature-x"),
            }]
        );
        assert_eq!(state.worktrees["nova-billing"][0].slug, "feature-x");
        assert_eq!(
            state.stacks().into_iter().collect::<Vec<_>>(),
            vec!["feature-x"]
        );

        let messages: Vec<_> = world
            .problems
            .iter()
            .map(|p| (p.project.as_deref(), p.message.as_str()))
            .collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(
            messages
                .iter()
                .any(|(p, m)| *p == Some("broken") && m.contains("broken.toml:1:"))
        );
        assert!(
            messages
                .iter()
                .any(|(p, m)| *p == Some("xy") && m.contains("project name \"xy\""))
        );
    }

    #[test]
    fn reports_branch_named_like_an_app() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        repo(&root.join("acme-web"));
        repo(&root.join("acme-api"));
        git(
            &root.join("acme-web"),
            &["worktree", "add", "-q", "-b", "api", "../acme-web-api"],
        );
        let file = config::parse_project(
            &root.join("acme.toml"),
            &format!(
                "repos = [\"{0}/acme-web\", \"{0}/acme-api\", \"{0}/missing\"]\nstrip_prefix = \"acme-\"\n",
                root.display()
            ),
        )
        .unwrap();
        let (state, problems) = discover_project(file);
        assert_eq!(state.project.apps.len(), 2);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].message.contains("\"") && problems[0].message.contains("missing"));
        assert!(problems[1].message.contains("same name as app acme-api"));
    }

    #[test]
    fn linked_worktree_dirs_are_not_apps() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        repo(&root.join("app"));
        git(
            &root.join("app"),
            &["worktree", "add", "-q", "-b", "x", "../app-x"],
        );
        let (dirs, problems) = expand_repos(&[format!("{}/*", root.display())]);
        assert_eq!(dirs, vec![root.join("app")]);
        assert!(problems.is_empty());
    }
}
