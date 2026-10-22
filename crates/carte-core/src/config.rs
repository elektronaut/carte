//! Parsing of the machine config, project files and repo-level `carte.toml`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use thiserror::Error;

use crate::detach::DetachKeys;
use crate::kube::{KubeProject, KubeService, ServicePort};
use crate::model::{
    App, AppSettings, AttachConfig, EnvProviderKind, EnvTemplate, Limits, MachineConfig,
    MemoryTarget, Policy, Preset, Problem, ProcessConfig, ProcessKind, ProcessOverride, Project,
    Readiness, Restart, Scheme, Scope, ShareConfig,
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

    pub fn parse(path: &Path, text: &str, err: toml::de::Error) -> Self {
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
    teardown_delay: Option<DurationValue>,
    notifications: Option<bool>,
    #[serde(default)]
    limits: RawLimits,
    share: Option<RawShare>,
    #[serde(default)]
    attach: RawAttach,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawAttach {
    detach_key: Option<String>,
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
        teardown_delay: raw.teardown_delay.map_or(defaults.teardown_delay, |d| d.0),
        notifications: raw.notifications.unwrap_or(defaults.notifications),
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
        attach: AttachConfig {
            detach_keys: match raw.attach.detach_key {
                Some(keys) => keys.parse().map_err(|err| ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!("attach.detach_key: {err}"),
                })?,
                None => DetachKeys::default(),
            },
        },
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

/// A parsed project file, before its apps are discovered.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectFile {
    pub name: String,
    pub path: PathBuf,
    /// The directory relative `include` entries and app paths start from, unexpanded.
    pub root: Option<String>,
    /// Paths or globs, unexpanded; entries starting with `!` remove matches.
    pub include: Vec<String>,
    /// Paths or globs of directories that each become a project made from this
    /// file; non-empty makes the file a template.
    pub projects: Vec<String>,
    pub strip_prefix: Option<String>,
    pub env_provider: Option<EnvProviderKind>,
    pub env: Vec<EnvTemplate>,
    pub requires: Vec<String>,
    pub scheme: Scheme,
    pub idle_timeout: Option<Duration>,
    pub teardown_delay: Option<Duration>,
    /// The default `serve_files` of the project's apps.
    pub serve_files: Option<String>,
    /// `[apps.<full name>]` tables.
    pub apps: BTreeMap<String, AppConfig>,
    /// `[processes.<name>]` tables, declared for many apps at once.
    pub processes: BTreeMap<String, ProjectProcess>,
    /// Made by a template, whose `[apps.X]` tables only apply where X is an app.
    pub from_template: bool,
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
    root: Option<String>,
    #[serde(default)]
    include: OneOrMany,
    #[serde(default)]
    projects: OneOrMany,
    strip_prefix: Option<String>,
    env_provider: Option<EnvProviderKind>,
    #[serde(default)]
    env: EnvTable,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    scheme: Scheme,
    idle_timeout: Option<DurationValue>,
    teardown_delay: Option<DurationValue>,
    serve_files: Option<String>,
    #[serde(default)]
    apps: BTreeMap<String, AppConfig>,
    #[serde(default, deserialize_with = "project_process_table")]
    processes: BTreeMap<String, ProjectProcess>,
}

/// Per-app settings as written in a project file's `[apps.X]` table or a repo's `carte.toml`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    /// Where a named app is, relative to the project's root, or to the repo in a
    /// repo's `carte.toml`.
    pub path: Option<String>,
    /// A repo's `carte.toml` only: the apps inside the repo, relative to it.
    #[serde(default, deserialize_with = "one_or_many")]
    pub include: Vec<String>,
    /// A repo's `carte.toml` only: apps inside the repo named with `path`.
    #[serde(default)]
    pub apps: BTreeMap<String, AppConfig>,
    #[serde(default, deserialize_with = "env_table")]
    pub env: Vec<EnvTemplate>,
    pub env_files: Option<Vec<String>>,
    pub depends_on: Option<Vec<String>>,
    pub aliases: Option<Vec<String>>,
    pub subdomains: Option<bool>,
    pub policy: Option<Policy>,
    pub port: Option<u16>,
    pub serve_files: Option<String>,
    pub warm: Option<bool>,
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
            path: self.path.or(base.path),
            include: Vec::new(),
            apps: BTreeMap::new(),
            env: merge_env(base.env, self.env),
            env_files: self.env_files.or(base.env_files),
            depends_on: self.depends_on.or(base.depends_on),
            aliases: self.aliases.or(base.aliases),
            subdomains: self.subdomains.or(base.subdomains),
            policy: self.policy.or(base.policy),
            port: self.port.or(base.port),
            serve_files: self.serve_files.or(base.serve_files),
            warm: self.warm.or(base.warm),
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
            serve_files: self.serve_files.map(PathBuf::from),
            never_warm: self.warm == Some(false),
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
        restart: over.restart.or(base.restart),
        provides: merge_env(base.provides, over.provides),
        paths: over.paths.or(base.paths),
        preset: over.preset.or(base.preset),
        if_present: over.if_present && base.if_present,
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
    restart: Option<Restart>,
    #[serde(default)]
    provides: EnvTable,
    paths: Option<Vec<String>>,
    preset: Option<Preset>,
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
        for path in self.paths.iter().flatten() {
            check_process_path(path).map_err(|message| {
                E::custom(format!("process {name:?}: path {path:?} {message}"))
            })?;
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
                restart: self.restart,
                provides: self.provides.0,
                paths: self.paths,
                preset: self.preset,
                if_present: false,
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
            let mut declared = p.into_project_process::<D::Error>(&name)?;
            declared.process.if_present = declared.process.command.is_none();
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
    if text
        .parse::<toml::Table>()
        .is_ok_and(|table| table.contains_key("repos"))
    {
        return Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            message: "`repos` is replaced by `root` and `include`: `repos = \"~/work/acme-*\"` is now `root = \"~/work\"` and `include = \"acme-*\"`, and a single repo is just `root = \"~/work/shop\"`".into(),
        });
    }
    if is_kube_project(text) {
        return Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            message: "this is a kube project, not one with apps on disk".into(),
        });
    }
    let raw: RawProject = from_toml(path, text)?;
    if let Some(name) = raw
        .apps
        .iter()
        .find(|(_, app)| app.declares_apps())
        .map(|(name, _)| name)
    {
        return Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            message: format!(
                "[apps.{name}]: `include` and `apps` only apply to a repo's carte.toml; use the project's `include`"
            ),
        });
    }
    if let Some(root) = raw.root.as_deref() {
        check_absolute("root", root).map_err(|message| ConfigError::Invalid {
            path: path.to_path_buf(),
            message,
        })?;
    }
    Ok(ProjectFile {
        name,
        path: path.to_path_buf(),
        root: raw.root,
        include: raw.include.0,
        projects: raw.projects.0,
        strip_prefix: raw.strip_prefix.filter(|p| !p.is_empty()),
        env_provider: raw.env_provider,
        env: raw.env.0,
        requires: raw.requires,
        scheme: raw.scheme,
        idle_timeout: raw.idle_timeout.map(|d| d.0),
        teardown_delay: raw.teardown_delay.map(|d| d.0),
        serve_files: raw.serve_files,
        apps: raw.apps,
        processes: raw.processes,
        from_template: false,
    })
}

