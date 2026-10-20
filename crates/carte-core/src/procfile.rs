//! The process list for one checkout: Procfile, then config.

use std::fmt;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::model::{
    AppSettings, EnvTemplate, Preset, ProcessKind, ProcessOverride, ProcessSpec, Readiness,
    Restart, Scope,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSource {
    ProcfileDev,
    Procfile,
    /// No Procfile, but a `config.ru`: `web` runs puma.
    ConfigRu,
    /// No Procfile or `config.ru`, but a `package.json` with a `dev` script: `web` runs it.
    PackageJson,
    /// Only processes from config.
    Config,
    /// A fixed-port app without processes.
    RouteOnly,
    /// Nothing to run, only files to serve.
    ServeFiles,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessList {
    pub source: ProcessSource,
    /// What the instance starts.
    pub processes: Vec<ProcessSpec>,
    /// Processes with `kind = "teardown"`, which never start with the instance.
    pub teardown: Vec<ProcessSpec>,
    pub notices: Vec<Notice>,
}

/// Something worth telling the user once, without failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A hardcoded port in the HTTP process's command was replaced with `$PORT`.
    PortRewritten {
        process: String,
        from: String,
        to: String,
    },
    IgnoredLine {
        file: PathBuf,
        line: usize,
        text: String,
    },
    /// Config adjusts a process that doesn't exist and gives no command to add it.
    UnknownProcess { process: String },
    /// The command looks like a preset's dev server, but its config rules out
    /// serving paths.
    PresetSkipped {
        process: String,
        preset: Preset,
        setting: &'static str,
    },
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Notice::PortRewritten { process, from, to } => write!(
                f,
                "{process}: made the command use $PORT: `{from}` runs as `{to}`"
            ),
            Notice::IgnoredLine { file, line, text } => {
                write!(f, "{}:{line}: ignored line `{text}`", file.display())
            }
            Notice::UnknownProcess { process } => write!(
                f,
                "config for process {process:?} is ignored: it is not in the Procfile and has no command"
            ),
            Notice::PresetSkipped {
                process,
                preset,
                setting,
            } => write!(
                f,
                "{process}: looks like {preset}'s dev server, but `{setting}` keeps carte from routing its paths; remove it and set preset = \"{preset}\" to have carte route them",
                preset = preset.name()
            ),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProcfileError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} has no Procfile.dev, Procfile, config.ru or package.json dev script, and no processes are configured", path.display())]
    NotRunnable { path: PathBuf },
    #[error("process {process:?} has no command")]
    MissingCommand { process: String },
    #[error("more than one process serves HTTP: {}", processes.join(", "))]
    MultipleHttp { processes: Vec<String> },
    #[error("process {process:?} serves HTTP, so it can't have a kind")]
    KindOnHttp { process: String },
    #[error(
        "process {process:?} is a {kind}, which runs per instance: remove scope = \"checkout\""
    )]
    CheckoutScopedKind { process: String, kind: &'static str },
    #[error(
        "process {process:?} runs once per checkout, so it can't have `provides`: remove scope = \"checkout\""
    )]
    CheckoutScopedProvides { process: String },
    #[error("process {process:?} is a teardown, which runs on its own: remove `after`")]
    TeardownAfter { process: String },
    #[error(
        "process {process:?} serves HTTP, so it can't have `restart`: carte always restarts it when it exits"
    )]
    RestartOnHttp { process: String },
    #[error(
        "process {process:?} serves HTTP, so it can't have `paths`: it gets every path the other processes don't"
    )]
    PathsOnHttp { process: String },
    #[error("process {process:?} serves paths, so it can't have a kind")]
    KindOnPaths { process: String },
    #[error(
        "process {process:?} serves paths, so it can't have `restart`: carte always restarts it when it exits"
    )]
    RestartOnPaths { process: String },
    #[error(
        "process {process:?} serves paths, which runs per instance: remove scope = \"checkout\""
    )]
    CheckoutScopedPaths { process: String },
    #[error("process {process:?} lists path {path:?} twice")]
    RepeatedPath { process: String, path: String },
    #[error("path {path:?} is served by both {first:?} and {second:?}")]
    DuplicatePath {
        path: String,
        first: String,
        second: String,
    },
    #[error("process {process:?} is a teardown, which runs once: remove `restart`")]
    RestartOnTeardown { process: String },
    #[error(
        "process {process:?} is a task, which runs once per start: remove restart = \"on-failure\""
    )]
    RestartOnTask { process: String },
    #[error("process {process:?} starts after {after:?}, which isn't a process of the app")]
    UnknownAfter { process: String, after: String },
    #[error(
        "process {process:?} can't start after {after:?}: `after` only orders processes of the same kind"
    )]
    AfterOtherTier { process: String, after: String },
    #[error("processes start after each other in a cycle: {}", processes.join(" -> "))]
    AfterCycle { processes: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedProcfile {
    /// `(name, command)` in file order.
    pub entries: Vec<(String, String)>,
    /// Lines that were not understood: 1-based line number and text.
    pub ignored: Vec<(usize, String)>,
}

/// Parses `name: command` lines.
pub fn parse_procfile(text: &str) -> ParsedProcfile {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut ignored = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parsed = trimmed.split_once(':').and_then(|(name, command)| {
            let name = name.trim();
            let command = command.trim();
            let valid_name = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            (valid_name && !command.is_empty()).then(|| (name.to_string(), command.to_string()))
        });
        match parsed {
            Some(entry) if !entries.iter().any(|(n, _)| *n == entry.0) => entries.push(entry),
            _ => ignored.push((i + 1, trimmed.to_string())),
        }
    }
    ParsedProcfile { entries, ignored }
}

/// Reads the base process list for a checkout and applies the app's config.
pub fn load(checkout: &Path, settings: &AppSettings) -> Result<ProcessList, ProcfileError> {
    let mut list = load_any(checkout, settings)?;
    if list.processes.is_empty() && settings.serve_files.is_some() {
        list.source = ProcessSource::ServeFiles;
    }
    if list.processes.is_empty()
        && !matches!(
            list.source,
            ProcessSource::RouteOnly | ProcessSource::ServeFiles
        )
    {
        return Err(ProcfileError::NotRunnable {
            path: checkout.to_path_buf(),
        });
    }
    Ok(list)
}

