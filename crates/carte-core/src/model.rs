use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Machine-wide settings from `~/.config/carte/config.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineConfig {
    pub tld: String,
    pub http_port: u16,
    pub https_port: u16,
    pub dns_port: u16,
    pub idle_timeout: Duration,
    pub limits: Limits,
    pub share: Option<ShareConfig>,
}

impl MachineConfig {
    /// The TLDs carte routes and issues certificates for: the configured one, and
    /// `localhost`, which browsers resolve to loopback without any DNS setup.
    pub fn served_tlds(&self) -> Vec<String> {
        let tld = self.tld.to_ascii_lowercase();
        if tld == "localhost" {
            vec![tld]
        } else {
            vec![tld, "localhost".into()]
        }
    }
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            tld: "test".into(),
            http_port: 80,
            https_port: 443,
            dns_port: 9253,
            idle_timeout: Duration::from_secs(15 * 60),
            limits: Limits::default(),
            share: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    pub memory_target: MemoryTarget,
    pub instance_target: Option<u32>,
    pub hard_ceiling: u32,
    pub pty_reserve: u32,
    pub log_buffer_bytes: usize,
    pub log_budget_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_target: MemoryTarget::PercentOfRam(50),
            instance_target: None,
            hard_ceiling: 50,
            pty_reserve: 64,
            log_buffer_bytes: 1024 * 1024,
            log_budget_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTarget {
    PercentOfRam(u8),
    Bytes(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareConfig {
    pub provider: String,
    pub domain: String,
    /// The provider's tunnel name (Cloudflare: a named tunnel).
    pub tunnel: String,
    /// The loopback port the tunnel delivers requests to.
    pub port: u16,
}

impl ShareConfig {
    /// The provider serves the domain itself and nothing under it (ngrok's free dev
    /// domain), so one app is shared at a time.
    pub fn single_host(&self) -> bool {
        self.provider == "ngrok"
    }
}

/// A loaded and validated project.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub name: String,
    pub config_path: PathBuf,
    pub strip_prefix: Option<String>,
    pub env_provider: Option<EnvProviderKind>,
    /// Project-level env templates, in file order.
    pub env: Vec<EnvTemplate>,
    pub requires: Vec<String>,
    pub scheme: Scheme,
    pub idle_timeout: Option<Duration>,
    pub apps: Vec<App>,
}

impl Project {
    pub fn app(&self, name: &str) -> Option<&App> {
        self.apps.iter().find(|a| a.name == name)
    }

    pub fn app_by_short(&self, short: &str) -> Option<&App> {
        self.apps.iter().find(|a| a.short == short)
    }
}

/// One repo in a project. `name` is the directory name and the app's identity.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub name: String,
    /// The hostname label: `name` with the project's prefix stripped, or the `name` override.
    pub short: String,
    /// The primary checkout.
    pub path: PathBuf,
    pub settings: AppSettings,
}

impl App {
    /// A collapsed app's label is omitted from hostnames.
    pub fn is_collapsed(&self, project: &str) -> bool {
        self.short == project
    }
}

/// Per-app settings after merging the project file over the repo's `carte.toml`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppSettings {
    pub env: Vec<EnvTemplate>,
    pub env_files: Vec<String>,
    pub depends_on: Vec<String>,
    pub aliases: Vec<String>,
    pub subdomains: bool,
    pub policy: Option<Policy>,
    /// A route-only app (imported puma-dev port file): no processes, fixed port.
    pub port: Option<u16>,
    pub processes: ProcessConfig,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProcessConfig {
    /// Replace the Procfile's list instead of adjusting it.
    pub replace: bool,
    pub entries: BTreeMap<String, ProcessOverride>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProcessOverride {
    pub command: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<EnvTemplate>,
    pub http: Option<bool>,
    pub ready: Option<Readiness>,
    pub scope: Option<Scope>,
    pub pty: Option<bool>,
    pub policy: Option<Policy>,
}

/// A process ready to run in one checkout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessSpec {
    pub name: String,
    pub command: String,
    /// Relative to the checkout unless absolute.
    pub cwd: Option<PathBuf>,
    pub env: Vec<EnvTemplate>,
    pub http: bool,
    pub ready: Readiness,
    pub scope: Scope,
    pub pty: bool,
    pub policy: Policy,
}

/// An env entry whose key and value may both contain `{{ }}` expressions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvTemplate {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    Pinned,
    #[default]
    Auto,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Instance,
    Checkout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Readiness {
    /// The process's port accepts connections.
    #[default]
    Port,
    /// An HTTP GET of this path returns a non-5xx status.
    Http(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    #[default]
    Https,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvProviderKind {
    Mise,
    Direnv,
    Shell,
    None,
}

/// A linked worktree of an app (never the primary checkout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub branch: String,
    /// `slug::branch_slug(branch)`; the stack this worktree belongs to.
    pub slug: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stack {
    Main,
    /// A branch slug.
    Branch(String),
}

impl Stack {
    pub fn slug(&self) -> &str {
        match self {
            Stack::Main => "main",
            Stack::Branch(slug) => slug,
        }
    }
}

/// Where an instance's code comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checkout {
    Main { path: PathBuf },
    Worktree(Worktree),
}

impl Checkout {
    pub fn path(&self) -> &PathBuf {
        match self {
            Checkout::Main { path } => path,
            Checkout::Worktree(wt) => &wt.path,
        }
    }

    /// `main` or the worktree's branch slug (`CARTE_CHECKOUT`).
    pub fn slug(&self) -> &str {
        match self {
            Checkout::Main { .. } => "main",
            Checkout::Worktree(wt) => &wt.slug,
        }
    }
}

/// One app running for one stack.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InstanceKey {
    pub project: String,
    /// The app's full (directory) name.
    pub app: String,
    pub stack: Stack,
}

/// Everything discovered about one project: config plus the worktrees found on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectState {
    pub project: Project,
    /// Linked worktrees per app name.
    pub worktrees: BTreeMap<String, Vec<Worktree>>,
}

impl ProjectState {
    /// Branch slugs of every stack in the project.
    pub fn stacks(&self) -> std::collections::BTreeSet<String> {
        self.worktrees
            .values()
            .flatten()
            .map(|wt| wt.slug.clone())
            .collect()
    }

    pub fn has_stack(&self, slug: &str) -> bool {
        self.worktrees.values().flatten().any(|wt| wt.slug == slug)
    }

    /// The checkout an app runs from in a stack: its own worktree, else main.
    pub fn checkout(&self, app: &App, stack: &Stack) -> Checkout {
        if let Stack::Branch(slug) = stack
            && let Some(wt) = self
                .worktrees
                .get(&app.name)
                .and_then(|wts| wts.iter().find(|wt| &wt.slug == slug))
        {
            return Checkout::Worktree(wt.clone());
        }
        Checkout::Main {
            path: app.path.clone(),
        }
    }
}

/// The daemon's whole view of configuration and disk.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct World {
    pub machine: MachineConfig,
    pub projects: Vec<ProjectState>,
    pub problems: Vec<Problem>,
}

impl World {
    pub fn project(&self, name: &str) -> Option<&ProjectState> {
        self.projects.iter().find(|p| p.project.name == name)
    }
}

/// A config or discovery problem shown in `carte ls` and the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub project: Option<String>,
    pub app: Option<String>,
    pub message: String,
}
