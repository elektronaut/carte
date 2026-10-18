//! `carte import puma-dev`: projects from the apps linked in `~/.puma-dev`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use carte_core::model::{MachineConfig, Problem, ProjectState, Stack, World};
use carte_core::{config, discovery, naming, slug};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::project;
use super::prompt::{self, tilde};
use crate::paths::Paths;

/// An app directory linked from puma-dev under one or more names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Linked {
    names: Vec<String>,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PortApp {
    name: String,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Skipped {
    name: String,
    reason: String,
}

#[derive(Debug, Default)]
struct Scan {
    linked: Vec<Linked>,
    ports: Vec<PortApp>,
    skipped: Vec<Skipped>,
}

fn scan(dir: &Path) -> anyhow::Result<Scan> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", tilde(dir)))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    let mut scan = Scan::default();
    for entry in entries {
        let Some(name) = entry.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let mut skip = |reason: String| {
            scan.skipped.push(Skipped {
                name: name.clone(),
                reason,
            })
        };
        let Ok(target) = fs::canonicalize(&entry) else {
            let to = fs::read_link(&entry).map(|t| tilde(&t)).unwrap_or_default();
            skip(format!("dangling link to {to}"));
            continue;
        };
        if target.is_file() {
            match fs::read_to_string(&target)
                .map_err(|e| e.to_string())
                .and_then(|text| parse_port_file(&text))
            {
                Ok(port) => scan.ports.push(PortApp { name, port }),
                Err(reason) => skip(reason),
            }
        } else if target.to_string_lossy().contains(['*', '?', '[']) {
            skip(format!(
                "{} has glob characters in its path",
                tilde(&target)
            ));
        } else if discovery::is_primary_checkout(&target) {
            match scan.linked.iter_mut().find(|l| l.path == target) {
                Some(linked) => linked.names.push(name),
                None => scan.linked.push(Linked {
                    names: vec![name],
                    path: target,
                }),
            }
        } else if target.join(".git").exists() {
            skip(format!(
                "{} is a linked worktree; carte finds worktrees from the primary checkout",
                tilde(&target)
            ));
        } else {
            skip(format!("{} is not the root of a git repo", tilde(&target)));
        }
    }
    Ok(scan)
}

/// A puma-dev port file: a port, or a URL whose host must be on this machine.
fn parse_port_file(text: &str) -> Result<u16, String> {
    let text = text.trim();
    if let Ok(port) = text.parse::<u16>() {
        return Ok(port);
    }
    let Some((scheme, rest)) = text.split_once("://") else {
        return Err(format!(
            "port file holds {text:?}, neither a port nor a URL"
        ));
    };
    if scheme != "http" {
        return Err(format!(
            "port file proxies to {text}; carte forwards plain http only"
        ));
    }
    let authority = rest.split('/').next().unwrap_or_default();
    let Some((host, port)) = authority
        .rsplit_once(':')
        .filter(|(_, port)| !port.contains(']'))
    else {
        return Err(format!("port file proxies to {text}, which has no port"));
    };
    let loopback = host == "localhost"
        || host == "[::1]"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        return Err(format!(
            "port file proxies to {host}; carte only routes to apps on this machine"
        ));
    }
    port.parse()
        .map_err(|_| format!("port file proxies to {text}, whose port is invalid"))
}

/// One project file to write.
#[derive(Debug, Clone)]
struct Draft {
    name: String,
    apps: Vec<DraftApp>,
}

#[derive(Debug, Clone)]
struct DraftApp {
    /// The app's name: the project's, as the project's root or a port.
    name: String,
    links: Vec<String>,
    path: Option<PathBuf>,
    port: Option<u16>,
    env_files: Vec<String>,
}

/// A written (or, on a dry run, would-be) project and how puma-dev names map onto it.
struct Built {
    file: PathBuf,
    text: String,
    state: ProjectState,
    problems: Vec<Problem>,
    /// puma-dev name, carte URL, whether an alias keeps the old name.
    rows: Vec<(String, String, bool)>,
}

