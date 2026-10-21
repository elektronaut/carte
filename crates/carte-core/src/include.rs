//! Expanding a project's `include` entries and named app paths into apps: in the
//! main checkout of each repo they reach, in each of its worktrees, and outside git.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use glob::{MatchOptions, Pattern};

use crate::config::{self, Naming};
use crate::discovery::{expand_home, is_primary_checkout, list_worktrees};
use crate::model::Worktree;

/// An app found in at least one checkout.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub name: String,
    pub naming: Naming,
    /// Where the app is in the main checkout, whether or not it's there.
    pub path: PathBuf,
    pub repo: Option<PathBuf>,
    pub in_main: bool,
    pub worktrees: Vec<Worktree>,
}

#[derive(Debug, Default)]
pub struct Expanded {
    pub apps: Vec<Found>,
    pub repo_worktrees: BTreeMap<PathBuf, BTreeMap<String, PathBuf>>,
    pub scan_dirs: BTreeSet<PathBuf>,
    pub problems: Vec<Issue>,
}

/// A problem, with the app and stack it concerns.
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub app: Option<String>,
    pub stack: Option<String>,
    pub message: String,
}

impl Issue {
    fn new(message: String) -> Issue {
        Issue {
            app: None,
            stack: None,
            message,
        }
    }
}

struct Entry {
    text: String,
    negate: bool,
    pattern: PathBuf,
    matcher: Pattern,
    wildcard: bool,
    /// The app's name, for an `[apps.X] path`.
    name: Option<String>,
}

/// A directory found in a checkout.
struct Candidate {
    /// Where it is in the main checkout.
    path: PathBuf,
    dir: PathBuf,
    repo: Option<PathBuf>,
    worktree: Option<Worktree>,
    /// The entries of the repo's `carte.toml` that found it, when it names its apps.
    declared: Option<Rc<Vec<Entry>>>,
}

const MATCH: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: true,
};

