use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, anyhow, bail};
use carte_api::{AppInfo, ProjectList, ProjectSummary};
use carte_core::model::{MachineConfig, Problem, ProjectState, Stack};
use carte_core::procfile::{self, ProcessSource, ProcfileError};
use carte_core::{config, discovery, naming, slug};
use toml_edit::{DocumentMut, Item, Table, value};

use super::ProjectCommand;
use super::prompt::{self, tilde};
use crate::paths::Paths;

pub fn run(paths: &Paths, command: ProjectCommand, json: bool) -> anyhow::Result<ExitCode> {
    match command {
        ProjectCommand::Add {
            path,
            name,
            strip_prefix,
            command,
            yes,
        } => add(
            paths,
            AddArgs {
                path,
                name,
                strip_prefix,
                command,
                yes,
            },
        ),
        ProjectCommand::Ls => ls(paths, json),
        ProjectCommand::Edit { name } => edit(paths, &name),
        ProjectCommand::Rm { name, yes } => rm(paths, &name, yes),
    }
}

struct AddArgs {
    path: Option<PathBuf>,
    name: Option<String>,
    strip_prefix: Option<String>,
    command: Option<String>,
    yes: bool,
}

fn add(paths: &Paths, args: AddArgs) -> anyhow::Result<ExitCode> {
    let interactive = prompt::interactive(args.yes);
    let dir = match &args.path {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    let dir = dir
        .canonicalize()
        .with_context(|| format!("{}", dir.display()))?;
    let machine = config::load_machine_config(&paths.machine_config())?;
    let dir_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let single = discovery::is_primary_checkout(&dir);
    let (repos, strip_prefix, repo_names) = if single {
        (
            tilde(&dir),
            args.strip_prefix.clone(),
            vec![dir_name.clone()],
        )
    } else if dir.join(".git").exists() {
        bail!(
            "{} is a linked worktree; add the repo's primary checkout instead",
            tilde(&dir)
        );
    } else {
        repos_in_dir(&dir, args.strip_prefix.clone(), interactive)?
    };

    let name = choose_name(args.name.as_deref(), &dir_name, interactive)?;
    let file_path = paths.project_file(&name);
    if file_path.exists() {
        bail!(
            "project {name} already exists ({}); change it with `carte project edit {name}`",
            tilde(&file_path)
        );
    }

    let mut entries = name_overrides(&repo_names, strip_prefix.as_deref(), &name, single);
    let mut text = project_toml(&repos, strip_prefix.as_deref(), &entries);
    let (mut state, mut problems) = discover(&file_path, &text)?;

    let unrunnable: Vec<String> = state
        .project
        .apps
        .iter()
        .filter(|app| {
            matches!(
                procfile::load(&app.path, &app.settings),
                Err(ProcfileError::NotRunnable { .. })
            )
        })
        .map(|app| app.name.clone())
        .collect();
    if !unrunnable.is_empty() {
        if let Some(command) = &args.command {
            for app in &unrunnable {
                entries.entry(app.clone()).or_default().web_command = Some(command.clone());
            }
        } else if interactive {
            for app in &unrunnable {
                println!("{app} has no Procfile.dev, Procfile or config.ru.");
                let command = prompt::ask(
                    &format!("Web command for {app} (empty to leave it unrunnable)"),
                    None,
                )?;
                if !command.is_empty() {
                    entries.entry(app.clone()).or_default().web_command = Some(command);
                }
            }
        } else {
            bail!(
                "no Procfile.dev, Procfile or config.ru in {}; pass --command \"<web command>\" \
                 to run it as their `web` process",
                unrunnable.join(", ")
            );
        }
        if entries.values().any(|e| e.web_command.is_some()) {
            text = project_toml(&repos, strip_prefix.as_deref(), &entries);
            (state, problems) = discover(&file_path, &text)?;
        }
    }

    print_summary(&machine, &state, &problems, &file_path, &text);
    if state.project.apps.is_empty() {
        bail!("the project would have no apps");
    }
    if interactive && !prompt::confirm(&format!("Write {}?", tilde(&file_path)), true)? {
        println!("Nothing written.");
        return Ok(ExitCode::SUCCESS);
    }

    fs::create_dir_all(paths.projects_dir())?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file_path)
        .with_context(|| format!("{}", file_path.display()))?;
    file.write_all(text.as_bytes())?;
    println!("Wrote {}", tilde(&file_path));
    Ok(ExitCode::SUCCESS)
}

