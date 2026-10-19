//! Types exchanged between the daemon and its clients (CLI, TUI) over the Unix socket.
//! Every CLI `--json` output is one of these types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Endpoint paths, shared by the daemon's router and the client.
pub mod paths {
    pub const SNAPSHOT: &str = "/v1/snapshot";
    /// Newline-delimited JSON `Snapshot`s, one per change.
    pub const WATCH: &str = "/v1/watch";
    pub const CONTROL: &str = "/v1/control";
    pub const STOP_ALL: &str = "/v1/stop-all";
    /// Newline-delimited JSON `LogLine`s.
    pub const LOGS: &str = "/v1/logs";
    /// Adds a marker to the logs of an address; answers with the marker line.
    pub const MARK: &str = "/v1/logs/mark";
    /// The daemon's own environment, which apps build on.
    pub const ENV: &str = "/v1/env";
    /// The env a process of an `Address` gets, as an `EnvResponse`.
    pub const PROCESS_ENV: &str = "/v1/process-env";
    pub const RELOAD: &str = "/v1/reload";
    pub const SHARE: &str = "/v1/share";
    pub const UNSHARE: &str = "/v1/unshare";
    /// Newline-delimited JSON `Event`s.
    pub const EVENTS: &str = "/v1/events";
    /// GET lists leftovers as a `PruneList`; POST a `PruneRequest` removes them.
    pub const PRUNE: &str = "/v1/prune";
}

/// A CLI address (`app@stack/process`, any part optional) and the directory it was
/// typed in, which fills in the omitted parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Address {
    pub address: String,
    pub cwd: Option<PathBuf>,
    /// Resolve inside this project; an empty address then means the whole project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub projects: Vec<ProjectStatus>,
    pub problems: Vec<Problem>,
    pub running_instances: u32,
    pub memory_bytes: u64,
    #[serde(default)]
    pub shares: Vec<ShareStatus>,
    /// `None` when sharing isn't configured.
    #[serde(default)]
    pub tunnel: Option<TunnelStatus>,
}