/// Expands `include` (or the implied `"."`) and the named paths of a project.
pub fn expand(
    project: &str,
    root: Option<&str>,
    include: &[String],
    named: &[(String, String)],
) -> Expanded {
    let mut out = Expanded::default();
    let root = root.map(physical_root);
    let implied = include.is_empty() && named.is_empty();
    if implied && root.is_none() {
        out.problems.push(Issue::new(
            "the project has no apps: set `root`, `include` or an app's `path`".into(),
        ));
        return out;
    }
    if implied
        && let Some(root) = &root
        && !root.is_dir()
    {
        out.problems
            .push(Issue::new(format!("root {} doesn't exist", root.display())));
        return out;
    }
    let texts: Vec<(String, Option<String>)> = if implied {
        vec![(".".into(), None)]
    } else {
        include
            .iter()
            .map(|t| (t.clone(), None))
            .chain(
                named
                    .iter()
                    .map(|(name, path)| (path.clone(), Some(name.clone()))),
            )
            .collect()
    };
    let mut entries = Vec::new();
    for (text, name) in texts {
        match entry(&text, name, root.as_deref()) {
            Ok(entry) => entries.push(entry),
            Err(message) => out.problems.push(Issue::new(message)),
        }
    }

    let mut repos: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut candidates = Vec::new();
    let mut declaring = BTreeSet::new();
    for entry in entries.iter().filter(|e| !e.negate) {
        let components: Vec<Component> = entry.pattern.components().collect();
        for depth in 1..=components.len() {
            let prefix: PathBuf = components[..depth].iter().collect();
            for dir in dirs(&prefix) {
                if is_primary_checkout(&dir) {
                    let rest: PathBuf = components[depth..].iter().collect();
                    repos.entry(dir).or_default().insert(rest);
                }
            }
        }
        for dir in dirs(&entry.pattern) {
            if git_root(&dir).is_none() {
                candidates.push(Candidate {
                    path: dir.clone(),
                    dir,
                    repo: None,
                    worktree: None,
                    declared: None,
                });
            }
        }
        if entry.wildcard {
            out.scan_dirs.extend(scan_dir(&entry.pattern));
        }
    }

    for (repo, patterns) in repos {
        let worktrees = match list_worktrees(&repo) {
            Ok(worktrees) => usable_worktrees(worktrees, &mut out.problems),
            Err(err) => {
                out.problems.push(Issue::new(err.to_string()));
                Vec::new()
            }
        };
        out.repo_worktrees.insert(
            repo.clone(),
            worktrees
                .iter()
                .map(|wt| (wt.slug.clone(), wt.path.clone()))
                .collect(),
        );
        let checkouts = std::iter::once((repo.clone(), None))
            .chain(worktrees.into_iter().map(|wt| (wt.path.clone(), Some(wt))));
        for (checkout, worktree) in checkouts {
            for pattern in &patterns {
                if pattern.as_os_str().is_empty()
                    && let Some(declared) = declared_entries(&checkout, &repo, &mut out.problems)
                {
                    let itself = Candidate {
                        path: repo.clone(),
                        dir: checkout.clone(),
                        repo: Some(repo.clone()),
                        worktree: worktree.clone(),
                        declared: None,
                    };
                    if selects(&entries, &itself) {
                        declaring.insert(repo.clone());
                        candidates.extend(declared_candidates(
                            &declared,
                            &itself,
                            &mut out.scan_dirs,
                        ));
                    }
                    continue;
                }
                let pattern = normalize(&checkout.join(pattern));
                if worktree.is_some()
                    && pattern
                        .components()
                        .any(|c| has_wildcard(&c.as_os_str().to_string_lossy()))
                {
                    out.scan_dirs.extend(scan_dir(&pattern));
                }
                for dir in dirs(&pattern) {
                    if nested_git(&checkout, &dir) {
                        continue;
                    }
                    let Ok(inside) = dir.strip_prefix(&checkout) else {
                        continue;
                    };
                    candidates.push(Candidate {
                        path: normalize(&repo.join(inside)),
                        dir,
                        repo: Some(repo.clone()),
                        worktree: worktree.clone(),
                        declared: None,
                    });
                }
            }
        }
    }

    let mut found: BTreeMap<PathBuf, Found> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        let checkout = candidate.worktree.as_ref().map(|wt| wt.slug.clone());
        if !seen.insert((candidate.path.clone(), checkout)) {
            continue;
        }
        let decided_by = candidate.declared.as_deref().unwrap_or(&entries);
        let Some((name, naming)) = decide(decided_by, &candidate, project, root.as_deref()) else {
            continue;
        };
        let app = found
            .entry(candidate.path.clone())
            .or_insert_with(|| Found {
                name,
                naming,
                path: candidate.path.clone(),
                repo: candidate.repo.clone(),
                in_main: false,
                worktrees: Vec::new(),
            });
        match candidate.worktree {
            None => app.in_main = true,
            Some(wt) => app.worktrees.push(Worktree {
                path: candidate.dir,
                ..wt
            }),
        }
    }

    for entry in &entries {
        if entry.negate {
            continue;
        }
        let matched = found
            .keys()
            .chain(&declaring)
            .any(|path| match &entry.name {
                Some(_) => *path == entry.pattern,
                None => entry.matcher.matches_path_with(path, MATCH),
            });
        if !matched {
            out.problems.push(Issue::new(match &entry.name {
                Some(name) => format!("[apps.{name}] path {:?} is not in any checkout", entry.text),
                None if entry.wildcard => {
                    format!("include entry {:?} matches no apps", entry.text)
                }
                None => format!("include entry {:?} matches nothing", entry.text),
            }));
        }
    }

    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for app in found.into_values() {
        if let Some(other) = by_name.get(&app.name) {
            out.problems.push(Issue {
                app: Some(app.name.clone()),
                stack: None,
                message: format!(
                    "{} is skipped: {} is also named {}; name one of them with [apps.<name>] path",
                    app.path.display(),
                    other.display(),
                    app.name
                ),
            });
            continue;
        }
        by_name.insert(app.name.clone(), app.path.clone());
        out.apps.push(app);
    }
    out
}

