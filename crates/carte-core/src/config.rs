//! Parsing of the machine config, project files and repo-level `carte.toml`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use thiserror::Error;

use crate::model::{
    App, AppSettings, EnvProviderKind, EnvTemplate, Limits, MachineConfig, MemoryTarget, Policy,
    Problem, ProcessConfig, ProcessKind, ProcessOverride, Project, Readiness, Scheme, Scope,
    ShareConfig,
};
use crate::{duration, slug};

/// The repo-level config file name.
pub const REPO_CONFIG: &str = "carte.toml";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}{}: {message}", path.display(), location.as_deref().unwrap_or(""))]
    Parse {
        path: PathBuf,
        /// `:line:column`, when known.
        location: Option<String>,
        message: String,
    },
    #[error("{}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
}

impl ConfigError {
    pub fn path(&self) -> &Path {
        match self {
            ConfigError::Io { path, .. }
            | ConfigError::Parse { path, .. }
            | ConfigError::Invalid { path, .. } => path,
        }
    }

    fn parse(path: &Path, text: &str, err: toml::de::Error) -> Self {
        let Some(span) = err.span() else {
            return ConfigError::Parse {
                path: path.to_path_buf(),
                location: None,
                message: err.message().to_string(),
            };
        };
        let start = span.start.min(text.len());
        let before = &text[..start];
        let line_index = before.matches('\n').count();
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let column = before[line_start..].chars().count() + 1;
        let line = text.lines().nth(line_index).unwrap_or("").trim();
        let table = text
            .lines()
            .take(line_index)
            .map(str::trim)
            .filter(|l| l.starts_with('['))
            .last();
        let mut message = err.message().trim_end().to_string();
        if let Some(table) = table
            && !line.starts_with('[')
        {
            message.push_str(&format!(" (in {table})"));
        }
        if !line.is_empty() {
            message.push_str(&format!(" at `{line}`"));
        }
        ConfigError::Parse {
            path: path.to_path_buf(),
            location: Some(format!(":{}:{column}", line_index + 1)),
            message,
        }
    }
}

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn from_toml<'a, T: Deserialize<'a>>(path: &Path, text: &'a str) -> Result<T, ConfigError> {
    toml::from_str(text).map_err(|err| ConfigError::parse(path, text, err))
}

// Machine config

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawMachine {
    tld: Option<String>,
    http_port: Option<u16>,
    https_port: Option<u16>,
    dns_port: Option<u16>,
    idle_timeout: Option<DurationValue>,
    #[serde(default)]
    limits: RawLimits,
    share: Option<RawShare>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    memory_target: Option<MemoryTargetValue>,
    instance_target: Option<u32>,
    hard_ceiling: Option<u32>,
    pty_reserve: Option<u32>,
    log_buffer: Option<ByteSize>,
    log_budget: Option<ByteSize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawShare {
    #[serde(default = "default_share_provider")]
    provider: String,
    domain: String,
    #[serde(default = "default_share_tunnel")]
    tunnel: String,
    #[serde(default = "default_share_port")]
    port: u16,
}

fn default_share_tunnel() -> String {
    "carte".into()
}

fn default_share_port() -> u16 {
    9254
}

fn default_share_provider() -> String {
    "cloudflare".into()
}

/// Parses `config.toml`.
pub fn parse_machine_config(path: &Path, text: &str) -> Result<MachineConfig, ConfigError> {
    let raw: RawMachine = from_toml(path, text)?;
    let defaults = MachineConfig::default();
    let limit_defaults = Limits::default();
    let to_usize = |size: Option<ByteSize>, default: usize| {
        size.map_or(Ok(default), |ByteSize(n)| {
            usize::try_from(n).map_err(|_| ConfigError::Invalid {
                path: path.to_path_buf(),
                message: format!("log size {n} is too large"),
            })
        })
    };
    Ok(MachineConfig {
        tld: raw.tld.unwrap_or(defaults.tld),
        http_port: raw.http_port.unwrap_or(defaults.http_port),
        https_port: raw.https_port.unwrap_or(defaults.https_port),
        dns_port: raw.dns_port.unwrap_or(defaults.dns_port),
        idle_timeout: raw.idle_timeout.map_or(defaults.idle_timeout, |d| d.0),
        limits: Limits {
            memory_target: raw
                .limits
                .memory_target
                .map_or(limit_defaults.memory_target, |m| m.0),
            instance_target: raw.limits.instance_target,
            hard_ceiling: raw
                .limits
                .hard_ceiling
                .unwrap_or(limit_defaults.hard_ceiling),
            pty_reserve: raw.limits.pty_reserve.unwrap_or(limit_defaults.pty_reserve),
            log_buffer_bytes: to_usize(raw.limits.log_buffer, limit_defaults.log_buffer_bytes)?,
            log_budget_bytes: to_usize(raw.limits.log_budget, limit_defaults.log_budget_bytes)?,
        },
        share: raw.share.map(|s| ShareConfig {
            provider: s.provider,
            domain: s.domain,
            tunnel: s.tunnel,
            port: s.port,
        }),
    })
}

/// Reads `config.toml`; a missing file means defaults.
pub fn load_machine_config(path: &Path) -> Result<MachineConfig, ConfigError> {
    if !path.exists() {
        return Ok(MachineConfig::default());
    }
    parse_machine_config(path, &read(path)?)
}

// Project files

/// A parsed project file, before its repos are discovered.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectFile {
    pub name: String,
    pub path: PathBuf,
    /// Paths or globs, unexpanded.
    pub repos: Vec<String>,
    pub strip_prefix: Option<String>,
    pub env_provider: Option<EnvProviderKind>,
    pub env: Vec<EnvTemplate>,
    pub requires: Vec<String>,
    pub scheme: Scheme,
    pub idle_timeout: Option<Duration>,
    /// `[apps.<full name>]` tables.
    pub apps: BTreeMap<String, AppConfig>,
    /// `[processes.<name>]` tables, declared for many apps at once.
    pub processes: BTreeMap<String, ProjectProcess>,
}

