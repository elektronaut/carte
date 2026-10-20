//! Choosing a Service's port and a pod behind it, from what the cluster reports.

use carte_core::kube::ServicePort;

/// The parts of a Service carte uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub namespace: String,
    pub name: String,
    /// An ExternalName Service, which has no pods to forward to.
    pub external: bool,
    pub ports: Vec<PortInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortInfo {
    pub name: Option<String>,
    pub port: u16,
    /// The port's `appProtocol`, like `https` or `kubernetes.io/h2c`.
    pub app_protocol: Option<String>,
}

impl PortInfo {
    fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("{name} ({})", self.port),
            None => self.port.to_string(),
        }
    }
}

/// One EndpointSlice of a Service: its ports, already resolved to the pods' container
/// ports, and its endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceInfo {
    pub ports: Vec<PortInfo>,
    pub endpoints: Vec<EndpointInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointInfo {
    /// The pod behind the endpoint; `None` for endpoints that aren't pods.
    pub pod: Option<String>,
    pub ready: bool,
}

/// Where connections to a Service go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodTarget {
    pub pod: String,
    /// The pod's port.
    pub port: u16,
    /// The port speaks TLS, so carte connects with TLS without verifying it.
    pub tls: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    ExternalName,
    NoPorts,
    /// None of the rules picks a port; the Service's ports, as `name (port)`.
    PortNotChosen(Vec<String>),
    /// `service_port` names a port the Service doesn't have.
    UnknownPort {
        wanted: String,
        ports: Vec<String>,
    },
    NoReadyPod,
}

impl LookupError {
    pub fn title(&self) -> &'static str {
        match self {
            LookupError::ExternalName => "Can't forward to an ExternalName Service",
            LookupError::NoPorts => "The Service has no ports",
            LookupError::PortNotChosen(_) | LookupError::UnknownPort { .. } => "Which port?",
            LookupError::NoReadyPod => "No pod is ready",
        }
    }

    pub fn detail(&self, service: &str, project: &str) -> String {
        match self {
            LookupError::ExternalName => format!(
                "{service} is an ExternalName Service: it points at a hostname outside the cluster, with no pods to forward to."
            ),
            LookupError::NoPorts => format!("{service} declares no ports."),
            LookupError::PortNotChosen(ports) => format!(
                "{service} has several ports: {}. Pick one with `service_port` in [apps.{service}] of the {project} project file.",
                ports.join(", ")
            ),
            LookupError::UnknownPort { wanted, ports } => format!(
                "service_port = {wanted:?}, but {service} has these ports: {}.",
                ports.join(", ")
            ),
            LookupError::NoReadyPod => format!(
                "No ready pod is behind {service}. It may be scaled to zero, starting up, or have no selector."
            ),
        }
    }
}

/// The Service port to forward to: `wanted`, else the only port, else one named
/// `http`, `web` or `http-*`, else 80, 8080 or 443.
pub fn choose_port(
    service: &ServiceInfo,
    wanted: Option<&ServicePort>,
) -> Result<PortInfo, LookupError> {
    if service.external {
        return Err(LookupError::ExternalName);
    }
    let labels = || service.ports.iter().map(PortInfo::label).collect();
    if let Some(wanted) = wanted {
        let found = service.ports.iter().find(|p| match wanted {
            ServicePort::Number(n) => p.port == *n,
            ServicePort::Name(name) => p.name.as_deref() == Some(name.as_str()),
        });
        return found.cloned().ok_or_else(|| LookupError::UnknownPort {
            wanted: wanted.to_string(),
            ports: labels(),
        });
    }
    match service.ports.as_slice() {
        [] => return Err(LookupError::NoPorts),
        [only] => return Ok(only.clone()),
        _ => {}
    }
    let named = service.ports.iter().find(|p| {
        p.name
            .as_deref()
            .is_some_and(|n| n == "http" || n == "web" || n.starts_with("http-"))
    });
    let numbered = || {
        [80, 8080, 443]
            .iter()
            .find_map(|n| service.ports.iter().find(|p| p.port == *n))
    };
    named
        .or_else(numbered)
        .cloned()
        .ok_or_else(|| LookupError::PortNotChosen(labels()))
}

/// Whether carte speaks TLS to the pod on this Service port.
/// Whether carte speaks TLS to the pod on this Service port: its `appProtocol` says
/// so, else its name, else port 443. A port named or marked as HTTP never is, like a
/// load balancer's 443 that ends TLS before the pod.
pub fn is_tls(port: &PortInfo) -> bool {
    let says = |value: Option<&str>| {
        let value = value?.to_ascii_lowercase();
        if value == "https" || value.starts_with("https-") {
            Some(true)
        } else if value.starts_with("http") || value.contains("h2c") || value.starts_with("ws") {
            Some(false)
        } else {
            None
        }
    };
    says(port.app_protocol.as_deref())
        .or_else(|| says(port.name.as_deref()))
        .unwrap_or(port.port == 443)
}