/// The directories a template's `projects` entries match, each with whether a
/// wildcard picked it, and the directories new ones appear in. Linked worktrees
/// are skipped.
pub fn expand_projects(
    root: Option<&str>,
    projects: &[String],
) -> (Vec<(PathBuf, bool)>, BTreeSet<PathBuf>, Vec<String>) {
    let root = root.map(physical_root);
    let mut problems = Vec::new();
    let mut entries = Vec::new();
    for text in projects {
        match entry(text, None, root.as_deref()) {
            Ok(entry) => entries.push(entry),
            Err(message) => problems.push(message.replacen("include entry", "projects entry", 1)),
        }
    }
    let mut scans = BTreeSet::new();
    let mut found: Vec<PathBuf> = Vec::new();
    for entry in entries.iter().filter(|e| !e.negate) {
        let matched = dirs(&entry.pattern);
        if matched.is_empty() {
            problems.push(format!("projects entry {:?} matches nothing", entry.text));
        }
        found.extend(matched);
        if entry.wildcard {
            scans.extend(scan_dir(&entry.pattern));
        }
    }
    found.sort();
    found.dedup();
    let picked = found
        .into_iter()
        .filter(|dir| !(dir.join(".git").exists() && !is_primary_checkout(dir)))
        .filter_map(|dir| {
            let last = entries
                .iter()
                .rev()
                .find(|e| e.matcher.matches_path_with(&dir, MATCH))?;
            (!last.negate).then_some((dir, last.wildcard))
        })
        .collect();
    (picked, scans, problems)
}

/// The entries of a checkout's `carte.toml` when it names the apps inside its repo,
/// resolved against the repo's main checkout.
fn declared_entries(
    checkout: &Path,
    repo: &Path,
    problems: &mut Vec<Issue>,
) -> Option<Rc<Vec<Entry>>> {
    let config = match config::load_repo_config(checkout) {
        Ok(Some(config)) if config.declares_apps() => config,
        Ok(_) => return None,
        Err(err) => {
            problems.push(Issue::new(err.to_string()));
            return None;
        }
    };
    let texts = config.include.iter().map(|t| (t.clone(), None)).chain(
        config
            .apps
            .iter()
            .filter_map(|(name, app)| Some((app.path.clone()?, Some(name.clone())))),
    );
    let mut entries = Vec::new();
    for (text, name) in texts {
        match entry(&text, name, Some(repo)) {
            Ok(entry) => entries.push(entry),
            Err(message) => problems.push(Issue::new(format!(
                "{}: {message}",
                checkout.join(config::REPO_CONFIG).display()
            ))),
        }
    }
    Some(Rc::new(entries))
}

/// The directories a repo's own entries find in one of its checkouts.
fn declared_candidates(
    declared: &Rc<Vec<Entry>>,
    itself: &Candidate,
    scan_dirs: &mut BTreeSet<PathBuf>,
) -> Vec<Candidate> {
    let mut found = Vec::new();
    for entry in declared.iter().filter(|e| !e.negate) {
        let Ok(inside) = entry.pattern.strip_prefix(&itself.path) else {
            continue;
        };
        let pattern = normalize(&itself.dir.join(inside));
        if entry.wildcard {
            scan_dirs.extend(scan_dir(&pattern));
        }
        for dir in dirs(&pattern) {
            if nested_git(&itself.dir, &dir) {
                continue;
            }
            let Ok(inside) = dir.strip_prefix(&itself.dir) else {
                continue;
            };
            found.push(Candidate {
                path: normalize(&itself.path.join(inside)),
                dir,
                repo: itself.repo.clone(),
                worktree: itself.worktree.clone(),
                declared: Some(declared.clone()),
            });
        }
    }
    found
}

/// Whether the project's entries pick a directory, whatever it has to run.
fn selects(entries: &[Entry], candidate: &Candidate) -> bool {
    entries
        .iter()
        .any(|e| e.name.is_some() && e.pattern == candidate.path)
        || entries
            .iter()
            .rev()
            .filter(|e| e.name.is_none())
            .find(|e| e.matcher.matches_path_with(&candidate.path, MATCH))
            .is_some_and(|e| !e.negate)
}

fn entry(text: &str, name: Option<String>, root: Option<&Path>) -> Result<Entry, String> {
    let (negate, body) = match text.strip_prefix('!') {
        Some(rest) if name.is_none() => (true, rest),
        _ => (false, text),
    };
    let what = || match &name {
        Some(name) => format!("[apps.{name}] path {text:?}"),
        None => format!("include entry {text:?}"),
    };
    if body.contains("**") {
        return Err(format!("{}: `**` is not supported", what()));
    }
    let path = expand_home(body);
    let pattern = if path.is_absolute() {
        normalize(&path)
    } else {
        match root {
            Some(root) => normalize(&root.join(path)),
            None => {
                return Err(format!(
                    "{} is relative, but the project has no `root`",
                    what()
                ));
            }
        }
    };
    let wildcard = pattern
        .components()
        .any(|c| has_wildcard(&c.as_os_str().to_string_lossy()));
    if wildcard && name.is_some() {
        return Err(format!("{} can't be a pattern", what()));
    }
    let matcher = Pattern::new(&pattern.to_string_lossy())
        .map_err(|err| format!("{} is not a valid glob: {err}", what()))?;
    Ok(Entry {
        text: text.to_string(),
        negate,
        pattern,
        matcher,
        wildcard,
        name,
    })
}