/// Lists the repos in `dir` and settles the `repos` glob and `strip_prefix`;
/// also returns the names of the repos the glob matches.
fn repos_in_dir(
    dir: &Path,
    strip_prefix: Option<String>,
    interactive: bool,
) -> anyhow::Result<(String, Option<String>, Vec<String>)> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .with_context(|| format!("{}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && discovery::is_primary_checkout(p))
        .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    if names.is_empty() {
        bail!(
            "{} is not a git repo and has no git repos in it",
            tilde(dir)
        );
    }
    println!(
        "Found {} git repos in {}: {}",
        names.len(),
        tilde(dir),
        names.join(", ")
    );

    let prefix = match strip_prefix {
        Some(prefix) => Some(prefix),
        None => {
            let suggested = suggest_prefix(&names);
            if interactive {
                let answer = prompt::ask(
                    "Prefix to strip from app names (- for none)",
                    Some(suggested.as_deref().unwrap_or("-")),
                )?;
                Some(answer).filter(|a| a != "-")
            } else {
                suggested
            }
        }
    }
    .filter(|p| !p.is_empty());

    if let Some(prefix) = &prefix {
        let left_out: Vec<&str> = names
            .iter()
            .filter(|n| !n.starts_with(prefix.as_str()))
            .map(String::as_str)
            .collect();
        if !left_out.is_empty() {
            println!("Left out (no {prefix:?} prefix): {}", left_out.join(", "));
        }
        names.retain(|n| n.starts_with(prefix.as_str()));
    }
    let pattern = dir.join(format!("{}*", prefix.as_deref().unwrap_or("")));
    Ok((tilde(&pattern), prefix, names))
}

/// The `xxx-` prefix shared by the most repos (more than half of them), the longest on a
/// tie, as long as every name it shortens stays a valid hostname label.
fn suggest_prefix(names: &[String]) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for name in names {
        for (i, _) in name.match_indices('-').filter(|(i, _)| *i > 0) {
            let prefix = &name[..=i];
            let matching: Vec<&str> = names
                .iter()
                .filter_map(|n| n.strip_prefix(prefix))
                .collect();
            let count = matching.len();
            if count < 2 || count * 2 <= names.len() {
                continue;
            }
            if matching
                .iter()
                .any(|short| slug::validate_app_short_name(short).is_err())
            {
                continue;
            }
            if best.is_none_or(|(c, p)| (count, prefix.len()) > (c, p.len())) {
                best = Some((count, prefix));
            }
        }
    }
    best.map(|(_, prefix)| prefix.to_string())
}

fn choose_name(given: Option<&str>, dir_name: &str, interactive: bool) -> anyhow::Result<String> {
    if let Some(name) = given {
        slug::validate_project_name(name)?;
        return Ok(name.to_string());
    }
    let suggestion = Some(dir_name.to_string())
        .filter(|n| slug::validate_project_name(n).is_ok())
        .or_else(|| {
            let fixed = slug::branch_slug(dir_name);
            slug::validate_project_name(&fixed).is_ok().then_some(fixed)
        });
    if !interactive {
        return match (slug::validate_project_name(dir_name), suggestion) {
            (Ok(()), _) => Ok(dir_name.to_string()),
            (Err(err), Some(fixed)) => Err(anyhow!("{err}; pass --name {fixed}")),
            (Err(err), None) => Err(anyhow!("{err}; pass --name <name>")),
        };
    }
    loop {
        let name = prompt::ask("Project name", suggestion.as_deref())?;
        match slug::validate_project_name(&name) {
            Ok(()) => return Ok(name),
            Err(err) => println!("{err}"),
        }
    }
}

/// What `add` writes into an app's table.
#[derive(Default)]
struct AppEntry {
    /// A short-name override for a directory name that isn't a valid hostname label.
    name: Option<String>,
    web_command: Option<String>,
}

