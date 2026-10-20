//! Kube projects against a fake cluster whose pods are local HTTP servers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use carte::kube::{
    ApiError, Cluster, Connector, EndpointInfo, Kube, PodStream, PortInfo, ServiceInfo, SliceInfo,
};
use carte::paths::Paths;
use carte::process::Spawner;
use carte::proxy::{BoxFuture, Route};
use carte::supervisor::{self, Handle, Options};
use carte_api::{Event, EventKind};
use carte_core::discovery;
use carte_core::kube::{KubeProject, KubeTarget};
use http::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
struct FakeState {
    services: Vec<ServiceInfo>,
    slices: HashMap<(String, String), Vec<SliceInfo>>,
    /// Each pod's local HTTP server.
    pods: HashMap<String, u16>,
    gone: HashSet<String>,
    auth_fails: bool,
    connects: usize,
    /// Listing Services across namespaces is forbidden.
    forbid_all: bool,
    /// Listing namespaces is forbidden.
    forbid_namespaces: bool,
    /// Namespaces whose Services can't be listed.
    forbidden: HashSet<String>,
}

#[derive(Clone, Default)]
struct FakeCluster(Arc<Mutex<FakeState>>);

impl FakeCluster {
    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.0.lock().unwrap()
    }

    fn check_auth(&self) -> Result<(), ApiError> {
        if self.state().auth_fails {
            return Err(ApiError::Auth {
                message: "aws exited with exit status: 255:\nthe SSO session has expired".into(),
                hint: Some("Run `aws sso login`, then reload.".into()),
            });
        }
        Ok(())
    }

    /// A Service with one `http` port whose pods serve their own name.
    async fn add_service(&self, namespace: &str, name: &str, pods: &[&str]) {
        let mut endpoints = Vec::new();
        for pod in pods {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let body = pod.to_string();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let body = body.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf).await;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    });
                }
            });
            self.state().pods.insert(pod.to_string(), port);
            endpoints.push(EndpointInfo {
                pod: Some(pod.to_string()),
                ready: true,
            });
        }
        let mut state = self.state();
        state.services.push(ServiceInfo {
            namespace: namespace.into(),
            name: name.into(),
            external: false,
            ports: vec![PortInfo {
                name: Some("http".into()),
                port: 80,
                ..PortInfo::default()
            }],
        });
        state.slices.insert(
            (namespace.into(), name.into()),
            vec![SliceInfo {
                ports: vec![PortInfo {
                    name: Some("http".into()),
                    port: 3000,
                    ..PortInfo::default()
                }],
                endpoints,
            }],
        );
    }
}

impl Cluster for FakeCluster {
    fn service<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ServiceInfo>, ApiError>> {
        Box::pin(async move {
            self.check_auth()?;
            Ok(self
                .state()
                .services
                .iter()
                .find(|s| s.namespace == namespace && s.name == name)
                .cloned())
        })
    }

    fn services_named<'a>(
        &'a self,
        name: &'a str,
        namespaces: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<ServiceInfo>, ApiError>> {
        Box::pin(async move {
            self.check_auth()?;
            Ok(self
                .state()
                .services
                .iter()
                .filter(|s| s.name == name)
                .filter(|s| namespaces.is_empty() || namespaces.contains(&s.namespace))
                .cloned()
                .collect())
        })
    }

    fn endpoint_slices<'a>(
        &'a self,
        namespace: &'a str,
        service: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SliceInfo>, ApiError>> {
        Box::pin(async move {
            self.check_auth()?;
            Ok(self
                .state()
                .slices
                .get(&(namespace.to_string(), service.to_string()))
                .cloned()
                .unwrap_or_default())
        })
    }

    fn services<'a>(
        &'a self,
        namespace: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<ServiceInfo>, ApiError>> {
        Box::pin(async move {
            self.check_auth()?;
            let state = self.state();
            if namespace.is_none() && state.forbid_all
                || namespace.is_some_and(|n| state.forbidden.contains(n))
            {
                return Err(ApiError::Forbidden("services is forbidden".into()));
            }
            Ok(state
                .services
                .iter()
                .filter(|s| namespace.is_none_or(|n| s.namespace == n))
                .cloned()
                .collect())
        })
    }

    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<String>, ApiError>> {
        Box::pin(async move {
            self.check_auth()?;
            let state = self.state();
            if state.forbid_namespaces {
                return Err(ApiError::Forbidden("namespaces is forbidden".into()));
            }
            let mut namespaces: Vec<String> =
                state.services.iter().map(|s| s.namespace.clone()).collect();
            namespaces.dedup();
            Ok(namespaces)
        })
    }

