//! Kube projects: a kubeconfig context whose Services are the apps and whose
//! namespaces are the stacks, reached as `service.namespace.project.<tld>`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model::{EnvProviderKind, Policy, World};
use crate::naming::{self, NotFound};

/// A loaded kube project file.
#[derive(Debug, Clone, PartialEq)]
pub struct KubeProject {
    pub name: String,
    pub config_path: PathBuf,
    /// The subdirectory of `projects/` its file is in, for grouping in the TUI only.
    pub folder: Option<String>,
    pub context: String,
    /// `None` uses `$KUBECONFIG`, then `~/.kube/config`.
    pub kubeconfig: Option<PathBuf>,
    /// The namespaces lookups and browsing are limited to; empty for all.
    pub namespaces: Vec<String>,
    /// The env kubeconfig login plugins run with.
    pub env_provider: Option<EnvProviderKind>,
    pub idle_timeout: Option<Duration>,
    /// `[apps.<service>]` tables, which add settings to Services the cluster has.
    pub services: BTreeMap<String, KubeService>,
}

impl KubeProject {
    pub fn service(&self, name: &str) -> Option<&KubeService> {
        self.services.get(name)
    }

    /// Whether lookups may look in `namespace`.
    pub fn allows_namespace(&self, namespace: &str) -> bool {
        self.namespaces.is_empty() || self.namespaces.iter().any(|n| n == namespace)
    }

    /// The service's policy; only `auto` and `disabled` apply.
    pub fn policy(&self, service: &str) -> Policy {
        self.service(service)
            .and_then(|s| s.policy)
            .unwrap_or_default()
    }
}

/// Settings of one Service from its `[apps.<service>]` table.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct KubeService {
    pub aliases: Vec<String>,
    /// The namespace the short hostname and aliases mean, instead of looking it up.
    pub namespace: Option<String>,
    pub service_port: Option<ServicePort>,
    pub policy: Option<Policy>,
}

/// A port of a Service, by its name or number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ServicePort {
    Number(u16),
    Name(String),
}

impl<'de> Deserialize<'de> for ServicePort {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(i64),
            Name(String),
        }
        let invalid = || {
            D::Error::custom(
                "service_port is a port name, like \"http\", or a number from 1 to 65535",
            )
        };
        match Raw::deserialize(d).map_err(|_| invalid())? {
            Raw::Number(n) => u16::try_from(n)
                .ok()
                .filter(|n| *n > 0)
                .map(ServicePort::Number)
                .ok_or_else(invalid),
            Raw::Name(name) if name.parse::<u16>().is_ok() => Err(D::Error::custom(format!(
                "service_port = \"{name}\" is a number in quotes; write service_port = {name}"
            ))),
            Raw::Name(name) if name.is_empty() => Err(invalid()),
            Raw::Name(name) => Ok(ServicePort::Name(name)),
        }
    }
}

impl fmt::Display for ServicePort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServicePort::Number(n) => write!(f, "{n}"),
            ServicePort::Name(name) => f.write_str(name),
        }
    }
}

/// A Service a hostname names. `namespace: None` is the short form, which the cluster
/// resolves to the one namespace that has the Service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KubeTarget {
    pub project: String,
    pub service: String,
    pub namespace: Option<String>,
    pub via_alias: bool,
}

/// Resolves a `Host` to a Service of a kube project; `None` when the host isn't a
/// kube project's or one of its aliases.
pub fn resolve_host(world: &World, host: &str) -> Option<Result<KubeTarget, NotFound>> {
    let labels = naming::labels_under_tld(world, host).ok()?;
    let (top, left) = labels.split_last()?;
    if let Some(project) = world.kube_project(top) {
        return Some(resolve_in_project(project, left));
    }
    let (project, service) = world.kube_projects.iter().find_map(|project| {
        project
            .services
            .iter()
            .find(|(_, s)| s.aliases.iter().any(|a| a == top))
            .map(|(name, _)| (project, name))
    })?;
    if !left.is_empty() {
        return Some(Err(NotFound::KubeSubdomains {
            project: project.name.clone(),
            labels: left.to_vec(),
        }));
    }
    let mut labels = vec![service.clone()];
    labels.extend(project.service(service).and_then(|s| s.namespace.clone()));
    Some(
        resolve_in_project(project, &labels).map(|target| KubeTarget {
            via_alias: true,
            ..target
        }),
    )
}

fn resolve_in_project(project: &KubeProject, labels: &[String]) -> Result<KubeTarget, NotFound> {
    let (service, namespace) = match labels {
        [] => {
            return Err(NotFound::NoApp {
                project: project.name.clone(),
                stack: None,
            });
        }
        [service] => (
            service,
            project.service(service).and_then(|s| s.namespace.clone()),
        ),
        [service, namespace] => (service, Some(namespace.clone())),
        [extra @ .., _, _] => {
            return Err(NotFound::KubeSubdomains {
                project: project.name.clone(),
                labels: extra.to_vec(),
            });
        }
    };
    if let Some(namespace) = &namespace
        && !project.allows_namespace(namespace)
    {
        return Err(NotFound::KubeNamespace {
            project: project.name.clone(),
            namespace: namespace.clone(),
        });
    }
    Ok(KubeTarget {
        project: project.name.clone(),
        service: service.clone(),
        namespace,
        via_alias: false,
    })
}

