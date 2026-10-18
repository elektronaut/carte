use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::procfile::ProcessList;

/// Machine-wide settings from `~/.config/carte/config.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineConfig {
    pub tld: String,
    pub http_port: u16,
    pub https_port: u16,
    pub dns_port: u16,
    pub idle_timeout: Duration,
    /// How long a stack stays removed before its teardown runs.
    pub teardown_delay: Duration,
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
            tld: "localhost".into(),
            http_port: 80,
            https_port: 443,
            dns_port: 9253,
            idle_timeout: Duration::from_secs(15 * 60),
            teardown_delay: Duration::from_secs(10 * 60),
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
    /// The subdirectory of `projects/` its file is in, for grouping in the TUI only.
    pub folder: Option<String>,
    pub strip_prefix: Option<String>,
    pub env_provider: Option<EnvProviderKind>,
    /// Project-level env templates, in file order.
    pub env: Vec<EnvTemplate>,
    pub requires: Vec<String>,
    pub scheme: Scheme,
    pub idle_timeout: Option<Duration>,
    pub teardown_delay: Option<Duration>,
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

/// An app of a project: a directory found by `include`, named by `[apps.X] path`, or
/// the project's root. `name` is its identity: the directory name, the table's key, or
/// the project's name for the root.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub name: String,
    /// The hostname label: `name`, with the project's prefix stripped from directory names.
    pub short: String,
    /// Where the app is in the main checkout, which may not have it (`branch_only`).
    pub path: PathBuf,
    /// The main checkout of the repo the app is in; `None` outside git.
    pub repo: Option<PathBuf>,
    /// Found only in worktrees, so it has no instance on main.
    pub branch_only: bool,
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
    /// Apps of the same project, by full name; they run in the dependent's stack.
    pub depends_on: Vec<String>,
    /// Apps of other projects; they run in their project's main stack.
    pub depends_on_elsewhere: Vec<AppRef>,
    /// `depends_on` names that aren't apps of this project, until every project is
    /// loaded and they resolve into `depends_on_elsewhere`.
    pub unresolved_deps: Vec<String>,
    pub aliases: Vec<String>,
    pub subdomains: bool,
    pub policy: Option<Policy>,
    /// A route-only app (imported puma-dev port file): no processes, fixed port.
    pub port: Option<u16>,
    /// A folder in the checkout whose files the proxy serves ahead of the app.
    pub serve_files: Option<PathBuf>,
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
    pub kind: Option<ProcessKind>,
    pub after: Option<Vec<String>>,
    pub restart: Option<Restart>,
    /// Env templates added to every process of the instance.
    pub provides: Vec<EnvTemplate>,
    /// Adjusts the process only where it exists: a project-wide entry without a
    /// command, which apps without the process skip silently.
    pub if_present: bool,
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
    /// `None` for an ordinary process.
    #[serde(default)]
    pub kind: Option<ProcessKind>,
    /// Processes of the same tier this one starts after.
    #[serde(default)]
    pub after: Vec<String>,
    /// Env templates added to every process of the instance.
    #[serde(default)]
    pub provides: Vec<EnvTemplate>,
    #[serde(default)]
    pub restart: Restart,
}

impl ProcessSpec {
    /// The start order: services, then tasks, then everything else. Teardowns
    /// never start with the instance.
    pub fn tier(&self) -> u8 {
        match self.kind {
            Some(ProcessKind::Service) => 1,
            Some(ProcessKind::Task) => 2,
            None => 3,
            Some(ProcessKind::Teardown) => 4,
        }
    }

    pub fn is_service(&self) -> bool {
        self.kind == Some(ProcessKind::Service)
    }

    pub fn is_task(&self) -> bool {
        self.kind == Some(ProcessKind::Task)
    }
}

/// The role a process plays in its instance's start and stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessKind {
    /// Starts first; the rest waits until it's ready, and it stops last.
    Service,
    /// Runs to completion after the services and before everything else.
    Task,
    /// Runs once, in the app's main checkout, after the stack's worktrees are gone.
    Teardown,
}

/// Whether carte restarts a process that crashes after its instance is ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    /// After a non-zero exit or death by a signal.
    OnFailure,
    #[default]
    Never,
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
    /// The checked-out branch; `None` on a detached HEAD.
    pub branch: Option<String>,
    /// The stack this worktree belongs to, from its folder name.
    pub slug: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stack {
    Main,
    /// A stack named after worktree folders.
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

    /// `main` or the worktree's stack name (`CARTE_CHECKOUT`).
    pub fn slug(&self) -> &str {
        match self {
            Checkout::Main { .. } => "main",
            Checkout::Worktree(wt) => &wt.slug,
        }
    }
}

