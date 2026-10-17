//! Maps tree nodes to the addresses the daemon resolves.

use carte_api::Address;

use super::tree::NodeId;

/// The node as the CLI would address it, for titles and keys.
pub fn label(id: &NodeId) -> String {
    match id {
        NodeId::Folder { folder } => format!("{folder}/"),
        NodeId::Project { project } => project.clone(),
        NodeId::Stack { stack, .. } => format!("@{stack}"),
        NodeId::Instance { stack, app, .. } => format!("{app}@{stack}"),
        NodeId::Process {
            stack,
            app,
            process,
            ..
        } => format!("{app}@{stack}/{process}"),
    }
}

/// The address for a node, resolved inside the node's project so that other
/// projects' stacks and apps, or an app named like the project, don't interfere.
pub fn address(id: &NodeId) -> Address {
    let address = match id {
        NodeId::Project { .. } => String::new(),
        _ => label(id),
    };
    let project = match id {
        NodeId::Folder { .. } => None,
        _ => Some(id.project().to_string()),
    };
    Address {
        address,
        cwd: None,
        project,
    }
}

#[cfg(test)]
mod tests {
    use carte_core::address::Address as Parsed;

    use super::*;

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
        let labels: Vec<String> = ids().iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "make",
                "@feature-x",
                "make-billing@feature-x",
                "make-billing@main/worker"
            ]
        );
        let addresses: Vec<String> = ids().iter().map(|id| address(id).address).collect();
        assert_eq!(
            addresses,
            [
                "",
                "@feature-x",
                "make-billing@feature-x",
                "make-billing@main/worker"
            ]
        );
        for id in ids() {
            assert_eq!(address(&id).project.as_deref(), Some("make"));
        }
    }

    #[test]
    fn addresses_parse_back_into_their_parts() {
        let parsed: Vec<Parsed> = ids()
            .iter()
            .map(|id| address(id).address.parse().unwrap())
            .collect();
        assert_eq!(parsed[0], Parsed::default());
        assert_eq!(parsed[1].branch.as_deref(), Some("feature-x"));
        assert_eq!(parsed[1].app, None);
        assert_eq!(parsed[2].app.as_deref(), Some("make-billing"));
        assert_eq!(parsed[3].branch.as_deref(), Some("main"));
        assert_eq!(parsed[3].process.as_deref(), Some("worker"));
    }
}