/// A process declared in a project file's `[processes]`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProjectProcess {
    /// The apps it's declared for, by full or short name; `None` for every app.
    pub apps: Option<Vec<String>>,
    pub process: ProcessOverride,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    #[serde(default)]
    repos: OneOrMany,
    strip_prefix: Option<String>,
    env_provider: Option<EnvProviderKind>,
    #[serde(default)]
    env: EnvTable,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    scheme: Scheme,
    idle_timeout: Option<DurationValue>,
    #[serde(default)]
    apps: BTreeMap<String, AppConfig>,
    #[serde(default, deserialize_with = "project_process_table")]
    processes: BTreeMap<String, ProjectProcess>,
}

/// Per-app settings as written in a project file's `[apps.X]` table or a repo's `carte.toml`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    /// Short-name override.
    pub name: Option<String>,
    #[serde(default, deserialize_with = "env_table")]
    pub env: Vec<EnvTemplate>,
    pub env_files: Option<Vec<String>>,
    pub depends_on: Option<Vec<String>>,
    pub aliases: Option<Vec<String>>,
    pub subdomains: Option<bool>,
    pub policy: Option<Policy>,
    pub port: Option<u16>,
    pub replace_processes: Option<bool>,
    #[serde(default, deserialize_with = "process_table")]
    pub processes: BTreeMap<String, ProcessOverride>,
}

impl AppConfig {
    /// Merges `self` over `base` key by key: scalars and lists from `self` win,
    /// env entries and processes are merged.
    pub fn merged_over(self, base: AppConfig) -> AppConfig {
        let mut processes = base.processes;
        for (name, over) in self.processes {
            let merged = match processes.remove(&name) {
                Some(base) => merge_process(over, base),
                None => over,
            };
            processes.insert(name, merged);
        }
        AppConfig {
            name: self.name.or(base.name),
            env: merge_env(base.env, self.env),
            env_files: self.env_files.or(base.env_files),
            depends_on: self.depends_on.or(base.depends_on),
            aliases: self.aliases.or(base.aliases),
            subdomains: self.subdomains.or(base.subdomains),
            policy: self.policy.or(base.policy),
            port: self.port.or(base.port),
            replace_processes: self.replace_processes.or(base.replace_processes),
            processes,
        }
    }

    pub fn into_settings(self) -> AppSettings {
        AppSettings {
            env: self.env,
            env_files: self.env_files.unwrap_or_default(),
            depends_on: self.depends_on.unwrap_or_default(),
            depends_on_elsewhere: Vec::new(),
            unresolved_deps: Vec::new(),
            aliases: self.aliases.unwrap_or_default(),
            subdomains: self.subdomains.unwrap_or(false),
            policy: self.policy,
            port: self.port,
            processes: ProcessConfig {
                replace: self.replace_processes.unwrap_or(false),
                entries: self.processes,
            },
        }
    }
}

fn merge_env(base: Vec<EnvTemplate>, over: Vec<EnvTemplate>) -> Vec<EnvTemplate> {
    let mut merged = base;
    for entry in over {
        match merged.iter_mut().find(|e| e.key == entry.key) {
            Some(existing) => existing.value = entry.value,
            None => merged.push(entry),
        }
    }
    merged
}

