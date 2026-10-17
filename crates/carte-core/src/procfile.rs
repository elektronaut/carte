//! The process list for one checkout: Procfile, then config.

use std::fmt;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::model::{AppSettings, ProcessKind, ProcessSpec, Readiness, Scope};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSource {
    ProcfileDev,
    Procfile,
    /// No Procfile, but a `config.ru`: `web` runs puma.
    ConfigRu,
    /// Only processes from config.
    Config,
    /// A fixed-port app without processes.
    RouteOnly,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessList {
    pub source: ProcessSource,
    pub processes: Vec<ProcessSpec>,
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
    #[error("{} has no Procfile.dev, Procfile or config.ru, and no processes are configured", path.display())]
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
    if settings.port.is_some() {
        return Ok(ProcessList {
            source: ProcessSource::RouteOnly,
            processes: Vec::new(),
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
    } else {
        (ProcessSource::Config, Vec::new())
    };

    let mut list = build(source, base, settings)?;
    notices.append(&mut list.notices);
    list.notices = notices;
    if list.processes.is_empty() {
        return Err(ProcfileError::NotRunnable {
            path: checkout.to_path_buf(),
        });
    }
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
            let ov = config.entries.get(&name).cloned().unwrap_or_default();
            let http = ov.http.unwrap_or(name == "web" && !explicit_http);
            if http && let Some(rewritten) = rewrite_port(&command) {
                notices.push(Notice::PortRewritten {
                    process: name.clone(),
                    from: command.clone(),
                    to: rewritten.clone(),
                });
                command = rewritten;
            }
            let scope = ov.scope.unwrap_or(
                if !http && ov.kind.is_none() && is_watcher(&name, &command) {
                    Scope::Checkout
                } else {
                    Scope::Instance
                },
            );
            ProcessSpec {
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
                name,
                command,
            }
        })
        .collect();

    let http: Vec<String> = processes
        .iter()
        .filter(|p| p.http)
        .map(|p| p.name.clone())
        .collect();
    if http.len() > 1 {
        return Err(ProcfileError::MultipleHttp { processes: http });
    }
    check_kinds(&processes)?;
    Ok(ProcessList {
        source,
        processes,
        notices,
    })
}

fn check_kinds(processes: &[ProcessSpec]) -> Result<(), ProcfileError> {
    for p in processes {
        if p.http && p.kind.is_some() {
            return Err(ProcfileError::KindOnHttp {
                process: p.name.clone(),
            });
        }
        if let Some(kind) = p.kind
            && p.scope == Scope::Checkout
        {
            return Err(ProcfileError::CheckoutScopedKind {
                process: p.name.clone(),
                kind: match kind {
                    ProcessKind::Service => "service",
                    ProcessKind::Task => "task",
                },
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
    use crate::model::{Policy, ProcessConfig, ProcessOverride};

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
}
