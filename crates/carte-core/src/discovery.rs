//! Finding app checkouts and their worktrees on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::config::{self, DiscoveredApp, ProjectFile};
use crate::include;
use crate::model::{AppRef, Problem, ProjectState, Worktree, World};
use crate::naming;
use crate::procfile;
use crate::slug;
use crate::slug::slugify;

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

/// The linked worktrees that can run: not the main worktree, not bare and not
/// prunable (their directory is gone). Each is named by `stack_names`.
pub fn linked_worktrees(entries: &[WorktreeEntry]) -> Vec<Worktree> {
    let repo = entries
        .first()
        .and_then(|e| e.path.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let linked: Vec<&WorktreeEntry> = entries
        .iter()
        .skip(1)
        .filter(|e| !e.bare && !e.prunable)
        .collect();
    let paths: Vec<&Path> = linked.iter().map(|e| e.path.as_path()).collect();
    linked
        .iter()
        .zip(stack_names(&repo, &paths))
        .map(|(e, slug)| Worktree {
            branch: e.branch.clone(),
            slug,
            path: e.path.clone(),
        })
        .collect()
}

/// The stack name of each of a repo's linked worktrees: its slugged folder name, or
/// the nearest ancestor's while the folder is named like the repo itself, shortened
/// with a hash to fit a DNS label. Worktrees that end up with the same name keep it;
/// discovery reports them.
pub fn stack_names(repo: &str, paths: &[&Path]) -> Vec<String> {
    let repo = slugify(repo);
    paths
        .iter()
        .map(|path| {
            path.components()
                .rev()
                .filter_map(|c| match c {
                    Component::Normal(name) => Some(slugify(&name.to_string_lossy())),
                    _ => None,
                })
                .find(|name| !name.is_empty() && *name != repo)
                .unwrap_or_else(|| repo.clone())
        })
        .map(|name| slug::shorten(&name, 63).expect("63 leaves room for the hash"))
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

/// Finds a project's apps in each checkout, reads each app's `carte.toml` and
/// process lists, and assembles the project.
pub fn discover_project(file: ProjectFile) -> (ProjectState, Vec<Problem>) {
    let project_name = file.name.clone();
    let named: Vec<(String, String)> = file
        .apps
        .iter()
        .filter_map(|(name, table)| table.path.clone().map(|path| (name.clone(), path)))
        .collect();
    let only_ports =
        file.root.is_none() && file.include.is_empty() && named.is_empty() && !file.apps.is_empty();
    let expanded = if only_ports {
        include::Expanded::default()
    } else {
        include::expand(&project_name, file.root.as_deref(), &file.include, &named)
    };
    let mut problems: Vec<Problem> = expanded
        .problems
        .into_iter()
        .map(|issue| Problem {
            project: Some(project_name.clone()),
            app: issue.app,
            stack: issue.stack,
            message: issue.message,
        })
        .collect();

    let mut worktrees = BTreeMap::new();
    let discovered: Vec<DiscoveredApp> = expanded
        .apps
        .into_iter()
        .map(|found| {
            let config_dir = if found.in_main {
                Some(found.path.clone())
            } else {
                found.worktrees.first().map(|wt| wt.path.clone())
            };
            let repo_config = config_dir.and_then(|dir| {
                config::load_repo_config(&dir).unwrap_or_else(|err| {
                    problems.push(Problem {
                        project: Some(project_name.clone()),
                        app: Some(found.name.clone()),
                        stack: None,
                        message: err.to_string(),
                    });
                    None
                })
            });
            if !found.worktrees.is_empty() {
                worktrees.insert(found.name.clone(), found.worktrees);
            }
            DiscoveredApp {
                name: found.name,
                naming: found.naming,
                path: found.path,
                repo: found.repo,
                branch_only: !found.in_main,
                repo_config,
            }
        })
        .collect();
    let (project, assembly_problems) = config::assemble_project(file.clone(), discovered.clone());
    problems.extend(assembly_problems);
    worktrees.retain(|name, _| project.app(name).is_some());
    let checkout_apps = checkout_apps(&file, &discovered, &project, &worktrees, &mut problems);

    let processes = project
        .apps
        .iter()
        .filter(|a| !a.path.as_os_str().is_empty())
        .flat_map(|app| {
            let main = (!app.branch_only).then(|| app.path.clone());
            let checkouts = main.into_iter().chain(
                worktrees
                    .get(&app.name)
                    .into_iter()
                    .flatten()
                    .map(|wt| wt.path.clone()),
            );
            checkouts.map(|path| {
                let settings = checkout_apps
                    .get(&path)
                    .map_or(&app.settings, |a| &a.settings);
                let list = procfile::load(&path, settings).map_err(|err| err.to_string());
                (path, list)
            })
        })
        .collect();
    let state = ProjectState {
        project,
        worktrees,
        processes,
        repo_worktrees: expanded.repo_worktrees,
        scan_dirs: expanded.scan_dirs,
        checkout_apps,
    };
    problems.extend(naming::collisions(&state));
    (state, problems)
}

/// The apps of worktrees whose `carte.toml` differs from the one the app's settings
/// come from, with that file's processes, env and env files. The rest of an app's
/// settings shape the whole project, so they stay as configured.
fn checkout_apps(
    file: &ProjectFile,
    discovered: &[DiscoveredApp],
    project: &crate::model::Project,
    worktrees: &BTreeMap<String, Vec<Worktree>>,
    problems: &mut Vec<Problem>,
) -> BTreeMap<PathBuf, crate::model::App> {
    let mut apps = BTreeMap::new();
    for (name, wts) in worktrees {
        let (Some(index), Some(app)) = (
            discovered.iter().position(|d| &d.name == name),
            project.app(name),
        ) else {
            continue;
        };
        for wt in wts {
            let config = match config::load_repo_config(&wt.path) {
                Ok(config) => config,
                Err(err) => {
                    problems.push(Problem {
                        project: Some(project.name.clone()),
                        app: Some(name.clone()),
                        stack: Some(wt.slug.clone()),
                        message: err.to_string(),
                    });
                    continue;
                }
            };
            if config == discovered[index].repo_config {
                continue;
            }
            let mut variant = discovered.to_vec();
            variant[index].repo_config = config;
            let (assembled, _) = config::assemble_project(file.clone(), variant);
            let Some(there) = assembled.app(name) else {
                continue;
            };
            let mut app = app.clone();
            app.settings.env = there.settings.env.clone();
            app.settings.env_files = there.settings.env_files.clone();
            app.settings.processes = there.settings.processes.clone();
            apps.insert(wt.path.clone(), app);
        }
    }
    apps
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
            stack: None,
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
                stack: None,
                message: format!("{}: {err}", projects_dir.display()),
            });
            Vec::new()
        }
    };

    let mut templates = Vec::new();
    for ProjectFileEntry { name, folder, path } in files {
        let other = world
            .project(&name)
            .map(|state| &state.project.config_path)
            .or_else(|| world.kube_project(&name).map(|p| &p.config_path));
        if let Some(other) = other {
            world.problems.push(Problem {
                project: Some(name.clone()),
                app: None,
                stack: None,
                message: format!(
                    "{} is skipped: {} already defines a project named {name}, and names are hostnames, so they must be unique across folders",
                    path.display(),
                    other.display()
                ),
            });
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && config::is_kube_project(&text)
        {
            match config::parse_kube_project(&path, &text) {
                Ok((mut project, problems)) => {
                    project.folder = folder;
                    world.kube_projects.push(project);
                    world.problems.extend(problems);
                }
                Err(err) => world.problems.push(Problem {
                    project: Some(name),
                    app: None,
                    stack: None,
                    message: err.to_string(),
                }),
            }
            continue;
        }
        match config::load_project_file(&path) {
            Ok(file) if !file.projects.is_empty() => templates.push((file, folder)),
            Ok(file) => {
                let (mut state, problems) = discover_project(file);
                state.project.folder = folder;
                world.projects.push(state);
                world.problems.extend(problems);
            }
            Err(err) => world.problems.push(Problem {
                project: path.file_stem().map(|s| s.to_string_lossy().into_owned()),
                app: None,
                stack: None,
                message: err.to_string(),
            }),
        }
    }
    let mut generated = BTreeSet::new();
    for (file, folder) in templates {
        expand_template(&mut world, &mut generated, file, folder);
    }
    resolve_elsewhere(&mut world);
    drop_taken_aliases(&mut world);
    world
}