struct Importer<'a> {
    paths: &'a Paths,
    machine: &'a MachineConfig,
    interactive: bool,
    /// Project names in use, by existing files or earlier drafts.
    taken: BTreeSet<String>,
    /// Aliases already configured in existing projects.
    aliases: BTreeSet<String>,
    /// App paths already in a project, with that project's name.
    configured: BTreeMap<PathBuf, String>,
    skipped: Vec<Skipped>,
    notes: Vec<String>,
}

impl<'a> Importer<'a> {
    fn new(paths: &'a Paths, machine: &'a MachineConfig, world: &World, interactive: bool) -> Self {
        let mut taken: BTreeSet<String> = world
            .projects
            .iter()
            .map(|p| p.project.name.clone())
            .collect();
        if let Ok(files) = carte_core::discovery::project_files(&paths.projects_dir()) {
            taken.extend(files.into_iter().map(|f| f.name));
        }
        let apps = world.projects.iter().flat_map(|p| {
            p.project
                .apps
                .iter()
                .map(move |a| (p.project.name.clone(), a))
        });
        let mut aliases = BTreeSet::new();
        let mut configured = BTreeMap::new();
        for (project, app) in apps {
            aliases.extend(app.settings.aliases.iter().cloned());
            if !app.path.as_os_str().is_empty() {
                configured.insert(app.path.clone(), project);
            }
        }
        Importer {
            paths,
            machine,
            interactive,
            taken,
            aliases,
            configured,
            skipped: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn skip_all(&mut self, names: impl IntoIterator<Item = String>, reason: &str) {
        for name in names {
            self.skipped.push(Skipped {
                name,
                reason: reason.to_string(),
            });
        }
    }

    /// Settles a project name: the suggestion when it is free, otherwise whatever the
    /// user types; `None` skips the group.
    fn project_name(
        &mut self,
        what: &str,
        suggestion: Option<String>,
    ) -> anyhow::Result<Option<String>> {
        let free = |taken: &BTreeSet<String>, name: &str| {
            slug::validate_project_name(name).is_ok() && !taken.contains(name)
        };
        if !self.interactive {
            return Ok(suggestion.filter(|s| free(&self.taken, s)));
        }
        let default = suggestion.filter(|s| free(&self.taken, s));
        loop {
            let name = prompt::ask(
                &format!("Project name for {what} (- to skip)"),
                Some(default.as_deref().unwrap_or("-")),
            )?;
            if name == "-" {
                return Ok(None);
            }
            match slug::validate_project_name(&name) {
                Err(err) => println!("{err}"),
                Ok(()) if self.taken.contains(&name) => {
                    println!("There is already a project called {name}.")
                }
                Ok(()) => return Ok(Some(name)),
            }
        }
    }

    /// A project for one linked repo, named after its puma-dev link so the app keeps
    /// its hostname.
    fn app(&mut self, link: Linked) -> anyhow::Result<Option<Draft>> {
        if let Some(project) = self.configured.get(&link.path) {
            let reason = format!("{} is already in project {project}", tilde(&link.path));
            self.skip_all(link.names, &reason);
            return Ok(None);
        }
        let what = tilde(&link.path);
        let Some(name) = self.project_name(&what, valid_project_name(&link.names[0]))? else {
            let reason = if self.interactive {
                "skipped when asked for a project name".to_string()
            } else {
                format!("no free project name for {what}; run the import in a terminal to pick one")
            };
            self.skip_all(link.names, &reason);
            return Ok(None);
        };
        self.taken.insert(name.clone());
        let env_files = self.env_files(&link);
        Ok(Some(Draft {
            name: name.clone(),
            apps: vec![DraftApp {
                name,
                links: link.names,
                path: Some(link.path),
                port: None,
                env_files,
            }],
        }))
    }

    /// The env files puma-dev would have sourced and carte can read, noting the rest.
    fn env_files(&mut self, link: &Linked) -> Vec<String> {
        let mut files = Vec::new();
        let env = link.path.join(".env");
        if fs::read_to_string(&env).is_ok_and(|text| {
            text.lines()
                .any(|line| line.trim_start().starts_with("export "))
        }) {
            files.push(".env".to_string());
        }
        for name in [".powenv", ".pumaenv"] {
            if link.path.join(name).is_file() {
                files.push(name.to_string());
            }
        }
        if link.path.join(".powrc").is_file() {
            self.notes.push(format!(
                "{}: {} is a shell script puma-dev ran; carte doesn't, so move what it sets into env_files or the project's [env]",
                link.names[0],
                tilde(&link.path.join(".powrc"))
            ));
        }
        files
    }

    /// A project for one port file: an app that only routes to a fixed port.
    fn port(&mut self, port: PortApp) -> anyhow::Result<Option<Draft>> {
        let what = format!("port file {}", port.name);
        let Some(name) = self.project_name(&what, valid_project_name(&port.name))? else {
            self.skip_all(
                [port.name],
                "no free or valid project name can be made from it",
            );
            return Ok(None);
        };
        self.taken.insert(name.clone());
        Ok(Some(Draft {
            name: name.clone(),
            apps: vec![DraftApp {
                name,
                links: vec![port.name],
                path: None,
                port: Some(port.port),
                env_files: Vec::new(),
            }],
        }))
    }

    /// Adds aliases for puma-dev names carte doesn't already answer to, and validates.
    fn build(&mut self, draft: &Draft) -> anyhow::Result<Built> {
        let file = self.paths.project_file(&draft.name);
        let text = project_text(draft, &BTreeMap::new());
        let (state, _) = project::discover(&file, &text)?;
        let tld = &self.machine.tld;
        let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut rows = Vec::new();
        for app in &draft.apps {
            let Some(carte_app) = state.project.app(&app.name) else {
                continue;
            };
            let host = naming::local_host(tld, &state.project, carte_app, &Stack::Main);
            let url = naming::instance_url(self.machine, &state.project, carte_app, &Stack::Main);
            for link in &app.links {
                let old = format!("{link}.{tld}");
                if old == host {
                    rows.push((old, url.clone(), false));
                    continue;
                }
                let problem = if !valid_label(link) {
                    Some(format!("{link:?} isn't a valid hostname label"))
                } else if self.taken.contains(link) {
                    Some(format!("project {link} answers {old} instead"))
                } else if self.aliases.contains(link) {
                    Some(format!("another app already has the alias {link}"))
                } else {
                    None
                };
                match problem {
                    Some(problem) => {
                        self.notes
                            .push(format!("{link}: no alias for {old}: {problem}"));
                        rows.push((old, url.clone(), false));
                    }
                    None => {
                        self.aliases.insert(link.clone());
                        aliases
                            .entry(app.name.clone())
                            .or_default()
                            .push(link.clone());
                        rows.push((old, url.clone(), true));
                    }
                }
            }
        }
        let text = project_text(draft, &aliases);
        let (state, problems) = project::discover(&file, &text)?;
        Ok(Built {
            file,
            text,
            state,
            problems,
            rows,
        })
    }
}

fn valid_project_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if slug::validate_project_name(&lower).is_ok() {
        return Some(lower);
    }
    let fixed = slug::slugify(name);
    slug::validate_project_name(&fixed).is_ok().then_some(fixed)
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn string_array<'s>(items: impl IntoIterator<Item = &'s str>) -> Array {
    items.into_iter().collect()
}

fn project_text(draft: &Draft, aliases: &BTreeMap<String, Vec<String>>) -> String {
    let mut doc = DocumentMut::new();
    if let Some(path) = draft.apps.iter().find_map(|a| a.path.as_deref()) {
        doc["root"] = value(tilde(path));
    }
    let mut apps = Table::new();
    apps.set_implicit(true);
    for app in &draft.apps {
        let mut table = Table::new();
        if let Some(port) = app.port {
            table["port"] = value(i64::from(port));
        }
        if let Some(aliases) = aliases.get(&app.name) {
            table["aliases"] = value(string_array(aliases.iter().map(String::as_str)));
        }
        if !app.env_files.is_empty() {
            table["env_files"] = value(string_array(app.env_files.iter().map(String::as_str)));
        }
        if !table.is_empty() {
            table.decor_mut().set_prefix("\n");
            apps.insert(&app.name, Item::Table(table));
        }
    }
    if !apps.is_empty() {
        doc.insert("apps", Item::Table(apps));
    }
    doc.to_string().trim_start().to_string()
}

pub fn puma_dev(
    paths: &Paths,
    from: Option<PathBuf>,
    dry_run: bool,
    yes: bool,
) -> anyhow::Result<ExitCode> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let dir = from.unwrap_or_else(|| home.join(".puma-dev"));
    let interactive = !dry_run && prompt::interactive(yes);
    let machine = config::load_machine_config(&paths.machine_config())?;
    let world = discovery::load_world(&paths.config_dir);