/// Apps of one stack exposed on the public internet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareStatus {
    pub project: String,
    pub stack: String,
    pub apps: Vec<SharedApp>,
    /// Visitors need the credentials returned when the share was made.
    pub auth: bool,
    pub pinned: bool,
    pub shared_secs: u64,
    pub expires_in_secs: Option<u64>,
    /// The latest requests from the internet, newest last.
    pub requests: Vec<ExternalRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedApp {
    pub app: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRequest {
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub app: String,
    pub method: String,
    pub path: String,
    /// Refused because the credentials were missing or wrong.
    pub refused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelStatus {
    pub provider: String,
    /// The provider carries one shared app at a time.
    #[serde(default)]
    pub single_host: bool,
    pub state: TunnelState,
    /// The provider's last words, for when it fails.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TunnelState {
    /// Nothing is shared, so no tunnel runs.
    Idle,
    Starting,
    Connected,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRequest {
    /// An app (`wiki`, `api@feature-x`) or a stack (`@feature-x`) with `apps`.
    pub target: Address,
    /// More apps of the same stack, full or short names.
    #[serde(default)]
    pub apps: Vec<String>,
    pub ttl_secs: Option<u64>,
    #[serde(default)]
    pub auth: bool,
    #[serde(default)]
    pub pin: bool,
    /// When the provider carries one app at a time, end the current share instead of
    /// refusing.
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareResponse {
    pub share: ShareStatus,
    pub credentials: Option<Credentials>,
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnshareRequest {
    /// Any app of the share, or its stack.
    pub target: Address,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub project: Option<String>,
    pub app: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectStatus {
    pub name: String,
    /// The subdirectory of `projects/` its file is in, `/`-separated at any depth,
    /// for grouping in the TUI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub apps: Vec<AppInfo>,
    /// `main` first, then stacks by name.
    pub stacks: Vec<StackStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    pub short: String,
    pub path: PathBuf,
    pub runnable: bool,
}

/// `carte project ls --json`: what the config files say, without the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectList {
    pub projects: Vec<ProjectSummary>,
    /// Problems that belong to no project, such as an invalid `config.toml`.
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub name: String,
    pub config_path: PathBuf,
    /// Empty when the project file can't be read.
    pub apps: Vec<AppInfo>,
    /// Stack names other than `main`, sorted.
    pub stacks: Vec<String>,
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackStatus {
    /// `main` or the stack's name, from its worktree folders.
    pub name: String,
    /// The branch the stack's worktrees have checked out; `None` on main, when they
    /// have different branches, or when any has a detached HEAD.
    pub branch: Option<String>,
    /// One entry per app in the project: what the app resolves to in this stack,
    /// running or not.
    pub instances: Vec<InstanceStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub app: String,
    pub stack: String,
    /// `main` or the worktree's stack name; `main` inside a stack is a fallback.
    pub checkout: String,
    pub checkout_path: PathBuf,
    pub url: String,
    pub public_url: Option<String>,
    pub state: InstanceState,
    pub processes: Vec<ProcessStatus>,
    /// What keeps it from idling out.
    pub holds: Vec<Hold>,
    pub idle_in_secs: Option<u64>,
    pub memory_bytes: u64,
    pub cpu_percent: f32,
    pub last_error: Option<String>,
    /// Its tool versions changed since it started; a restart picks them up.
    #[serde(default)]
    pub env_changed: bool,
    /// The latest event about it or one of its processes.
    #[serde(default)]
    pub last_event: Option<Event>,
    /// How far a start has come, while it is starting.
    #[serde(default)]
    pub boot: Option<BootProgress>,
    /// The apps an open of it starts ahead, from what usually opens next.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warms: Vec<Warm>,
}

/// An app that followed `hits` of an app's last `opens` opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warm {
    pub app: String,
    pub hits: u32,
    pub opens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootProgress {
    pub elapsed_ms: u64,
    /// What the start waits for: `web to answer /up on port 51234`.
    pub waiting_for: String,
}

/// `carte status --json`: the instances an address names. An app's instance is listed
/// even when stopped; a stack or project lists what is running or shared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub instances: Vec<InstanceStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstanceState {
    Stopped,
    Starting,
    Ready,
    Stopping,
    Crashed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Hold {
    Pinned,
    Requests { count: u32 },
    Connections { count: u32 },
    Dependent { app: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessStatus {
    pub name: String,
    pub state: ProcessState,
    pub policy: Policy,
    /// The policy was set with `pin`/`disable` and lasts until the daemon restarts.
    pub temporary: bool,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub http: bool,
    pub restarts: u32,
    pub memory_bytes: u64,
    pub cpu_percent: f32,
    /// It crashed too often after the start, and carte stopped restarting it.
    #[serde(default)]
    pub gave_up: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state")]
pub enum ProcessState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Disabled,
    /// A task that finished with exit code 0.
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    Pinned,
    Auto,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest {
    pub target: Address,
    pub action: Action,
    /// For `up` and `restart`: answer once the instances are ready or have failed.
    #[serde(default)]
    pub wait: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "action")]
pub enum Action {
    Up,
    Down,
    Restart,
    Pin { save: bool },
    Unpin { save: bool },
    Disable { save: bool },
    Enable { save: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResponse {
    /// `app@stack` or `app@stack/process` for everything the action touched.
    pub affected: Vec<String>,
    pub messages: Vec<String>,
    /// Instances that failed to get ready, when the request waited.
    #[serde(default)]
    pub failures: Vec<StartFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartFailure {
    /// `app@stack`.
    pub instance: String,
    pub title: String,
    pub detail: String,
    /// The last lines of its log.
    pub log: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopAllRequest {
    pub idle_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LogsRequest {
    pub target: Address,
    pub follow: bool,
    /// Lines from the buffer to send before following: 100 by default, every line
    /// when `since_ms` or `since_start` narrows them.
    pub tail: Option<usize>,
    pub search: Option<String>,
    /// Only lines from this time on, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// Only lines from each process's latest run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub since_start: bool,
    /// The lines most relevant to these words, best first, instead of the latest
    /// ones. Never followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<String>,
    /// How many ranked lines to send: 20 by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    #[serde(default)]
    pub project: String,
    pub app: String,
    pub stack: String,
    pub process: String,
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The line with escape codes stripped.
    pub text: String,
    /// A marker someone added at `at_ms`, with no text. Its app, stack and process
    /// are empty when the marked scope didn't name them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub marker: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkRequest {
    pub target: Address,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsRequest {
    /// Events about this and everything under it; `None` for every event.
    pub target: Option<Address>,
    pub follow: bool,
    /// Events from the buffer to send before following.
    pub tail: Option<usize>,
}

/// Something that happened in the daemon: an instance starting or stopping, a config
/// reload, a share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Increases by one per event while the daemon runs.
    pub seq: u64,
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub kind: EventKind,
    /// The details, like why an instance stopped.
    pub message: String,
    /// What the event is about, the main one first; empty for the whole machine.
    pub subjects: Vec<EventSubject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl Event {
    /// What the event concerns: `acme-web@feature-x/worker`, a project, or `carte`
    /// for the whole machine.
    pub fn subject_label(&self) -> String {
        self.subjects
            .first()
            .map_or_else(|| "carte".to_string(), EventSubject::label)
    }

    /// The kind and the details: `stopped: idle for 15m`.
    pub fn summary(&self) -> String {
        if self.message.is_empty() {
            self.kind.words().to_string()
        } else {
            format!("{}: {}", self.kind.words(), self.message)
        }
    }
}

/// A project, or a stack, app or process in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSubject {
    pub project: String,
    /// The stack's slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

impl EventSubject {
    /// `app@stack/process` as the CLI takes it, `@main` left out; the project name
    /// when no app is involved.
    pub fn label(&self) -> String {
        let Some(app) = &self.app else {
            return match &self.stack {
                Some(stack) if stack != "main" => format!("@{stack} ({})", self.project),
                _ => self.project.clone(),
            };
        };
        let mut label = app.clone();
        if let Some(stack) = self.stack.as_ref().filter(|s| *s != "main") {
            label.push('@');
            label.push_str(stack);
        }
        if let Some(process) = &self.process {
            label.push('/');
            label.push_str(process);
        }
        label
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    Started,
    Ready,
    Stopped,
    Crashed,
    Exited,
    Restarted,
    ConfigReloaded,
    ConfigError,
    WorktreeAdded,
    WorktreeRemoved,
    Shared,
    Unshared,
    ShareExpired,
    TunnelConnected,
    TunnelFailed,
    PolicyChanged,
    TeardownScheduled,
    TeardownPaused,
    TeardownCancelled,
    TeardownFinished,
    TeardownFailed,
}

impl EventKind {
    pub fn words(self) -> &'static str {
        match self {
            EventKind::Started => "started",
            EventKind::Ready => "ready",
            EventKind::Stopped => "stopped",
            EventKind::Crashed => "crashed",
            EventKind::Exited => "exited",
            EventKind::Restarted => "restarted",
            EventKind::ConfigReloaded => "config reloaded",
            EventKind::ConfigError => "config error",
            EventKind::WorktreeAdded => "worktree added",
            EventKind::WorktreeRemoved => "worktree removed",
            EventKind::Shared => "shared",
            EventKind::Unshared => "unshared",
            EventKind::ShareExpired => "share expired",
            EventKind::TunnelConnected => "tunnel connected",
            EventKind::TunnelFailed => "tunnel failed",
            EventKind::PolicyChanged => "policy changed",
            EventKind::TeardownScheduled => "teardown scheduled",
            EventKind::TeardownPaused => "teardown paused",
            EventKind::TeardownCancelled => "teardown cancelled",
            EventKind::TeardownFinished => "teardown finished",
            EventKind::TeardownFailed => "teardown failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonEnv {
    pub vars: std::collections::BTreeMap<String, String>,
    /// Where the daemon puts instances' run directories.
    #[serde(default)]
    pub run_root: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvResponse {
    pub instance: String,
    pub vars: Vec<EnvVar>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    /// `None` when masked.
    pub value: Option<String>,
    /// The value is hidden: it comes from the environment or an env file, and wasn't
    /// asked for by name or with `--values`.
    #[serde(default)]
    pub masked: bool,
    pub source: String,
    /// Sources this value replaced, earliest first.
    pub overrides: Vec<String>,
}

/// Directories and pidfiles nothing that exists owns any more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneList {
    /// Grouped by project and stack; unattributed run directories last.
    pub candidates: Vec<PruneCandidate>,
    pub total_bytes: u64,
    /// Stacks whose folder is among the candidates: their teardown never ran or
    /// failed, so external resources such as databases may remain.
    pub stacks: Vec<PruneStack>,
    /// What was left alone, and why.
    pub skipped: Vec<PruneSkip>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneCandidate {
    pub kind: PruneKind,
    /// `None` for a run directory nothing on disk names.
    pub project: Option<String>,
    pub stack: Option<String>,
    pub app: Option<String>,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PruneKind {
    /// The state folder of a project whose file is gone.
    Project,
    /// The state folder of a stack with no worktree.
    Stack,
    /// The state folder of an app no longer in its project.
    App,
    RunDir,
    Pidfile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneStack {
    pub project: String,
    pub stack: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneSkip {
    /// `None` when the reason covers every project.
    pub project: Option<String>,
    pub stack: Option<String>,
    pub reason: String,
}

/// Removes these candidates, each only if it is still a leftover and not running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneRequest {
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneResult {
    pub removed: Vec<PruneCandidate>,
    pub removed_bytes: u64,
    /// Asked for, but no longer a leftover, or running now.
    pub kept: Vec<PruneKept>,
    /// Couldn't be removed.
    pub errors: Vec<PruneKept>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneKept {
    pub path: PathBuf,
    pub reason: String,
}

/// `carte prune --json`: the leftovers, and what removing them did; `result` is null
/// with `--dry-run` or when there was nothing to remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneOutput {
    pub list: PruneList,
    pub result: Option<PruneResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_enums_serialize_readably() {
        let action = serde_json::to_string(&Action::Pin { save: true }).unwrap();
        assert_eq!(action, r#"{"action":"pin","save":true}"#);
        let state = serde_json::to_string(&ProcessState::Exited {
            code: Some(1),
            signal: None,
        })
        .unwrap();
        assert_eq!(state, r#"{"state":"exited","code":1,"signal":null}"#);
        let state = serde_json::to_string(&ProcessState::Done).unwrap();
        assert_eq!(state, r#"{"state":"done"}"#);
    }

    #[test]
    fn event_subjects_read_like_addresses() {
        let subject =
            |stack: Option<&str>, app: Option<&str>, process: Option<&str>| EventSubject {
                project: "acme".into(),
                stack: stack.map(Into::into),
                app: app.map(Into::into),
                process: process.map(Into::into),
            };
        assert_eq!(subject(None, None, None).label(), "acme");
        assert_eq!(subject(Some("x"), None, None).label(), "@x (acme)");
        assert_eq!(
            subject(Some("main"), Some("acme-web"), None).label(),
            "acme-web"
        );
        assert_eq!(
            subject(Some("x"), Some("acme-web"), Some("worker")).label(),
            "acme-web@x/worker"
        );
    }
}