/// The teardown processes of an app in a checkout, which may have nothing else to run.
pub fn teardowns(
    checkout: &Path,
    settings: &AppSettings,
) -> Result<Vec<ProcessSpec>, ProcfileError> {
    load_any(checkout, settings).map(|list| list.teardown)
}

fn load_any(checkout: &Path, settings: &AppSettings) -> Result<ProcessList, ProcfileError> {
    if settings.port.is_some() {
        return Ok(ProcessList {
            source: ProcessSource::RouteOnly,
            processes: Vec::new(),
            teardown: Vec::new(),
            notices: Vec::new(),
        });
    }
    let mut notices = Vec::new();
    let (source, base) = if settings.processes.replace {
        (ProcessSource::Config, Vec::new())
    } else if let Some((source, path)) = [
        (ProcessSource::ProcfileDev, "Procfile.dev"),
        (ProcessSource::Procfile, "Procfile"),
    ]
    .into_iter()
    .map(|(source, name)| (source, checkout.join(name)))
    .find(|(_, path)| path.is_file())
    {
        let text = std::fs::read_to_string(&path).map_err(|source| ProcfileError::Io {
            path: path.clone(),
            source,
        })?;
        let ParsedProcfile { entries, ignored } = parse_procfile(&text);
        notices.extend(ignored.into_iter().map(|(line, text)| Notice::IgnoredLine {
            file: path.clone(),
            line,
            text,
        }));
        (source, entries)
    } else if checkout.join("config.ru").is_file() {
        (
            ProcessSource::ConfigRu,
            vec![("web".to_string(), "bundle exec puma -p $PORT".to_string())],
        )
    } else if let Some(command) = crate::node::dev_command(checkout) {
        (
            ProcessSource::PackageJson,
            vec![("web".to_string(), command)],
        )
    } else {
        (ProcessSource::Config, Vec::new())
    };

    let mut list = build(source, base, settings)?;
    notices.append(&mut list.notices);
    list.notices = notices;
    Ok(list)
}

/// Applies the app's process config to a base list and fills in defaults.
pub fn build(
    source: ProcessSource,
    base: Vec<(String, String)>,
    settings: &AppSettings,
) -> Result<ProcessList, ProcfileError> {
    let config = &settings.processes;
    let mut notices = Vec::new();
    let mut commands = if config.replace { Vec::new() } else { base };
    for (name, ov) in &config.entries {
        match (commands.iter_mut().find(|(n, _)| n == name), &ov.command) {
            (Some(entry), Some(command)) => entry.1 = command.clone(),
            (Some(_), None) => {}
            (None, Some(command)) => commands.push((name.clone(), command.clone())),
            (None, None) if ov.if_present => {}
            (None, None) if config.replace => {
                return Err(ProcfileError::MissingCommand {
                    process: name.clone(),
                });
            }
            (None, None) => notices.push(Notice::UnknownProcess {
                process: name.clone(),
            }),
        }
    }

    let explicit_http = commands
        .iter()
        .any(|(n, _)| config.entries.get(n).and_then(|ov| ov.http) == Some(true));
    let processes: Vec<ProcessSpec> = commands
        .into_iter()
        .map(|(name, mut command)| {
            let mut ov = config.entries.get(&name).cloned().unwrap_or_default();
            let http = ov.http.unwrap_or(name == "web" && !explicit_http);
            let detected = (!http && ov.preset.is_none())
                .then(|| detect_preset(&command))
                .flatten();
            let conflict = if ov.paths.is_some() {
                None
            } else if ov.kind.is_some() {
                Some("kind")
            } else if ov.restart.is_some() {
                Some("restart")
            } else if ov.scope == Some(Scope::Checkout) {
                Some("scope = \"checkout\"")
            } else {
                None
            };
            let preset = match (detected, conflict) {
                (Some(preset), Some(setting)) => {
                    notices.push(Notice::PresetSkipped {
                        process: name.clone(),
                        preset,
                        setting,
                    });
                    None
                }
                (Some(preset), None) => Some(preset),
                (None, _) => ov.preset.filter(|p| *p != Preset::None),
            };
            if let Some(preset) = preset {
                apply_preset(preset, &mut ov);
            }
            let paths = ov.paths.unwrap_or_default();
            let always_restarted = http || !paths.is_empty();
            let restart = restart(&name, http, &paths, ov.kind, ov.restart)?;
            if always_restarted && let Some(rewritten) = rewrite_port(&command) {
                notices.push(Notice::PortRewritten {
                    process: name.clone(),
                    from: command.clone(),
                    to: rewritten.clone(),
                });
                command = rewritten;
            }
            let scope = ov.scope.unwrap_or(
                if !always_restarted
                    && ov.kind.is_none()
                    && ov.provides.is_empty()
                    && is_watcher(&name, &command)
                {
                    Scope::Checkout
                } else {
                    Scope::Instance
                },
            );
            Ok(ProcessSpec {
                cwd: ov.cwd,
                env: ov.env,
                http,
                ready: ov.ready.unwrap_or(Readiness::Port),
                scope,
                pty: ov.pty.unwrap_or(true),
                policy: ov.policy.or(settings.policy).unwrap_or_default(),
                kind: ov.kind,
                after: ov.after.unwrap_or_default(),
                provides: ov.provides,
                restart,
                paths,
                preset,
                name,
                command,
            })
        })
        .collect::<Result<_, _>>()?;

    let http: Vec<String> = processes
        .iter()
        .filter(|p| p.http)
        .map(|p| p.name.clone())
        .collect();
    if http.len() > 1 {
        return Err(ProcfileError::MultipleHttp { processes: http });
    }
    check_paths(&processes)?;
    check_kinds(&processes)?;
    let (teardown, processes) = processes
        .into_iter()
        .partition(|p| p.kind == Some(ProcessKind::Teardown));
    Ok(ProcessList {
        source,
        processes,
        teardown,
        notices,
    })
}