    let scan = scan(&dir)?;
    let mut importer = Importer::new(paths, &machine, &world, interactive);
    importer.skipped = scan.skipped;
    if home.join(".powconfig").is_file() {
        importer.notes.push(
            "~/.powconfig is a shell script puma-dev ran for every app; carte doesn't, so move what it sets into a project's [env] or env_files".into(),
        );
    }

    let mut drafts = Vec::new();
    for link in scan.linked {
        drafts.extend(importer.app(link)?);
    }
    for port in scan.ports {
        drafts.extend(importer.port(port)?);
    }

    let mut built = Vec::new();
    for draft in &drafts {
        built.push(importer.build(draft)?);
    }
    for project in &built {
        project::print_summary(
            &machine,
            &project.state,
            &project.problems,
            &project.file,
            &project.text,
        );
    }
    print_report(&built, &importer);

    if built.is_empty() {
        println!("\nNothing to import.");
        return Ok(ExitCode::SUCCESS);
    }
    if dry_run {
        println!("\nDry run: nothing was written.");
        return Ok(ExitCode::SUCCESS);
    }
    if interactive && !prompt::confirm(&format!("\nWrite {} project files?", built.len()), true)? {
        println!("Nothing written.");
        return Ok(ExitCode::SUCCESS);
    }
    fs::create_dir_all(paths.projects_dir())?;
    for project in &built {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&project.file);
        match file {
            Ok(mut file) => {
                file.write_all(project.text.as_bytes())?;
                println!("Wrote {}", tilde(&project.file));
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                println!("Skipped {}: it exists", tilde(&project.file));
            }
            Err(err) => {
                return Err(anyhow::Error::from(err).context(project.file.display().to_string()));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_report(built: &[Built], importer: &Importer) {
    let rows: Vec<&(String, String, bool)> = built.iter().flat_map(|b| &b.rows).collect();
    if !rows.is_empty() {
        let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
        println!("puma-dev → carte:");
        for (old, url, alias) in rows {
            let alias = if *alias { "  (alias)" } else { "" };
            println!("  {old:width$}  →  {url}{alias}");
        }
    }
    if !importer.skipped.is_empty() {
        let width = importer
            .skipped
            .iter()
            .map(|s| s.name.len())
            .max()
            .unwrap_or(0);
        println!("\nSkipped:");
        for skipped in &importer.skipped {
            println!("  {:width$}  {}", skipped.name, skipped.reason);
        }
    }
    if !importer.notes.is_empty() {
        println!("\nCheck by hand:");
        for note in &importer.notes {
            println!("  {note}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    fn git_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success());
        fs::write(dir.join("config.ru"), "run ->(_) { [200, {}, []] }\n").unwrap();
    }

    #[test]
    fn reads_port_files() {
        assert_eq!(parse_port_file("3000\n"), Ok(3000));
        assert_eq!(parse_port_file("http://localhost:4000"), Ok(4000));
        assert_eq!(parse_port_file("http://127.0.0.1:4001/"), Ok(4001));
        assert_eq!(parse_port_file("http://[::1]:4002"), Ok(4002));
        for (text, expected) in [
            (
                "http://10.0.0.5:3000",
                "carte only routes to apps on this machine",
            ),
            ("https://localhost:3000", "plain http only"),
            ("http://localhost", "has no port"),
            ("http://[::1]", "has no port"),
            ("http://localhost:99999", "port is invalid"),
            ("nonsense", "neither a port nor a URL"),
        ] {
            let err = parse_port_file(text).unwrap_err();
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    /// A fake `~/.puma-dev` linking into a directory of repos and a single repo elsewhere.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let apps = root.join("work/apps");
        for name in ["shop-api", "shop-web", "shop-admin", "unlinked"] {
            git_repo(&apps.join(name));
        }
        fs::write(apps.join("shop-api/.env"), "SECRET=1\n").unwrap();
        fs::write(apps.join("shop-web/.env"), "export API_URL=x\nOTHER=1\n").unwrap();
        fs::write(apps.join("shop-web/.powenv"), "export A=1\n").unwrap();
        fs::write(apps.join("shop-admin/.powrc"), "source x\n").unwrap();
        git_repo(&root.join("clients/blog/develop"));
        git_repo(&root.join("gems/thing"));
        fs::create_dir_all(root.join("gems/thing/spec/internal")).unwrap();

        let puma = root.join("puma-dev");
        fs::create_dir_all(&puma).unwrap();
        symlink(apps.join("shop-api"), puma.join("shop-api")).unwrap();
        symlink(apps.join("shop-web"), puma.join("web")).unwrap();
        symlink(apps.join("shop-web"), puma.join("storefront")).unwrap();
        symlink(apps.join("shop-admin"), puma.join("shop-admin")).unwrap();
        symlink(root.join("clients/blog/develop"), puma.join("blog")).unwrap();
        symlink(root.join("gems/thing/spec/internal"), puma.join("thing")).unwrap();
        symlink(root.join("nowhere"), puma.join("gone")).unwrap();
        fs::write(puma.join("legacy"), "3005\n").unwrap();
        fs::write(puma.join("remote"), "http://10.0.0.5:80\n").unwrap();
        Fixture { _dir: dir, root }
    }

    #[test]
    fn scans_links_and_port_files() {
        let fixture = fixture();
        let scan = scan(&fixture.root.join("puma-dev")).unwrap();
        let linked: Vec<(Vec<&str>, String)> = scan
            .linked
            .iter()
            .map(|l| {
                (
                    l.names.iter().map(String::as_str).collect(),
                    l.path.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(
            linked,
            [
                (vec!["blog"], "develop".to_string()),
                (vec!["shop-admin"], "shop-admin".to_string()),
                (vec!["shop-api"], "shop-api".to_string()),
                (vec!["storefront", "web"], "shop-web".to_string()),
            ]
        );
        assert_eq!(
            scan.ports,
            [PortApp {
                name: "legacy".into(),
                port: 3005
            }]
        );
        let skipped: Vec<&str> = scan.skipped.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(skipped, ["gone", "remote", "thing"]);
        assert!(scan.skipped[0].reason.starts_with("dangling link to "));
        assert!(
            scan.skipped[2]
                .reason
                .ends_with("is not the root of a git repo")
        );
    }

    fn import(fixture: &Fixture, paths: &Paths) -> (Vec<Built>, Vec<Skipped>, Vec<String>) {
        let machine = MachineConfig::default();
        let world = discovery::load_world(&paths.config_dir);
        let scan = scan(&fixture.root.join("puma-dev")).unwrap();
        let mut importer = Importer::new(paths, &machine, &world, false);
        importer.skipped = scan.skipped;
        let mut drafts = Vec::new();
        for link in scan.linked {
            drafts.extend(importer.app(link).unwrap());
        }
        for port in scan.ports {
            drafts.extend(importer.port(port).unwrap());
        }
        let built = drafts.iter().map(|d| importer.build(d).unwrap()).collect();
        (built, importer.skipped, importer.notes)
    }

    #[test]
    fn makes_a_project_for_each_app() {
        let fixture = fixture();
        let paths = Paths::under(&fixture.root.join("carte"));
        let (built, skipped, notes) = import(&fixture, &paths);
        let names: Vec<&str> = built
            .iter()
            .map(|b| b.state.project.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["blog", "shop-admin", "shop-api", "storefront", "legacy"]
        );
        let root = fixture.root.display();

        assert_eq!(
            built[0].text,
            format!("root = \"{root}/clients/blog/develop\"\n")
        );
        assert_eq!(
            built[1].text,
            format!("root = \"{root}/work/apps/shop-admin\"\n")
        );
        assert_eq!(
            built[3].text,
            format!(
                "root = \"{root}/work/apps/shop-web\"\n\n\
                 [apps.storefront]\naliases = [\"web\"]\nenv_files = [\".env\", \".powenv\"]\n"
            )
        );
        assert_eq!(built[4].text, "[apps.legacy]\nport = 3005\n");
        for project in &built {
            assert!(project.problems.is_empty(), "{:?}", project.problems);
        }
        let rows: Vec<(&str, &str, bool)> = built
            .iter()
            .flat_map(|b| &b.rows)
            .map(|(old, url, alias)| (old.as_str(), url.as_str(), *alias))
            .collect();
        assert_eq!(
            rows,
            [
                ("blog.test", "https://blog.test", false),
                ("shop-admin.test", "https://shop-admin.test", false),
                ("shop-api.test", "https://shop-api.test", false),
                ("storefront.test", "https://storefront.test", false),
                ("web.test", "https://storefront.test", true),
                ("legacy.test", "https://legacy.test", false),
            ]
        );
        let skipped: Vec<&str> = skipped.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(skipped, ["gone", "remote", "thing"]);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("shop-admin: ") && notes[0].contains(".powrc"));
    }

    #[test]
    fn leaves_existing_projects_alone() {
        let fixture = fixture();
        let paths = Paths::under(&fixture.root.join("carte"));
        fs::create_dir_all(paths.projects_dir()).unwrap();
        fs::write(paths.project_file("blog"), "root = \"/elsewhere\"\n").unwrap();
        fs::write(
            paths.project_file("mine"),
            format!("root = \"{}/work/apps/shop-api\"\n", fixture.root.display()),
        )
        .unwrap();
        let (built, skipped, _) = import(&fixture, &paths);
        let names: Vec<&str> = built
            .iter()
            .map(|b| b.state.project.name.as_str())
            .collect();
        assert_eq!(names, ["shop-admin", "storefront", "legacy"]);
        let reasons: BTreeMap<&str, &str> = skipped
            .iter()
            .map(|s| (s.name.as_str(), s.reason.as_str()))
            .collect();
        assert!(reasons["shop-api"].ends_with("is already in project mine"));
        assert!(
            reasons["blog"].contains("no free project name"),
            "{reasons:?}"
        );
    }

    #[test]
    fn imports_through_the_command_line() {
        let fixture = fixture();
        let paths = Paths::under(&fixture.root.join("carte"));
        let from = Some(fixture.root.join("puma-dev"));
        puma_dev(&paths, from.clone(), true, true).unwrap();
        assert!(!paths.projects_dir().exists());

        puma_dev(&paths, from.clone(), false, true).unwrap();
        let names = ["blog", "shop-admin", "shop-api", "storefront", "legacy"];
        for name in names {
            assert!(paths.project_file(name).is_file(), "{name}");
        }
        let world = discovery::load_world(&paths.config_dir);
        assert!(world.problems.is_empty(), "{:?}", world.problems);
        assert_eq!(world.projects.len(), names.len());

        let before = fs::read_to_string(paths.project_file("storefront")).unwrap();
        puma_dev(&paths, from, false, true).unwrap();
        assert_eq!(
            fs::read_to_string(paths.project_file("storefront")).unwrap(),
            before
        );
    }
}