/// The name of a directory found in a checkout, or `None` when it isn't an app there:
/// a named path wins, then the last entry matching it decides, and a wildcard match
/// must be runnable.
fn decide(
    entries: &[Entry],
    candidate: &Candidate,
    project: &str,
    root: Option<&Path>,
) -> Option<(String, Naming)> {
    if let Some(entry) = entries
        .iter()
        .find(|e| e.name.is_some() && e.pattern == candidate.path)
    {
        return entry.name.clone().map(|name| (name, Naming::Named));
    }
    let last = entries
        .iter()
        .rev()
        .filter(|e| e.name.is_none())
        .find(|e| e.matcher.matches_path_with(&candidate.path, MATCH))?;
    if last.negate || (last.wildcard && !is_runnable(&candidate.dir)) {
        return None;
    }
    if root == Some(candidate.path.as_path()) {
        return Some((project.to_string(), Naming::Root));
    }
    let name = candidate.path.file_name()?.to_string_lossy().into_owned();
    Some((name, Naming::Directory))
}

/// Whether a directory has something to run of its own: a Procfile.dev, Procfile,
/// config.ru, a `package.json` with a `dev` script, or a `carte.toml` with processes
/// or `serve_files`.
pub fn is_runnable(dir: &Path) -> bool {
    ["Procfile.dev", "Procfile", "config.ru"]
        .iter()
        .any(|file| dir.join(file).is_file())
        || crate::node::dev_script(dir).is_some()
        || config::load_repo_config(dir)
            .ok()
            .flatten()
            .is_some_and(|c| {
                c.processes.values().any(|p| p.command.is_some()) || c.serve_files.is_some()
            })
}

/// The worktrees that can have stacks, with problems for those that can't.
fn usable_worktrees(worktrees: Vec<Worktree>, problems: &mut Vec<Issue>) -> Vec<Worktree> {
    let (on_main, worktrees): (Vec<Worktree>, Vec<Worktree>) =
        worktrees.into_iter().partition(|wt| wt.slug == "main");
    for wt in on_main {
        problems.push(Issue {
            app: None,
            stack: Some("main".into()),
            message: format!(
                "worktree {} is skipped: its stack would be named main, like the main checkout's; rename its folder",
                wt.path.display()
            ),
        });
    }
    let mut usable: Vec<Worktree> = Vec::new();
    for wt in worktrees {
        if let Some(other) = usable.iter().find(|o| o.slug == wt.slug) {
            problems.push(Issue {
                app: None,
                stack: Some(wt.slug.clone()),
                message: format!(
                    "worktrees {} and {} have the same stack name ({}); rename one of their folders",
                    other.path.display(),
                    wt.path.display(),
                    wt.slug
                ),
            });
        }
        usable.push(wt);
    }
    usable
}