/// The restart policy a process gets: `on-failure` for services and ordinary
/// processes, `never` for tasks and teardowns. The HTTP process and processes that
/// serve paths get `never` too, though the supervisor restarts them whenever they
/// exit after the start.
fn restart(
    name: &str,
    http: bool,
    paths: &[String],
    kind: Option<ProcessKind>,
    configured: Option<Restart>,
) -> Result<Restart, ProcfileError> {
    let process = name.to_string();
    match (configured, kind) {
        (Some(_), _) if http => Err(ProcfileError::RestartOnHttp { process }),
        (Some(_), _) if !paths.is_empty() => Err(ProcfileError::RestartOnPaths { process }),
        (Some(_), Some(ProcessKind::Teardown)) => Err(ProcfileError::RestartOnTeardown { process }),
        (Some(Restart::OnFailure), Some(ProcessKind::Task)) => {
            Err(ProcfileError::RestartOnTask { process })
        }
        (Some(restart), _) => Ok(restart),
        (None, Some(ProcessKind::Task | ProcessKind::Teardown)) => Ok(Restart::Never),
        (None, _) if http || !paths.is_empty() => Ok(Restart::Never),
        (None, _) => Ok(Restart::OnFailure),
    }
}

fn check_paths(processes: &[ProcessSpec]) -> Result<(), ProcfileError> {
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for p in processes.iter().filter(|p| p.serves_paths()) {
        let process = p.name.clone();
        if p.http {
            return Err(ProcfileError::PathsOnHttp { process });
        }
        if p.kind.is_some() {
            return Err(ProcfileError::KindOnPaths { process });
        }
        if p.scope == Scope::Checkout {
            return Err(ProcfileError::CheckoutScopedPaths { process });
        }
        for path in &p.paths {
            let normalized = path.trim_end_matches('/');
            if let Some((_, first)) = seen.iter().find(|(seen, _)| *seen == normalized) {
                if *first == p.name {
                    return Err(ProcfileError::RepeatedPath {
                        process,
                        path: path.clone(),
                    });
                }
                return Err(ProcfileError::DuplicatePath {
                    path: path.clone(),
                    first: first.to_string(),
                    second: process,
                });
            }
            seen.push((normalized, &p.name));
        }
    }
    Ok(())
}

fn check_kinds(processes: &[ProcessSpec]) -> Result<(), ProcfileError> {
    for p in processes {
        if p.http && p.kind.is_some() {
            return Err(ProcfileError::KindOnHttp {
                process: p.name.clone(),
            });
        }
        if p.kind == Some(ProcessKind::Teardown) && !p.after.is_empty() {
            return Err(ProcfileError::TeardownAfter {
                process: p.name.clone(),
            });
        }
        let kind = match p.kind {
            Some(ProcessKind::Service) => Some("service"),
            Some(ProcessKind::Task) => Some("task"),
            Some(ProcessKind::Teardown) | None => None,
        };
        if let Some(kind) = kind
            && p.scope == Scope::Checkout
        {
            return Err(ProcfileError::CheckoutScopedKind {
                process: p.name.clone(),
                kind,
            });
        }
        if p.scope == Scope::Checkout
            && p.kind != Some(ProcessKind::Teardown)
            && !p.provides.is_empty()
        {
            return Err(ProcfileError::CheckoutScopedProvides {
                process: p.name.clone(),
            });
        }
        for after in &p.after {
            let Some(other) = processes.iter().find(|o| o.name == *after) else {
                return Err(ProcfileError::UnknownAfter {
                    process: p.name.clone(),
                    after: after.clone(),
                });
            };
            if other.tier() != p.tier() {
                return Err(ProcfileError::AfterOtherTier {
                    process: p.name.clone(),
                    after: after.clone(),
                });
            }
        }
    }
    for start in processes {
        if let Some(cycle) = find_cycle(processes, &mut vec![start.name.as_str()]) {
            return Err(ProcfileError::AfterCycle { processes: cycle });
        }
    }
    Ok(())
}

/// A cycle of `after` edges from the last process of `path` back to its first.
fn find_cycle<'a>(processes: &'a [ProcessSpec], path: &mut Vec<&'a str>) -> Option<Vec<String>> {
    let last = *path.last()?;
    let current = processes.iter().find(|p| p.name == last)?;
    for next in &current.after {
        if *next == path[0] {
            let mut cycle: Vec<String> = path.iter().map(|n| n.to_string()).collect();
            cycle.push(next.clone());
            return Some(cycle);
        }
        if path.contains(&next.as_str()) {
            continue;
        }
        path.push(next);
        if let Some(cycle) = find_cycle(processes, path) {
            return Some(cycle);
        }
        path.pop();
    }
    None
}

/// The preset a command's dev server matches: vite_ruby's `bin/vite dev` or
/// Shakapacker's `bin/shakapacker-dev-server`.
pub fn detect_preset(command: &str) -> Option<Preset> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let binstub = |token: &str, name: &str| {
        let path = format!("bin/{name}");
        token == path || token.ends_with(&format!("/{path}"))
    };
    let bundle = |token: &str| token == "bundle" || binstub(token, "bundle");
    let vite_ruby = tokens
        .windows(2)
        .any(|pair| matches!(pair, [vite, "dev"] if binstub(vite, "vite")))
        || tokens
            .windows(4)
            .any(|four| matches!(four, [b, "exec", "vite", "dev"] if bundle(b)));
    if vite_ruby {
        Some(Preset::ViteRuby)
    } else if tokens
        .iter()
        .any(|token| binstub(token, "shakapacker-dev-server"))
    {
        Some(Preset::Shakapacker)
    } else {
        None
    }
}

/// Fills in the paths a preset's dev server serves and the variable that tells
/// it and Rails its port, under what the process configures itself.
fn apply_preset(preset: Preset, ov: &mut ProcessOverride) {
    let (paths, port_var): (&[&str], &str) = match preset {
        Preset::ViteRuby => (&["/vite-dev/"], "VITE_RUBY_PORT"),
        Preset::Shakapacker => (&["/packs/", "/ws"], "SHAKAPACKER_DEV_SERVER_PORT"),
        Preset::None => return,
    };
    ov.paths
        .get_or_insert_with(|| paths.iter().map(|p| p.to_string()).collect());
    if !ov.provides.iter().any(|e| e.key == port_var) {
        ov.provides.insert(
            0,
            EnvTemplate {
                key: port_var.into(),
                value: "{{port}}".into(),
            },
        );
    }
}