/// A path in a project file must not depend on the directory carte runs in,
/// which is `/` for the daemon.
fn check_absolute(key: &str, value: &str) -> Result<(), String> {
    if value == "~" || value.starts_with("~/") || value.starts_with('/') {
        Ok(())
    } else if value.starts_with('~') {
        Err(format!(
            "`{key}` is {value:?}, but ~user/ isn't supported: start it with / or ~/"
        ))
    } else {
        Err(format!(
            "`{key}` must start with / or ~/, but {value:?} is relative"
        ))
    }
}

pub fn load_project_file(path: &Path) -> Result<ProjectFile, ConfigError> {
    parse_project(path, &read(path)?)
}

// Kube project files

/// Keys of local project files, which a kube project refuses by name.
const LOCAL_PROJECT_KEYS: &[&str] = &[
    "root",
    "include",
    "projects",
    "strip_prefix",
    "env",
    "requires",
    "scheme",
    "teardown_delay",
    "serve_files",
    "processes",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKubeProject {
    kube: RawKube,
    #[serde(default)]
    namespaces: Vec<String>,
    env_provider: Option<EnvProviderKind>,
    idle_timeout: Option<DurationValue>,
    #[serde(default)]
    apps: BTreeMap<String, RawKubeService>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKube {
    context: String,
    kubeconfig: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKubeService {
    #[serde(default)]
    aliases: Vec<String>,
    namespace: Option<String>,
    service_port: Option<ServicePort>,
    policy: Option<Policy>,
}

/// Whether a project file's text declares a kube project.
pub fn is_kube_project(text: &str) -> bool {
    text.parse::<toml::Table>()
        .is_ok_and(|table| table.contains_key("kube"))
}

/// Parses a kube project file. Invalid aliases are left out and reported; anything
/// else invalid fails the file.
pub fn parse_kube_project(
    path: &Path,
    text: &str,
) -> Result<(KubeProject, Vec<Problem>), ConfigError> {
    let invalid = |message: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        message,
    };
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    slug::validate_project_name(&name).map_err(|err| invalid(err.to_string()))?;
    if let Ok(table) = text.parse::<toml::Table>()
        && let Some(key) = LOCAL_PROJECT_KEYS.iter().find(|k| table.contains_key(**k))
    {
        return Err(invalid(format!(
            "`{key}` doesn't apply to a kube project, whose apps are the cluster's Services"
        )));
    }
    let raw: RawKubeProject = from_toml(path, text)?;
    if let Some(kubeconfig) = raw.kube.kubeconfig.as_deref() {
        check_absolute("kube.kubeconfig", kubeconfig).map_err(invalid)?;
    }
    if raw.kube.context.trim().is_empty() {
        return Err(invalid(
            "`kube.context` must name a kubeconfig context".into(),
        ));
    }
    for namespace in &raw.namespaces {
        check_dns_label(namespace).map_err(|m| invalid(format!("namespace {namespace:?}: {m}")))?;
    }
    let mut problems = Vec::new();
    let mut seen_aliases = BTreeSet::new();
    let mut services = BTreeMap::new();
    for (service, table) in raw.apps {
        check_dns_label(&service).map_err(|m| invalid(format!("[apps.{service}]: {m}")))?;
        if let Some(namespace) = &table.namespace {
            check_dns_label(namespace)
                .map_err(|m| invalid(format!("[apps.{service}] namespace {namespace:?}: {m}")))?;
            if !raw.namespaces.is_empty() && !raw.namespaces.contains(namespace) {
                return Err(invalid(format!(
                    "[apps.{service}] namespace {namespace:?} isn't one of `namespaces`"
                )));
            }
        }
        if table.policy == Some(Policy::Pinned) {
            return Err(invalid(format!(
                "[apps.{service}]: a Service can't be pinned; its policy is \"auto\" or \"disabled\""
            )));
        }
        let mut aliases = Vec::new();
        for alias in table.aliases {
            let message = if alias.contains('.') {
                Some("an alias is one hostname label, like \"shop\"".to_string())
            } else if let Err(message) = check_dns_label(&alias) {
                Some(message)
            } else if !seen_aliases.insert(alias.clone()) {
                Some("it's used by more than one app".to_string())
            } else {
                None
            };
            match message {
                Some(message) => problems.push(Problem {
                    project: Some(name.clone()),
                    app: Some(service.clone()),
                    stack: None,
                    message: format!("alias {alias:?}: {message}"),
                }),
                None => aliases.push(alias),
            }
        }
        services.insert(
            service,
            KubeService {
                aliases,
                namespace: table.namespace,
                service_port: table.service_port,
                policy: table.policy,
            },
        );
    }
    let project = KubeProject {
        name,
        config_path: path.to_path_buf(),
        folder: None,
        context: raw.kube.context,
        kubeconfig: raw
            .kube
            .kubeconfig
            .as_deref()
            .map(crate::discovery::expand_home),
        namespaces: raw.namespaces,
        env_provider: raw.env_provider,
        idle_timeout: raw.idle_timeout.map(|d| d.0),
        services,
    };
    Ok((project, problems))
}

/// Parses a repo's `carte.toml`: the shape of one `[apps.X]` table, plus `include`
/// and `[apps.X] path` naming the apps inside the repo.
pub fn parse_repo_config(path: &Path, text: &str) -> Result<AppConfig, ConfigError> {
    let config: AppConfig = from_toml(path, text)?;
    let invalid = |message: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        message,
    };
    if config.path.is_some() {
        return Err(invalid("`path` only applies to an [apps.X] table".into()));
    }
    for (name, app) in &config.apps {
        let only_path = AppConfig {
            path: app.path.clone(),
            ..AppConfig::default()
        };
        if app.path.is_none() || *app != only_path {
            return Err(invalid(format!(
                "[apps.{name}] in carte.toml only takes `path`; its settings go in the app's own carte.toml"
            )));
        }
    }
    Ok(config)
}

impl AppConfig {
    /// Whether a repo's `carte.toml` names the apps inside the repo.
    pub fn declares_apps(&self) -> bool {
        !self.include.is_empty() || !self.apps.is_empty()
    }
}

/// Reads `carte.toml` from a checkout, if it has one.
pub fn load_repo_config(checkout: &Path) -> Result<Option<AppConfig>, ConfigError> {
    let path = checkout.join(REPO_CONFIG);
    if !path.is_file() {
        return Ok(None);
    }
    parse_repo_config(&path, &read(&path)?).map(Some)
}

/// An app found by discovery.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredApp {
    pub name: String,
    /// How it got its name, which decides its short name.
    pub naming: Naming,
    pub path: PathBuf,
    pub repo: Option<PathBuf>,
    pub branch_only: bool,
    pub repo_config: Option<AppConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Naming {
    /// After its directory; the project's `strip_prefix` applies.
    Directory,
    /// By an `[apps.X] path`.
    Named,
    /// The project's root, named after the project.
    Root,
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
        teardown_delay,
        serve_files,
        apps: mut app_tables,
        processes: project_processes,
        root: _,
        include: _,
        projects: _,
        from_template,
    } = file;
    let mut problems = Vec::new();
    let problem = |app: &str, message: String| Problem {
        project: Some(project_name.clone()),
        app: Some(app.to_string()),
        stack: None,
        message,
    };

    let mut candidates: Vec<Candidate> = discovered
        .into_iter()
        .map(|d| {
            let table = app_tables.remove(&d.name).unwrap_or_default();
            let short = match d.naming {
                Naming::Directory => short_name(&d.name, strip_prefix.as_deref()),
                Naming::Named | Naming::Root => d.name.clone(),
            };
            Candidate {
                short,
                table,
                discovered: d,
            }
        })
        .collect();
    for (name, table) in std::mem::take(&mut app_tables) {
        if table.path.is_some() {
            continue;
        }
        if table.port.is_some() {
            candidates.push(Candidate {
                short: short_name(&name, strip_prefix.as_deref()),
                discovered: DiscoveredApp {
                    name,
                    naming: Naming::Directory,
                    path: PathBuf::new(),
                    repo: None,
                    branch_only: false,
                    repo_config: None,
                },
                table,
            });
        } else if !from_template {
            problems.push(problem(
                &name,
                format!("[apps.{name}] does not match any app in the project"),
            ));
        }
    }
    candidates.sort_by(|a, b| a.discovered.name.cmp(&b.discovered.name));

    let short_names: Vec<(String, String)> = candidates
        .iter()
        .map(|c| (c.discovered.name.clone(), c.short.clone()))
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
                        stack: None,
                        message: format!("[processes.{process}] lists unknown app {entry:?}"),
                    }),
                }
            }
            targets
        });
        declared.push((process, targets, ov));
    }
    let mut apps: Vec<App> = Vec::new();
    for Candidate {
        short,
        table,
        discovered,
    } in candidates
    {
        let DiscoveredApp {
            name,
            path,
            repo,
            branch_only,
            repo_config,
            ..
        } = discovered;
        let project_wide = AppConfig {
            processes: declared
                .iter()
                .filter(|(_, targets, _)| targets.as_ref().is_none_or(|t| t.contains(&name)))
                .map(|(process, _, ov)| (process.clone(), ov.clone()))
                .collect(),
            serve_files: serve_files.clone(),
            ..AppConfig::default()
        };
        let config = table.merged_over(project_wide.merged_over(repo_config.unwrap_or_default()));
        if apps.iter().any(|a| a.name == name) {
            problems.push(problem(&name, format!("app {name:?} is found twice")));
            continue;
        }
        let mut config = config;
        if let Some(dir) = config.serve_files.take() {
            match check_serve_files(&dir) {
                Ok(()) => config.serve_files = Some(dir),
                Err(message) => problems.push(problem(&name, message)),
            }
        }
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
            repo,
            branch_only,
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
            if alias.contains('.') {
                problems.push(problem(
                    &app.name,
                    format!("alias {alias:?}: an alias is one hostname label, like \"shop\""),
                ));
            } else if let Err(message) = check_dns_label(&alias) {
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
        teardown_delay,
        serve_files: serve_files
            .filter(|dir| check_serve_files(dir).is_ok())
            .map(PathBuf::from),
        from_template,
        apps,
    };
    (project, problems)
}