/// Leaves out aliases that would shadow a name: a local alias equal to a project's
/// name or an alias of a project loaded earlier, and a kube alias equal to a
/// project's name, a local alias or an alias of another kube project. Project names
/// win, then local aliases in load order, then the kube project loaded first.
fn drop_taken_aliases(world: &mut World) {
    let kube_names: BTreeSet<String> = world.kube_projects.iter().map(|p| p.name.clone()).collect();
    let project_names: BTreeSet<String> = world
        .projects
        .iter()
        .map(|state| state.project.name.clone())
        .collect();
    let mut claims: BTreeMap<String, Vec<AliasClaim>> = BTreeMap::new();
    for state in &world.projects {
        let template = state
            .project
            .config_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| *stem != state.project.name);
        for app in &state.project.apps {
            for alias in &app.settings.aliases {
                claims.entry(alias.clone()).or_default().push(AliasClaim {
                    project: state.project.name.clone(),
                    app: app.name.clone(),
                    template: template.clone(),
                });
            }
        }
    }
    let mut problems = Vec::new();
    let mut skipped: BTreeSet<(String, String, String)> = BTreeSet::new();
    for (alias, claims) in claims {
        let (losers, message) = if project_names.contains(&alias) || kube_names.contains(&alias) {
            let reason = match claims.as_slice() {
                [only] if only.project == alias => "it's the project's own name".to_string(),
                _ => format!("project {alias} already uses it"),
            };
            let message = format!(
                "alias {alias:?} is skipped for {}: {reason}",
                claim_list(&claims, None)
            );
            (&claims[..], message)
        } else if claims.len() > 1 {
            let winner = &claims[0];
            let message = format!(
                "alias {alias:?} goes to {}/{}; skipped for {}",
                winner.project,
                winner.app,
                claim_list(&claims[1..], Some(&winner.app))
            );
            (&claims[1..], message)
        } else {
            continue;
        };
        let first = &losers[0];
        let template = first
            .template
            .as_ref()
            .filter(|t| losers.iter().all(|c| c.template.as_ref() == Some(*t)));
        problems.push(Problem {
            project: Some(template.unwrap_or(&first.project).clone()),
            app: match (template, losers) {
                (None, [only]) => Some(only.app.clone()),
                _ => None,
            },
            stack: None,
            message,
        });
        for claim in losers {
            skipped.insert((claim.project.clone(), claim.app.clone(), alias.clone()));
        }
    }
    for state in &mut world.projects {
        let project = state.project.name.clone();
        for app in &mut state.project.apps {
            app.settings.aliases.retain(|alias| {
                !skipped.contains(&(project.clone(), app.name.clone(), alias.clone()))
            });
        }
    }

    let mut taken: BTreeSet<String> = world
        .projects
        .iter()
        .flat_map(|state| {
            std::iter::once(state.project.name.clone()).chain(
                state
                    .project
                    .apps
                    .iter()
                    .flat_map(|app| app.settings.aliases.clone()),
            )
        })
        .chain(kube_names)
        .collect();
    for project in &mut world.kube_projects {
        for (service, settings) in &mut project.services {
            settings.aliases.retain(|alias| {
                let free = taken.insert(alias.clone());
                if !free {
                    problems.push(Problem {
                        project: Some(project.name.clone()),
                        app: Some(service.clone()),
                        stack: None,
                        message: format!(
                            "alias {alias:?} is skipped: a project or another app already uses it"
                        ),
                    });
                }
                free
            });
        }
    }
    world.problems.extend(problems);
}