fn project_toml(
    repos: &str,
    strip_prefix: Option<&str>,
    entries: &BTreeMap<String, AppEntry>,
) -> String {
    let mut doc = DocumentMut::new();
    doc["repos"] = value(repos);
    if let Some(prefix) = strip_prefix {
        doc["strip_prefix"] = value(prefix);
    }
    if !entries.is_empty() {
        let mut apps = Table::new();
        apps.set_implicit(true);
        for (app, entry) in entries {
            let mut app_table = Table::new();
            app_table.set_implicit(true);
            if let Some(name) = &entry.name {
                app_table.set_implicit(false);
                app_table.decor_mut().set_prefix("\n");
                app_table["name"] = value(name.as_str());
            }
            if let Some(command) = &entry.web_command {
                let mut web = Table::new();
                web["command"] = value(command.as_str());
                web.decor_mut().set_prefix("\n");
                let mut processes = Table::new();
                processes.set_implicit(true);
                processes.insert("web", Item::Table(web));
                app_table.insert("processes", Item::Table(processes));
            }
            apps.insert(app, Item::Table(app_table));
        }
        doc.insert("apps", Item::Table(apps));
    }
    doc.to_string()
}

/// Short-name overrides for repos whose name, after the prefix, isn't a valid label.
fn name_overrides(
    repos: &[String],
    strip_prefix: Option<&str>,
    project: &str,
    single: bool,
) -> BTreeMap<String, AppEntry> {
    repos
        .iter()
        .filter_map(|repo| {
            let short = strip_prefix
                .and_then(|p| repo.strip_prefix(p))
                .filter(|s| !s.is_empty())
                .unwrap_or(repo);
            if slug::validate_app_short_name(short).is_ok() {
                return None;
            }
            let fixed = if single {
                project.to_string()
            } else {
                slug::branch_slug(short)
            };
            slug::validate_app_short_name(&fixed).ok()?;
            let entry = AppEntry {
                name: Some(fixed),
                web_command: None,
            };
            Some((repo.clone(), entry))
        })
        .collect()
}

fn discover(path: &Path, text: &str) -> anyhow::Result<(ProjectState, Vec<Problem>)> {
    let file = config::parse_project(path, text)?;
    Ok(discovery::discover_project(file))
}

fn print_summary(
    machine: &MachineConfig,
    state: &ProjectState,
    problems: &[Problem],
    path: &Path,
    text: &str,
) {
    println!();
    println!("{}:", tilde(path));
    for line in text.lines() {
        println!("  {line}");
    }
    println!();

    let project = &state.project;
    let width = project.apps.iter().map(|a| a.name.len()).max().unwrap_or(0);
    let url_width = project
        .apps
        .iter()
        .map(|a| naming::instance_url(machine, project, a, &Stack::Main).len())
        .max()
        .unwrap_or(0);
    let mut notices: Vec<(String, Vec<&str>)> = Vec::new();
    for app in &project.apps {
        let url = naming::instance_url(machine, project, app, &Stack::Main);
        let processes = match procfile::load(&app.path, &app.settings) {
            Ok(list) => {
                for notice in list.notices.iter().map(ToString::to_string) {
                    match notices.iter_mut().find(|(n, _)| *n == notice) {
                        Some((_, apps)) => apps.push(&app.name),
                        None => notices.push((notice, vec![&app.name])),
                    }
                }
                let names: Vec<&str> = list.processes.iter().map(|p| p.name.as_str()).collect();
                format!("{} ({})", names.join(", "), source_name(list.source))
            }
            Err(err) => format!("not runnable: {err}"),
        };
        println!("  {:width$}  {url:url_width$}  {processes}", app.name);
    }
    if !notices.is_empty() {
        println!();
        for (notice, apps) in notices {
            println!("  note: {notice}");
            println!("        in {}", apps.join(", "));
        }
    }
    if !problems.is_empty() {
        println!();
        for problem in problems {
            println!("  problem: {}", problem_text(problem));
        }
    }
    println!();
}

