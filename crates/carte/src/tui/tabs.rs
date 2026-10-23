//! The tabs of the pane below the details, and the state of those that keep their own.

use carte_api::{EnvResponse, ResolvedConfig};

use super::tree::NodeId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Logs,
    Requests,
    Events,
    Env,
    Config,
}

impl Tab {
    /// In the order the border lists them.
    pub const ALL: [Tab; 5] = [Tab::Logs, Tab::Requests, Tab::Events, Tab::Env, Tab::Config];

    /// The key that picks it.
    pub fn number(self) -> u8 {
        match self {
            Tab::Logs => 1,
            Tab::Requests => 2,
            Tab::Events => 3,
            Tab::Env => 4,
            Tab::Config => 5,
        }
    }

    pub fn from_number(number: u8) -> Option<Tab> {
        Tab::ALL.into_iter().find(|tab| tab.number() == number)
    }

    pub fn name(self) -> &'static str {
        match self {
            Tab::Logs => "logs",
            Tab::Events => "events",
            Tab::Env => "env",
            Tab::Config => "config",
            Tab::Requests => "requests",
        }
    }

    /// The tab `delta` places along, wrapping around.
    pub fn step(self, delta: isize) -> Tab {
        let count = Tab::ALL.len() as isize;
        let at = Tab::ALL.iter().position(|&t| t == self).unwrap_or(0) as isize;
        Tab::ALL[(at + delta).rem_euclid(count) as usize]
    }

    /// Whether the tab has something to show for the selection, `None` when nothing
    /// is selected; `kube` when it is in a kube project.
    pub fn applies(self, id: Option<&NodeId>, kube: bool) -> bool {
        match self {
            Tab::Logs => id.is_some_and(|id| !matches!(id, NodeId::Folder { .. })),
            Tab::Events | Tab::Requests => true,
            Tab::Env => {
                !kube && matches!(id, Some(NodeId::Instance { .. } | NodeId::Process { .. }))
            }
            Tab::Config => id.is_some_and(|id| !matches!(id, NodeId::Folder { .. })),
        }
    }
}

/// What the env tab shows for the selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EnvSubject {
    /// The tab isn't open.
    #[default]
    None,
    /// Why there's no env to show.
    Unavailable(String),
    /// The env of a process, or of an app as the daemon picks its process; `note` says
    /// which process an app's env is from.
    Fetch { id: NodeId, note: Option<String> },
}

#[derive(Debug, Default)]
pub struct EnvTab {
    pub subject: EnvSubject,
    /// Counts subjects; a fetch's answer carries the one it was for.
    pub generation: u64,
    /// A fetch hasn't answered yet.
    pub in_flight: bool,
    /// None while the daemon renders it.
    pub env: Option<Result<EnvResponse, String>>,
    pub scroll: usize,
    /// Rows the tab showed last time it was drawn.
    pub height: usize,
}

impl EnvTab {
    /// Starts over for a new subject.
    pub fn restart(&mut self, subject: EnvSubject) {
        self.subject = subject;
        self.generation += 1;
        self.env = None;
        self.scroll = 0;
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
    }
}

/// The config tab: what carte resolved for the selection.
#[derive(Debug, Default)]
pub struct ConfigTab {
    /// The node the config is for; `None` while the tab isn't open or the selection
    /// is a folder.
    pub subject: Option<NodeId>,
    /// Counts subjects; a fetch's answer carries the one it was for.
    pub generation: u64,
    /// A fetch hasn't answered yet.
    pub in_flight: bool,
    /// None while the daemon resolves it.
    pub config: Option<Result<Box<ResolvedConfig>, String>>,
    /// The runtime the last fetch saw, from `App::config_stamp`.
    pub stamp: u64,
    pub scroll: usize,
    /// Rows the tab showed last time it was drawn.
    pub height: usize,
}

impl ConfigTab {
    /// Starts over for a new subject.
    pub fn restart(&mut self, subject: Option<NodeId>) {
        self.subject = subject;
        self.generation += 1;
        self.config = None;
        self.scroll = 0;
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_are_numbered_and_step_around() {
        assert_eq!(Tab::ALL.map(Tab::number), [1, 2, 3, 4, 5]);
        assert_eq!(Tab::from_number(4), Some(Tab::Env));
        assert_eq!(Tab::from_number(2), Some(Tab::Requests));
        assert_eq!(Tab::from_number(0), None);
        assert_eq!(Tab::from_number(5), Some(Tab::Config));
        assert_eq!(Tab::from_number(6), None);
        assert_eq!(Tab::Logs.step(-1), Tab::Config);
        assert_eq!(Tab::Env.step(1), Tab::Config);
        assert_eq!(Tab::Config.step(1), Tab::Logs);
        assert_eq!(Tab::Requests.step(1), Tab::Events);
        assert_eq!(Tab::Logs.step(1), Tab::Requests);
    }

    #[test]
    fn env_applies_to_apps_and_processes_outside_kube() {
        let folder = NodeId::Folder {
            folder: "work".into(),
        };
        let app = NodeId::Instance {
            project: "nova".into(),
            stack: "main".into(),
            app: "nova-billing".into(),
        };
        assert!(!Tab::Env.applies(Some(&folder), false));
        assert!(!Tab::Env.applies(None, false));
        assert!(Tab::Env.applies(Some(&app), false));
        assert!(!Tab::Env.applies(Some(&app), true));
        assert!(!Tab::Logs.applies(Some(&folder), false));
        assert!(Tab::Events.applies(None, false));
        assert!(Tab::Config.applies(Some(&app), true));
        assert!(!Tab::Config.applies(Some(&folder), false));
        assert!(!Tab::Config.applies(None, false));
    }
}