struct AliasClaim {
    project: String,
    app: String,
    /// The template that made the project.
    template: Option<String>,
}

/// `nova, orbit and shop/api`, cut short after three: an app named like
/// `same_app` is given by its project alone.
fn claim_list(claims: &[AliasClaim], same_app: Option<&str>) -> String {
    let names: Vec<String> = claims
        .iter()
        .map(|c| {
            if Some(c.app.as_str()) == same_app {
                c.project.clone()
            } else {
                format!("{}/{}", c.project, c.app)
            }
        })
        .collect();
    match names.as_slice() {
        [only] => only.clone(),
        [shown @ .., last] if names.len() <= 3 => format!("{} and {last}", shown.join(", ")),
        _ => format!("{} and {} more", names[..3].join(", "), names.len() - 3),
    }
}

/// Adds a project for each directory a template's `projects` entries match, made from
/// the template with the directory as its root. A project file of the same name wins.
/// A directory a wildcard matched is left out unless it has an app with something to run.
fn expand_template(
    world: &mut World,
    generated: &mut BTreeSet<String>,
    file: ProjectFile,
    folder: Option<String>,
) {
    let template = file.name.clone();
    let problem = |project: &str, message: String| Problem {
        project: Some(project.to_string()),
        app: None,
        stack: None,
        message,
    };
    let (dirs, scans, messages) = include::expand_projects(file.root.as_deref(), &file.projects);
    world.scan_dirs.extend(scans);
    world
        .problems
        .extend(messages.into_iter().map(|m| problem(&template, m)));
    let mut unmatched: BTreeSet<String> = file
        .apps
        .iter()
        .filter(|(_, table)| table.path.is_none() && table.port.is_none())
        .map(|(name, _)| name.clone())
        .collect();
    for (dir, wildcard) in dirs {
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if generated.contains(&name) {
            world.problems.push(problem(
                &name,
                format!(
                    "{} is skipped: another template already made a project named {name}",
                    dir.display()
                ),
            ));
            continue;
        }
        if world.project(&name).is_some() || world.kube_project(&name).is_some() {
            continue;
        }
        if let Err(err) = slug::validate_project_name(&name) {
            if !wildcard || include::is_runnable(&dir) {
                world.problems.push(problem(
                    &template,
                    format!(
                        "{} is skipped: {err}; give it a project file of its own",
                        dir.display()
                    ),
                ));
            }
            continue;
        }
        let project = ProjectFile {
            name: name.clone(),
            root: Some(dir.to_string_lossy().into_owned()),
            projects: Vec::new(),
            from_template: true,
            ..file.clone()
        };
        let (mut state, problems) = discover_project(project);
        if wildcard
            && !state.project.apps.iter().any(|app| {
                let checkout = state.any_checkout(app);
                include::is_runnable(&checkout)
                    || app
                        .settings
                        .serve_files
                        .as_ref()
                        .is_some_and(|dir| checkout.join(dir).is_dir())
            })
        {
            continue;
        }
        state.project.folder = folder.clone();
        unmatched.retain(|table| state.project.app(table).is_none());
        generated.insert(name);
        world.projects.push(state);
        world.problems.extend(problems);
    }
    for table in unmatched {
        world.problems.push(problem(
            &template,
            format!("[apps.{table}] does not match any app in the template's projects"),
        ));
    }
}

pub struct ProjectFileEntry {
    pub name: String,
    /// The subdirectory of `projects/` the file is in, `/`-separated.
    pub folder: Option<String>,
    pub path: PathBuf,
}