fn source_name(source: ProcessSource) -> &'static str {
    match source {
        ProcessSource::ProcfileDev => "Procfile.dev",
        ProcessSource::Procfile => "Procfile",
        ProcessSource::ConfigRu => "config.ru",
        ProcessSource::Config => "config",
        ProcessSource::RouteOnly => "fixed port",
    }
}

fn problem_text(problem: &Problem) -> String {
    match &problem.app {
        Some(app) => format!("{app}: {}", problem.message),
        None => problem.message.clone(),
    }
}

fn api_problem(problem: &Problem) -> carte_api::Problem {
    carte_api::Problem {
        project: problem.project.clone(),
        app: problem.app.clone(),
        message: problem.message.clone(),
    }
}

fn project_list(paths: &Paths) -> ProjectList {
    let world = discovery::load_world(&paths.config_dir);
    let mut projects: Vec<ProjectSummary> = world
        .projects
        .iter()
        .map(|state| ProjectSummary {
            name: state.project.name.clone(),
            config_path: state.project.config_path.clone(),
            apps: state
                .project
                .apps
                .iter()
                .map(|app| AppInfo {
                    name: app.name.clone(),
                    short: app.short.clone(),
                    path: app.path.clone(),
                    runnable: procfile::load(&app.path, &app.settings).is_ok(),
                })
                .collect(),
            stacks: state.stacks().into_iter().collect(),
            problems: Vec::new(),
        })
        .collect();
    let mut global = Vec::new();
    for problem in &world.problems {
        let Some(name) = &problem.project else {
            global.push(api_problem(problem));
            continue;
        };
        let index = match projects.iter().position(|p| &p.name == name) {
            Some(index) => index,
            None => {
                projects.push(ProjectSummary {
                    name: name.clone(),
                    config_path: paths.project_file(name),
                    apps: Vec::new(),
                    stacks: Vec::new(),
                    problems: Vec::new(),
                });
                projects.len() - 1
            }
        };
        projects[index].problems.push(api_problem(problem));
    }
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    ProjectList {
        projects,
        problems: global,
    }
}

fn ls(paths: &Paths, json: bool) -> anyhow::Result<ExitCode> {
    let list = project_list(paths);
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(ExitCode::SUCCESS);
    }
    if list.projects.is_empty() {
        println!("No projects yet. Add one with `carte project add <path>`.");
    } else {
        let width = list
            .projects
            .iter()
            .map(|p| p.name.len())
            .max()
            .unwrap_or(0)
            .max("PROJECT".len());
        println!("{:width$}  APPS  STACKS", "PROJECT");
        for project in &list.projects {
            let stacks = if project.stacks.is_empty() {
                "-".to_string()
            } else {
                project.stacks.join(", ")
            };
            println!(
                "{:width$}  {:>4}  {stacks}",
                project.name,
                project.apps.len()
            );
            for problem in &project.problems {
                let text = match &problem.app {
                    Some(app) => format!("{app}: {}", problem.message),
                    None => problem.message.clone(),
                };
                println!("  ! {text}");
            }
        }
    }
    for problem in &list.problems {
        println!("! {}", problem.message);
    }
    Ok(ExitCode::SUCCESS)
}

/// The project's problems after a parse and discovery, or the parse error.
fn validate(path: &Path, text: &str) -> Result<(ProjectState, Vec<Problem>), String> {
    discover(path, text).map_err(|err| format!("{err:#}"))
}