/// Whether a process looks like a file watcher, which runs once per checkout.
pub fn is_watcher(name: &str, command: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("watch")
        || matches!(name.as_str(), "css" | "js")
        || command.split_whitespace().any(|token| {
            token == "-w"
                || token == "watch"
                || token == "--watch"
                || token.starts_with("--watch=")
                || token.ends_with(":watch")
        })
}

/// Replaces a hardcoded `-p NNNN`, `-pNNNN`, `--port NNNN` or `--port=NNNN` with `$PORT`,
/// and drops a leading `unset PORT &&` or `unset PORT;`.
pub fn rewrite_port(command: &str) -> Option<String> {
    let unset = ["unset PORT &&", "unset PORT;"]
        .iter()
        .find_map(|prefix| command.trim_start().strip_prefix(prefix))
        .map(str::trim_start);
    let rewritten = rewrite_port_flag(unset.unwrap_or(command));
    match (unset, rewritten) {
        (_, Some(out)) => Some(out),
        (Some(rest), None) => Some(rest.to_string()),
        (None, None) => None,
    }
}

fn rewrite_port_flag(command: &str) -> Option<String> {
    let tokens = tokens_with_offsets(command);
    let mut ranges = Vec::new();
    for (i, &(start, token)) in tokens.iter().enumerate() {
        if token == "-p" || token == "--port" {
            if let Some(&(next_start, next)) = tokens.get(i + 1)
                && is_port_number(next)
            {
                ranges.push(next_start..next_start + next.len());
            }
        } else if let Some(value) = ["--port=", "-p="]
            .iter()
            .find_map(|prefix| token.strip_prefix(prefix))
            .or_else(|| token.strip_prefix("-p"))
            && is_port_number(value)
        {
            ranges.push(start + token.len() - value.len()..start + token.len());
        }
    }
    if ranges.is_empty() {
        return None;
    }
    let mut out = command.to_string();
    for range in ranges.into_iter().rev() {
        out.replace_range(range, "$PORT");
    }
    Some(out)
}

fn is_port_number(s: &str) -> bool {
    !s.is_empty() && s.len() <= 5 && s.chars().all(|c| c.is_ascii_digit())
}