/// The project files in `projects/` and its subdirectories at any depth: a
/// directory's files sorted by name, then its subdirectories in name order. A missing
/// directory has none.
pub fn project_files(projects_dir: &Path) -> std::io::Result<Vec<ProjectFileEntry>> {
    let entries = match std::fs::read_dir(projects_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut seen = BTreeSet::new();
    if let Ok(dir) = projects_dir.canonicalize() {
        seen.insert(dir);
    }
    let mut files = Vec::new();
    collect_project_files(entries, None, &mut seen, &mut files);
    Ok(files)
}

/// Directories already in `seen` are skipped, so a symlink loop ends.
fn collect_project_files(
    entries: std::fs::ReadDir,
    folder: Option<&str>,
    seen: &mut BTreeSet<PathBuf>,
    out: &mut Vec<ProjectFileEntry>,
) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        if path.is_dir() {
            dirs.push(path);
        } else if path.extension().is_some_and(|ext| ext == "toml") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    dirs.sort();
    for path in files {
        out.push(ProjectFileEntry {
            name: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            folder: folder.map(str::to_string),
            path,
        });
    }
    for dir in dirs {
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if name.starts_with('.') || !dir.canonicalize().is_ok_and(|real| seen.insert(real)) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let sub = match folder {
            Some(parent) => format!("{parent}/{name}"),
            None => name,
        };
        collect_project_files(entries, Some(&sub), seen, out);
    }
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
                        stack: None,
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
    use crate::model::{Policy, Stack};

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
                    branch: Some("Feature/New_Thing".into()),
                    slug: "app-feature".into(),
                    path: "/work/app-feature".into(),
                },
                Worktree {
                    branch: None,
                    slug: "app-detached".into(),
                    path: "/work/app-detached".into(),
                },
                Worktree {
                    branch: Some("locked-branch".into()),
                    slug: "app-locked".into(),
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
        assert_eq!(worktrees[0].branch.as_deref(), Some("x"));
        assert_eq!(worktrees[0].slug, "app-x");
    }

    fn names(repo: &str, paths: &[&str]) -> Vec<String> {
        let paths: Vec<&Path> = paths.iter().map(Path::new).collect();
        stack_names(repo, &paths)
    }

    #[test]
    fn stacks_are_named_after_worktree_folders() {
        assert_eq!(
            names("acme-billing", &["/w/acme-billing-feature-x"]),
            ["acme-billing-feature-x"]
        );
        assert_eq!(
            names(
                "acme-billing",
                &["/home/me/worktrees/acme-billing/feature-x"]
            ),
            ["feature-x"]
        );
        assert_eq!(
            names("acme-billing", &["/w/acme-billing/.worktrees/feature-x"]),
            ["feature-x"]
        );
        assert_eq!(names("acme-billing", &["/w/Feature_X.2"]), ["feature-x-2"]);
    }

    #[test]
    fn long_folders_give_stack_names_that_fit_a_label() {
        let folder = "implement-the-entire-newsletter-subscription-flow-with-tests-v2-ok";
        assert_eq!(folder.len(), 66);
        let short = names("nova", &[&format!("/w/{folder}")]).remove(0);
        assert_eq!(short.len(), 63);
        assert!(short.starts_with("implement-the-entire-newsletter-subscription-flow-with-t-"));
        assert_eq!(short, slug::shorten(folder, 63).unwrap());
        assert_ne!(
            short,
            names("nova", &[&format!("/w/{folder}-again")])[0],
            "the hash keeps similar folders apart"
        );
        let exact = "x".repeat(63);
        assert_eq!(names("nova", &[&format!("/w/{exact}")]), [exact]);
    }

    #[test]
    fn folders_named_like_the_repo_take_the_parents_name() {
        assert_eq!(
            names(
                "acme-start",
                &[
                    "/Users/me/Dev/acme/workspaces/checkout-flow/acme-start",
                    "/Users/me/Dev/acme/workspaces/Hotfix/acme-start",
                ]
            ),
            ["checkout-flow", "hotfix"]
        );
        assert_eq!(
            names("Acme-Start", &["/w/acme-start/acme-start"]),
            ["w"],
            "every ancestor named like the repo is skipped"
        );
    }

    #[test]
    fn worktrees_sharing_a_name_keep_it() {
        assert_eq!(
            names(
                "app",
                &["/w/one/feature", "/w/two/feature", "/w/three/other"]
            ),
            ["feature", "feature", "other"]
        );
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
                format!("root = \"{}/work/{repo}\"\n", root.display()),
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
    fn folders_nest_and_names_stay_unique_across_depths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for name in ["shop", "nova", "mono", "acme"] {
            repo(&root.join("work").join(name));
        }
        let projects = root.join("config/projects");
        std::fs::create_dir_all(projects.join("clients/sites/.cache")).unwrap();
        std::fs::create_dir_all(projects.join("clients/apps")).unwrap();
        let file = |path: &str, repo: &str| {
            std::fs::write(
                projects.join(path),
                format!("root = \"{}/work/{repo}\"\n", root.display()),
            )
            .unwrap();
        };
        file("acme.toml", "acme");
        file("clients/shop.toml", "shop");
        file("clients/sites/nova.toml", "nova");
        file("clients/apps/mono.toml", "mono");
        file("clients/apps/acme.toml", "acme");
        file("clients/sites/.cache/stray.toml", "mono");

        let files: Vec<_> = project_files(&projects)
            .unwrap()
            .into_iter()
            .map(|f| (f.name, f.folder))
            .collect();
        assert_eq!(
            files,
            [
                ("acme".to_string(), None),
                ("shop".to_string(), Some("clients".to_string())),
                ("acme".to_string(), Some("clients/apps".to_string())),
                ("mono".to_string(), Some("clients/apps".to_string())),
                ("nova".to_string(), Some("clients/sites".to_string())),
            ]
        );

        let world = load_world(&root.join("config"));
        let folders: Vec<_> = world
            .projects
            .iter()
            .map(|p| (p.project.name.as_str(), p.project.folder.as_deref()))
            .collect();
        assert_eq!(
            folders,
            [
                ("acme", None),
                ("shop", Some("clients")),
                ("mono", Some("clients/apps")),
                ("nova", Some("clients/sites")),
            ]
        );
        assert_eq!(world.problems.len(), 1, "{:?}", world.problems);
        assert!(
            world.problems[0]
                .message
                .contains("already defines a project named acme")
        );
    }

    #[test]
    fn discovers_repos_and_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let work = root.join("work");
        repo(&work.join("nova-frontend"));
        repo(&work.join("nova-billing"));
        std::fs::write(work.join("nova-frontend/Procfile"), "web: bin/web\n").unwrap();
        git(&work.join("nova-frontend"), &["add", "Procfile"]);
        git(
            &work.join("nova-frontend"),
            &["commit", "-q", "-m", "procfile"],
        );
        std::fs::create_dir_all(work.join("nova-notes")).unwrap();
        git(
            &work.join("nova-frontend"),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/x",
                "../workspaces/Feature-X/nova-frontend",
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
                "other",
                "../workspaces/Feature-X/nova-billing",
            ],
        );
        std::fs::write(
            work.join("nova-billing/carte.toml"),
            "[processes.web]\ncommand = \"bin/web\"\n",
        )
        .unwrap();

        let config_dir = root.join("config");
        std::fs::create_dir_all(config_dir.join("projects")).unwrap();
        std::fs::write(
            config_dir.join("projects/nova.toml"),
            format!(
                "root = \"{}\"\ninclude = \"nova-*\"\nstrip_prefix = \"nova-\"\n[apps.invoices]\npath = \"nova-billing\"\n[apps.nova-frontend]\ndepends_on = [\"invoices\"]\n",
                work.display()
            ),
        )
        .unwrap();
        std::fs::write(config_dir.join("projects/xy.toml"), "root = \"/nowhere\"\n").unwrap();
        std::fs::write(config_dir.join("projects/broken.toml"), "root = 3\n").unwrap();

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
            vec![("invoices", "invoices"), ("nova-frontend", "frontend")]
        );
        let frontend = state.project.app("nova-frontend").unwrap();
        assert_eq!(frontend.settings.depends_on, vec!["invoices"]);
        let billing = state.project.app("invoices").unwrap();
        assert_eq!(
            billing.settings.processes.entries["web"].command.as_deref(),
            Some("bin/web")
        );
        assert_eq!(
            state.worktrees["nova-frontend"],
            vec![
                Worktree {
                    branch: None,
                    slug: "nova-frontend-detached".into(),
                    path: work.join("nova-frontend-detached"),
                },
                Worktree {
                    branch: Some("feature/x".into()),
                    slug: "feature-x".into(),
                    path: work.join("workspaces/Feature-X/nova-frontend"),
                },
            ]
        );
        assert_eq!(state.worktrees["invoices"][0].slug, "feature-x");
        assert_eq!(
            state.stacks().into_iter().collect::<Vec<_>>(),
            vec!["feature-x", "nova-frontend-detached"]
        );
        assert_eq!(naming::stack_branch(state, "feature-x"), None);
        assert_eq!(naming::stack_branch(state, "nova-frontend-detached"), None);

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
    fn worktrees_in_long_folders_resolve_under_the_shortened_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let shop = root.join("shop");
        repo(&shop);
        std::fs::write(shop.join("Procfile"), "web: x\n").unwrap();
        let folder = format!("{}-{}", "newsletter-subscription-flow", "x".repeat(41));
        assert_eq!(folder.len(), 70);
        git(
            &shop,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "long",
                &format!("../{folder}"),
            ],
        );
        let config_dir = root.join("config");
        std::fs::create_dir_all(config_dir.join("projects")).unwrap();
        std::fs::write(
            config_dir.join("projects/shop.toml"),
            format!("root = \"{}\"\n", shop.display()),
        )
        .unwrap();

        let world = load_world(&config_dir);
        assert!(world.problems.is_empty(), "{:?}", world.problems);
        let stack = slug::shorten(&folder, 63).unwrap();
        assert_eq!(stack.len(), 63);
        let resolved =
            crate::naming::resolve_host(&world, &format!("{stack}.shop.localhost")).unwrap();
        assert_eq!(resolved.key.stack, Stack::Branch(stack));
    }

    #[test]
    fn reads_each_checkouts_process_list() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let work = root.join("work");
        repo(&work.join("shop"));
        repo(&work.join("notes"));
        git(
            &work.join("shop"),
            &["worktree", "add", "-q", "-b", "feature", "../shop-feature"],
        );
        std::fs::write(
            work.join("shop/Procfile.dev"),
            "web: rails s\nworker: jobs\n",
        )
        .unwrap();
        std::fs::write(work.join("shop-feature/Procfile"), "web: rails s\n").unwrap();
        let config_dir = root.join("config");
        std::fs::create_dir_all(config_dir.join("projects")).unwrap();
        std::fs::write(
            config_dir.join("projects/shop.toml"),
            format!(
                "root = \"{0}\"\ninclude = [\"shop\", \"notes\"]\n[apps.shop.processes.worker]\npolicy = \"disabled\"\n",
                work.display()
            ),
        )
        .unwrap();

        let world = load_world(&config_dir);
        let state = world.project("shop").unwrap();
        let shop = state.project.app("shop").unwrap();
        let names = |stack: &Stack| -> Vec<(String, Policy)> {
            state
                .process_list(shop, stack)
                .unwrap()
                .as_ref()
                .unwrap()
                .processes
                .iter()
                .map(|p| (p.name.clone(), p.policy))
                .collect()
        };
        assert_eq!(
            names(&Stack::Main),
            [
                ("web".to_string(), Policy::Auto),
                ("worker".to_string(), Policy::Disabled)
            ]
        );
        assert_eq!(
            names(&Stack::Branch("shop-feature".into())),
            [("web".to_string(), Policy::Auto)]
        );
        let notes = state.project.app("notes").unwrap();
        assert!(
            state
                .process_list(notes, &Stack::Main)
                .unwrap()
                .as_ref()
                .unwrap_err()
                .contains("no Procfile.dev")
        );
    }

    #[test]
    fn loads_kube_projects_beside_local_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        let projects = config_dir.join("projects");
        std::fs::create_dir_all(projects.join("clusters")).unwrap();
        std::fs::create_dir_all(tmp.path().join("shop")).unwrap();
        std::fs::write(
            projects.join("shop.toml"),
            format!(
                "root = \"{}\"\n[apps.shop]\naliases = [\"store\", \"prod\"]\n",
                tmp.path().join("shop").display()
            ),
        )
        .unwrap();
        std::fs::write(
            projects.join("clusters/prod.toml"),
            "kube = { context = \"prod\" }\n[apps.grafana]\naliases = [\"dash\", \"store\", \"shop\"]\n",
        )
        .unwrap();
        std::fs::write(
            projects.join("clusters/shop.toml"),
            "kube = { context = \"other\" }\n",
        )
        .unwrap();
        std::fs::write(projects.join("broken.toml"), "kube = { context = 3 }\n").unwrap();
        std::fs::write(
            projects.join("clusters/staging.toml"),
            "kube = { context = \"staging\" }\n[apps.grafana]\naliases = [\"dash\", \"prod\"]\n",
        )
        .unwrap();

        let world = load_world(&config_dir);
        let shop = world.project("shop").unwrap();
        assert_eq!(
            shop.project.apps[0].settings.aliases,
            vec!["store"],
            "a kube project's name beats a local alias"
        );
        let prod = world.kube_project("prod").unwrap();
        assert_eq!(prod.folder.as_deref(), Some("clusters"));
        assert_eq!(prod.services["grafana"].aliases, vec!["dash"]);
        assert_eq!(world.kube_projects.len(), 2);
        assert!(
            world.kube_project("staging").unwrap().services["grafana"]
                .aliases
                .is_empty(),
            "dash is prod's, and prod is a project"
        );
        let messages: Vec<_> = world.problems.iter().map(|p| p.message.as_str()).collect();
        assert!(
            messages.iter().any(|m| m.contains("broken.toml")),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("already defines a project named shop")),
            "{messages:?}"
        );
        for alias in ["store", "shop"] {
            assert!(
                messages
                    .iter()
                    .any(|m| m.contains(&format!("alias \"{alias}\" is skipped"))),
                "{messages:?}"
            );
        }
    }

    #[test]
    fn project_names_beat_local_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for name in ["nova", "shop", "wiki"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let projects = root.join("config/projects");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(
            projects.join("nova.toml"),
            format!(
                "root = \"{}\"\n[apps.nova]\naliases = [\"shop\", \"orders\"]\n",
                root.join("nova").display()
            ),
        )
        .unwrap();
        std::fs::write(
            projects.join("shop.toml"),
            format!("root = \"{}\"\n", root.join("shop").display()),
        )
        .unwrap();
        std::fs::write(
            projects.join("wiki.toml"),
            format!(
                "root = \"{}\"\n[apps.wiki]\naliases = [\"wiki\"]\n",
                root.join("wiki").display()
            ),
        )
        .unwrap();

        let world = load_world(&root.join("config"));
        let nova = world.project("nova").unwrap();
        assert_eq!(nova.project.apps[0].settings.aliases, ["orders"]);
        assert!(
            world.project("wiki").unwrap().project.apps[0]
                .settings
                .aliases
                .is_empty()
        );
        let problems: Vec<_> = world
            .problems
            .iter()
            .map(|p| (p.project.as_deref(), p.app.as_deref(), p.message.as_str()))
            .collect();
        assert_eq!(
            problems,
            [
                (
                    Some("nova"),
                    Some("nova"),
                    "alias \"shop\" is skipped for nova/nova: project shop already uses it"
                ),
                (
                    Some("wiki"),
                    Some("wiki"),
                    "alias \"wiki\" is skipped for wiki/wiki: it's the project's own name"
                ),
            ]
        );
        let resolved = crate::naming::resolve_host(&world, "shop.localhost").unwrap();
        assert_eq!(resolved.key.project, "shop");
    }

    #[test]
    fn projects_from_a_template_dont_share_an_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sites = root.join("sites");
        let names = ["acme", "mono", "nova", "orbit", "shop"];
        for name in names {
            let dir = sites.join(name);
            repo(&dir);
            std::fs::create_dir_all(dir.join("apps/web")).unwrap();
            std::fs::write(dir.join("apps/web/Procfile"), "web: x\n").unwrap();
        }
        let projects = root.join("config/projects");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(
            projects.join("sites.toml"),
            format!(
                "root = \"{}\"\nprojects = \"*\"\ninclude = \"apps/*\"\n\n[apps.web]\naliases = [\"store\"]\n",
                sites.display()
            ),
        )
        .unwrap();

        let world = load_world(&root.join("config"));
        let aliases = |project: &str| {
            world
                .project(project)
                .unwrap()
                .project
                .app("web")
                .unwrap()
                .settings
                .aliases
                .clone()
        };
        assert_eq!(aliases("acme"), ["store"]);
        for name in &names[1..] {
            assert!(aliases(name).is_empty(), "{name}");
        }
        assert_eq!(world.problems.len(), 1, "{:?}", world.problems);
        assert_eq!(world.problems[0].project.as_deref(), Some("sites"));
        assert_eq!(world.problems[0].app, None);
        assert_eq!(
            world.problems[0].message,
            "alias \"store\" goes to acme/web; skipped for mono, nova, orbit and 1 more"
        );
        let resolved = crate::naming::resolve_host(&world, "store.localhost").unwrap();
        assert_eq!(resolved.key.project, "acme");
    }

    #[test]
    fn templates_make_a_project_per_match() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let apps = root.join("apps");
        for (name, runnable) in [
            ("wiki", true),
            ("shop", true),
            ("lib", false),
            ("My_Site", true),
            ("archived-blog", true),
        ] {
            repo(&apps.join(name));
            if runnable {
                std::fs::write(apps.join(name).join("Procfile"), "web: x\n").unwrap();
            }
        }
        let projects = root.join("config/projects");
        std::fs::create_dir_all(projects.join("clients")).unwrap();
        std::fs::write(
            projects.join("clients/apps.toml"),
            format!(
                "root = \"{}\"\nprojects = [\"*\", \"!archived-*\"]\nidle_timeout = \"30m\"\n\n[apps.wiki]\naliases = [\"old-wiki\"]\n",
                apps.display()
            ),
        )
        .unwrap();
        std::fs::write(
            projects.join("shop.toml"),
            format!("root = \"{}\"\n", apps.join("shop").display()),
        )
        .unwrap();

        let world = load_world(&root.join("config"));
        let found: Vec<_> = world
            .projects
            .iter()
            .map(|p| {
                (
                    p.project.name.as_str(),
                    p.project.folder.as_deref(),
                    p.project.idle_timeout,
                )
            })
            .collect();
        let half_hour = Some(std::time::Duration::from_secs(1800));
        assert_eq!(
            found,
            [("shop", None, None), ("wiki", Some("clients"), half_hour),]
        );
        let wiki = world.project("wiki").unwrap();
        assert_eq!(wiki.project.apps[0].name, "wiki");
        assert_eq!(wiki.project.apps[0].settings.aliases, ["old-wiki"]);
        assert_eq!(world.problems.len(), 1, "{:?}", world.problems);
        assert!(world.problems[0].message.contains("My_Site is skipped"));
        assert!(world.scan_dirs.contains(&apps));
    }

    #[test]
    fn template_app_tables_only_apply_where_the_app_is() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sites = root.join("sites");
        for name in ["wiki", "shop"] {
            repo(&sites.join(name));
            std::fs::write(sites.join(name).join("Procfile"), "web: x\n").unwrap();
        }
        let projects = root.join("config/projects");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(
            projects.join("sites.toml"),
            format!(
                "root = \"{}\"\nprojects = \"*\"\n\n[apps.wiki]\naliases = [\"old-wiki\"]\n\n[apps.nova]\naliases = [\"old-nova\"]\n",
                sites.display()
            ),
        )
        .unwrap();

        let world = load_world(&root.join("config"));
        assert_eq!(world.projects.len(), 2);
        let wiki = world.project("wiki").unwrap();
        assert_eq!(wiki.project.apps[0].settings.aliases, ["old-wiki"]);
        let messages: Vec<_> = world.problems.iter().map(|p| p.message.as_str()).collect();
        assert_eq!(
            messages,
            ["[apps.nova] does not match any app in the template's projects"]
        );
        assert_eq!(world.problems[0].project.as_deref(), Some("sites"));
    }

    #[test]
    fn a_worktrees_carte_toml_sets_its_processes_and_env() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let shop = root.join("shop");
        repo(&shop);
        std::fs::write(
            shop.join("carte.toml"),
            "aliases = [\"store\"]\n[env]\nMODE = \"main\"\n[processes.web]\ncommand = \"bin/web\"\n",
        )
        .unwrap();
        git(&shop, &["add", "-A"]);
        git(&shop, &["commit", "-q", "-m", "config"]);
        git(&shop, &["worktree", "add", "-q", "-b", "fx", "../shop-fx"]);
        let fx = root.join("shop-fx");
        std::fs::write(
            fx.join("carte.toml"),
            "aliases = [\"elsewhere\"]\n[env]\nMODE = \"branch\"\n[processes.web]\ncommand = \"bin/web --new\"\n",
        )
        .unwrap();

        let file = config::parse_project(
            &root.join("shop.toml"),
            &format!("root = \"{}\"\n", shop.display()),
        )
        .unwrap();
        let (state, problems) = discover_project(file);
        assert!(problems.is_empty(), "{problems:?}");
        let app = state.project.app("shop").unwrap();
        let fx_stack = Stack::Branch("shop-fx".into());
        let web = |stack: &Stack| {
            state
                .process_list(app, stack)
                .unwrap()
                .as_ref()
                .unwrap()
                .processes[0]
                .command
                .clone()
        };
        assert_eq!(web(&Stack::Main), "bin/web");
        assert_eq!(web(&fx_stack), "bin/web --new");
        let there = state.app_at(app, &fx_stack);
        assert_eq!(there.settings.env[0].value, "branch");
        assert_eq!(there.settings.aliases, ["store"]);
        assert_eq!(
            state.app_at(app, &Stack::Main).settings.env[0].value,
            "main"
        );
    }

    #[test]
    fn monorepo_stacks_have_the_apps_their_worktree_has() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mono = root.join("mono");
        repo(&mono);
        for app in ["web", "old"] {
            std::fs::create_dir_all(mono.join("apps").join(app)).unwrap();
            std::fs::write(mono.join("apps").join(app).join("Procfile"), "web: x\n").unwrap();
        }
        git(&mono, &["add", "-A"]);
        git(&mono, &["commit", "-q", "-m", "apps"]);
        git(&mono, &["worktree", "add", "-q", "-b", "fx", "../mono-fx"]);
        let fx = root.join("mono-fx");
        std::fs::remove_dir_all(fx.join("apps/old")).unwrap();
        std::fs::create_dir_all(fx.join("apps/new")).unwrap();
        std::fs::write(fx.join("apps/new/Procfile"), "web: x\n").unwrap();
        repo(&root.join("auth"));
        std::fs::write(root.join("auth/Procfile"), "web: x\n").unwrap();

        let file = config::parse_project(
            &root.join("mono.toml"),
            &format!(
                "root = \"{}\"\ninclude = [\"mono/apps/*\", \"auth\"]\n",
                root.display()
            ),
        )
        .unwrap();
        let (state, problems) = discover_project(file);
        assert!(problems.is_empty(), "{problems:?}");
        let fx_stack = Stack::Branch("mono-fx".into());
        let path = |name: &str, stack: &Stack| {
            let app = state.project.app(name).unwrap();
            state.checkout(app, stack).map(|c| c.path().clone())
        };
        assert_eq!(path("web", &fx_stack), Some(fx.join("apps/web")));
        assert_eq!(path("new", &fx_stack), Some(fx.join("apps/new")));
        assert_eq!(path("new", &Stack::Main), None);
        assert_eq!(path("old", &fx_stack), None);
        assert_eq!(path("old", &Stack::Main), Some(mono.join("apps/old")));
        assert_eq!(path("auth", &fx_stack), Some(root.join("auth")));
        let new = state.project.app("new").unwrap();
        assert_eq!(
            state.not_in_stack(new, &Stack::Main),
            "new isn't in the main stack; it's in mono-fx"
        );

        let world = World {
            projects: vec![state],
            ..World::default()
        };
        let here = crate::address::locate(&world, &fx).unwrap();
        assert_eq!((here.app, here.stack), (None, fx_stack.clone()));
        let here = crate::address::locate(&world, &fx.join("apps/web/lib")).unwrap();
        assert_eq!(here.app.as_deref(), Some("web"));
        let target =
            crate::address::resolve(&world, &crate::address::Address::default(), Some(&fx))
                .unwrap();
        assert_eq!((target.app, target.stack), (None, Some(fx_stack)));
    }

    #[test]
    fn reports_stacks_named_like_an_app_or_main_or_shared_by_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        repo(&root.join("acme-web"));
        repo(&root.join("acme-api"));
        for (branch, path) in [
            ("x", "../wt/api/acme-web"),
            ("y", "../wt/Main/acme-web"),
            ("z1", "../one/feature"),
            ("z2", "../two/feature"),
        ] {
            git(
                &root.join("acme-web"),
                &["worktree", "add", "-q", "-b", branch, path],
            );
        }
        let file = config::parse_project(
            &root.join("acme.toml"),
            &format!(
                "root = \"{0}\"\ninclude = [\"acme-web\", \"acme-api\", \"missing\"]\nstrip_prefix = \"acme-\"\n",
                root.display()
            ),
        )
        .unwrap();
        let (state, problems) = discover_project(file);
        assert_eq!(state.project.apps.len(), 2);
        let found: Vec<_> = problems
            .iter()
            .map(|p| (p.app.as_deref(), p.stack.as_deref()))
            .collect();
        assert_eq!(
            found,
            [
                (None, Some("main")),
                (None, Some("feature")),
                (None, None),
                (Some("acme-api"), Some("api")),
            ],
            "{problems:?}"
        );
        assert!(
            problems[0]
                .message
                .contains("its stack would be named main"),
            "{}",
            problems[0].message
        );
        assert!(
            problems[1]
                .message
                .contains("have the same stack name (feature)"),
            "{}",
            problems[1].message
        );
        assert!(problems[2].message.contains("\"missing\""));
        assert!(problems[3].message.contains("same name as app acme-api"));
        assert_eq!(
            state.stacks().into_iter().collect::<Vec<_>>(),
            vec!["api", "feature"]
        );
    }
}
