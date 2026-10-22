//! The tabs of the pane below the details, and the state of those that keep their own.

use carte_api::EnvResponse;

use super::tree::NodeId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Logs,
    Events,
    Env,
}

impl Tab {
    /// In the order the border lists them, numbered from 1.
    pub const ALL: [Tab; 3] = [Tab::Logs, Tab::Events, Tab::Env];

    pub fn number(self) -> u8 {
        Tab::ALL.iter().position(|&t| t == self).unwrap_or(0) as u8 + 1
    }

    pub fn from_number(number: u8) -> Option<Tab> {
        Tab::ALL.get(usize::from(number).checked_sub(1)?).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            Tab::Logs => "logs",
            Tab::Events => "events",
            Tab::Env => "env",
        }
    }

    /// The tab `delta` places along, wrapping around.
    pub fn step(self, delta: isize) -> Tab {
        let count = Tab::ALL.len() as isize;
        let index = (isize::from(self.number()) - 1 + delta).rem_euclid(count);
        Tab::ALL[index as usize]
    }

    /// Whether the tab has something to show for the selection, `None` when nothing
    /// is selected; `kube` when it is in a kube project.
    pub fn applies(self, id: Option<&NodeId>, kube: bool) -> bool {
        match self {
            Tab::Logs => id.is_some_and(|id| !matches!(id, NodeId::Folder { .. })),
            Tab::Events => true,
            Tab::Env => {
                !kube && matches!(id, Some(NodeId::Instance { .. } | NodeId::Process { .. }))
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_are_numbered_and_step_around() {
        assert_eq!(Tab::ALL.map(Tab::number), [1, 2, 3]);
        assert_eq!(Tab::from_number(3), Some(Tab::Env));
        assert_eq!(Tab::from_number(0), None);
        assert_eq!(Tab::from_number(4), None);
        assert_eq!(Tab::Logs.step(-1), Tab::Env);
        assert_eq!(Tab::Env.step(1), Tab::Logs);
        assert_eq!(Tab::Logs.step(1), Tab::Events);
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
    }
}