    fn connect<'a>(
        &'a self,
        _namespace: &'a str,
        pod: &'a str,
        _port: u16,
    ) -> BoxFuture<'a, Result<PodStream, ApiError>> {
        Box::pin(async move {
            let port = {
                let mut state = self.state();
                state.connects += 1;
                if state.gone.contains(pod) {
                    return Err(ApiError::NotFound(format!("pods \"{pod}\" not found")));
                }
                state.pods[pod]
            };
            let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            Ok(Box::new(stream) as PodStream)
        })
    }
}

struct FakeConnector(FakeCluster);

impl Connector for FakeConnector {
    fn connect<'a>(
        &'a self,
        _project: &'a KubeProject,
    ) -> BoxFuture<'a, Result<Arc<dyn Cluster>, ApiError>> {
        Box::pin(async move { Ok(Arc::new(self.0.clone()) as Arc<dyn Cluster>) })
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    paths: Paths,
    handle: Handle,
    kube: Kube,
    cluster: FakeCluster,
}

impl Setup {
    fn new(project: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(&dir.path().canonicalize().unwrap());
        std::fs::create_dir_all(paths.projects_dir()).unwrap();
        std::fs::write(paths.project_file("prod"), project).unwrap();
        let world = discovery::load_world(&paths.config_dir);
        assert!(world.problems.is_empty(), "{:?}", world.problems);
        let handle = supervisor::start(
            world,
            paths.clone(),
            Spawner::new(env!("CARGO_BIN_EXE_carte")),
            Options::new(&paths),
        );
        let cluster = FakeCluster::default();
        let kube = Kube::new(
            handle.clone(),
            Arc::new(FakeConnector(cluster.clone())),
            Duration::from_millis(100),
        );
        Setup {
            _dir: dir,
            paths: paths.clone(),
            handle,
            kube,
            cluster,
        }
    }

    async fn events(&self) -> Vec<Event> {
        self.handle.events(None, 100).await.unwrap().0
    }
}

fn target(service: &str, namespace: Option<&str>) -> KubeTarget {
    KubeTarget {
        project: "prod".into(),
        service: service.into(),
        namespace: namespace.map(Into::into),
        via_alias: false,
    }
}

/// The local port of a route, or the error page's status and text.
fn upstream(route: Route) -> Result<u16, (StatusCode, String)> {
    match route {
        Route::Upstream { port, .. } => Ok(port),
        Route::Error(page) => Err((page.status, format!("{}\n{}", page.title, page.detail))),
        Route::File { .. } => panic!("a kube route never serves files"),
    }
}

async fn get(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: grafana.prod.localhost\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default()
}

const PROJECT: &str = "kube = { context = \"prod\" }\nidle_timeout = \"1s\"\n";

#[tokio::test]
async fn forwards_a_short_hostname_to_the_one_namespace_with_the_service() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let port = upstream(setup.kube.route(target("grafana", None)).await).unwrap();
    assert_eq!(get(port).await, "grafana-a");
    assert_eq!(get(port).await, "grafana-a");
    let again = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    assert_eq!(again, port, "one forward per Service");
    assert_eq!(setup.cluster.state().connects, 2, "a stream per connection");
    let events = setup.events().await;
    assert!(
        events.iter().any(|e| e.kind == EventKind::Ready
            && e.message == "pod grafana-a port 3000"
            && e.subject_label() == "grafana@monitoring"),
        "{events:?}"
    );
}

#[tokio::test]
async fn an_ambiguous_short_hostname_lists_the_namespaces() {
    let setup = Setup::new(PROJECT);
    setup.cluster.add_service("blue", "web", &["web-a"]).await;
    setup.cluster.add_service("green", "web", &["web-b"]).await;
    let (status, text) = upstream(setup.kube.route(target("web", None)).await).unwrap_err();
    assert_eq!(status, StatusCode::MULTIPLE_CHOICES);
    assert!(text.contains("https://web.blue.prod.localhost"), "{text}");
    assert!(text.contains("https://web.green.prod.localhost"), "{text}");
    let port = upstream(setup.kube.route(target("web", Some("green"))).await).unwrap();
    assert_eq!(get(port).await, "web-b");
}

#[tokio::test]
async fn a_replaced_pod_is_followed() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a", "grafana-b"])
        .await;
    let port = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    assert_eq!(get(port).await, "grafana-a");
    {
        let mut state = setup.cluster.state();
        state.gone.insert("grafana-a".into());
        let slices = state
            .slices
            .get_mut(&("monitoring".into(), "grafana".into()))
            .unwrap();
        slices[0].endpoints.remove(0);
    }
    assert_eq!(get(port).await, "grafana-b");
    let events = setup.events().await;
    assert!(
        events.iter().any(|e| e.kind == EventKind::Restarted
            && e.message == "pod grafana-a is gone; now pod grafana-b"),
        "{events:?}"
    );
}

