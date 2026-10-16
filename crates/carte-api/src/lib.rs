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
    pub const ENV: &str = "/v1/env";
    pub const RELOAD: &str = "/v1/reload";
}

/// A CLI address (`app@branch/process`, any part optional) and the directory it was
/// typed in, which fills in the omitted parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Address {
    pub address: String,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub projects: Vec<ProjectStatus>,
    pub problems: Vec<Problem>,
    pub running_instances: u32,
    pub memory_bytes: u64,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackStatus {
    /// `main` or the branch slug.
    pub name: String,
    pub branch: Option<String>,
    /// One entry per app in the project: what the app resolves to in this stack,
    /// running or not.
    pub instances: Vec<InstanceStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub app: String,
    pub stack: String,
    /// `main` or the worktree's branch slug; `main` inside a stack is a fallback.
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopAllRequest {
    pub idle_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsRequest {
    pub target: Address,
    pub follow: bool,
    /// Lines from the buffer to send before following.
    pub tail: Option<usize>,
    pub search: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    pub app: String,
    pub stack: String,
    pub process: String,
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The line with escape codes stripped.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvResponse {
    pub instance: String,
    pub vars: Vec<EnvVar>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
    pub source: String,
    /// Sources this value replaced, earliest first.
    pub overrides: Vec<String>,
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
    }
}
