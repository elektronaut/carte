use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, anyhow, bail};
use carte_api::{AppInfo, ProjectList, ProjectSummary};
use carte_core::model::{MachineConfig, Problem, ProjectState, Stack};
use carte_core::procfile::{self, ProcessSource};
use carte_core::{config, discovery, include, naming, slug};
use toml_edit::{Array, DocumentMut, Item, Table, value};

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
    let (include, strip_prefix, repo_names) = if single {
        (None, None, vec![dir_name.clone()])
    } else if dir.join(".git").exists() {
        bail!(
            "{} is a linked worktree; add the repo's primary checkout instead",
            tilde(&dir)
        );
    } else {
        let (pattern, prefix, names) = repos_in_dir(&dir, args.strip_prefix.clone(), interactive)?;
        (Some(pattern), prefix, names)
    };

    let name = choose_name(args.name.as_deref(), &dir_name, interactive)?;
    let file_path = paths.project_file(&name);
    if file_path.exists() {
        bail!(
            "project {name} already exists ({}); change it with `carte project edit {name}`",
            tilde(&file_path)
        );
    }

    let mut entries = if single {
        BTreeMap::new()
    } else {
        named_apps(&repo_names, strip_prefix.as_deref())
    };
    let app_name = |repo: &str| -> String {
        if single {
            return name.clone();
        }
        entries
            .iter()
            .find(|(_, e)| e.path.as_deref() == Some(repo))
            .map_or_else(|| repo.to_string(), |(app, _)| app.clone())
    };
    let unrunnable: Vec<(String, String)> = repo_names
        .iter()
        .filter(|repo| {
            let path = if single { dir.clone() } else { dir.join(repo) };
            !include::is_runnable(&path)
        })
        .map(|repo| (repo.clone(), app_name(repo)))
        .collect();
    let mut commands: Vec<(String, String, String)> = Vec::new();
    if !unrunnable.is_empty() {
        if let Some(command) = &args.command {
            for (repo, app) in &unrunnable {
                commands.push((repo.clone(), app.clone(), command.clone()));
            }
        } else if interactive {
            for (repo, app) in &unrunnable {
                println!(
                    "{app} has no Procfile.dev, Procfile, config.ru or package.json dev script."
                );
                let command = prompt::ask(
                    &format!("Web command for {app} (empty to leave it out)"),
                    None,
                )?;
                if !command.is_empty() {
                    commands.push((repo.clone(), app.clone(), command));
                }
            }
        } else if single {
            bail!(
                "no Procfile.dev, Procfile, config.ru or package.json dev script in {}; pass --command \"<web command>\" \
                 to run it as its `web` process",
                tilde(&dir)
            );
        }
    }
    let mut include: Vec<String> = include.into_iter().collect();
    for (repo, app, command) in commands {
        if !single && !entries.contains_key(&app) {
            include.push(repo);
        }
        entries.entry(app).or_default().web_command = Some(command);
    }
    if !single {
        let left_out: Vec<&str> = unrunnable
            .iter()
            .filter(|(_, app)| entries.get(app).is_none_or(|e| e.web_command.is_none()))
            .map(|(_, app)| app.as_str())
            .collect();
        if !left_out.is_empty() {
            println!("Left out (nothing to run): {}", left_out.join(", "));
        }
    }
    let root = tilde(&dir);
    let text = project_toml(&root, &include, strip_prefix.as_deref(), &entries);
    let (state, problems) = discover(&machine.tld, &file_path, &text)?;

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

/// Lists the repos in `dir` and settles the `include` pattern and `strip_prefix`;
/// also returns the names of the repos the pattern matches.
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
    let pattern = format!("{}*", prefix.as_deref().unwrap_or(""));
    Ok((pattern, prefix, names))
}