/// `service.namespace.project.<tld>`.
pub fn host(tld: &str, project: &str, namespace: &str, service: &str) -> String {
    format!("{service}.{namespace}.{project}.{tld}")
}

/// The hostname label a kubeconfig context suggests for its project: the cluster name
/// of EKS and GKE contexts, otherwise the context made into a label.
pub fn context_label(context: &str) -> String {
    let name = if let Some((_, cluster)) = context
        .strip_prefix("arn:")
        .and_then(|arn| arn.rsplit_once(":cluster/"))
    {
        cluster
    } else if context.starts_with("gke_") {
        context.rsplit('_').next().unwrap_or(context)
    } else {
        context
    };
    crate::slug::slugify(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    fn project(name: &str) -> KubeProject {
        KubeProject {
            name: name.into(),
            config_path: PathBuf::from(format!("/c/projects/{name}.toml")),
            folder: None,
            context: format!("{name}-context"),
            kubeconfig: None,
            namespaces: Vec::new(),
            env_provider: None,
            idle_timeout: None,
            services: BTreeMap::new(),
        }
    }

    fn world() -> World {
        let mut prod = project("prod");
        prod.services.insert(
            "grafana".into(),
            KubeService {
                aliases: vec!["grafana-prod".into()],
                namespace: Some("monitoring".into()),
                ..KubeService::default()
            },
        );
        prod.services.insert(
            "vlogs".into(),
            KubeService {
                aliases: vec!["logs".into()],
                ..KubeService::default()
            },
        );
        let mut staging = project("staging");
        staging.namespaces = vec!["monitoring".into()];
        World {
            kube_projects: vec![prod, staging],
            ..test_support::world()
        }
    }

    fn target(service: &str, namespace: Option<&str>, via_alias: bool) -> KubeTarget {
        KubeTarget {
            project: "prod".into(),
            service: service.into(),
            namespace: namespace.map(Into::into),
            via_alias,
        }
    }

    #[test]
    fn resolves_service_and_namespace() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "vlogs.logging.prod.test"),
            Some(Ok(target("vlogs", Some("logging"), false)))
        );
        assert_eq!(
            resolve_host(&world, "Vlogs.prod.test:443"),
            Some(Ok(target("vlogs", None, false)))
        );
    }

    #[test]
    fn a_configured_namespace_fills_in_the_short_form() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "grafana.prod.test"),
            Some(Ok(target("grafana", Some("monitoring"), false)))
        );
        assert_eq!(
            resolve_host(&world, "grafana.other.prod.test"),
            Some(Ok(target("grafana", Some("other"), false)))
        );
    }

    #[test]
    fn aliases_route_to_their_service() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "grafana-prod.test"),
            Some(Ok(target("grafana", Some("monitoring"), true)))
        );
        assert_eq!(
            resolve_host(&world, "logs.localhost"),
            Some(Ok(target("vlogs", None, true)))
        );
        assert!(matches!(
            resolve_host(&world, "www.logs.test"),
            Some(Err(NotFound::KubeSubdomains { .. }))
        ));
    }

    #[test]
    fn refuses_extra_labels_and_other_namespaces() {
        let world = world();
        assert_eq!(
            resolve_host(&world, "www.grafana.monitoring.prod.test"),
            Some(Err(NotFound::KubeSubdomains {
                project: "prod".into(),
                labels: vec!["www".into()],
            }))
        );
        assert_eq!(
            resolve_host(&world, "grafana.billing.staging.test"),
            Some(Err(NotFound::KubeNamespace {
                project: "staging".into(),
                namespace: "billing".into(),
            }))
        );
        assert_eq!(
            resolve_host(&world, "prod.test"),
            Some(Err(NotFound::NoApp {
                project: "prod".into(),
                stack: None,
            }))
        );
    }

    #[test]
    fn leaves_other_hosts_alone() {
        let world = world();
        assert_eq!(resolve_host(&world, "shop.test"), None);
        assert_eq!(resolve_host(&world, "grafana.prod.example.com"), None);
        assert_eq!(resolve_host(&world, "[::1]"), None);
    }

    #[test]
    fn labels_from_contexts() {
        assert_eq!(
            context_label("arn:aws:eks:eu-west-1:123456:cluster/production"),
            "production"
        );
        assert_eq!(
            context_label("gke_acme-infra_europe-north1_staging"),
            "staging"
        );
        assert_eq!(context_label("kind-nova"), "kind-nova");
        assert_eq!(context_label("Nova Admin@shop"), "nova-admin-shop");
    }
}