/// An app of some project.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AppRef {
    pub project: String,
    /// The app's full (directory) name.
    pub app: String,
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
    /// Each checkout's process list as read at load, by checkout path.
    pub processes: BTreeMap<PathBuf, Result<ProcessList, String>>,
    /// The worktrees of each repo the project's apps are in, by its main checkout:
    /// each worktree's path by its stack.
    pub repo_worktrees: BTreeMap<PathBuf, BTreeMap<String, PathBuf>>,
    /// Directories in every checkout where new apps can appear.
    pub scan_dirs: BTreeSet<PathBuf>,
    /// Apps as configured in worktrees whose `carte.toml` differs from the one their
    /// settings come from, by checkout path: those take that file's processes, env and
    /// env files.
    pub checkout_apps: BTreeMap<PathBuf, App>,
}

impl ProjectState {
    /// The names of every stack in the project.
    pub fn stacks(&self) -> BTreeSet<String> {
        self.worktrees
            .values()
            .flatten()
            .map(|wt| wt.slug.clone())
            .collect()
    }

    pub fn has_stack(&self, slug: &str) -> bool {
        self.worktrees.values().flatten().any(|wt| wt.slug == slug)
    }

    /// The checkout an app runs from in a stack: its own worktree, else main. `None`
    /// when the stack has a worktree of the app's repo without the app, or the app is
    /// only on branches and the stack has no worktree of it.
    pub fn checkout(&self, app: &App, stack: &Stack) -> Option<Checkout> {
        if let Stack::Branch(slug) = stack {
            if let Some(wt) = self
                .worktrees
                .get(&app.name)
                .and_then(|wts| wts.iter().find(|wt| &wt.slug == slug))
            {
                return Some(Checkout::Worktree(wt.clone()));
            }
            if app
                .repo
                .as_ref()
                .and_then(|repo| self.repo_worktrees.get(repo))
                .is_some_and(|worktrees| worktrees.contains_key(slug))
            {
                return None;
            }
        }
        (!app.branch_only).then(|| Checkout::Main {
            path: app.path.clone(),
        })
    }

    /// Why an app has no instance in a stack, and where it has one.
    pub fn not_in_stack(&self, app: &App, stack: &Stack) -> String {
        let stacks: Vec<String> = self
            .stacks_of(app)
            .iter()
            .map(|s| s.slug().to_string())
            .collect();
        let elsewhere = match stacks.as_slice() {
            [] => String::new(),
            [one] => format!("; it's in {one}"),
            many => format!("; it's in {}", many.join(", ")),
        };
        format!(
            "{} isn't in the {} stack{elsewhere}",
            app.name,
            stack.slug()
        )
    }

    /// The app's main checkout, or its first worktree when it's only on branches.
    pub fn any_checkout(&self, app: &App) -> PathBuf {
        if !app.branch_only {
            return app.path.clone();
        }
        self.worktrees
            .get(&app.name)
            .and_then(|wts| wts.first())
            .map_or_else(|| app.path.clone(), |wt| wt.path.clone())
    }

    /// The app as configured in its checkout in a stack.
    pub fn app_at<'a>(&'a self, app: &'a App, stack: &Stack) -> &'a App {
        self.checkout(app, stack)
            .and_then(|checkout| self.checkout_apps.get(checkout.path()))
            .unwrap_or(app)
    }

    /// Whether an app has an instance in a stack.
    pub fn runs_in(&self, app: &App, stack: &Stack) -> bool {
        self.checkout(app, stack).is_some()
    }

    /// The stacks an app has an instance in, main first.
    pub fn stacks_of(&self, app: &App) -> Vec<Stack> {
        std::iter::once(Stack::Main)
            .chain(self.stacks().into_iter().map(Stack::Branch))
            .filter(|stack| self.runs_in(app, stack))
            .collect()
    }

    /// The process list of an app's checkout in a stack, as read at load.
    pub fn process_list(&self, app: &App, stack: &Stack) -> Option<&Result<ProcessList, String>> {
        self.processes.get(self.checkout(app, stack)?.path())
    }
}

/// The daemon's whole view of configuration and disk.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct World {
    pub machine: MachineConfig,
    pub projects: Vec<ProjectState>,
    pub problems: Vec<Problem>,
    /// Directories where templates' new projects can appear.
    pub scan_dirs: BTreeSet<PathBuf>,
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
    /// The one stack the problem concerns, which leaves the project's other stacks
    /// known for sure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    pub message: String,
}