/// The `xxx-` prefix shared by the most repos (more than half of them), the longest on a
/// tie, as long as every name it shortens stays a valid hostname label.
pub(super) fn suggest_prefix(names: &[String]) -> Option<String> {
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
            let fixed = slug::slugify(dir_name);
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
pub(super) struct AppEntry {
    /// The directory of an app named for a directory name that isn't a valid hostname label.
    pub(super) path: Option<String>,
    pub(super) web_command: Option<String>,
}

fn project_toml(
    root: &str,
    include: &[String],
    strip_prefix: Option<&str>,
    entries: &BTreeMap<String, AppEntry>,
) -> String {
    let mut doc = DocumentMut::new();
    doc["root"] = value(root);
    match include {
        [] => {}
        [one] => doc["include"] = value(one.as_str()),
        many => doc["include"] = value(many.iter().map(String::as_str).collect::<Array>()),
    }
    if let Some(prefix) = strip_prefix {
        doc["strip_prefix"] = value(prefix);
    }
    if !entries.is_empty() {
        let mut apps = Table::new();
        apps.set_implicit(true);
        for (app, entry) in entries {
            let mut app_table = Table::new();
            app_table.set_implicit(true);
            if let Some(path) = &entry.path {
                app_table.set_implicit(false);
                app_table.decor_mut().set_prefix("\n");
                app_table["path"] = value(path.as_str());
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

/// Named apps for repos whose name, after the prefix, isn't a valid label.
pub(super) fn named_apps(
    repos: &[String],
    strip_prefix: Option<&str>,
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
            let fixed = slug::slugify(short);
            slug::validate_app_short_name(&fixed).ok()?;
            let entry = AppEntry {
                path: Some(repo.clone()),
                web_command: None,
            };
            Some((fixed, entry))
        })
        .collect()
}

pub(super) fn discover(
    tld: &str,
    path: &Path,
    text: &str,
) -> anyhow::Result<(ProjectState, Vec<Problem>)> {
    let file = config::parse_project(path, text)?;
    let (state, mut problems) = discovery::discover_project(file);
    problems.extend(naming::collisions(tld, &state));
    Ok((state, problems))
}

pub(super) fn print_summary(
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
        let processes = match procfile::load(&state.any_checkout(app), &app.settings) {
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
        ProcessSource::PackageJson => "package.json",
        ProcessSource::Config => "config",
        ProcessSource::RouteOnly => "fixed port",
        ProcessSource::ServeFiles => "files only",
    }
}

pub(super) fn problem_text(problem: &Problem) -> String {
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
                    runnable: procfile::load(&state.any_checkout(app), &app.settings).is_ok(),
                })
                .collect(),
            stacks: state.stacks().into_iter().collect(),
            problems: Vec::new(),
            kube_context: None,
        })
        .chain(world.kube_projects.iter().map(|project| ProjectSummary {
            name: project.name.clone(),
            config_path: project.config_path.clone(),
            apps: Vec::new(),
            stacks: Vec::new(),
            problems: Vec::new(),
            kube_context: Some(project.context.clone()),
        }))
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
                    kube_context: None,
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
            if let Some(context) = &project.kube_context {
                println!("{:width$}     -  kube context {context}", project.name);
                print_problems(project);
                continue;
            }
            let stacks = if project.stacks.is_empty() {
                "-".to_string()
            } else {
                project.stacks.join(", ")
            };
            let template = project
                .config_path
                .file_stem()
                .map(|stem| stem.to_string_lossy())
                .filter(|stem| *stem != project.name.as_str())
                .map(|stem| format!("  (from {stem})"))
                .unwrap_or_default();
            println!(
                "{:width$}  {:>4}  {stacks}{template}",
                project.name,
                project.apps.len()
            );
            print_problems(project);
        }
    }
    for problem in &list.problems {
        println!("! {}", problem.message);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_problems(project: &ProjectSummary) {
    for problem in &project.problems {
        let text = match &problem.app {
            Some(app) => format!("{app}: {}", problem.message),
            None => problem.message.clone(),
        };
        println!("  ! {text}");
    }
}

/// The project's problems after a parse and discovery, or the parse error.
fn validate(tld: &str, path: &Path, text: &str) -> Result<(ProjectState, Vec<Problem>), String> {
    discover(tld, path, text).map_err(|err| format!("{err:#}"))
}

fn edit(paths: &Paths, name: &str) -> anyhow::Result<ExitCode> {
    let path = paths.project_file(name);
    let tld = config::load_machine_config(&paths.machine_config())
        .map(|machine| machine.tld)
        .unwrap_or_else(|_| MachineConfig::default().tld);
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
        if config::is_kube_project(&text) {
            match config::parse_kube_project(&path, &text) {
                Ok((project, problems)) => {
                    for problem in &problems {
                        println!("problem: {}", problem_text(problem));
                    }
                    println!(
                        "{} is valid: a kube project for context {}.",
                        tilde(&path),
                        project.context
                    );
                    return Ok(ExitCode::SUCCESS);
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
                    continue;
                }
            }
        }
        if let Ok(file) = config::parse_project(&path, &text)
            && !file.projects.is_empty()
        {
            let (dirs, _, problems) =
                include::expand_projects(file.root.as_deref(), &file.projects);
            for problem in &problems {
                println!("problem: {problem}");
            }
            println!(
                "{} is a template: its projects entries match {} directories.",
                tilde(&path),
                dirs.len()
            );
            return Ok(ExitCode::SUCCESS);
        }
        match validate(&tld, &path, &text) {
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
        let repos = names(&["nova-api", "nova-frontend", "nova-postcard-api", "other"]);
        assert_eq!(suggest_prefix(&repos).as_deref(), Some("nova-"));
    }

    #[test]
    fn suggests_the_longest_prefix_on_a_tie() {
        let repos = names(&["nova-postcard-api", "nova-postcard-frontend"]);
        assert_eq!(suggest_prefix(&repos).as_deref(), Some("nova-postcard-"));
    }

    #[test]
    fn skips_prefixes_that_leave_invalid_names() {
        let repos = names(&["nova-postcard-a", "nova-postcard-b"]);
        assert_eq!(suggest_prefix(&repos).as_deref(), Some("nova-"));
    }

    #[test]
    fn suggests_nothing_without_a_majority() {
        let repos = names(&["atlas-site", "agency", "eventportal", "schoolwebsite"]);
        assert_eq!(suggest_prefix(&repos), None);
    }

    #[test]
    fn writes_commands_as_process_tables() {
        let entries = BTreeMap::from([
            (
                "nova-api".to_string(),
                AppEntry {
                    path: None,
                    web_command: Some("bin/serve -p $PORT".into()),
                },
            ),
            (
                "web-app".to_string(),
                AppEntry {
                    path: Some("nova-web_app".into()),
                    web_command: Some("bin/web".into()),
                },
            ),
        ]);
        assert_eq!(
            project_toml(
                "~/work",
                &names(&["nova-*", "nova-api"]),
                Some("nova-"),
                &entries
            ),
            "root = \"~/work\"\ninclude = [\"nova-*\", \"nova-api\"]\nstrip_prefix = \"nova-\"\n\n\
             [apps.nova-api.processes.web]\ncommand = \"bin/serve -p $PORT\"\n\n\
             [apps.web-app]\npath = \"nova-web_app\"\n\n\
             [apps.web-app.processes.web]\ncommand = \"bin/web\"\n"
        );
        assert_eq!(
            project_toml("~/work/shop", &[], None, &BTreeMap::new()),
            "root = \"~/work/shop\"\n"
        );
    }

    #[test]
    fn names_apps_whose_directory_isnt_a_valid_label() {
        let repos = names(&["nova-web_app", "nova-api", "nova-x"]);
        let entries = named_apps(&repos, Some("nova-"));
        let fixed: Vec<(&str, Option<&str>)> = entries
            .iter()
            .map(|(app, e)| (app.as_str(), e.path.as_deref()))
            .collect();
        assert_eq!(fixed, [("web-app", Some("nova-web_app"))]);
    }
}