#[tokio::test]
async fn errors_become_pages_and_events() {
    let setup = Setup::new(PROJECT);
    let (status, text) = upstream(setup.kube.route(target("nope", None)).await).unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        text.contains("No namespace of prod has a Service named nope"),
        "{text}"
    );

    setup.cluster.state().auth_fails = true;
    let (status, text) = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap_err();
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(text.contains("aws sso login"), "{text}");
    assert!(text.contains("SSO session has expired"), "{text}");
    let events = setup.events().await;
    assert!(
        events.iter().any(|e| e.kind == EventKind::Failed
            && e.subject_label() == "prod"
            && e.message == "login failed: Run `aws sso login`, then reload."),
        "{events:?}"
    );
}

#[tokio::test]
async fn disabled_services_are_refused() {
    let setup = Setup::new(&format!("{PROJECT}[apps.grafana]\npolicy = \"disabled\"\n"));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let (status, _) = upstream(setup.kube.route(target("grafana", None)).await).unwrap_err();
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn idle_forwards_stop() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let route = setup
        .kube
        .route(target("grafana", Some("monitoring")))
        .await;
    let port = upstream(route).unwrap();
    assert_eq!(get(port).await, "grafana-a");
    let mut stopped = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if setup
            .events()
            .await
            .iter()
            .any(|e| e.kind == EventKind::Stopped && e.message == "idle for 1s")
        {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "{:?}", setup.events().await);
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    let port = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    assert_eq!(
        get(port).await,
        "grafana-a",
        "the next request opens it again"
    );
}
#[tokio::test]
async fn a_connection_left_open_between_requests_doesnt_keep_a_forward() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let port = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    let pooled = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut stopped = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if setup
            .events()
            .await
            .iter()
            .any(|e| e.kind == EventKind::Stopped)
        {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "{:?}", setup.events().await);
    drop(pooled);
}

#[tokio::test]
async fn a_changed_project_restarts_its_forwards() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let first = setup
        .kube
        .route(target("grafana", Some("monitoring")))
        .await;
    let first = upstream(first).unwrap();
    std::fs::write(
        setup.paths.project_file("prod"),
        "kube = { context = \"other\" }\nidle_timeout = \"1s\"\n",
    )
    .unwrap();
    setup
        .handle
        .reload(discovery::load_world(&setup.paths.config_dir));
    let mut second = first;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        second = upstream(
            setup
                .kube
                .route(target("grafana", Some("monitoring")))
                .await,
        )
        .unwrap();
        if second != first {
            break;
        }
    }
    assert_ne!(second, first, "a new forward for the new context");
    assert!(
        setup
            .events()
            .await
            .iter()
            .any(|e| e.kind == EventKind::Stopped && e.message == "the project changed")
    );
}

fn address(service: Option<&str>, namespace: Option<&str>) -> carte_core::kube::KubeAddress {
    carte_core::kube::KubeAddress {
        project: "prod".into(),
        service: service.map(Into::into),
        namespace: namespace.map(Into::into),
    }
}

#[tokio::test]
async fn reports_status_and_controls_forwards() {
    use carte_api::{Action, Hold, InstanceState};

    let setup = Setup::new(&format!("{PROJECT}[apps.vlogs]\nnamespace = \"logging\"\n"));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    setup
        .cluster
        .add_service("logging", "vlogs", &["vlogs-a"])
        .await;
    let world = setup.handle.world();

    let status = setup.kube.status(&world);
    let prod = &status[0];
    assert_eq!(prod.name, "prod");
    assert_eq!(prod.kube.as_ref().unwrap().context, "prod");
    let names: Vec<_> = prod.stacks.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["logging"],
        "configured Services with a namespace show"
    );
    assert_eq!(prod.stacks[0].instances[0].state, InstanceState::Stopped);

    let response = setup
        .kube
        .control(address(Some("grafana"), None), Action::Up)
        .await
        .unwrap();
    assert_eq!(response.affected, vec!["grafana.prod@monitoring"]);
    let route = setup.kube.route(target("grafana", None)).await;
    let status = setup.kube.status(&world);
    let grafana = status[0]
        .stacks
        .iter()
        .find(|s| s.name == "monitoring")
        .unwrap()
        .instances[0]
        .clone();
    assert_eq!(grafana.state, InstanceState::Ready);
    assert_eq!(grafana.url, "https://grafana.monitoring.prod.localhost");
    assert_eq!(grafana.holds, vec![Hold::Requests { count: 1 }]);
    let kube = grafana.kube.unwrap();
    assert_eq!(kube.pod.as_deref(), Some("grafana-a"));
    assert_eq!(kube.port, Some(3000));
    drop(route);

    let response = setup
        .kube
        .control(address(None, Some("monitoring")), Action::Restart)
        .await
        .unwrap();
    assert_eq!(response.affected, vec!["grafana.prod@monitoring"]);
    let response = setup
        .kube
        .control(address(None, None), Action::Down)
        .await
        .unwrap();
    assert_eq!(response.affected, vec!["grafana.prod@monitoring"]);
    let status = setup.kube.status(&world);
    assert!(
        status[0]
            .stacks
            .iter()
            .flat_map(|s| &s.instances)
            .all(|i| i.state == InstanceState::Stopped)
    );

    let response = setup
        .kube
        .control(address(Some("nope"), Some("monitoring")), Action::Up)
        .await
        .unwrap();
    assert_eq!(response.failures[0].instance, "nope.prod@monitoring");
    assert_eq!(response.failures[0].title, "No such Service");
    assert!(
        setup
            .kube
            .control(address(Some("grafana"), None), Action::Pin { save: false })
            .await
            .is_err()
    );
    assert!(
        setup
            .kube
            .control(address(None, None), Action::Up)
            .await
            .is_err(),
        "up needs a Service"
    );
}

