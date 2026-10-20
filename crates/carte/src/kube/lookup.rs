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

/// Whether carte speaks TLS to the pod on this Service port: its `appProtocol` says
/// so, else its name, else port 443. A port named or marked as HTTP never is, like a
/// load balancer's 443 that ends TLS before the pod.
pub fn is_tls(port: &PortInfo) -> bool {
    let says = |value: Option<&str>| {
        let value = value?.to_ascii_lowercase();
        let value = value.strip_prefix("kubernetes.io/").unwrap_or(&value);
        if value == "https"
            || value.starts_with("https-")
            || value == "wss"
            || value.starts_with("wss-")
        {
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

/// A container of a pod, for picking whose logs to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    pub name: String,
    pub ports: Vec<u16>,
}

/// The container serving `port`, rather than a sidecar; else the first one.
pub fn choose_container(containers: &[ContainerInfo], port: u16) -> Option<&ContainerInfo> {
    containers
        .iter()
        .find(|c| c.ports.contains(&port))
        .or_else(|| containers.first())
}

/// Splits the RFC 3339 timestamp the API server puts in front of each log line with
/// `timestamps=true`: nanoseconds since the Unix epoch, and the rest of the line.
pub fn split_timestamp(line: &str) -> (Option<u64>, &str) {
    let Some((stamp, text)) = line.split_once(' ') else {
        return (None, line);
    };
    match parse_rfc3339_ns(stamp) {
        Some(at_ns) => (Some(at_ns), text),
        None => (None, line),
    }
}

/// `2026-10-20T08:15:30.123456789Z` in nanoseconds; only UTC, as the API server
/// writes it.
fn parse_rfc3339_ns(stamp: &str) -> Option<u64> {
    let stamp = stamp.strip_suffix('Z')?;
    let (date, time) = stamp.split_once('T')?;
    let mut date = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut clock = clock.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    let nanos: i64 = format!("{fraction:0<9}").get(..9)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(0..10_000).contains(&year) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(secs.checked_mul(1_000_000_000)? + nanos).ok()
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
        assert!(is_tls(&with(Some("wss"), 8443, None)));
        assert!(is_tls(&with(Some("wss-api"), 8443, None)));
        assert!(is_tls(&with(None, 8443, Some("wss"))));
        assert!(!is_tls(&with(Some("ws"), 443, None)));
        assert!(!is_tls(&with(None, 443, Some("ws"))));
        assert!(is_tls(&with(None, 8443, Some("kubernetes.io/wss"))));
        assert!(!is_tls(&with(None, 443, Some("kubernetes.io/ws"))));
        assert!(!is_tls(&with(
            Some("https"),
            8443,
            Some("kubernetes.io/ws")
        )));
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

    #[test]
    fn picks_the_container_serving_the_port() {
        let container = |name: &str, ports: &[u16]| ContainerInfo {
            name: name.into(),
            ports: ports.to_vec(),
        };
        let containers = vec![
            container("istio-proxy", &[15090]),
            container("grafana", &[3000]),
        ];
        assert_eq!(
            choose_container(&containers, 3000).map(|c| c.name.as_str()),
            Some("grafana")
        );
        assert_eq!(
            choose_container(&containers, 8080).map(|c| c.name.as_str()),
            Some("istio-proxy")
        );
        assert_eq!(choose_container(&[], 80), None);
    }

    #[test]
    fn splits_timestamps_off_log_lines() {
        assert_eq!(
            split_timestamp("2026-10-20T08:15:30.123456789Z GET /api/health 200"),
            (Some(1_792_484_130_123_456_789), "GET /api/health 200")
        );
        assert_eq!(
            split_timestamp("1970-01-01T00:00:01Z started"),
            (Some(1_000_000_000), "started")
        );
        assert_eq!(
            split_timestamp("no timestamp here"),
            (None, "no timestamp here")
        );
        assert_eq!(split_timestamp("plain"), (None, "plain"));
        assert_eq!(
            split_timestamp("2026-10-20T08:15:30.aaé started"),
            (None, "2026-10-20T08:15:30.aaé started")
        );
        assert_eq!(
            split_timestamp("99999999999-01-01T00:00:00Z x"),
            (None, "99999999999-01-01T00:00:00Z x")
        );
    }
}
