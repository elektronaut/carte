//! A kube project's Services as listed by the cluster, merged into the snapshot so
//! the ones carte doesn't know of yet show beside those it does.

use std::collections::BTreeMap;
use std::path::PathBuf;

use carte_api::{
    InstanceState, InstanceStatus, KubeInstanceInfo, KubeServiceEntry, Snapshot, StackStatus,
};

/// Where a kube project's listing stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Browse {
    /// Asked for, still showing the last listing while the new one comes.
    Loading(Option<Vec<KubeServiceEntry>>),
    Loaded(Vec<KubeServiceEntry>),
    Failed(String),
}

/// Adds the listed Services carte doesn't know of to their kube projects as stopped
/// instances, and orders every kube project's namespaces by name.
pub fn merge(snapshot: &mut Snapshot, browse: &BTreeMap<String, Browse>) {
    for project in snapshot.projects.iter_mut().filter(|p| p.kube.is_some()) {
        if let Some(Browse::Loaded(services) | Browse::Loading(Some(services))) =
            browse.get(&project.name)
        {
            for service in services {
                let stack = match project
                    .stacks
                    .iter_mut()
                    .position(|s| s.name == service.namespace)
                {
                    Some(i) => &mut project.stacks[i],
                    None => {
                        project.stacks.push(StackStatus {
                            name: service.namespace.clone(),
                            branch: None,
                            instances: Vec::new(),
                        });
                        project.stacks.last_mut().expect("just pushed")
                    }
                };
                if stack.instances.iter().all(|i| i.app != service.name) {
                    stack.instances.push(listed(service));
                }
            }
            for stack in &mut project.stacks {
                stack.instances.sort_by(|a, b| a.app.cmp(&b.app));
            }
        }
        project.stacks.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

fn listed(service: &KubeServiceEntry) -> InstanceStatus {
    InstanceStatus {
        app: service.name.clone(),
        stack: service.namespace.clone(),
        checkout: String::new(),
        checkout_path: PathBuf::new(),
        url: service.url.clone(),
        public_url: None,
        state: InstanceState::Stopped,
        processes: Vec::new(),
        holds: Vec::new(),
        idle_in_secs: None,
        memory_bytes: 0,
        cpu_percent: 0.0,
        last_error: None,
        env_changed: false,
        last_event: None,
        boot: None,
        warms: Vec::new(),
        kube: Some(KubeInstanceInfo {
            pod: None,
            port: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fixtures::kube_sample;

    fn entry(namespace: &str, name: &str) -> KubeServiceEntry {
        KubeServiceEntry {
            namespace: namespace.into(),
            name: name.into(),
            url: format!("https://{name}.{namespace}.prod.test"),
        }
    }

    #[test]
    fn adds_unknown_services_and_orders_namespaces() {
        let mut snapshot = kube_sample();
        let browse = BTreeMap::from([(
            "prod".to_string(),
            Browse::Loaded(vec![
                entry("kube-system", "kube-dns"),
                entry("monitoring", "alertmanager"),
                entry("monitoring", "grafana"),
                entry("billing", "api"),
            ]),
        )]);
        merge(&mut snapshot, &browse);
        let prod = &snapshot.projects[0];
        let namespaces: Vec<&str> = prod.stacks.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            namespaces,
            vec!["billing", "kube-system", "logging", "monitoring"]
        );
        let monitoring = &prod.stacks[3];
        let names: Vec<&str> = monitoring
            .instances
            .iter()
            .map(|i| i.app.as_str())
            .collect();
        assert_eq!(names, vec!["alertmanager", "grafana"]);
        assert_eq!(
            monitoring.instances[1].state,
            InstanceState::Ready,
            "a known Service keeps its status"
        );
        assert_eq!(monitoring.instances[0].state, InstanceState::Stopped);
        assert_eq!(
            monitoring.instances[0].url,
            "https://alertmanager.monitoring.prod.test"
        );
    }

    #[test]
    fn leaves_projects_without_a_listing_alone() {
        let mut snapshot = kube_sample();
        let before = snapshot.clone();
        merge(&mut snapshot, &BTreeMap::new());
        assert_eq!(snapshot, before);
    }
}