#[tokio::test]
async fn stop_all_includes_forwards() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let route = setup.kube.route(target("grafana", None)).await;
    assert!(setup.kube.stop_all(true).is_empty(), "a request holds it");
    drop(route);
    assert_eq!(setup.kube.stop_all(true), vec!["grafana.prod@monitoring"]);
}
#[tokio::test]
async fn failed_lookups_are_forgotten_after_the_idle_timeout() {
    let setup = Setup::new(PROJECT);
    let (status, _) =
        upstream(setup.kube.route(target("nope", Some("monitoring"))).await).unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
    let listed = |setup: &Setup| {
        setup.kube.status(&setup.handle.world())[0]
            .stacks
            .iter()
            .flat_map(|s| &s.instances)
            .any(|i| i.app == "nope")
    };
    assert!(listed(&setup));
    let mut gone = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !listed(&setup) {
            gone = true;
            break;
        }
    }
    assert!(gone);
}

#[tokio::test]
async fn namespaces_outside_the_project_are_refused() {
    let setup = Setup::new(&format!("{PROJECT}namespaces = [\"logging\"]\n"));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let response = setup
        .kube
        .control(
            address(Some("grafana"), Some("monitoring")),
            carte_api::Action::Up,
        )
        .await
        .unwrap();
    assert_eq!(
        response.failures[0].title,
        "Not one of the project's namespaces"
    );
}

#[tokio::test]
async fn lists_services_across_namespaces_as_far_as_allowed() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    setup
        .cluster
        .add_service("logging", "vlogs", &["vlogs-a"])
        .await;
    let names = |services: Vec<carte_api::KubeServiceEntry>| -> Vec<String> {
        services
            .into_iter()
            .map(|s| format!("{}/{}", s.namespace, s.name))
            .collect()
    };
    let services = setup.kube.services("prod").await.unwrap();
    assert_eq!(
        services[0].url, "https://vlogs.logging.prod.localhost",
        "by namespace, then name"
    );
    assert_eq!(names(services), vec!["logging/vlogs", "monitoring/grafana"]);

    setup.cluster.state().forbid_all = true;
    let services = setup.kube.services("prod").await.unwrap();
    assert_eq!(names(services), vec!["logging/vlogs", "monitoring/grafana"]);

    setup.cluster.state().forbid_namespaces = true;
    let err = setup.kube.services("prod").await.unwrap_err();
    assert!(err.contains("`namespaces`"), "{err}");
}

#[tokio::test]
async fn a_listing_where_every_namespace_is_forbidden_fails() {
    let setup = Setup::new(&format!(
        "{PROJECT}namespaces = [\"logging\", \"monitoring\"]\n"
    ));
    setup
        .cluster
        .add_service("logging", "vlogs", &["vlogs-a"])
        .await;
    setup.cluster.state().forbidden.insert("monitoring".into());
    assert_eq!(setup.kube.services("prod").await.unwrap().len(), 1);
    setup.cluster.state().forbidden.insert("logging".into());
    let err = setup.kube.services("prod").await.unwrap_err();
    assert!(err.contains("forbidden"), "{err}");
}

#[tokio::test]
async fn lists_only_the_configured_namespaces() {
    let setup = Setup::new(&format!("{PROJECT}namespaces = [\"logging\"]\n"));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    setup
        .cluster
        .add_service("logging", "vlogs", &["vlogs-a"])
        .await;
    let services = setup.kube.services("prod").await.unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].name, "vlogs");
}