fn merge_process(over: ProcessOverride, base: ProcessOverride) -> ProcessOverride {
    ProcessOverride {
        command: over.command.or(base.command),
        cwd: over.cwd.or(base.cwd),
        env: merge_env(base.env, over.env),
        http: over.http.or(base.http),
        ready: over.ready.or(base.ready),
        scope: over.scope.or(base.scope),
        pty: over.pty.or(base.pty),
        policy: over.policy.or(base.policy),
        kind: over.kind.or(base.kind),
        after: over.after.or(base.after),
        provides: merge_env(base.provides, over.provides),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProcess {
    command: Option<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: EnvTable,
    http: Option<bool>,
    ready: Option<ReadyValue>,
    scope: Option<Scope>,
    pty: Option<bool>,
    policy: Option<Policy>,
    kind: Option<ProcessKind>,
    after: Option<Vec<String>>,
    #[serde(default)]
    provides: EnvTable,
    apps: Option<Vec<String>>,
}

impl RawProcess {
    fn into_project_process<E: de::Error>(self, name: &str) -> Result<ProjectProcess, E> {
        if let Some(entry) = self.provides.0.iter().find(|e| e.key.contains("{{")) {
            return Err(E::custom(format!(
                "process {name:?} provides {:?}: names in `provides` can't be templates",
                entry.key
            )));
        }
        Ok(ProjectProcess {
            apps: self.apps,
            process: ProcessOverride {
                command: self.command,
                cwd: self.cwd,
                env: self.env.0,
                http: self.http,
                ready: self.ready.map(|r| r.0),
                scope: self.scope,
                pty: self.pty,
                policy: self.policy,
                kind: self.kind,
                after: self.after,
                provides: self.provides.0,
            },
        })
    }
}

fn process_table<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, ProcessOverride>, D::Error> {
    let raw = BTreeMap::<String, RawProcess>::deserialize(d)?;
    raw.into_iter()
        .map(|(name, p)| {
            let declared = p.into_project_process::<D::Error>(&name)?;
            if declared.apps.is_some() {
                return Err(de::Error::custom(format!(
                    "process {name:?}: `apps` only applies to a project's [processes.{name}]"
                )));
            }
            Ok((name, declared.process))
        })
        .collect()
}

fn project_process_table<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, ProjectProcess>, D::Error> {
    let raw = BTreeMap::<String, RawProcess>::deserialize(d)?;
    raw.into_iter()
        .map(|(name, p)| {
            let declared = p.into_project_process::<D::Error>(&name)?;
            Ok((name, declared))
        })
        .collect()
}

/// Parses a project file. The project's name is the file stem.
pub fn parse_project(path: &Path, text: &str) -> Result<ProjectFile, ConfigError> {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    slug::validate_project_name(&name).map_err(|err| ConfigError::Invalid {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    let raw: RawProject = from_toml(path, text)?;
    Ok(ProjectFile {
        name,
        path: path.to_path_buf(),
        repos: raw.repos.0,
        strip_prefix: raw.strip_prefix.filter(|p| !p.is_empty()),
        env_provider: raw.env_provider,
        env: raw.env.0,
        requires: raw.requires,
        scheme: raw.scheme,
        idle_timeout: raw.idle_timeout.map(|d| d.0),
        apps: raw.apps,
        processes: raw.processes,
    })
}

pub fn load_project_file(path: &Path) -> Result<ProjectFile, ConfigError> {
    parse_project(path, &read(path)?)
}

/// Parses a repo's `carte.toml`, which has the shape of one `[apps.X]` table.
pub fn parse_repo_config(path: &Path, text: &str) -> Result<AppConfig, ConfigError> {
    from_toml(path, text)
}

/// Reads `carte.toml` from a checkout, if it has one.
pub fn load_repo_config(checkout: &Path) -> Result<Option<AppConfig>, ConfigError> {
    let path = checkout.join(REPO_CONFIG);
    if !path.is_file() {
        return Ok(None);
    }
    parse_repo_config(&path, &read(&path)?).map(Some)
}

/// An app directory found by expanding a project's `repos`.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredApp {
    /// The directory name.
    pub name: String,
    pub path: PathBuf,
    pub repo_config: Option<AppConfig>,
}

/// Builds a project from its file and discovered app directories, merging each
/// app's `carte.toml`, the project's `[processes]` and its `[apps.X]` table, in
/// that order, and validating names and references. Apps that fail validation
/// are left out and reported.
pub fn assemble_project(
    file: ProjectFile,
    discovered: Vec<DiscoveredApp>,
) -> (Project, Vec<Problem>) {
    let ProjectFile {
        name: project_name,
        path: config_path,
        strip_prefix,
        env_provider,
        env,
        requires,
        scheme,
        idle_timeout,
        apps: mut app_tables,
        processes: project_processes,
        repos: _,
    } = file;
    let mut problems = Vec::new();
    let problem = |app: &str, message: String| Problem {
        project: Some(project_name.clone()),
        app: Some(app.to_string()),
        message,
    };

    let mut candidates: Vec<(String, PathBuf, AppConfig, Option<AppConfig>)> = discovered
        .into_iter()
        .map(|d| {
            let table = app_tables.remove(&d.name).unwrap_or_default();
            (d.name, d.path, table, d.repo_config)
        })
        .collect();
    for (name, table) in std::mem::take(&mut app_tables) {
        if table.port.is_some() {
            candidates.push((name, PathBuf::new(), table, None));
        } else {
            problems.push(problem(
                &name,
                format!("[apps.{name}] does not match any repo in the project"),
            ));
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let short_names: Vec<(String, String)> = candidates
        .iter()
        .map(|(name, _, table, repo)| {
            let override_name = table
                .name
                .as_deref()
                .or(repo.as_ref().and_then(|r| r.name.as_deref()));
            let short = short_name(name, override_name, strip_prefix.as_deref());
            (name.clone(), short)
        })
        .collect();
    let mut declared: Vec<(String, Option<BTreeSet<String>>, ProcessOverride)> = Vec::new();
    for (process, ProjectProcess { apps, process: ov }) in project_processes {
        let targets = apps.map(|entries| {
            let mut targets = BTreeSet::new();
            for entry in entries {
                let target = short_names
                    .iter()
                    .find(|(n, _)| *n == entry)
                    .or_else(|| short_names.iter().find(|(_, s)| *s == entry));
                match target {
                    Some((full, _)) => {
                        targets.insert(full.clone());
                    }
                    None => problems.push(Problem {
                        project: Some(project_name.clone()),
                        app: None,
                        message: format!("[processes.{process}] lists unknown app {entry:?}"),
                    }),
                }
            }
            targets
        });
        declared.push((process, targets, ov));
    }
    let candidates = candidates.into_iter().map(|(name, path, table, repo)| {
        let project_wide = AppConfig {
            processes: declared
                .iter()
                .filter(|(_, targets, _)| targets.as_ref().is_none_or(|t| t.contains(&name)))
                .map(|(process, _, ov)| (process.clone(), ov.clone()))
                .collect(),
            ..AppConfig::default()
        };
        let config = table.merged_over(project_wide.merged_over(repo.unwrap_or_default()));
        (name, path, config)
    });

    let mut apps: Vec<App> = Vec::new();
    for (name, path, mut config) in candidates {
        if apps.iter().any(|a| a.name == name) {
            problems.push(problem(&name, format!("app {name:?} is found twice")));
            continue;
        }
        let short = short_name(
            &name,
            config.name.take().as_deref(),
            strip_prefix.as_deref(),
        );
        if let Err(err) = slug::validate_app_short_name(&short) {
            problems.push(problem(&name, err.to_string()));
            continue;
        }
        if let Some(other) = apps.iter().find(|a| a.short == short) {
            problems.push(problem(
                &name,
                format!("app name {short:?} is already used by {}", other.name),
            ));
            continue;
        }
        apps.push(App {
            name,
            short,
            path,
            settings: config.into_settings(),
        });
    }

    let names: Vec<(String, String)> = apps
        .iter()
        .map(|a| (a.name.clone(), a.short.clone()))
        .collect();
    let mut seen_aliases = BTreeSet::new();
    for app in &mut apps {
        let mut depends_on = Vec::new();
        for dep in std::mem::take(&mut app.settings.depends_on) {
            let target = names
                .iter()
                .find(|(n, _)| *n == dep)
                .or_else(|| names.iter().find(|(_, s)| *s == dep));
            match target {
                Some((full, _)) if *full == app.name => {
                    problems.push(problem(&app.name, "an app cannot depend on itself".into()));
                }
                Some((full, _)) => {
                    if !depends_on.contains(full) {
                        depends_on.push(full.clone());
                    }
                }
                None => app.settings.unresolved_deps.push(dep),
            }
        }
        app.settings.depends_on = depends_on;

        let mut aliases = Vec::new();
        for alias in std::mem::take(&mut app.settings.aliases) {
            if let Err(message) = check_hostname(&alias) {
                problems.push(problem(&app.name, format!("alias {alias:?}: {message}")));
            } else if !seen_aliases.insert(alias.clone()) {
                problems.push(problem(
                    &app.name,
                    format!("alias {alias:?} is used by more than one app"),
                ));
            } else {
                aliases.push(alias);
            }
        }
        app.settings.aliases = aliases;
    }

    let project = Project {
        folder: None,
        name: project_name,
        config_path,
        strip_prefix,
        env_provider,
        env,
        requires,
        scheme,
        idle_timeout,
        apps,
    };
    (project, problems)
}

/// The hostname label of an app: its `name` override, else the directory name
/// without the project's prefix.
fn short_name(name: &str, override_name: Option<&str>, strip_prefix: Option<&str>) -> String {
    match override_name {
        Some(short) => short.to_string(),
        None => strip_prefix
            .and_then(|p| name.strip_prefix(p))
            .filter(|s| !s.is_empty())
            .unwrap_or(name)
            .to_string(),
    }
}

fn check_dns_label(label: &str) -> Result<(), String> {
    if label.is_empty() {
        return Err("must not be empty".into());
    }
    if label.len() > 63 {
        return Err("must be at most 63 characters".into());
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("may only contain lowercase letters, digits and dashes".into());
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err("must not start or end with a dash".into());
    }
    Ok(())
}

fn check_hostname(name: &str) -> Result<(), String> {
    name.split('.').try_for_each(check_dns_label)
}

// Value types

/// A duration string like `"15m"`.
struct DurationValue(Duration);

impl<'de> Deserialize<'de> for DurationValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        duration::parse(&s)
            .map(DurationValue)
            .map_err(de::Error::custom)
    }
}

/// `"port"` or an HTTP path such as `"/up"`.
struct ReadyValue(Readiness);

impl<'de> Deserialize<'de> for ReadyValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "port" {
            Ok(ReadyValue(Readiness::Port))
        } else if s.starts_with('/') {
            Ok(ReadyValue(Readiness::Http(s)))
        } else {
            Err(de::Error::custom(format!(
                "invalid ready check {s:?}: use \"port\" or an HTTP path like \"/up\""
            )))
        }
    }
}

/// A byte count: an integer, or a string like `"256MB"` (binary units).
struct ByteSize(u64);

impl ByteSize {
    fn parse(s: &str) -> Option<u64> {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (digits, unit) = s.split_at(split);
        let n: u64 = digits.parse().ok()?;
        let exp = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 0,
            "k" | "kb" | "kib" => 1,
            "m" | "mb" | "mib" => 2,
            "g" | "gb" | "gib" => 3,
            "t" | "tb" | "tib" => 4,
            _ => return None,
        };
        n.checked_mul(1024u64.pow(exp))
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = ByteSize;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a size like \"256MB\" or a number of bytes")
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ByteSize, E> {
                u64::try_from(v)
                    .map(ByteSize)
                    .map_err(|_| E::custom("size must not be negative"))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ByteSize, E> {
                Ok(ByteSize(v))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ByteSize, E> {
                ByteSize::parse(v).map(ByteSize).ok_or_else(|| {
                    E::custom(format!(
                        "invalid size {v:?}: use a number with B, KB, MB, GB or TB"
                    ))
                })
            }
        }
        d.deserialize_any(V)
    }
}

