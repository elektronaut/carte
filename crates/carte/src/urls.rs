//! URLs and hostnames that templates and injected variables point at: every app in the same stack.

use std::collections::BTreeSet;

use carte_core::model::{App, MachineConfig, ProjectState, Stack};
use carte_core::naming;
use carte_core::template::UrlResolver;

pub struct StackUrls<'a> {
    pub machine: &'a MachineConfig,
    pub state: &'a ProjectState,
    pub stack: &'a Stack,
    /// Apps of this stack that are shared right now.
    pub shared: &'a BTreeSet<String>,
}

impl UrlResolver for StackUrls<'_> {
    fn url(&self, app: &App) -> String {
        naming::instance_url(self.machine, &self.state.project, app, self.stack)
    }

    fn public_url(&self, app: &App) -> Option<String> {
        if !self.shared.contains(&app.name) {
            return None;
        }
        let share = self.machine.share.as_ref()?;
        naming::public_url(share, self.state, app, self.stack).ok()
    }

    fn host(&self, app: &App) -> String {
        naming::local_host(&self.machine.tld, &self.state.project, app, self.stack)
    }

    fn public_host(&self, app: &App) -> Option<String> {
        if !self.shared.contains(&app.name) {
            return None;
        }
        let share = self.machine.share.as_ref()?;
        naming::public_host(share, self.state, app, self.stack).ok()
    }
}