/// A path a process serves: absolute, below the root, without a query.
fn check_process_path(path: &str) -> Result<(), &'static str> {
    if !path.starts_with('/') {
        Err("must start with /")
    } else if path.trim_end_matches('/').is_empty() {
        Err("is the whole app: everything outside other processes' paths goes to the HTTP process")
    } else if path.contains(['?', '#']) || path.chars().any(char::is_whitespace) {
        Err("can't have a query, a fragment or spaces")
    } else {
        Ok(())
    }
}

/// `serve_files` must name a folder inside the checkout.
fn check_serve_files(dir: &str) -> Result<(), String> {
    let path = Path::new(dir);
    if dir.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "serve_files {dir:?} must be a folder inside the checkout, like \"public\""
        ));
    }
    Ok(())
}

struct Candidate {
    short: String,
    table: AppConfig,
    discovered: DiscoveredApp,
}

/// The hostname label of an app named after its directory: the name without the
/// project's prefix.
fn short_name(name: &str, strip_prefix: Option<&str>) -> String {
    strip_prefix
        .and_then(|p| name.strip_prefix(p))
        .filter(|s| !s.is_empty())
        .unwrap_or(name)
        .to_string()
}

pub(crate) fn check_dns_label(label: &str) -> Result<(), String> {
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

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    OneOrMany::deserialize(d).map(|list| list.0)
}

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
                    if !key.contains("{{") && !crate::env::is_valid_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "invalid variable name {key:?}: use letters, digits, underscores and dots"
                        )));
                    }
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
    use proptest::prelude::*;

    use super::*;

    fn project(text: &str) -> Result<ProjectFile, ConfigError> {
        parse_project(Path::new("/c/projects/nova.toml"), text)
    }

    fn kube(text: &str) -> Result<(KubeProject, Vec<Problem>), ConfigError> {
        parse_kube_project(Path::new("/c/projects/prod.toml"), text)
    }

    fn invalid_message(result: Result<(KubeProject, Vec<Problem>), ConfigError>) -> String {
        match result {
            Err(ConfigError::Invalid { message, .. } | ConfigError::Parse { message, .. }) => {
                message
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_kube_project() {
        let text = r#"
            kube = { context = "arn:aws:eks:eu-west-1:123456:cluster/production", kubeconfig = "/etc/kube/config" }
            namespaces = ["monitoring", "logging"]
            env_provider = "mise"
            idle_timeout = "2h"

            [apps.grafana]
            aliases = ["grafana-prod"]
            namespace = "monitoring"

            [apps.vlogs]
            service_port = "http"
            policy = "disabled"

            [apps.alertmanager]
            service_port = 9093
        "#;
        assert!(is_kube_project(text));
        let (project, problems) = kube(text).unwrap();
        assert!(problems.is_empty());
        assert_eq!(project.name, "prod");
        assert_eq!(
            project.context,
            "arn:aws:eks:eu-west-1:123456:cluster/production"
        );
        assert_eq!(project.kubeconfig, Some(PathBuf::from("/etc/kube/config")));
        assert_eq!(project.namespaces, vec!["monitoring", "logging"]);
        assert_eq!(project.env_provider, Some(EnvProviderKind::Mise));
        assert_eq!(project.idle_timeout, Some(Duration::from_secs(7200)));
        assert_eq!(
            project.services["grafana"],
            KubeService {
                aliases: vec!["grafana-prod".into()],
                namespace: Some("monitoring".into()),
                service_port: None,
                policy: None,
            }
        );
        assert_eq!(
            project.services["vlogs"].service_port,
            Some(ServicePort::Name("http".into()))
        );
        assert_eq!(project.policy("vlogs"), Policy::Disabled);
        assert_eq!(
            project.services["alertmanager"].service_port,
            Some(ServicePort::Number(9093))
        );
    }

    #[test]
    fn paths_must_not_be_relative() {
        let err = project("root = \"work\"\n").unwrap_err().to_string();
        assert!(
            err.contains("`root` must start with / or ~/, but \"work\" is relative"),
            "{err}"
        );
        for root in ["/work", "~/work", "~"] {
            assert!(project(&format!("root = \"{root}\"\n")).is_ok(), "{root}");
        }
        let message = invalid_message(kube(
            "kube = { context = \"prod\", kubeconfig = \"kube/config\" }\n",
        ));
        assert!(
            message.contains("`kube.kubeconfig` must start with"),
            "{message}"
        );
        let err = project("root = \"~bob/work\"\n").unwrap_err().to_string();
        assert!(err.contains("~user/ isn't supported"), "{err}");
    }

    #[test]
    fn a_kube_project_needs_only_its_context() {
        let (project, _) = kube("kube = { context = \"kind-nova\" }\n").unwrap();
        assert_eq!(project.context, "kind-nova");
        assert!(project.namespaces.is_empty());
        assert!(project.services.is_empty());
        assert!(project.allows_namespace("anything"));
    }

    #[test]
    fn kube_projects_refuse_local_keys() {
        let message = invalid_message(kube(
            "root = \"~/work\"\nkube = { context = \"kind-nova\" }\n",
        ));
        assert!(
            message.contains("`root` doesn't apply to a kube project"),
            "{message}"
        );
        let message = invalid_message(kube(
            "kube = { context = \"kind-nova\" }\n[apps.grafana]\nport = 3000\n",
        ));
        assert!(message.contains("unknown field `port`"), "{message}");
        let message = invalid_message(kube(
            "kube = { context = \"kind-nova\" }\n[apps.grafana]\npolicy = \"pinned\"\n",
        ));
        assert!(message.contains("can't be pinned"), "{message}");
        let message = invalid_message(kube("kube = { context = \" \" }\n"));
        assert!(message.contains("kube.context"), "{message}");
        let message = invalid_message(kube(
            "kube = { context = \"kind-nova\" }\nnamespaces = [\"Bad_NS\"]\n",
        ));
        assert!(message.contains("namespace \"Bad_NS\""), "{message}");
        let message = invalid_message(kube(
            "kube = { context = \"kind-nova\" }\nnamespaces = [\"monitoring\"]\n[apps.grafana]\nnamespace = \"kube-system\"\n",
        ));
        assert!(message.contains("isn't one of `namespaces`"), "{message}");
        for (value, wanted) in [
            ("\"8080\"", "a number in quotes"),
            ("0", "from 1 to 65535"),
            ("70000", "from 1 to 65535"),
        ] {
            let message = invalid_message(kube(&format!(
                "kube = {{ context = \"kind-nova\" }}\n[apps.grafana]\nservice_port = {value}\n"
            )));
            assert!(message.contains(wanted), "{value}: {message}");
        }
    }

    #[test]
    fn invalid_kube_aliases_are_reported() {
        let (project, problems) = kube(
            "kube = { context = \"kind-nova\" }\n[apps.grafana]\naliases = [\"dash\", \"a.b\"]\n[apps.vlogs]\naliases = [\"dash\"]\n",
        )
        .unwrap();
        assert_eq!(project.services["grafana"].aliases, vec!["dash"]);
        assert!(project.services["vlogs"].aliases.is_empty());
        let messages: Vec<_> = problems.iter().map(|p| p.message.as_str()).collect();
        assert_eq!(
            messages,
            vec![
                "alias \"a.b\": an alias is one hostname label, like \"shop\"",
                "alias \"dash\": it's used by more than one app",
            ]
        );
    }

    #[test]
    fn a_local_project_parser_refuses_kube_projects() {
        assert!(matches!(
            project("kube = { context = \"kind-nova\" }\n"),
            Err(ConfigError::Invalid { message, .. }) if message.contains("kube project")
        ));
    }

    fn discovered(name: &str) -> DiscoveredApp {
        DiscoveredApp {
            name: name.into(),
            naming: Naming::Directory,
            path: PathBuf::from(format!("/work/{name}")),
            repo: None,
            branch_only: false,
            repo_config: None,
        }
    }

    fn named(name: &str, dir: &str) -> DiscoveredApp {
        DiscoveredApp {
            naming: Naming::Named,
            path: PathBuf::from(format!("/work/{dir}")),
            ..discovered(name)
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
            teardown_delay = "1m"
            notifications = false

            [limits]
            memory_target = "16GB"
            instance_target = 20
            hard_ceiling = 40
            pty_reserve = 32
            log_buffer = "2MB"
            log_budget = 1048576

            [share]
            domain = "dev.example.com"

            [attach]
            detach_key = "ctrl-] d"
            "#,
        )
        .unwrap();
        assert_eq!(config.tld, "localhost");
        assert_eq!(config.http_port, 8080);
        assert_eq!(config.https_port, 443);
        assert_eq!(config.idle_timeout, Duration::from_secs(1800));
        assert_eq!(config.teardown_delay, Duration::from_secs(60));
        assert!(!config.notifications);
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
        assert_eq!(config.attach.detach_keys.to_string(), "ctrl-] d");
        let err = parse_machine_config(Path::new("c"), "[attach]\ndetach_key = \"ctrl-\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("attach.detach_key"), "{err}");
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
            root = "~/work"
            include = "nova-*"
            strip_prefix = "nova-"
            env_provider = "mise"
            requires = ["tcp://localhost:5432"]
            scheme = "http"
            idle_timeout = "1h"
            teardown_delay = "30s"

            [env]
            "{{app.name | upper}}_URL" = "{{url:app}}"
            DATABASE_URL = "postgres:///{{app.name}}_{{checkout}}"
            A_NUMBER = 3
            BEFORE = "z"

            [apps.api]
            path = "nova-postcard-api"
            env_files = [".env"]
            depends_on = ["nova-billing"]
            aliases = ["nova-postcard-api"]
            subdomains = true
            policy = "pinned"
            replace_processes = true

            [apps.api.env]
            POSTCARD_API_URL = "{{url:nova-postcard-api}}"

            [apps.api.processes.web]
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
        assert_eq!(file.root.as_deref(), Some("~/work"));
        assert_eq!(file.include, vec!["nova-*"]);
        assert_eq!(file.strip_prefix.as_deref(), Some("nova-"));
        assert_eq!(file.env_provider, Some(EnvProviderKind::Mise));
        assert_eq!(file.scheme, Scheme::Http);
        assert_eq!(file.idle_timeout, Some(Duration::from_secs(3600)));
        assert_eq!(file.teardown_delay, Some(Duration::from_secs(30)));
        assert_eq!(
            file.env,
            env(&[
                ("{{app.name | upper}}_URL", "{{url:app}}"),
                ("DATABASE_URL", "postgres:///{{app.name}}_{{checkout}}"),
                ("A_NUMBER", "3"),
                ("BEFORE", "z"),
            ])
        );
        let app = &file.apps["api"];
        assert_eq!(app.path.as_deref(), Some("nova-postcard-api"));
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
    fn explains_that_repos_is_replaced() {
        let err = project("repos = \"~/work/acme-*\"\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`repos` is replaced by `root` and `include`"),
            "{err}"
        );
    }

    #[test]
    fn include_accepts_a_list() {
        let file = project("include = [\"~/a\", \"/b/*\", \"!/b/c\"]").unwrap();
        assert_eq!(file.include, vec!["~/a", "/b/*", "!/b/c"]);
    }

    #[test]
    fn path_only_applies_in_project_files() {
        let err = parse_repo_config(Path::new("/r/carte.toml"), "path = \"x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("only applies"), "{err}");
    }

    #[test]
    fn env_names_must_be_valid() {
        for key in ["A=B", "A\\u0000B", "1A", ""] {
            let err = project(&format!("root = \"/x\"\n[env]\n\"{key}\" = \"1\"\n"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("invalid variable name"), "{key}: {err}");
        }
        let file =
            project("root = \"/x\"\n[env]\n\"{{app.short | upper}}_URL\" = \"1\"\n").unwrap();
        assert_eq!(file.env[0].key, "{{app.short | upper}}_URL");
    }

    #[test]
    fn project_errors_point_at_file_and_key() {
        let err =
            project("root = \"x\"\n[apps.foo.processes.web]\ncommand = \"x\"\npty = \"yes\"\n")
                .unwrap_err()
                .to_string();
        assert!(err.starts_with("/c/projects/nova.toml:4:"), "{err}");
        assert!(err.contains("[apps.foo.processes.web]"), "{err}");
        assert!(err.contains("`pty = \"yes\"`"), "{err}");

        let err = project("root = \"x\"\nenv = \"mise\"\n")
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

        let file = project("[apps.foo.processes.drop]\nkind = \"teardown\"\n").unwrap();
        assert_eq!(
            file.apps["foo"].processes["drop"].kind,
            Some(ProcessKind::Teardown)
        );
        let err = project("[apps.foo.processes.drop]\nkind = \"cleanup\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown variant `cleanup`"), "{err}");

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
    fn parses_and_merges_restart_policies() {
        let file = project(
            r#"
            [processes.worker]
            restart = "never"
            [apps.foo.processes.css]
            restart = "on-failure"
            "#,
        )
        .unwrap();
        assert_eq!(
            file.processes["worker"].process.restart,
            Some(Restart::Never)
        );
        assert_eq!(
            file.apps["foo"].processes["css"].restart,
            Some(Restart::OnFailure)
        );
        let err = project("[apps.foo.processes.css]\nrestart = \"always\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown variant `always`"), "{err}");

        let repo = parse_repo_config(
            Path::new("carte.toml"),
            "[processes.worker]\ncommand = \"bin/jobs\"\nrestart = \"never\"\n",
        )
        .unwrap();
        let merged = project("[apps.foo.processes.worker]\npty = false\n")
            .unwrap()
            .apps["foo"]
            .clone()
            .merged_over(repo.clone());
        assert_eq!(merged.processes["worker"].restart, Some(Restart::Never));
        let merged = project("[apps.foo.processes.worker]\nrestart = \"on-failure\"\n")
            .unwrap()
            .apps["foo"]
            .clone()
            .merged_over(repo);
        assert_eq!(merged.processes["worker"].restart, Some(Restart::OnFailure));
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
    fn parses_paths_and_presets() {
        let file = project(
            "[apps.foo.processes.vite]\npaths = [\"/vite-dev/\"]\npreset = \"vite_ruby\"\n[processes.webpack]\npreset = \"shakapacker\"\n",
        )
        .unwrap();
        let vite = &file.apps["foo"].processes["vite"];
        assert_eq!(vite.paths, Some(vec!["/vite-dev/".to_string()]));
        assert_eq!(vite.preset, Some(Preset::ViteRuby));
        assert_eq!(
            file.processes["webpack"].process.preset,
            Some(Preset::Shakapacker)
        );
        for (path, message) in [
            ("assets/", "must start with /"),
            ("/", "is the whole app"),
            ("/assets?x", "can't have a query"),
        ] {
            let err = project(&format!("[processes.assets]\npaths = [\"{path}\"]\n"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(message), "{err}");
        }
        let err = project("[processes.assets]\npreset = \"webpacker\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown variant"), "{err}");
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
    fn project_processes_without_a_command_only_adjust_existing_ones() {
        let file = project(
            r#"
            [processes.worker]
            policy = "disabled"

            [processes.redis]
            command = "redis-server --port $PORT"

            [apps.app-one]
            replace_processes = true
            [apps.app-one.processes.web]
            command = "bin/serve"

            [apps.app-two.processes.worker]
            pty = false
            "#,
        )
        .unwrap();
        let (project, problems) =
            assemble_project(file, vec![discovered("app-one"), discovered("app-two")]);
        assert!(problems.is_empty(), "{problems:?}");
        let entries = |app: &str| &project.app(app).unwrap().settings.processes.entries;
        assert!(entries("app-one")["worker"].if_present);
        assert!(!entries("app-one")["redis"].if_present);
        assert!(!entries("app-one")["web"].if_present);
        assert!(!entries("app-two")["worker"].if_present);
        assert_eq!(entries("app-two")["worker"].policy, Some(Policy::Disabled));

        let list = crate::procfile::build(
            crate::procfile::ProcessSource::Config,
            Vec::new(),
            &project.app("app-one").unwrap().settings,
        )
        .unwrap();
        let names: Vec<&str> = list.processes.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["redis", "web"]);
        assert!(list.notices.is_empty(), "{:?}", list.notices);
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
            "#,
        )
        .unwrap();
        let (project, problems) = assemble_project(
            file,
            vec![
                discovered("nova-billing"),
                named("api", "nova-postcard-api"),
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
        assert_eq!(processes("api"), ["mail", "redis"]);
        assert_eq!(processes("nova-frontend"), ["mail"]);
        assert_eq!(
            problems,
            vec![Problem {
                project: Some("nova".into()),
                app: None,
                stack: None,
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
    fn warm_false_opts_an_app_out_of_warming() {
        let repo = parse_repo_config(Path::new("carte.toml"), "warm = false\n").unwrap();
        assert!(repo.clone().into_settings().never_warm);
        let file = project("[apps.app-one]\nwarm = true\n[apps.app-two]\n").unwrap();
        let one = file.apps["app-one"].clone().merged_over(repo.clone());
        assert!(!one.into_settings().never_warm);
        let two = file.apps["app-two"].clone().merged_over(repo);
        assert!(two.into_settings().never_warm);
        assert!(!AppConfig::default().into_settings().never_warm);
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
            [apps.invoices]
            depends_on = ["frontend", "invoices", "nova-nope"]
            [apps.nova-frontend]
            depends_on = ["invoices", "invoices"]
            aliases = ["nova-frontend", "Bad_Alias", "shop.example"]
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
                named("invoices", "nova-billing"),
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
                ("invoices", "invoices"),
                ("legacy", "legacy"),
                ("nova", "nova"),
                ("nova-frontend", "frontend"),
            ]
        );
        assert!(project.app("nova").unwrap().is_collapsed("nova"));
        assert_eq!(project.app("legacy").unwrap().settings.port, Some(3000));
        assert_eq!(
            project.app("nova-frontend").unwrap().settings.depends_on,
            vec!["invoices"]
        );
        assert_eq!(
            project.app("nova-frontend").unwrap().settings.aliases,
            vec!["nova-frontend"]
        );
        assert_eq!(
            project.app("invoices").unwrap().settings.depends_on,
            vec!["nova-frontend"]
        );
        let messages: Vec<_> = problems.iter().map(|p| p.message.as_str()).collect();
        assert_eq!(problems.len(), 4, "{messages:?}");
        assert!(messages.iter().any(|m| m.contains("[apps.nova-missing]")));
        assert!(
            messages
                .iter()
                .any(|m| m.contains("alias \"shop.example\": an alias is one hostname label"))
        );
        assert!(messages.iter().any(|m| m.contains("depend on itself")));
        assert_eq!(
            project.app("invoices").unwrap().settings.unresolved_deps,
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
    fn checks_aliases_from_a_repos_carte_toml() {
        let repo = parse_repo_config(
            Path::new("/work/shop/carte.toml"),
            "aliases = [\"store\", \"shop.example\"]\n",
        )
        .unwrap();
        let (project, problems) = assemble_project(
            project("").unwrap(),
            vec![DiscoveredApp {
                repo_config: Some(repo),
                ..discovered("shop")
            }],
        );
        assert_eq!(project.app("shop").unwrap().settings.aliases, ["store"]);
        let messages: Vec<_> = problems.iter().map(|p| p.message.as_str()).collect();
        assert_eq!(
            messages,
            ["alias \"shop.example\": an alias is one hostname label, like \"shop\""]
        );
    }

    #[test]
    fn reports_invalid_and_duplicate_short_names() {
        let file = project(
            r#"
            strip_prefix = "nova-"
            "#,
        )
        .unwrap();
        let (project, problems) = assemble_project(
            file,
            vec![
                discovered("nova-ab"),
                discovered("nova-api"),
                named("api", "nova-other"),
                discovered("nova-x--y"),
            ],
        );
        let names: Vec<_> = project.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["api"]);
        assert_eq!(problems.len(), 3);
        assert_eq!(problems[0].app.as_deref(), Some("nova-ab"));
        assert!(problems[1].message.contains("already used by api"));
        assert!(problems[2].message.contains("\"--\""));
    }

    proptest! {
        #[test]
        fn byte_sizes_never_panic(input in "\\PC{0,16}|[0-9]{0,22} ?[kKmMgGtTiIbB]{0,3}") {
            let _ = ByteSize::parse(&input);
        }

        #[test]
        fn byte_sizes_multiply_by_their_unit(
            n in any::<u64>(),
            (unit, exp) in proptest::sample::select(vec![
                ("", 0), ("B", 0), ("k", 1), ("KB", 1), ("KiB", 1), ("MB", 2), ("mib", 2),
                ("G", 3), ("GB", 3), ("TB", 4), ("tib", 4),
            ]),
            space in any::<bool>(),
        ) {
            let input = format!("{n}{}{unit}", if space { " " } else { "" });
            prop_assert_eq!(ByteSize::parse(&input), n.checked_mul(1024u64.pow(exp)));
        }
    }
}