/// `"50%"` of RAM, or a size like `"16GB"`.
struct MemoryTargetValue(MemoryTarget);

impl<'de> Deserialize<'de> for MemoryTargetValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = MemoryTargetValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a percentage of RAM like \"50%\" or a size like \"16GB\"")
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                ByteSize::deserialize(de::value::I64Deserializer::<E>::new(v))
                    .map(|b| MemoryTargetValue(MemoryTarget::Bytes(b.0)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if let Some(percent) = v.trim().strip_suffix('%') {
                    return match percent.trim().parse::<u8>() {
                        Ok(p) if (1..=100).contains(&p) => {
                            Ok(MemoryTargetValue(MemoryTarget::PercentOfRam(p)))
                        }
                        _ => Err(E::custom(format!(
                            "invalid memory target {v:?}: the percentage must be 1-100"
                        ))),
                    };
                }
                ByteSize::parse(v)
                    .map(|b| MemoryTargetValue(MemoryTarget::Bytes(b)))
                    .ok_or_else(|| {
                        E::custom(format!(
                            "invalid memory target {v:?}: use a percentage like \"50%\" or a size like \"16GB\""
                        ))
                    })
            }
        }
        d.deserialize_any(V)
    }
}

/// A string or a list of strings.
#[derive(Default)]
struct OneOrMany(Vec<String>);