fn tokens_with_offsets(s: &str) -> Vec<(usize, &str)> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(st)) => {
                tokens.push((st, &s[st..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(st) = start {
        tokens.push((st, &s[st..]));
    }
    tokens
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{EnvTemplate, Policy, ProcessConfig, ProcessOverride};

    fn settings(replace: bool, entries: &[(&str, ProcessOverride)]) -> AppSettings {
        AppSettings {
            processes: ProcessConfig {
                replace,
                entries: entries
                    .iter()
                    .map(|(n, o)| (n.to_string(), o.clone()))
                    .collect::<BTreeMap<_, _>>(),
            },
            ..Default::default()
        }
    }

    fn names(list: &ProcessList) -> Vec<&str> {
        list.processes.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn parses_procfile_lines() {
        let ParsedProcfile { entries, ignored } = parse_procfile(
            "# comment\n\nweb: bin/rails server -p 3000\ncss:bin/rails tailwindcss:watch\n  worker:  bundle exec sidekiq  \nnot a process\nweb: again\n",
        );
        assert_eq!(
            entries,
            vec![
                ("web".into(), "bin/rails server -p 3000".into()),
                ("css".into(), "bin/rails tailwindcss:watch".into()),
                ("worker".into(), "bundle exec sidekiq".into()),
            ]
        );
        assert_eq!(
            ignored,
            vec![(6, "not a process".into()), (7, "web: again".into())]
        );
    }

    #[test]
    fn rewrites_hardcoded_ports() {
        assert_eq!(
            rewrite_port("bin/rails server -p 3000").as_deref(),
            Some("bin/rails server -p $PORT")
        );
        assert_eq!(
            rewrite_port("puma --port 3000 -C config/puma.rb").as_deref(),
            Some("puma --port $PORT -C config/puma.rb")
        );
        assert_eq!(
            rewrite_port("rackup --port=9292").as_deref(),
            Some("rackup --port=$PORT")
        );
        assert_eq!(
            rewrite_port("rails s -p3000 -b 0.0.0.0").as_deref(),
            Some("rails s -p$PORT -b 0.0.0.0")
        );
        assert_eq!(rewrite_port("bin/rails server -p $PORT"), None);
        assert_eq!(rewrite_port("bin/rails server -p ${PORT:-3000}"), None);
        assert_eq!(rewrite_port("bin/rails server"), None);
        assert_eq!(
            rewrite_port("unset PORT && bin/rails server").as_deref(),
            Some("bin/rails server")
        );
        assert_eq!(
            rewrite_port("unset PORT; bin/rails server -p 3000").as_deref(),
            Some("bin/rails server -p $PORT")
        );
    }

    #[test]
    fn detects_watchers() {
        assert!(is_watcher("css", "bin/rails tailwindcss:watch"));
        assert!(is_watcher("js", "yarn build"));
        assert!(is_watcher("assets", "yarn build --watch"));
        assert!(is_watcher("vite", "vite build --watch"));
        assert!(is_watcher(
            "tailwind",
            "tailwindcss -i in.css -o out.css -w"
        ));
        assert!(is_watcher("frontend", "npm run watch"));
        assert!(is_watcher("watcher", "bin/thing"));
        assert!(!is_watcher("worker", "bundle exec sidekiq"));
        assert!(!is_watcher("vite", "bin/vite dev"));
    }

    #[test]
    fn applies_defaults() {
        let base = vec![
            ("web".into(), "bin/rails server -p 3000".into()),
            ("css".into(), "bin/rails tailwindcss:watch".into()),
            ("worker".into(), "bin/jobs".into()),
        ];
        let list = build(ProcessSource::ProcfileDev, base, &AppSettings::default()).unwrap();
        let web = &list.processes[0];
        assert!(web.http);
        assert_eq!(web.command, "bin/rails server -p $PORT");
        assert_eq!(web.scope, Scope::Instance);
        assert_eq!(web.ready, Readiness::Port);
        assert!(web.pty);
        assert_eq!(web.policy, Policy::Auto);
        assert!(!list.processes[1].http);
        assert_eq!(list.processes[1].scope, Scope::Checkout);
        assert_eq!(list.processes[2].scope, Scope::Instance);
        assert_eq!(
            list.notices,
            vec![Notice::PortRewritten {
                process: "web".into(),
                from: "bin/rails server -p 3000".into(),
                to: "bin/rails server -p $PORT".into(),
            }]
        );
    }

    #[test]
    fn applies_overrides_and_additions() {
        let base = vec![
            ("web".into(), "bin/rails server".into()),
            ("frontend".into(), "bin/vite dev --port 3036".into()),
            ("clock".into(), "bin/clock".into()),
        ];
        let mut s = settings(
            false,
            &[
                (
                    "frontend",
                    ProcessOverride {
                        http: Some(true),
                        ready: Some(Readiness::Http("/".into())),
                        ..Default::default()
                    },
                ),
                (
                    "clock",
                    ProcessOverride {
                        policy: Some(Policy::Disabled),
                        pty: Some(false),
                        ..Default::default()
                    },
                ),
                (
                    "extra",
                    ProcessOverride {
                        command: Some("bin/extra".into()),
                        scope: Some(Scope::Checkout),
                        ..Default::default()
                    },
                ),
                ("ghost", ProcessOverride::default()),
            ],
        );
        s.policy = Some(Policy::Pinned);
        let list = build(ProcessSource::Procfile, base, &s).unwrap();
        assert_eq!(names(&list), vec!["web", "frontend", "clock", "extra"]);
        let [web, frontend, clock, extra] = &list.processes[..] else {
            unreachable!()
        };
        assert!(!web.http);
        assert_eq!(web.policy, Policy::Pinned);
        assert!(frontend.http);
        assert_eq!(frontend.command, "bin/vite dev --port $PORT");
        assert_eq!(frontend.ready, Readiness::Http("/".into()));
        assert_eq!(clock.policy, Policy::Disabled);
        assert!(!clock.pty);
        assert_eq!(extra.scope, Scope::Checkout);
        assert!(list.notices.contains(&Notice::UnknownProcess {
            process: "ghost".into()
        }));
    }

    #[test]
    fn replaces_the_list() {
        let s = settings(
            true,
            &[(
                "web",
                ProcessOverride {
                    command: Some("ruby app.rb -p $PORT".into()),
                    ..Default::default()
                },
            )],
        );
        let list = build(
            ProcessSource::Config,
            vec![("worker".into(), "x".into())],
            &s,
        )
        .unwrap();
        assert_eq!(names(&list), vec!["web"]);
        assert!(list.processes[0].http);

        let s = settings(true, &[("web", ProcessOverride::default())]);
        assert!(matches!(
            build(ProcessSource::Config, Vec::new(), &s),
            Err(ProcfileError::MissingCommand { .. })
        ));
    }

    #[test]
    fn rejects_two_http_processes() {
        let s = settings(
            false,
            &[(
                "api",
                ProcessOverride {
                    http: Some(true),
                    ..Default::default()
                },
            )],
        );
        let base = vec![("web".into(), "a".into()), ("api".into(), "b".into())];
        assert!(build(ProcessSource::Procfile, base.clone(), &s).is_ok());
        let s = settings(
            false,
            &[
                (
                    "api",
                    ProcessOverride {
                        http: Some(true),
                        ..Default::default()
                    },
                ),
                (
                    "web",
                    ProcessOverride {
                        http: Some(true),
                        ..Default::default()
                    },
                ),
            ],
        );
        assert!(matches!(
            build(ProcessSource::Procfile, base, &s),
            Err(ProcfileError::MultipleHttp { .. })
        ));
    }

    #[test]
    fn loads_from_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(dir.path(), &AppSettings::default()).unwrap_err();
        assert!(matches!(err, ProcfileError::NotRunnable { .. }));

        std::fs::write(dir.path().join("config.ru"), "run App").unwrap();
        let list = load(dir.path(), &AppSettings::default()).unwrap();
        assert_eq!(list.source, ProcessSource::ConfigRu);
        assert_eq!(list.processes[0].command, "bundle exec puma -p $PORT");
        assert!(list.processes[0].http);
        assert!(list.notices.is_empty());

        std::fs::write(dir.path().join("Procfile"), "web: rackup\n").unwrap();
        let list = load(dir.path(), &AppSettings::default()).unwrap();
        assert_eq!(list.source, ProcessSource::Procfile);

        std::fs::write(dir.path().join("Procfile.dev"), "web: bin/rails s\nbad\n").unwrap();
        let list = load(dir.path(), &AppSettings::default()).unwrap();
        assert_eq!(list.source, ProcessSource::ProcfileDev);
        assert_eq!(list.processes[0].command, "bin/rails s");
        assert!(matches!(
            &list.notices[..],
            [Notice::IgnoredLine { line: 2, .. }]
        ));

        let route_only = AppSettings {
            port: Some(3000),
            ..Default::default()
        };
        let list = load(dir.path(), &route_only).unwrap();
        assert_eq!(list.source, ProcessSource::RouteOnly);
        assert!(list.processes.is_empty());
    }

    #[test]
    fn config_only_processes() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(
            false,
            &[(
                "web",
                ProcessOverride {
                    command: Some("node server.js".into()),
                    ..Default::default()
                },
            )],
        );
        let list = load(dir.path(), &s).unwrap();
        assert_eq!(list.source, ProcessSource::Config);
        assert_eq!(names(&list), vec!["web"]);
    }

    fn with_kinds(entries: &[(&str, ProcessOverride)]) -> Result<ProcessList, ProcfileError> {
        let base = vec![
            ("web".into(), "bin/rails s".into()),
            ("worker".into(), "bin/jobs".into()),
        ];
        build(ProcessSource::Procfile, base, &settings(false, entries))
    }

    fn kind(command: &str, kind: Option<ProcessKind>, after: &[&str]) -> ProcessOverride {
        ProcessOverride {
            command: Some(command.into()),
            kind,
            after: Some(after.iter().map(|a| a.to_string()).collect()),
            ..Default::default()
        }
    }

    const SERVICE: Option<ProcessKind> = Some(ProcessKind::Service);
    const TASK: Option<ProcessKind> = Some(ProcessKind::Task);

    #[test]
    fn processes_have_kinds_and_tiers() {
        let list = with_kinds(&[
            ("redis", kind("redis-server", SERVICE, &[])),
            ("migrate", kind("bin/rails db:prepare", TASK, &[])),
            ("seed", kind("bin/rails db:seed", TASK, &["migrate"])),
            ("worker", kind("bin/jobs", None, &["web"])),
        ])
        .unwrap();
        let tier = |name: &str| {
            let p = list.processes.iter().find(|p| p.name == name).unwrap();
            (p.kind, p.tier(), p.after.clone())
        };
        assert_eq!(tier("redis"), (SERVICE, 1, vec![]));
        assert_eq!(tier("seed"), (TASK, 2, vec!["migrate".to_string()]));
        assert_eq!(tier("web"), (None, 3, vec![]));
        assert_eq!(tier("worker"), (None, 3, vec!["web".to_string()]));
    }

    #[test]
    fn services_and_tasks_run_per_instance() {
        let list = with_kinds(&[("watcher", kind("bin/watch --watch", SERVICE, &[]))]).unwrap();
        let watcher = list.processes.iter().find(|p| p.name == "watcher").unwrap();
        assert_eq!(watcher.scope, Scope::Instance);

        let checkout = ProcessOverride {
            scope: Some(Scope::Checkout),
            ..kind("bin/rails db:prepare", TASK, &[])
        };
        let err = with_kinds(&[("migrate", checkout)]).unwrap_err();
        assert!(
            matches!(err, ProcfileError::CheckoutScopedKind { kind: "task", .. }),
            "{err}"
        );
    }

    #[test]
    fn only_processes_run_per_instance_provide_env() {
        let provides = vec![EnvTemplate {
            key: "X_URL".into(),
            value: "http://localhost:{{port}}".into(),
        }];
        let watcher = ProcessOverride {
            command: Some("bin/watch --watch".into()),
            provides: provides.clone(),
            ..Default::default()
        };
        let list = with_kinds(&[("watcher", watcher.clone())]).unwrap();
        let p = list.processes.iter().find(|p| p.name == "watcher").unwrap();
        assert_eq!(p.scope, Scope::Instance);

        let checkout = ProcessOverride {
            scope: Some(Scope::Checkout),
            ..watcher
        };
        let err = with_kinds(&[("watcher", checkout)]).unwrap_err();
        assert!(
            matches!(err, ProcfileError::CheckoutScopedProvides { .. }),
            "{err}"
        );
    }

    #[test]
    fn project_wide_adjustments_skip_apps_without_the_process() {
        let disabled = ProcessOverride {
            policy: Some(Policy::Disabled),
            if_present: true,
            ..Default::default()
        };
        let base = vec![
            ("web".to_string(), "bin/serve".to_string()),
            ("worker".to_string(), "bin/jobs".to_string()),
        ];
        let s = settings(
            false,
            &[("worker", disabled.clone()), ("clock", disabled.clone())],
        );
        let list = build(ProcessSource::Procfile, base, &s).unwrap();
        assert_eq!(names(&list), vec!["web", "worker"]);
        assert_eq!(list.processes[1].policy, Policy::Disabled);
        assert!(list.notices.is_empty(), "{:?}", list.notices);

        let web = ProcessOverride {
            command: Some("bin/serve".into()),
            ..Default::default()
        };
        let s = settings(true, &[("web", web), ("worker", disabled)]);
        let list = build(ProcessSource::Config, Vec::new(), &s).unwrap();
        assert_eq!(names(&list), vec!["web"]);
        assert!(list.notices.is_empty(), "{:?}", list.notices);
    }

    #[test]
    fn processes_restart_on_failure_except_http_and_tasks() {
        let list = with_kinds(&[
            ("redis", kind("redis-server", SERVICE, &[])),
            ("migrate", kind("bin/rails db:prepare", TASK, &[])),
            ("drop", kind("dropdb x", Some(ProcessKind::Teardown), &[])),
        ])
        .unwrap();
        let restarts: Vec<_> = list
            .processes
            .iter()
            .chain(&list.teardown)
            .map(|p| (p.name.as_str(), p.restart))
            .collect();
        assert_eq!(
            restarts,
            [
                ("web", Restart::Never),
                ("worker", Restart::OnFailure),
                ("migrate", Restart::Never),
                ("redis", Restart::OnFailure),
                ("drop", Restart::Never),
            ]
        );

        let never = ProcessOverride {
            restart: Some(Restart::Never),
            ..Default::default()
        };
        let list = with_kinds(&[("worker", never)]).unwrap();
        assert_eq!(list.processes[1].restart, Restart::Never);
        let list = with_kinds(&[(
            "migrate",
            ProcessOverride {
                restart: Some(Restart::Never),
                ..kind("bin/rails db:prepare", TASK, &[])
            },
        )])
        .unwrap();
        assert_eq!(list.processes[2].name, "migrate");
        assert_eq!(list.processes[2].restart, Restart::Never);
    }

    #[test]
    fn restart_is_refused_where_it_cannot_apply() {
        let on_failure = |ov: ProcessOverride| ProcessOverride {
            restart: Some(Restart::OnFailure),
            ..ov
        };
        let err = with_kinds(&[(
            "web",
            ProcessOverride {
                restart: Some(Restart::Never),
                ..Default::default()
            },
        )])
        .unwrap_err();
        assert!(matches!(err, ProcfileError::RestartOnHttp { .. }), "{err}");
        let err = with_kinds(&[(
            "migrate",
            on_failure(kind("bin/rails db:prepare", TASK, &[])),
        )])
        .unwrap_err();
        assert!(matches!(err, ProcfileError::RestartOnTask { .. }), "{err}");
        let err = with_kinds(&[(
            "drop",
            ProcessOverride {
                restart: Some(Restart::Never),
                ..kind("dropdb x", Some(ProcessKind::Teardown), &[])
            },
        )])
        .unwrap_err();
        assert!(
            matches!(err, ProcfileError::RestartOnTeardown { .. }),
            "{err}"
        );
    }

    #[test]
    fn the_http_process_has_no_kind() {
        let web = ProcessOverride {
            kind: SERVICE,
            ..Default::default()
        };
        let err = with_kinds(&[("web", web)]).unwrap_err();
        assert!(matches!(err, ProcfileError::KindOnHttp { .. }), "{err}");
    }

    #[test]
    fn after_names_a_process_of_the_same_tier_without_cycles() {
        let err = with_kinds(&[("worker", kind("bin/jobs", None, &["ghost"]))]).unwrap_err();
        assert!(matches!(err, ProcfileError::UnknownAfter { .. }), "{err}");

        let err = with_kinds(&[
            ("redis", kind("redis-server", SERVICE, &[])),
            ("worker", kind("bin/jobs", None, &["redis"])),
        ])
        .unwrap_err();
        assert!(matches!(err, ProcfileError::AfterOtherTier { .. }), "{err}");

        let err = with_kinds(&[
            ("a", kind("a", TASK, &["b"])),
            ("b", kind("b", TASK, &["c"])),
            ("c", kind("c", TASK, &["a"])),
        ])
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "processes start after each other in a cycle: a -> b -> c -> a"
        );

        let err = with_kinds(&[("worker", kind("bin/jobs", None, &["worker"]))]).unwrap_err();
        assert!(matches!(err, ProcfileError::AfterCycle { .. }), "{err}");
    }

    const TEARDOWN: Option<ProcessKind> = Some(ProcessKind::Teardown);

    #[test]
    fn teardowns_are_kept_apart_from_what_starts() {
        let checkout = ProcessOverride {
            scope: Some(Scope::Checkout),
            ..kind("dropdb x", TEARDOWN, &[])
        };
        let list = with_kinds(&[("drop", checkout)]).unwrap();
        assert_eq!(names(&list), vec!["web", "worker"]);
        assert_eq!(list.teardown.len(), 1);
        assert_eq!(list.teardown[0].name, "drop");
        assert_eq!(list.teardown[0].tier(), 4);

        let err = with_kinds(&[("drop", kind("dropdb x", TEARDOWN, &["web"]))]).unwrap_err();
        assert!(matches!(err, ProcfileError::TeardownAfter { .. }), "{err}");
        let err = with_kinds(&[
            ("drop", kind("dropdb x", TEARDOWN, &[])),
            ("worker", kind("bin/jobs", None, &["drop"])),
        ])
        .unwrap_err();
        assert!(matches!(err, ProcfileError::AfterOtherTier { .. }), "{err}");
        let web = ProcessOverride {
            kind: TEARDOWN,
            ..Default::default()
        };
        let err = with_kinds(&[("web", web)]).unwrap_err();
        assert!(matches!(err, ProcfileError::KindOnHttp { .. }), "{err}");
    }

    fn paths(list: &[&str]) -> Option<Vec<String>> {
        Some(list.iter().map(|p| p.to_string()).collect())
    }

    #[test]
    fn a_process_with_paths_runs_per_instance_and_is_always_restarted() {
        let base = vec![
            ("web".into(), "bin/rails server".into()),
            ("assets".into(), "bin/assets --watch --port 3036".into()),
        ];
        let s = settings(
            false,
            &[(
                "assets",
                ProcessOverride {
                    paths: paths(&["/assets/"]),
                    ..Default::default()
                },
            )],
        );
        let list = build(ProcessSource::ProcfileDev, base, &s).unwrap();
        let assets = &list.processes[1];
        assert_eq!(assets.paths, ["/assets/"]);
        assert_eq!(assets.scope, Scope::Instance);
        assert_eq!(assets.restart, Restart::Never);
        assert_eq!(assets.command, "bin/assets --watch --port $PORT");
        assert!(assets.is_checked());
    }

    #[test]
    fn matches_paths_by_whole_segments() {
        let spec = build(
            ProcessSource::Config,
            vec![("assets".into(), "x".into())],
            &settings(
                false,
                &[(
                    "assets",
                    ProcessOverride {
                        paths: paths(&["/vite-dev/", "/vite-dev/@fs"]),
                        ..Default::default()
                    },
                )],
            ),
        )
        .unwrap()
        .processes
        .remove(0);
        assert_eq!(spec.path_match("/vite-dev/@vite/client"), Some(9));
        assert_eq!(spec.path_match("/vite-dev"), Some(9));
        assert_eq!(spec.path_match("/vite-dev?x=1"), Some(9));
        assert_eq!(spec.path_match("/vite-dev/@fs/src/app.ts"), Some(13));
        assert_eq!(spec.path_match("/vite-development"), None);
        assert_eq!(spec.path_match("/"), None);
    }

    #[test]
    fn rejects_paths_where_they_cant_work() {
        let check = |entries: &[(&str, ProcessOverride)]| {
            let base = vec![
                ("web".into(), "bin/rails server".into()),
                ("assets".into(), "bin/assets".into()),
                ("other".into(), "bin/other".into()),
            ];
            build(ProcessSource::Procfile, base, &settings(false, entries)).unwrap_err()
        };
        let with = |ov: ProcessOverride| ProcessOverride {
            paths: paths(&["/assets/"]),
            ..ov
        };
        let err = check(&[("web", with(ProcessOverride::default()))]);
        assert!(matches!(err, ProcfileError::PathsOnHttp { .. }), "{err}");
        let err = check(&[(
            "assets",
            with(ProcessOverride {
                restart: Some(Restart::OnFailure),
                ..Default::default()
            }),
        )]);
        assert!(matches!(err, ProcfileError::RestartOnPaths { .. }), "{err}");
        let err = check(&[(
            "assets",
            with(ProcessOverride {
                kind: Some(ProcessKind::Service),
                ..Default::default()
            }),
        )]);
        assert!(matches!(err, ProcfileError::KindOnPaths { .. }), "{err}");
        let err = check(&[(
            "assets",
            with(ProcessOverride {
                scope: Some(Scope::Checkout),
                ..Default::default()
            }),
        )]);
        assert!(
            matches!(err, ProcfileError::CheckoutScopedPaths { .. }),
            "{err}"
        );
        let err = check(&[
            ("assets", with(ProcessOverride::default())),
            (
                "other",
                ProcessOverride {
                    paths: paths(&["/assets"]),
                    ..Default::default()
                },
            ),
        ]);
        assert_eq!(
            err.to_string(),
            "path \"/assets\" is served by both \"assets\" and \"other\""
        );
        let err = check(&[(
            "assets",
            ProcessOverride {
                paths: paths(&["/assets/", "/assets"]),
                ..Default::default()
            },
        )]);
        assert_eq!(
            err.to_string(),
            "process \"assets\" lists path \"/assets\" twice"
        );
    }

    #[test]
    fn a_detected_preset_gives_way_to_settings_that_rule_out_paths() {
        let base = || vec![("vite".into(), "bin/vite dev".into())];
        let checkout = ProcessOverride {
            scope: Some(Scope::Checkout),
            ..Default::default()
        };
        let list = build(
            ProcessSource::ProcfileDev,
            base(),
            &settings(false, &[("vite", checkout.clone())]),
        )
        .unwrap();
        let vite = &list.processes[0];
        assert!(vite.paths.is_empty());
        assert!(vite.provides.is_empty());
        assert_eq!(vite.preset, None);
        assert_eq!(vite.scope, Scope::Checkout);
        assert_eq!(
            list.notices,
            [Notice::PresetSkipped {
                process: "vite".into(),
                preset: Preset::ViteRuby,
                setting: "scope = \"checkout\"",
            }]
        );
        assert!(
            list.notices[0]
                .to_string()
                .contains("set preset = \"vite_ruby\"")
        );

        let restart = ProcessOverride {
            restart: Some(Restart::Never),
            ..Default::default()
        };
        let list = build(
            ProcessSource::ProcfileDev,
            base(),
            &settings(false, &[("vite", restart)]),
        )
        .unwrap();
        assert_eq!(list.processes[0].restart, Restart::Never);
        assert!(list.processes[0].paths.is_empty());

        let explicit = ProcessOverride {
            preset: Some(Preset::ViteRuby),
            ..checkout
        };
        let err = build(
            ProcessSource::ProcfileDev,
            base(),
            &settings(false, &[("vite", explicit)]),
        )
        .unwrap_err();
        assert!(
            matches!(err, ProcfileError::CheckoutScopedPaths { .. }),
            "{err}"
        );
    }

    #[test]
    fn detects_presets_from_the_command() {
        assert_eq!(detect_preset("bin/vite dev"), Some(Preset::ViteRuby));
        assert_eq!(
            detect_preset("./bin/vite dev --debug"),
            Some(Preset::ViteRuby)
        );
        assert_eq!(
            detect_preset("bundle exec vite dev"),
            Some(Preset::ViteRuby)
        );
        assert_eq!(
            detect_preset("FOO=1 bundle exec vite dev"),
            Some(Preset::ViteRuby)
        );
        assert_eq!(
            detect_preset("exec bin/bundle exec vite dev"),
            Some(Preset::ViteRuby)
        );
        assert_eq!(detect_preset("bin/webpacker-dev-server"), None);
        assert_eq!(detect_preset("bin/webpack-dev-server"), None);
        assert_eq!(detect_preset("vite dev"), None);
        assert_eq!(detect_preset("bin/vite build --watch"), None);
        assert_eq!(
            detect_preset("bin/shakapacker-dev-server"),
            Some(Preset::Shakapacker)
        );
        assert_eq!(
            detect_preset("./bin/shakapacker-dev-server"),
            Some(Preset::Shakapacker)
        );
        assert_eq!(detect_preset("bin/shakapacker --watch"), None);
    }

    #[test]
    fn presets_fill_in_paths_and_the_port_under_config() {
        let base = vec![
            ("web".into(), "bin/rails server".into()),
            ("vite".into(), "bin/vite dev".into()),
            ("webpack".into(), "bin/shakapacker-dev-server".into()),
            ("custom".into(), "bin/my-vite".into()),
        ];
        let custom_paths = paths(&["/custom/"]);
        let s = settings(
            false,
            &[
                (
                    "webpack",
                    ProcessOverride {
                        paths: paths(&["/packs/"]),
                        ..Default::default()
                    },
                ),
                (
                    "custom",
                    ProcessOverride {
                        preset: Some(Preset::ViteRuby),
                        paths: custom_paths,
                        provides: vec![EnvTemplate {
                            key: "VITE_RUBY_PORT".into(),
                            value: "4000".into(),
                        }],
                        ..Default::default()
                    },
                ),
            ],
        );
        let list = build(ProcessSource::ProcfileDev, base, &s).unwrap();
        let [web, vite, webpack, custom] = &list.processes[..] else {
            unreachable!()
        };
        assert!(web.paths.is_empty());
        assert_eq!(vite.paths, ["/vite-dev/"]);
        let port = |key: &str| EnvTemplate {
            key: key.into(),
            value: "{{port}}".into(),
        };
        assert_eq!(vite.provides, [port("VITE_RUBY_PORT")]);
        assert_eq!(webpack.paths, ["/packs/"]);
        assert_eq!(webpack.provides, [port("SHAKAPACKER_DEV_SERVER_PORT")]);
        assert_eq!(custom.paths, ["/custom/"]);
        assert_eq!(custom.provides[0].value, "4000");

        let off = settings(
            false,
            &[(
                "vite",
                ProcessOverride {
                    preset: Some(Preset::None),
                    ..Default::default()
                },
            )],
        );
        let list = build(
            ProcessSource::ProcfileDev,
            vec![("vite".into(), "bin/vite dev".into())],
            &off,
        )
        .unwrap();
        assert!(list.processes[0].paths.is_empty());
        assert!(list.processes[0].provides.is_empty());
    }

    #[test]
    fn an_app_with_only_a_teardown_has_nothing_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(false, &[("drop", kind("dropdb x", TEARDOWN, &[]))]);
        assert!(matches!(
            load(dir.path(), &s),
            Err(ProcfileError::NotRunnable { .. })
        ));
        let teardowns = teardowns(dir.path(), &s).unwrap();
        assert_eq!(teardowns.len(), 1);
        assert_eq!(teardowns[0].command, "dropdb x");
    }
}