fn edit(paths: &Paths, name: &str) -> anyhow::Result<ExitCode> {
    let path = paths.project_file(name);
    let original = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => bail!(
            "no project {name} ({}); create one with `carte project add`",
            tilde(&path)
        ),
        Err(err) => return Err(anyhow!(err).context(path.display().to_string())),
    };
    let interactive = prompt::interactive(false);
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "vi".into());

    loop {
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&path)
            .status()
            .with_context(|| format!("running {editor}"))?;
        if !status.success() {
            fs::write(&path, &original)?;
            bail!(
                "{editor} exited with {status}; {} is unchanged",
                tilde(&path)
            );
        }
        let text = fs::read_to_string(&path)?;
        if text == original {
            println!("No changes.");
            return Ok(ExitCode::SUCCESS);
        }
        match validate(&path, &text) {
            Ok((state, problems)) if problems.is_empty() => {
                println!(
                    "{} is valid: {} apps.",
                    tilde(&path),
                    state.project.apps.len()
                );
                return Ok(ExitCode::SUCCESS);
            }
            Ok((_, problems)) => {
                for problem in &problems {
                    println!("problem: {}", problem_text(problem));
                }
                if !interactive || !prompt::confirm("Reopen the editor?", true)? {
                    println!("Saved {} with these problems.", tilde(&path));
                    return Ok(ExitCode::SUCCESS);
                }
            }
            Err(err) => {
                println!("error: {err}");
                if !interactive {
                    return Ok(ExitCode::FAILURE);
                }
                if !prompt::confirm("Reopen the editor?", true)? {
                    fs::write(&path, &original)?;
                    println!("Restored the previous version of {}.", tilde(&path));
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
    }
}

fn rm(paths: &Paths, name: &str, yes: bool) -> anyhow::Result<ExitCode> {
    let path = paths.project_file(name);
    if !path.is_file() {
        bail!("no project {name} ({})", tilde(&path));
    }
    if !yes {
        if !prompt::interactive(false) {
            bail!("not a terminal; pass --yes to delete {}", tilde(&path));
        }
        if !prompt::confirm(&format!("Delete project {name} ({})?", tilde(&path)), false)? {
            println!("Nothing deleted.");
            return Ok(ExitCode::SUCCESS);
        }
    }
    fs::remove_file(&path).with_context(|| format!("{}", path.display()))?;
    println!("Deleted {}", tilde(&path));
    if paths.socket().exists() {
        println!("The daemon stops {name}'s instances when it reloads its config.");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn suggests_the_prefix_most_repos_share() {
        let repos = names(&["make-api", "make-frontend", "make-newsmail-api", "other"]);
        assert_eq!(suggest_prefix(&repos).as_deref(), Some("make-"));
    }

    #[test]
    fn suggests_the_longest_prefix_on_a_tie() {
        let repos = names(&["make-newsmail-api", "make-newsmail-frontend"]);
        assert_eq!(suggest_prefix(&repos).as_deref(), Some("make-newsmail-"));
    }

    #[test]
    fn skips_prefixes_that_leave_invalid_names() {
        let repos = names(&["make-newsmail-a", "make-newsmail-b"]);
        assert_eq!(suggest_prefix(&repos).as_deref(), Some("make-"));
    }

    #[test]
    fn suggests_nothing_without_a_majority() {
        let repos = names(&["norla-site", "anyone", "delegatbase", "steinerskolen"]);
        assert_eq!(suggest_prefix(&repos), None);
    }

    #[test]
    fn writes_commands_as_process_tables() {
        let entries = BTreeMap::from([
            (
                "make-api".to_string(),
                AppEntry {
                    name: None,
                    web_command: Some("bin/serve -p $PORT".into()),
                },
            ),
            (
                "make-web_app".to_string(),
                AppEntry {
                    name: Some("web-app".into()),
                    web_command: Some("bin/web".into()),
                },
            ),
        ]);
        assert_eq!(
            project_toml("~/work/make-*", Some("make-"), &entries),
            "repos = \"~/work/make-*\"\nstrip_prefix = \"make-\"\n\n\
             [apps.make-api.processes.web]\ncommand = \"bin/serve -p $PORT\"\n\n\
             [apps.make-web_app]\nname = \"web-app\"\n\n\
             [apps.make-web_app.processes.web]\ncommand = \"bin/web\"\n"
        );
    }

    #[test]
    fn fixes_invalid_app_names() {
        let repos = names(&["make-web_app", "make-api", "make-x"]);
        let entries = name_overrides(&repos, Some("make-"), "make", false);
        let fixed: Vec<(&str, Option<&str>)> = entries
            .iter()
            .map(|(repo, e)| (repo.as_str(), e.name.as_deref()))
            .collect();
        assert_eq!(fixed, [("make-web_app", Some("web-app"))]);

        let entries = name_overrides(&names(&["My_App"]), None, "my-app", true);
        assert_eq!(entries["My_App"].name.as_deref(), Some("my-app"));
    }
}