impl<'de> Deserialize<'de> for OneOrMany {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OneOrMany;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a path or glob, or a list of them")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<OneOrMany, E> {
                Ok(OneOrMany(vec![v.to_string()]))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<OneOrMany, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element::<String>()? {
                    items.push(item);
                }
                Ok(OneOrMany(items))
            }
        }
        d.deserialize_any(V)
    }
}

/// An `[env]` table, in file order. Values may be strings, numbers or booleans.
#[derive(Default)]
struct EnvTable(Vec<EnvTemplate>);

fn env_table<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<EnvTemplate>, D::Error> {
    EnvTable::deserialize(d).map(|t| t.0)
}

impl<'de> Deserialize<'de> for EnvTable {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = EnvTable;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a table of environment variables")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<EnvTable, A::Error> {
                let mut entries: Vec<EnvTemplate> = Vec::new();
                while let Some((key, EnvValue(value))) = map.next_entry::<String, EnvValue>()? {
                    entries.retain(|e| e.key != key);
                    entries.push(EnvTemplate { key, value });
                }
                Ok(EnvTable(entries))
            }
        }
        d.deserialize_map(V)
    }
}

struct EnvValue(String);

impl<'de> Deserialize<'de> for EnvValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = EnvValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string, number or boolean")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<EnvValue, E> {
                Ok(EnvValue(v.to_string()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<EnvValue, E> {
                Ok(EnvValue(v.to_string()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<EnvValue, E> {
                Ok(EnvValue(v.to_string()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<EnvValue, E> {
                Ok(EnvValue(v.to_string()))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<EnvValue, E> {
                Ok(EnvValue(v.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(text: &str) -> Result<ProjectFile, ConfigError> {
        parse_project(Path::new("/c/projects/nova.toml"), text)
    }

    fn discovered(name: &str) -> DiscoveredApp {
        DiscoveredApp {
            name: name.into(),
            path: PathBuf::from(format!("/work/{name}")),
            repo_config: None,
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<EnvTemplate> {
        pairs
            .iter()
            .map(|(k, v)| EnvTemplate {
                key: k.to_string(),
                value: v.to_string(),
            })
            .collect()
    }

    #[test]
    fn machine_config_defaults_when_missing() {
        let config = load_machine_config(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(config, MachineConfig::default());
    }

    #[test]
    fn parses_machine_config() {
        let config = parse_machine_config(
            Path::new("config.toml"),
            r#"
            tld = "localhost"
            http_port = 8080
            idle_timeout = "30m"

            [limits]
            memory_target = "16GB"
            instance_target = 20
            hard_ceiling = 40
            pty_reserve = 32
            log_buffer = "2MB"
            log_budget = 1048576

            [share]
            domain = "dev.example.com"
            "#,
        )
        .unwrap();
        assert_eq!(config.tld, "localhost");
        assert_eq!(config.http_port, 8080);
        assert_eq!(config.https_port, 443);
        assert_eq!(config.idle_timeout, Duration::from_secs(1800));
        assert_eq!(
            config.limits,
            Limits {
                memory_target: MemoryTarget::Bytes(16 * 1024 * 1024 * 1024),
                instance_target: Some(20),
                hard_ceiling: 40,
                pty_reserve: 32,
                log_buffer_bytes: 2 * 1024 * 1024,
                log_budget_bytes: 1048576,
            }
        );
        assert_eq!(
            config.share,
            Some(ShareConfig {
                provider: "cloudflare".into(),
                domain: "dev.example.com".into(),
                tunnel: "carte".into(),
                port: 9254,
            })
        );
    }

    #[test]
    fn parses_memory_percentage() {
        let config =
            parse_machine_config(Path::new("c"), "[limits]\nmemory_target = \"40%\"").unwrap();
        assert_eq!(config.limits.memory_target, MemoryTarget::PercentOfRam(40));
        assert!(
            parse_machine_config(Path::new("c"), "[limits]\nmemory_target = \"140%\"").is_err()
        );
        assert!(
            parse_machine_config(Path::new("c"), "[limits]\nmemory_target = \"lots\"").is_err()
        );
    }

    #[test]
    fn machine_errors_point_at_file_and_key() {
        let err = parse_machine_config(
            Path::new("/c/config.toml"),
            "tld = \"test\"\n[limits]\nmemmory_target = \"50%\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.starts_with("/c/config.toml:3:1: unknown field `memmory_target`"),
            "{err}"
        );
        assert!(err.contains("(in [limits])"), "{err}");

        let err = parse_machine_config(Path::new("/c/config.toml"), "idle_timeout = \"15\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("/c/config.toml:1:"), "{err}");
        assert!(err.contains("invalid duration"), "{err}");
    }

    #[test]
    fn parses_project_file() {
        let file = project(
            r#"
            repos = "~/work/nova-*"
            strip_prefix = "nova-"
            env_provider = "mise"
            requires = ["tcp://localhost:5432"]
            scheme = "http"
            idle_timeout = "1h"

            [env]
            "{{app.name | upper}}_URL" = "{{url:app}}"
            DATABASE_URL = "postgres:///{{app.name}}_{{checkout}}"
            A_NUMBER = 3
            BEFORE = "z"

            [apps.nova-postcard-api]
            name = "api"
            env_files = [".env"]
            depends_on = ["nova-billing"]
            aliases = ["nova-postcard-api"]
            subdomains = true
            policy = "pinned"
            replace_processes = true

            [apps.nova-postcard-api.env]
            POSTCARD_API_URL = "{{url:nova-postcard-api}}"

            [apps.nova-postcard-api.processes.web]
            command = "bin/rails s"
            cwd = "backend"
            http = true
            ready = "/up"
            scope = "instance"
            pty = false
            policy = "disabled"
            env = { FOO = "bar" }
            "#,
        )
        .unwrap();
        assert_eq!(file.name, "nova");
        assert_eq!(file.repos, vec!["~/work/nova-*"]);
        assert_eq!(file.strip_prefix.as_deref(), Some("nova-"));
        assert_eq!(file.env_provider, Some(EnvProviderKind::Mise));
        assert_eq!(file.scheme, Scheme::Http);
        assert_eq!(file.idle_timeout, Some(Duration::from_secs(3600)));
        assert_eq!(
            file.env,
            env(&[
                ("{{app.name | upper}}_URL", "{{url:app}}"),
                ("DATABASE_URL", "postgres:///{{app.name}}_{{checkout}}"),
                ("A_NUMBER", "3"),
                ("BEFORE", "z"),
            ])
        );
        let app = &file.apps["nova-postcard-api"];
        assert_eq!(app.name.as_deref(), Some("api"));
        assert_eq!(app.replace_processes, Some(true));
        assert_eq!(app.policy, Some(Policy::Pinned));
        let web = &app.processes["web"];
        assert_eq!(web.ready, Some(Readiness::Http("/up".into())));
        assert_eq!(web.cwd, Some(PathBuf::from("backend")));
        assert_eq!(web.pty, Some(false));
        assert_eq!(web.policy, Some(Policy::Disabled));
        assert_eq!(web.env, env(&[("FOO", "bar")]));
    }

    #[test]
    fn repos_accepts_a_list() {
        let file = project("repos = [\"~/a\", \"/b/*\"]").unwrap();
        assert_eq!(file.repos, vec!["~/a", "/b/*"]);
    }

    #[test]
    fn project_errors_point_at_file_and_key() {
        let err =
            project("repos = \"x\"\n[apps.foo.processes.web]\ncommand = \"x\"\npty = \"yes\"\n")
                .unwrap_err()
                .to_string();
        assert!(err.starts_with("/c/projects/nova.toml:4:"), "{err}");
        assert!(err.contains("[apps.foo.processes.web]"), "{err}");
        assert!(err.contains("`pty = \"yes\"`"), "{err}");

        let err = project("repos = \"x\"\nenv = \"mise\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains(":2:"), "{err}");

        let err = project("[apps.foo]\nport = 3000\nbogus = 1\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `bogus`"), "{err}");

        let err = project("[apps.foo.processes.web]\nready = \"tcp\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid ready check"), "{err}");
    }

    #[test]
    fn parses_process_kinds_and_order() {
        let file = project(
            r#"
            [apps.foo.processes.redis]
            command = "redis-server --port $PORT"
            kind = "service"
            [apps.foo.processes.migrate]
            command = "bin/rails db:prepare"
            kind = "task"
            after = ["seed"]
            "#,
        )
        .unwrap();
        let processes = &file.apps["foo"].processes;
        assert_eq!(processes["redis"].kind, Some(ProcessKind::Service));
        assert_eq!(processes["migrate"].kind, Some(ProcessKind::Task));
        assert_eq!(processes["migrate"].after, Some(vec!["seed".into()]));

        let err = project("[apps.foo.processes.drop]\nkind = \"teardown\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown variant `teardown`"), "{err}");

        let repo = parse_repo_config(
            Path::new("carte.toml"),
            "[processes.redis]\ncommand = \"redis-server\"\nkind = \"service\"\n",
        )
        .unwrap();
        let merged = project("[apps.foo.processes.redis]\nafter = []\n")
            .unwrap()
            .apps["foo"]
            .clone()
            .merged_over(repo);
        assert_eq!(merged.processes["redis"].kind, Some(ProcessKind::Service));
        assert_eq!(merged.processes["redis"].after, Some(vec![]));
    }

    #[test]
    fn parses_project_processes() {
        let file = project(
            r#"
            [processes.redis]
            apps = ["nova-billing", "api"]
            command = "redis-server --port $PORT"
            kind = "service"
            [processes.redis.env]
            REDIS_ARGS = "--save ''"
            [processes.redis.provides]
            REDIS_URL = "unix://{{run_dir}}/redis.sock"
            [processes.mailcatcher]
            command = "mailcatcher -f"
            "#,
        )
        .unwrap();
        let redis = &file.processes["redis"];
        assert_eq!(
            redis.apps,
            Some(vec!["nova-billing".to_string(), "api".to_string()])
        );
        assert_eq!(redis.process.kind, Some(ProcessKind::Service));
        assert_eq!(redis.process.env, env(&[("REDIS_ARGS", "--save ''")]));
        assert_eq!(
            redis.process.provides,
            env(&[("REDIS_URL", "unix://{{run_dir}}/redis.sock")])
        );
        assert_eq!(file.processes["mailcatcher"].apps, None);
    }

    #[test]
    fn apps_filter_is_only_valid_in_project_processes() {
        let err = project("[apps.foo.processes.redis]\napps = [\"foo\"]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`apps` only applies to a project's"), "{err}");

        let err = parse_repo_config(
            Path::new("carte.toml"),
            "[processes.redis]\napps = [\"foo\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("`apps` only applies to a project's"), "{err}");

        let err = project("[processes.redis]\napp = [\"foo\"]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `app`"), "{err}");
    }

    #[test]
    fn provides_names_are_not_templates() {
        let err = project("[processes.redis.provides]\n\"{{app.name}}_URL\" = \"x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("can't be templates"), "{err}");
        let file = project("[apps.foo.processes.redis.provides]\nREDIS_URL = \"x\"\n").unwrap();
        assert_eq!(
            file.apps["foo"].processes["redis"].provides,
            env(&[("REDIS_URL", "x")])
        );
    }

    #[test]
    fn project_processes_sit_between_repo_config_and_app_tables() {
        let file = project(
            r#"
            [processes.redis]
            command = "redis-server --port $PORT"
            kind = "service"
            pty = false
            [processes.redis.env]
            A = "project-wide"
            B = "project-wide"
            [processes.redis.provides]
            REDIS_URL = "unix://{{run_dir}}/redis.sock"
            REDIS_DB = "0"

            [apps.app-one.processes.redis]
            command = "redis-server --port $PORT --appendonly yes"
            [apps.app-one.processes.redis.env]
            B = "app"
            [apps.app-one.processes.redis.provides]
            REDIS_DB = "1"
            "#,
        )
        .unwrap();
        let repo = parse_repo_config(
            Path::new("carte.toml"),
            r#"
            [processes.redis]
            command = "redis-server"
            ready = "port"
            pty = true
            policy = "disabled"
            [processes.redis.env]
            A = "repo"
            C = "repo"
            "#,
        )
        .unwrap();
        let mut one = discovered("app-one");
        one.repo_config = Some(repo.clone());
        let mut two = discovered("app-two");
        two.repo_config = Some(repo);
        let (project, problems) = assemble_project(file, vec![one, two]);
        assert!(problems.is_empty(), "{problems:?}");

        let redis = &project.app("app-one").unwrap().settings.processes.entries["redis"];
        assert_eq!(
            redis.command.as_deref(),
            Some("redis-server --port $PORT --appendonly yes")
        );
        assert_eq!(redis.kind, Some(ProcessKind::Service));
        assert_eq!(redis.pty, Some(false));
        assert_eq!(redis.policy, Some(Policy::Disabled));
        assert_eq!(
            redis.env,
            env(&[("A", "project-wide"), ("C", "repo"), ("B", "app")])
        );
        assert_eq!(
            redis.provides,
            env(&[
                ("REDIS_URL", "unix://{{run_dir}}/redis.sock"),
                ("REDIS_DB", "1")
            ])
        );

        let redis = &project.app("app-two").unwrap().settings.processes.entries["redis"];
        assert_eq!(redis.command.as_deref(), Some("redis-server --port $PORT"));
        assert_eq!(
            redis.env,
            env(&[("A", "project-wide"), ("C", "repo"), ("B", "project-wide")])
        );
        assert_eq!(
            redis.provides,
            env(&[
                ("REDIS_URL", "unix://{{run_dir}}/redis.sock"),
                ("REDIS_DB", "0")
            ])
        );
    }

    #[test]
    fn project_processes_apply_to_the_apps_they_list() {
        let file = project(
            r#"
            strip_prefix = "nova-"
            [processes.redis]
            apps = ["nova-billing", "api", "nova-nope"]
            command = "redis-server"
            [processes.mail]
            command = "mailcatcher"
            [processes.none]
            apps = []
            command = "true"
            [apps.nova-postcard-api]
            name = "api"
            "#,
        )
        .unwrap();
        let (project, problems) = assemble_project(
            file,
            vec![
                discovered("nova-billing"),
                discovered("nova-postcard-api"),
                discovered("nova-frontend"),
            ],
        );
        let processes = |app: &str| -> Vec<String> {
            project
                .app(app)
                .unwrap()
                .settings
                .processes
                .entries
                .keys()
                .cloned()
                .collect()
        };
        assert_eq!(processes("nova-billing"), ["mail", "redis"]);
        assert_eq!(processes("nova-postcard-api"), ["mail", "redis"]);
        assert_eq!(processes("nova-frontend"), ["mail"]);
        assert_eq!(
            problems,
            vec![Problem {
                project: Some("nova".into()),
                app: None,
                message: "[processes.redis] lists unknown app \"nova-nope\"".into(),
            }]
        );
    }

    #[test]
    fn rejects_bad_project_names() {
        for name in ["ab", "Nova", "a--b", "-abc", "with_underscore"] {
            let path = PathBuf::from(format!("/c/projects/{name}.toml"));
            assert!(
                matches!(parse_project(&path, ""), Err(ConfigError::Invalid { .. })),
                "{name}"
            );
        }
    }

    #[test]
    fn project_file_wins_over_repo_config_key_by_key() {
        let repo = parse_repo_config(
            Path::new("carte.toml"),
            r#"
            env_files = [".env"]
            subdomains = true
            policy = "pinned"
            [env]
            A = "repo"
            B = "repo"
            [processes.web]
            command = "bin/rails s"
            ready = "/up"
            [processes.worker]
            command = "bin/jobs"
            "#,
        )
        .unwrap();
        let file = project(
            r#"
            [apps.app-one]
            policy = "auto"
            [apps.app-one.env]
            B = "project"
            C = "project"
            [apps.app-one.processes.web]
            pty = false
            "#,
        )
        .unwrap();
        let merged = file.apps["app-one"].clone().merged_over(repo);
        assert_eq!(merged.policy, Some(Policy::Auto));
        assert_eq!(merged.subdomains, Some(true));
        assert_eq!(merged.env_files, Some(vec![".env".into()]));
        assert_eq!(
            merged.env,
            env(&[("A", "repo"), ("B", "project"), ("C", "project")])
        );
        let web = &merged.processes["web"];
        assert_eq!(web.command.as_deref(), Some("bin/rails s"));
        assert_eq!(web.ready, Some(Readiness::Http("/up".into())));
        assert_eq!(web.pty, Some(false));
        assert!(merged.processes.contains_key("worker"));
    }

    #[test]
    fn assembles_apps_with_short_names() {
        let file = project(
            r#"
            strip_prefix = "nova-"
            [apps.nova-billing]
            name = "invoices"
            depends_on = ["frontend", "nova-billing", "nova-nope"]
            [apps.nova-frontend]
            depends_on = ["invoices", "nova-billing"]
            aliases = ["nova-frontend", "Bad_Alias"]
            [apps.legacy]
            port = 3000
            [apps.nova-missing]
            policy = "pinned"
            "#,
        )
        .unwrap();
        let (project, problems) = assemble_project(
            file,
            vec![
                discovered("nova-frontend"),
                discovered("nova-billing"),
                discovered("nova"),
            ],
        );
        let shorts: Vec<_> = project
            .apps
            .iter()
            .map(|a| (a.name.as_str(), a.short.as_str()))
            .collect();
        assert_eq!(
            shorts,
            vec![
                ("legacy", "legacy"),
                ("nova", "nova"),
                ("nova-billing", "invoices"),
                ("nova-frontend", "frontend"),
            ]
        );
        assert!(project.app("nova").unwrap().is_collapsed("nova"));
        assert_eq!(project.app("legacy").unwrap().settings.port, Some(3000));
        assert_eq!(
            project.app("nova-frontend").unwrap().settings.depends_on,
            vec!["nova-billing"]
        );
        assert_eq!(
            project.app("nova-frontend").unwrap().settings.aliases,
            vec!["nova-frontend"]
        );
        assert_eq!(
            project.app("nova-billing").unwrap().settings.depends_on,
            vec!["nova-frontend"]
        );
        let messages: Vec<_> = problems.iter().map(|p| p.message.as_str()).collect();
        assert_eq!(problems.len(), 3, "{messages:?}");
        assert!(messages.iter().any(|m| m.contains("[apps.nova-missing]")));
        assert!(messages.iter().any(|m| m.contains("depend on itself")));
        assert_eq!(
            project
                .app("nova-billing")
                .unwrap()
                .settings
                .unresolved_deps,
            vec!["nova-nope"],
            "names outside the project resolve once every project is loaded"
        );
        assert!(
            !messages
                .iter()
                .any(|m| m.contains("unknown app \"nova-nope\""))
        );
        assert!(messages.iter().any(|m| m.contains("Bad_Alias")));
    }

    #[test]
    fn reports_invalid_and_duplicate_short_names() {
        let file = project(
            r#"
            strip_prefix = "nova-"
            [apps.nova-other]
            name = "api"
            "#,
        )
        .unwrap();
        let (project, problems) = assemble_project(
            file,
            vec![
                discovered("nova-ab"),
                discovered("nova-api"),
                discovered("nova-other"),
                discovered("nova-x--y"),
            ],
        );
        let names: Vec<_> = project.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["nova-api"]);
        assert_eq!(problems.len(), 3);
        assert_eq!(problems[0].app.as_deref(), Some("nova-ab"));
        assert!(problems[1].message.contains("already used by nova-api"));
        assert!(problems[2].message.contains("\"--\""));
    }
}