/// The directories a path or glob matches, dot-directories only when named.
fn dirs(pattern: &Path) -> Vec<PathBuf> {
    let text = pattern.to_string_lossy();
    if !has_wildcard(&text) {
        return if pattern.is_dir() {
            vec![pattern.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let Ok(paths) = glob::glob_with(&text, MATCH) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = paths
        .filter_map(Result::ok)
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// The deepest existing directory above a pattern's first wildcard, where new
/// matches appear.
fn scan_dir(pattern: &Path) -> Option<PathBuf> {
    let literal: PathBuf = pattern
        .components()
        .take_while(|c| !has_wildcard(&c.as_os_str().to_string_lossy()))
        .collect();
    literal.is_dir().then_some(literal)
}

fn has_wildcard(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

/// The nearest directory at or above `dir` with a `.git`.
fn git_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Whether `dir` is in another repo or worktree inside `checkout`.
fn nested_git(checkout: &Path, dir: &Path) -> bool {
    dir.ancestors()
        .take_while(|d| *d != checkout)
        .any(|d| d.join(".git").exists())
}

/// Removes `.` and resolves `..` without touching the disk.
/// A project's `root` with symlinks resolved, as git and the shell's `pwd -P`
/// give paths, so a checkout under it is found from either.
fn physical_root(root: &str) -> PathBuf {
    let root = normalize(&expand_home(root));
    root.canonicalize().unwrap_or(root)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
    }

    fn commit_all(dir: &Path) {
        git(dir, &["add", "-A"]);
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "apps",
            ],
        );
    }

    fn app(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("Procfile"), "web: run\n").unwrap();
    }

    fn summary(expanded: &Expanded) -> Vec<(String, bool, Vec<String>)> {
        expanded
            .apps
            .iter()
            .map(|a| {
                (
                    a.name.clone(),
                    a.in_main,
                    a.worktrees.iter().map(|wt| wt.slug.clone()).collect(),
                )
            })
            .collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_root_alone_is_one_app_named_after_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let shop = tmp.join("shop-rails");
        repo(&shop);
        let out = expand("shop", Some(&shop.to_string_lossy()), &[], &[]);
        assert_eq!(summary(&out), [("shop".into(), true, vec![])]);
        assert_eq!(out.apps[0].naming, Naming::Root);
        assert!(out.problems.is_empty(), "{:?}", out.problems);
    }

    #[test]
    fn separate_repos_are_apps_in_each_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let work = tmp;
        for name in ["acme-web", "acme-api"] {
            repo(&work.join(name));
            app(&work.join(name));
            commit_all(&work.join(name));
        }
        git(
            &work.join("acme-web"),
            &["worktree", "add", "-q", "-b", "fx", "../fx/acme-web"],
        );
        let out = expand(
            "acme",
            Some(&work.to_string_lossy()),
            &strings(&["acme-*"]),
            &[],
        );
        assert_eq!(
            summary(&out),
            [
                ("acme-api".into(), true, vec![]),
                ("acme-web".into(), true, vec!["fx".into()]),
            ]
        );
        let web = &out.apps[1];
        assert_eq!(web.worktrees[0].path, work.join("fx/acme-web"));
        assert_eq!(
            out.repo_worktrees[&work.join("acme-web")],
            BTreeMap::from([("fx".into(), work.join("fx/acme-web"))])
        );
    }

    #[test]
    fn monorepo_apps_are_found_per_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let mono = tmp.join("mono");
        repo(&mono);
        app(&mono.join("apps/web"));
        app(&mono.join("apps/old"));
        fs::create_dir_all(mono.join("apps/ui-kit/src")).unwrap();
        fs::write(mono.join("apps/ui-kit/src/x"), "").unwrap();
        commit_all(&mono);
        git(&mono, &["worktree", "add", "-q", "-b", "fx", "../mono-fx"]);
        let fx = tmp.join("mono-fx");
        app(&fx.join("apps/new"));
        fs::remove_dir_all(fx.join("apps/old")).unwrap();

        let out = expand(
            "mono",
            Some(&mono.to_string_lossy()),
            &strings(&["apps/*"]),
            &[],
        );
        assert_eq!(
            summary(&out),
            [
                ("new".into(), false, vec!["mono-fx".into()]),
                ("old".into(), true, vec![]),
                ("web".into(), true, vec!["mono-fx".into()]),
            ]
        );
        assert_eq!(out.apps[0].path, mono.join("apps/new"));
        assert_eq!(out.apps[0].worktrees[0].path, fx.join("apps/new"));
        assert!(out.scan_dirs.contains(&mono.join("apps")));
        assert!(out.scan_dirs.contains(&fx.join("apps")));
        assert!(out.problems.is_empty(), "{:?}", out.problems);
    }

    #[test]
    fn a_repo_can_name_its_own_apps_per_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let mono = tmp.join("mono");
        repo(&mono);
        app(&mono.join("apps/web"));
        app(&mono.join("apps/docs"));
        app(&mono.join("services/api"));
        fs::write(
            mono.join("carte.toml"),
            "include = [\"apps/*\", \"!apps/docs\"]\n\n[apps.backend]\npath = \"services/api\"\n",
        )
        .unwrap();
        commit_all(&mono);
        git(&mono, &["worktree", "add", "-q", "-b", "fx", "../mono-fx"]);
        let fx = tmp.join("mono-fx");
        fs::write(fx.join("carte.toml"), "include = \"apps/*\"\n").unwrap();

        let out = expand("mono", Some(&mono.to_string_lossy()), &[], &[]);
        assert!(out.problems.is_empty(), "{:?}", out.problems);
        assert_eq!(
            summary(&out),
            [
                ("docs".into(), false, vec!["mono-fx".into()]),
                ("web".into(), true, vec!["mono-fx".into()]),
                ("backend".into(), true, vec![]),
            ]
        );
        assert_eq!(out.apps[2].naming, Naming::Named);
        assert!(out.scan_dirs.contains(&fx.join("apps")));
    }

    #[test]
    fn negations_apply_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let mono = tmp.join("mono");
        repo(&mono);
        for name in ["web", "docs", "test-a", "test-kit"] {
            app(&mono.join("apps").join(name));
        }
        let out = expand(
            "mono",
            Some(&mono.to_string_lossy()),
            &strings(&["apps/*", "!apps/docs", "!apps/test-*", "apps/test-kit"]),
            &[],
        );
        let names: Vec<&str> = out.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["test-kit", "web"]);
    }

    #[test]
    fn named_paths_win_and_literal_entries_need_not_be_runnable() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let work = tmp;
        repo(&work.join("acme-billing"));
        repo(&work.join("acme-web"));
        let out = expand(
            "acme",
            Some(&work.to_string_lossy()),
            &strings(&["acme-web"]),
            &[("billing".into(), "acme-billing".into())],
        );
        let names: Vec<(&str, Naming)> = out
            .apps
            .iter()
            .map(|a| (a.name.as_str(), a.naming))
            .collect();
        assert_eq!(
            names,
            [("billing", Naming::Named), ("acme-web", Naming::Directory)]
        );
    }

    #[test]
    fn wildcard_matches_must_be_runnable() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let work = tmp;
        repo(&work.join("acme-web"));
        app(&work.join("acme-web"));
        repo(&work.join("acme-gem"));
        let out = expand(
            "acme",
            Some(&work.to_string_lossy()),
            &strings(&["acme-*"]),
            &[],
        );
        let names: Vec<&str> = out.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["acme-web"]);
    }

    #[test]
    fn directories_outside_git_are_apps_without_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let sites = tmp.join("sites");
        app(&sites.join("docs"));
        fs::create_dir_all(sites.join("empty")).unwrap();
        let out = expand(
            "sites",
            Some(&sites.to_string_lossy()),
            &strings(&["*"]),
            &[],
        );
        assert_eq!(summary(&out), [("docs".into(), true, vec![])]);
        assert_eq!(out.apps[0].repo, None);
    }

    #[test]
    fn linked_worktrees_matched_by_a_glob_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let work = tmp;
        repo(&work.join("acme-web"));
        app(&work.join("acme-web"));
        commit_all(&work.join("acme-web"));
        git(
            &work.join("acme-web"),
            &["worktree", "add", "-q", "-b", "fx", "../acme-web-fx"],
        );
        let out = expand(
            "acme",
            Some(&work.to_string_lossy()),
            &strings(&["acme-*"]),
            &[],
        );
        assert_eq!(
            summary(&out),
            [("acme-web".into(), true, vec!["acme-web-fx".into()])]
        );
    }

    #[test]
    fn same_names_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let mono = tmp.join("mono");
        repo(&mono);
        app(&mono.join("apps/web"));
        app(&mono.join("services/web"));
        let out = expand(
            "mono",
            Some(&mono.to_string_lossy()),
            &strings(&["apps/*", "services/*"]),
            &[],
        );
        assert_eq!(out.apps.len(), 1);
        assert!(
            out.problems[0].message.contains("also named web"),
            "{:?}",
            out.problems
        );
    }

    #[test]
    fn entries_are_checked() {
        let out = expand("acme", None, &strings(&["apps/*"]), &[]);
        assert!(out.problems[0].message.contains("no `root`"));
        let out = expand("acme", None, &[], &[]);
        assert!(out.problems[0].message.contains("has no apps"));
        let out = expand("acme", Some("/tmp"), &strings(&["a/**/b"]), &[]);
        assert!(out.problems[0].message.contains("`**`"));
        let out = expand("acme", Some("/tmp"), &[], &[("x".into(), "a/*".into())]);
        assert!(out.problems[0].message.contains("can't be a pattern"));
    }
}