/// A ready pod behind the Service port, preferring `current` while it stays ready.
pub fn choose_pod(
    port: &PortInfo,
    slices: &[SliceInfo],
    current: Option<&str>,
) -> Result<PodTarget, LookupError> {
    let mut ready = Vec::new();
    for slice in slices {
        let pod_port = slice
            .ports
            .iter()
            .find(|p| p.name == port.name)
            .or_else(|| (slice.ports.len() == 1).then(|| &slice.ports[0]));
        let Some(pod_port) = pod_port else {
            continue;
        };
        for endpoint in &slice.endpoints {
            if let (true, Some(pod)) = (endpoint.ready, &endpoint.pod) {
                ready.push((pod.clone(), pod_port.port));
            }
        }
    }
    let chosen = current
        .and_then(|current| ready.iter().find(|(pod, _)| pod == current))
        .or_else(|| ready.first())
        .ok_or(LookupError::NoReadyPod)?;
    Ok(PodTarget {
        pod: chosen.0.clone(),
        port: chosen.1,
        tls: is_tls(port),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(name: Option<&str>, port: u16) -> PortInfo {
        PortInfo {
            name: name.map(Into::into),
            port,
            app_protocol: None,
        }
    }

    #[test]
    fn tls_follows_app_protocol_then_name_then_443() {
        let with = |name: Option<&str>, number: u16, app: Option<&str>| PortInfo {
            app_protocol: app.map(Into::into),
            ..port(name, number)
        };
        assert!(is_tls(&with(None, 443, None)));
        assert!(is_tls(&with(Some("https"), 8443, None)));
        assert!(is_tls(&with(None, 8443, Some("https"))));
        assert!(
            !is_tls(&with(Some("http"), 443, None)),
            "a load balancer's 443"
        );
        assert!(!is_tls(&with(None, 443, Some("kubernetes.io/h2c"))));
        assert!(!is_tls(&with(Some("https"), 443, Some("http"))));
        assert!(!is_tls(&with(Some("web"), 80, None)));
    }

    fn service(ports: Vec<PortInfo>) -> ServiceInfo {
        ServiceInfo {
            namespace: "monitoring".into(),
            name: "grafana".into(),
            external: false,
            ports,
        }
    }

    fn endpoint(pod: &str, ready: bool) -> EndpointInfo {
        EndpointInfo {
            pod: Some(pod.into()),
            ready,
        }
    }

    #[test]
    fn takes_the_only_port() {
        let s = service(vec![port(Some("service"), 3000)]);
        assert_eq!(choose_port(&s, None), Ok(port(Some("service"), 3000)));
    }

    #[test]
    fn prefers_http_names_then_common_numbers() {
        let s = service(vec![port(Some("grpc"), 9000), port(Some("http-web"), 3000)]);
        assert_eq!(choose_port(&s, None), Ok(port(Some("http-web"), 3000)));
        let s = service(vec![port(Some("metrics"), 9100), port(None, 8080)]);
        assert_eq!(choose_port(&s, None), Ok(port(None, 8080)));
        let s = service(vec![port(Some("metrics"), 9100), port(Some("grpc"), 9000)]);
        assert_eq!(
            choose_port(&s, None),
            Err(LookupError::PortNotChosen(vec![
                "metrics (9100)".into(),
                "grpc (9000)".into()
            ]))
        );
    }

    #[test]
    fn a_configured_port_wins() {
        let s = service(vec![port(Some("http"), 80), port(Some("admin"), 9093)]);
        assert_eq!(
            choose_port(&s, Some(&ServicePort::Name("admin".into()))),
            Ok(port(Some("admin"), 9093))
        );
        assert_eq!(
            choose_port(&s, Some(&ServicePort::Number(9093))),
            Ok(port(Some("admin"), 9093))
        );
        assert!(matches!(
            choose_port(&s, Some(&ServicePort::Name("nope".into()))),
            Err(LookupError::UnknownPort { .. })
        ));
    }

    #[test]
    fn refuses_external_names_and_portless_services() {
        let mut s = service(vec![port(None, 80)]);
        s.external = true;
        assert_eq!(choose_port(&s, None), Err(LookupError::ExternalName));
        assert_eq!(
            choose_port(&service(Vec::new()), None),
            Err(LookupError::NoPorts)
        );
    }

    #[test]
    fn picks_a_ready_pod_on_the_matching_port() {
        let slices = vec![
            SliceInfo {
                ports: vec![port(Some("http"), 3000), port(Some("metrics"), 9100)],
                endpoints: vec![endpoint("grafana-a", false), endpoint("grafana-b", true)],
            },
            SliceInfo {
                ports: vec![port(Some("http"), 3000)],
                endpoints: vec![endpoint("grafana-c", true)],
            },
        ];
        let http = port(Some("http"), 80);
        assert_eq!(
            choose_pod(&http, &slices, None),
            Ok(PodTarget {
                pod: "grafana-b".into(),
                port: 3000,
                tls: false,
            })
        );
        assert_eq!(
            choose_pod(&http, &slices, Some("grafana-c")).map(|t| t.pod),
            Ok("grafana-c".into())
        );
        assert_eq!(
            choose_pod(&http, &slices, Some("grafana-a")).map(|t| t.pod),
            Ok("grafana-b".into())
        );
    }

    #[test]
    fn unnamed_single_ports_match_and_tls_follows_the_service_port() {
        let slices = vec![SliceInfo {
            ports: vec![port(None, 8443)],
            endpoints: vec![endpoint("api-a", true)],
        }];
        assert_eq!(
            choose_pod(&port(None, 443), &slices, None),
            Ok(PodTarget {
                pod: "api-a".into(),
                port: 8443,
                tls: true,
            })
        );
    }

    #[test]
    fn no_ready_pod_is_an_error() {
        let slices = vec![SliceInfo {
            ports: vec![port(None, 80)],
            endpoints: vec![
                endpoint("web-a", false),
                EndpointInfo {
                    pod: None,
                    ready: true,
                },
            ],
        }];
        assert_eq!(
            choose_pod(&port(None, 80), &slices, None),
            Err(LookupError::NoReadyPod)
        );
    }
}
