//! Maps tree nodes to the addresses the daemon resolves.

use carte_api::{Address, Snapshot};

use super::tree::NodeId;

/// The address for a node. The project's first checkout is sent as the working
/// directory, so `@stack` and app names resolve inside the node's project even when
/// other projects have the same stack or app.
pub fn address(snapshot: &Snapshot, id: &NodeId) -> Address {
    let address = match id {
        NodeId::Project { project } => project.clone(),
        NodeId::Stack { stack, .. } => format!("@{stack}"),
        NodeId::Instance { stack, app, .. } => format!("{app}@{stack}"),
        NodeId::Process {
            stack,
            app,
            process,
            ..
        } => format!("{app}@{stack}/{process}"),
    };
    let cwd = snapshot
        .projects
        .iter()
        .find(|p| p.name == id.project())
        .and_then(|p| p.apps.first())
        .map(|app| app.path.clone());
    Address { address, cwd }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use carte_core::address::Address as Parsed;

    use super::*;
    use crate::tui::fixtures::{project, snapshot};

    fn ids() -> [NodeId; 4] {
        [
            NodeId::Project {
                project: "make".into(),
            },
            NodeId::Stack {
                project: "make".into(),
                stack: "feature-x".into(),
            },
            NodeId::Instance {
                project: "make".into(),
                stack: "feature-x".into(),
                app: "make-billing".into(),
            },
            NodeId::Process {
                project: "make".into(),
                stack: "main".into(),
                app: "make-billing".into(),
                process: "worker".into(),
            },
        ]
    }

    #[test]
    fn every_level_has_an_address_in_its_project() {
        let snap = snapshot(vec![
            project("acme", &["acme"], Vec::new()),
            project("make", &["make-billing", "make-frontend"], Vec::new()),
        ]);
        let addresses: Vec<String> = ids().iter().map(|id| address(&snap, id).address).collect();
        assert_eq!(
            addresses,
            [
                "make",
                "@feature-x",
                "make-billing@feature-x",
                "make-billing@main/worker"
            ]
        );
        for id in ids() {
            assert_eq!(
                address(&snap, &id).cwd,
                Some(PathBuf::from("/work/make-billing"))
            );
        }
    }

    #[test]
    fn addresses_parse_back_into_their_parts() {
        let snap = snapshot(Vec::new());
        let parsed: Vec<Parsed> = ids()
            .iter()
            .map(|id| address(&snap, id).address.parse().unwrap())
            .collect();
        assert_eq!(parsed[0].app.as_deref(), Some("make"));
        assert_eq!(parsed[1].branch.as_deref(), Some("feature-x"));
        assert_eq!(parsed[1].app, None);
        assert_eq!(parsed[2].app.as_deref(), Some("make-billing"));
        assert_eq!(parsed[3].branch.as_deref(), Some("main"));
        assert_eq!(parsed[3].process.as_deref(), Some("worker"));
    }
}
