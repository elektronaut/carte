//! Kube projects against a fake cluster whose pods are local HTTP servers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use carte::kube::{
    ApiError, Cluster, Connector, ContainerInfo, EndpointInfo, Kube, Kubeconfig, LogLines,
    LogOptions, PodError, PodStream, PortInfo, Readable, ServiceInfo, SliceInfo,
    read_each_namespace,
};
use carte::paths::Paths;
use carte::process::Spawner;
use carte::proxy::{BoxFuture, Dialer, Route, UpstreamError};
use carte::supervisor::{self, Handle, Options};
use carte_api::{Event, EventKind};
use carte_core::discovery;
use carte_core::kube::{KubeProject, KubeTarget};
use futures_util::StreamExt;
use http::StatusCode;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
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
    /// Namespaces whose Services can't be listed or read.
    forbidden: HashSet<String>,
    containers: HashMap<String, Vec<ContainerInfo>>,
    /// Each pod's log lines.
    logs: HashMap<String, Vec<String>>,
    /// Lines a pod writes right after its logs were read without following.
    written_after_read: HashMap<String, Vec<String>>,
    /// How many following streams end once they caught up, rather than when their
    /// pod goes.
    follows_to_end: usize,
    log_requests: Vec<LogOptions>,
    /// The provider env the connector hands out.
    env: BTreeMap<String, String>,
    /// How many clients the connector built.
    clients: usize,
    /// Which client opened each port-forward stream.
    connected_by: Vec<usize>,
    /// Calls that never answer.
    hangs: HashSet<&'static str>,
    /// Held by each call that doesn't answer, for as long as it waits.
    hung: Arc<()>,
    /// What the pod side of every port-forward stream reports.
    pod_error: Option<String>,
}

/// A port-forward stream whose pod side failed.
struct FailedStream(String);

impl FailedStream {
    fn error(&self) -> std::io::Error {
        std::io::Error::other(PodError(self.0.clone()))
    }
}

impl AsyncRead for FailedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(self.error()))
    }
}

impl AsyncWrite for FailedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Err(self.error()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Clone, Default)]
struct FakeCluster {
    state: Arc<Mutex<FakeState>>,
    /// Which client of the connector this is.
    client: usize,
}

impl FakeCluster {
    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap()
    }

    /// Waits forever when `call` hangs.
    async fn hang(&self, call: &str) {
        let held = {
            let state = self.state();
            state.hangs.contains(call).then(|| state.hung.clone())
        };
        if let Some(_held) = held {
            std::future::pending::<()>().await;
        }
    }

    /// How many calls wait without answering.
    fn hanging(&self) -> usize {
        Arc::strong_count(&self.state().hung) - 1
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

    /// A Service with one `http` port whose pods serve their own name, or echo what
    /// they get when it isn't an HTTP request.
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
                        let Ok(n) = stream.read(&mut buf).await else {
                            return;
                        };
                        if !buf[..n].starts_with(b"GET ") {
                            let mut n = n;
                            while n > 0 && stream.write_all(&buf[..n]).await.is_ok() {
                                n = stream.read(&mut buf).await.unwrap_or(0);
                            }
                            return;
                        }
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
            let state = self.state();
            if state.forbidden.contains(namespace) {
                return Err(ApiError::Forbidden("services is forbidden".into()));
            }
            Ok(state
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
    ) -> BoxFuture<'a, Result<Readable<ServiceInfo>, ApiError>> {
        Box::pin(async move {
            self.check_auth()?;
            if namespaces.is_empty() {
                return Ok(Readable {
                    found: self
                        .state()
                        .services
                        .iter()
                        .filter(|s| s.name == name)
                        .cloned()
                        .collect(),
                    refused: Vec::new(),
                });
            }
            read_each_namespace(namespaces, |namespace| {
                Box::pin(
                    async move { Ok(self.service(namespace, name).await?.into_iter().collect()) },
                )
            })
            .await
        })
    }

    fn endpoint_slices<'a>(
        &'a self,
        namespace: &'a str,
        service: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SliceInfo>, ApiError>> {
        Box::pin(async move {
            self.hang("endpoint_slices").await;
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
            self.hang("services").await;
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

    fn containers<'a>(
        &'a self,
        _namespace: &'a str,
        pod: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ContainerInfo>, ApiError>> {
        Box::pin(async move {
            self.hang("containers").await;
            Ok(self
                .state()
                .containers
                .get(pod)
                .cloned()
                .unwrap_or_default())
        })
    }

    fn logs<'a>(
        &'a self,
        _namespace: &'a str,
        pod: &'a str,
        options: LogOptions,
    ) -> BoxFuture<'a, Result<LogLines, ApiError>> {
        Box::pin(async move {
            self.hang("logs").await;
            let mut state = self.state();
            state.log_requests.push(options.clone());
            let lines = state.logs.get(pod).cloned().unwrap_or_default();
            let since = options.since_time.map_or(0, |secs| {
                let second = k8s_openapi::jiff::Timestamp::from_second(secs)
                    .unwrap()
                    .to_string();
                lines
                    .iter()
                    .take_while(|line| line.get(..19) < second.get(..19))
                    .count()
            });
            let start = options.tail.map_or(since, |tail| {
                lines.len().saturating_sub(tail as usize).max(since)
            });
            if !options.follow {
                if let Some(written) = state.written_after_read.remove(pod) {
                    state
                        .logs
                        .entry(pod.to_string())
                        .or_default()
                        .extend(written);
                }
                let fake = self.clone();
                let stalled = futures_util::stream::once(async move {
                    fake.hang("reading logs").await;
                    None
                })
                .filter_map(|line: Option<String>| async move { line });
                return Ok(futures_util::stream::iter(lines.into_iter().skip(start))
                    .chain(stalled)
                    .boxed());
            }
            drop(state);
            let fake = self.clone();
            let pod = pod.to_string();
            let follow = futures_util::stream::unfold(start, move |next| {
                let fake = fake.clone();
                let pod = pod.clone();
                async move {
                    loop {
                        {
                            let mut state = fake.state();
                            if let Some(line) = state.logs.get(&pod).and_then(|l| l.get(next)) {
                                return Some((line.clone(), next + 1));
                            }
                            if state.gone.contains(&pod) {
                                return None;
                            }
                            if state.follows_to_end > 0 {
                                state.follows_to_end -= 1;
                                return None;
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            });
            Ok(follow.boxed())
        })
    }

    fn connect<'a>(
        &'a self,
        _namespace: &'a str,
        pod: &'a str,
        _port: u16,
    ) -> BoxFuture<'a, Result<PodStream, ApiError>> {
        Box::pin(async move {
            self.hang("connect").await;
            self.hang(&format!("connect {pod}")).await;
            let port = {
                let mut state = self.state();
                state.connects += 1;
                state.connected_by.push(self.client);
                if state.gone.contains(pod) {
                    return Err(ApiError::NotFound(format!("pods \"{pod}\" not found")));
                }
                if let Some(message) = &state.pod_error {
                    return Ok(Box::new(FailedStream(message.clone())) as PodStream);
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
    fn env<'a>(&'a self, _project: &'a KubeProject) -> BoxFuture<'a, BTreeMap<String, String>> {
        Box::pin(async move { self.0.state().env.clone() })
    }

    fn connect<'a>(
        &'a self,
        _project: &'a KubeProject,
        _kubeconfig: Kubeconfig,
        _env: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Arc<dyn Cluster>, ApiError>> {
        Box::pin(async move {
            self.0.hang("login").await;
            let client = {
                let mut state = self.0.state();
                state.clients += 1;
                state.clients
            };
            let cluster = FakeCluster {
                state: self.0.state.clone(),
                client,
            };
            Ok(Arc::new(cluster) as Arc<dyn Cluster>)
        })
    }
}

/// A kubeconfig with the contexts `prod` and `other`.
fn kubeconfig(current: &str, server: &str, token: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Config\ncurrent-context: {current}\nclusters:\n- name: prod\n  cluster:\n    server: {server}\n- name: other\n  cluster:\n    server: https://other.example:6443\ncontexts:\n- name: prod\n  context:\n    cluster: prod\n    user: prod\n- name: other\n  context:\n    cluster: other\n    user: prod\nusers:\n- name: prod\n  user:\n    token: {token}\n"
    )
}

const SERVER: &str = "https://prod.example:6443";

/// Writes the file, and makes sure its modification time moves on.
fn rewrite(path: &Path, text: &str) {
    let before = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    std::fs::write(path, text).unwrap();
    if let Some(before) = before {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(before + Duration::from_secs(1)).unwrap();
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    /// The kubeconfig in the provider env's home.
    kubeconfig: PathBuf,
    paths: Paths,
    handle: Handle,
    kube: Kube,
    cluster: FakeCluster,
}

impl Setup {
    fn new(project: &str) -> Self {
        Self::with_timeout(project, carte::kube::LOOKUP_TIMEOUT)
    }

    /// With `timeout` for each call to the cluster.
    fn with_timeout(project: &str, timeout: Duration) -> Self {
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
        let home = dir.path().join("home");
        let kubeconfig = home.join(".kube/config");
        std::fs::create_dir_all(kubeconfig.parent().unwrap()).unwrap();
        std::fs::write(&kubeconfig, self::kubeconfig("prod", SERVER, "one")).unwrap();
        let cluster = FakeCluster::default();
        cluster
            .state()
            .env
            .insert("HOME".into(), home.to_string_lossy().into_owned());
        let kube = Kube::new(
            handle.clone(),
            Arc::new(FakeConnector(cluster.clone())),
            Duration::from_millis(100),
            timeout,
        );
        Setup {
            _dir: dir,
            kubeconfig,
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

/// The dialer of a route, or the error page's status and text.
fn upstream(route: Route) -> Result<Dialer, (StatusCode, String)> {
    match route {
        Route::Dialed { dialer, .. } => Ok(dialer),
        Route::Error(page) => Err((page.status, format!("{}\n{}", page.title, page.detail))),
        Route::Upstream { .. } => panic!("a kube route has no port"),
        Route::File { .. } => panic!("a kube route never serves files"),
    }
}

async fn get(dialer: &Dialer) -> String {
    let mut stream = dialer.dial().await.unwrap();
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
    let dialer = upstream(setup.kube.route(target("grafana", None)).await).unwrap();
    assert_eq!(get(&dialer).await, "grafana-a");
    assert_eq!(get(&dialer).await, "grafana-a");
    let again = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    assert_eq!(again, dialer, "one forward per Service");
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
    let dialer = upstream(setup.kube.route(target("web", Some("green"))).await).unwrap();
    assert_eq!(get(&dialer).await, "web-b");
}

#[tokio::test]
async fn a_replaced_pod_is_followed() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a", "grafana-b"])
        .await;
    let dialer = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    assert_eq!(get(&dialer).await, "grafana-a");
    {
        let mut state = setup.cluster.state();
        state.gone.insert("grafana-a".into());
        let slices = state
            .slices
            .get_mut(&("monitoring".into(), "grafana".into()))
            .unwrap();
        slices[0].endpoints.remove(0);
    }
    assert_eq!(get(&dialer).await, "grafana-b");
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
async fn the_pods_port_forward_error_is_reported() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let message = "error forwarding port 3000 to pod grafana-a, uid : failed to connect to localhost:3000: connection refused";
    let failures = |setup: &Setup| {
        let setup = setup.handle.clone();
        async move {
            setup
                .events(None, 100)
                .await
                .unwrap()
                .0
                .iter()
                .filter(|e| e.kind == EventKind::Failed && e.message == message)
                .count()
        }
    };
    let last_error = |setup: &Setup| {
        let status = setup.kube.status(&setup.handle.world());
        status[0].stacks[0].instances[0].last_error.clone()
    };
    setup.cluster.state().pod_error = Some(message.into());
    let dialer = upstream(setup.kube.route(target("grafana", None)).await).unwrap();
    assert_eq!(fails(&dialer).await, "", "the request fails");
    assert_eq!(last_error(&setup).as_deref(), Some(message));
    assert_eq!(failures(&setup).await, 1);
    let page = dialer.failed(&UpstreamError::Request(
        "connection closed before message completed".into(),
    ));
    assert_eq!(page.title, "grafana didn't answer");
    assert_eq!(page.detail, message);

    setup.cluster.state().pod_error = None;
    let mut open = dialer.dial().await.unwrap();
    let mut idle = dialer.dial().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        last_error(&setup).as_deref(),
        Some(message),
        "a connection the pod sent nothing on yet doesn't clear it"
    );
    idle.shutdown().await.unwrap();
    drop(idle);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        last_error(&setup).as_deref(),
        Some(message),
        "nor does one that ends without an answer"
    );
    assert_eq!(echo(&mut open, "ping").await, "ping");
    assert_eq!(
        last_error(&setup),
        None,
        "the pod's first bytes clear it, with the connection still open"
    );

    setup.cluster.state().pod_error = Some(message.into());
    assert_eq!(fails(&dialer).await, "");
    assert_eq!(failures(&setup).await, 2);
    setup.cluster.state().pod_error = None;
    let quiet = dialer.dial().await.unwrap();
    drop(quiet);
    setup.cluster.state().pod_error = Some(message.into());
    assert_eq!(fails(&dialer).await, "");
    assert_eq!(
        failures(&setup).await,
        2,
        "a connection without an answer between failures adds no event"
    );
    assert_eq!(
        echo(&mut open, "pong").await,
        "pong",
        "the open one goes on"
    );
}

/// Sends a request on a new connection and reads what comes back until it ends.
async fn fails(dialer: &Dialer) -> String {
    let mut stream = dialer.dial().await.unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    String::from_utf8_lossy(&response).into_owned()
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
    let dialer = upstream(route).unwrap();
    assert_eq!(get(&dialer).await, "grafana-a");
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
    assert!(dialer.dial().await.is_err());
    let dialer = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    assert_eq!(
        get(&dialer).await,
        "grafana-a",
        "the next request opens it again"
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
async fn a_short_hostname_skips_namespaces_that_refuse() {
    let setup = Setup::new(&format!(
        "{PROJECT}namespaces = [\"logging\", \"monitoring\"]\n"
    ));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    setup.cluster.state().forbidden.insert("logging".into());
    let dialer = upstream(setup.kube.route(target("grafana", None)).await).unwrap();
    assert_eq!(get(&dialer).await, "grafana-a");
}

#[tokio::test]
async fn no_such_service_names_the_namespaces_it_couldnt_read() {
    let setup = Setup::new(&format!(
        "{PROJECT}namespaces = [\"logging\", \"monitoring\"]\n"
    ));
    setup
        .cluster
        .add_service("logging", "grafana", &["grafana-a"])
        .await;
    setup.cluster.state().forbidden.insert("logging".into());
    let (status, text) = upstream(setup.kube.route(target("grafana", None)).await).unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(text.contains("(couldn't read logging)"), "{text}");
}

#[tokio::test]
async fn a_short_hostname_where_every_namespace_refuses_names_them() {
    let setup = Setup::new(&format!(
        "{PROJECT}namespaces = [\"logging\", \"monitoring\"]\n"
    ));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    setup.cluster.state().forbidden.insert("logging".into());
    setup.cluster.state().forbidden.insert("monitoring".into());
    let (status, text) = upstream(setup.kube.route(target("grafana", None)).await).unwrap_err();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(text.contains("logging, monitoring"), "{text}");
    assert!(!text.contains("`namespaces`"), "{text}");
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

fn logs_request(follow: bool) -> carte_api::LogsRequest {
    carte_api::LogsRequest {
        target: carte_api::Address::default(),
        follow,
        tail: Some(50),
        ..Default::default()
    }
}

fn container(name: &str, ports: &[u16]) -> ContainerInfo {
    ContainerInfo {
        name: name.into(),
        ports: ports.to_vec(),
    }
}

#[tokio::test]
async fn shows_the_logs_of_the_container_serving_the_port() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    {
        let mut state = setup.cluster.state();
        state.containers.insert(
            "grafana-a".into(),
            vec![
                container("istio-proxy", &[15090]),
                container("grafana", &[3000]),
            ],
        );
        state.logs.insert(
            "grafana-a".into(),
            vec![
                "2026-10-20T08:15:29Z GET /api/health 200".into(),
                "2026-10-20T08:15:30.5Z GET /api/search 200".into(),
                "2026-10-20T08:15:30.7Z panic: no database".into(),
                "2026-10-20T08:15:31Z GET /api/search 500".into(),
            ],
        );
    }
    let mut request = logs_request(false);
    request.search = Some("GET".into());
    request.tail = Some(2);
    let mut lines = setup
        .kube
        .logs(address(Some("grafana"), None), &request)
        .await
        .unwrap();
    let mut got = Vec::new();
    while let Some(line) = lines.recv().await {
        got.push(line);
    }
    let texts: Vec<&str> = got.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["GET /api/search 200", "GET /api/search 500"],
        "the last 2 matches, not the matches in the last 2 lines"
    );
    assert_eq!(got[0].process, "grafana");
    assert_eq!(got[0].stack, "monitoring");
    assert_eq!(got[0].at_ms, 1_792_484_130_500);
    let requests = setup.cluster.state().log_requests.clone();
    assert_eq!(requests[0].container.as_deref(), Some("grafana"));
    assert_eq!(
        requests[0].tail,
        Some(10_000),
        "searching reads further back"
    );

    assert!(
        setup
            .events()
            .await
            .iter()
            .all(|e| e.kind != EventKind::Ready),
        "reading logs opens no forward"
    );
}

#[tokio::test]
async fn following_logs_continue_on_the_next_pod() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a", "grafana-b"])
        .await;
    {
        let mut state = setup.cluster.state();
        state.logs.insert("grafana-a".into(), vec!["from a".into()]);
        state.logs.insert("grafana-b".into(), vec!["from b".into()]);
    }
    let mut lines = setup
        .kube
        .logs(
            address(Some("grafana"), Some("monitoring")),
            &logs_request(true),
        )
        .await
        .unwrap();
    assert_eq!(lines.recv().await.unwrap().text, "from a");
    {
        let mut state = setup.cluster.state();
        state.gone.insert("grafana-a".into());
        let slices = state
            .slices
            .get_mut(&("monitoring".into(), "grafana".into()))
            .unwrap();
        slices[0].endpoints.remove(0);
    }
    let next = tokio::time::timeout(Duration::from_secs(5), lines.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.text, "from b", "and not from a again");
    let follows: Vec<_> = setup
        .cluster
        .state()
        .log_requests
        .iter()
        .filter(|o| o.follow)
        .cloned()
        .collect();
    assert_eq!(follows[0].tail, Some(0), "following adds only new lines");
    assert!(follows[1].since_secs.is_some());
}

#[tokio::test]
async fn followed_logs_arrive_once() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    {
        let mut state = setup.cluster.state();
        state.logs.insert(
            "grafana-a".into(),
            vec![
                "2026-10-20T08:15:29.1Z one".into(),
                "2026-10-20T08:15:30.2Z two".into(),
            ],
        );
        state.written_after_read.insert(
            "grafana-a".into(),
            vec!["2026-10-20T08:15:30.25Z three".into()],
        );
        state.follows_to_end = 1;
    }
    let mut lines = setup
        .kube
        .logs(
            address(Some("grafana"), Some("monitoring")),
            &logs_request(true),
        )
        .await
        .unwrap();
    let mut got = Vec::new();
    for _ in 0..3 {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.recv())
            .await
            .unwrap()
            .unwrap();
        got.push(line.text);
    }
    setup
        .cluster
        .state()
        .logs
        .get_mut("grafana-a")
        .unwrap()
        .push("2026-10-20T08:15:30.3Z four".into());
    let line = tokio::time::timeout(Duration::from_secs(5), lines.recv())
        .await
        .unwrap()
        .unwrap();
    got.push(line.text);
    assert!(
        tokio::time::timeout(Duration::from_millis(1500), lines.recv())
            .await
            .is_err(),
        "no line comes again"
    );
    assert_eq!(got, vec!["one", "two", "three", "four"]);
    let follows: Vec<_> = setup
        .cluster
        .state()
        .log_requests
        .iter()
        .filter(|o| o.follow)
        .cloned()
        .collect();
    assert_eq!(follows.len(), 2, "the follow reopened once");
    for follow in follows {
        assert_eq!(
            follow.since_time,
            Some(1_792_484_130),
            "from the last line's second"
        );
    }
}

#[tokio::test]
async fn followed_lines_out_of_order_arrive() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    setup.cluster.state().logs.insert(
        "grafana-a".into(),
        vec!["2026-10-20T08:15:30.2Z one".into()],
    );
    let mut lines = setup
        .kube
        .logs(
            address(Some("grafana"), Some("monitoring")),
            &logs_request(true),
        )
        .await
        .unwrap();
    assert_eq!(lines.recv().await.unwrap().text, "one");
    setup
        .cluster
        .state()
        .logs
        .get_mut("grafana-a")
        .unwrap()
        .extend([
            "2026-10-20T08:15:30.5Z two on stderr".into(),
            "2026-10-20T08:15:30.4Z three on stdout".into(),
        ]);
    let mut got = Vec::new();
    for _ in 0..2 {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.recv())
            .await
            .unwrap()
            .unwrap();
        got.push(line.text);
    }
    assert_eq!(got, vec!["two on stderr", "three on stdout"]);
}

fn now_ms() -> i128 {
    k8s_openapi::jiff::Timestamp::now().as_millisecond() as i128
}

/// A log line stamped `at_ms`, in milliseconds since the Unix epoch.
fn stamped(at_ms: i128, text: &str) -> String {
    let at = k8s_openapi::jiff::Timestamp::from_nanosecond(at_ms * 1_000_000).unwrap();
    format!("{at} {text}")
}

/// Follows grafana-a's logs until its first line, replaces the pod with grafana-b
/// and answers with the first line from it and the follow requests.
async fn switch_pods(
    setup: &Setup,
    a: Vec<String>,
    b: Vec<String>,
) -> (String, Vec<carte::kube::LogOptions>) {
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a", "grafana-b"])
        .await;
    {
        let mut state = setup.cluster.state();
        state.logs.insert("grafana-a".into(), a);
        state.logs.insert("grafana-b".into(), b);
    }
    let mut lines = setup
        .kube
        .logs(
            address(Some("grafana"), Some("monitoring")),
            &logs_request(true),
        )
        .await
        .unwrap();
    lines.recv().await.unwrap();
    {
        let mut state = setup.cluster.state();
        state.gone.insert("grafana-a".into());
        let slices = state
            .slices
            .get_mut(&("monitoring".into(), "grafana".into()))
            .unwrap();
        slices[0].endpoints.remove(0);
    }
    let next = tokio::time::timeout(Duration::from_secs(5), lines.recv())
        .await
        .unwrap()
        .unwrap();
    let follows = setup
        .cluster
        .state()
        .log_requests
        .iter()
        .filter(|o| o.follow)
        .cloned()
        .collect();
    (next.text, follows)
}

#[tokio::test]
async fn the_next_pod_is_followed_from_the_last_second_without_skipping() {
    let setup = Setup::new(PROJECT);
    let second = now_ms() / 1000 - 1;
    let (next, follows) = switch_pods(
        &setup,
        vec![stamped(second * 1000 + 500, "from a")],
        vec![
            stamped(second * 1000 - 60_000, "long before a"),
            stamped(second * 1000 + 200, "from b, its node's clock behind"),
        ],
    )
    .await;
    assert_eq!(next, "from b, its node's clock behind");
    assert_eq!(
        follows[1].since_time,
        Some(second as i64),
        "from a's last second"
    );
}

#[tokio::test]
async fn the_next_pod_is_followed_from_shortly_before_the_switch() {
    let setup = Setup::new(PROJECT);
    let (next, follows) = switch_pods(
        &setup,
        vec![stamped(now_ms() - 3 * 3_600_000, "from a, hours ago")],
        vec![
            stamped(now_ms() - 3 * 3_600_000 + 60_000, "the backlog of b"),
            stamped(now_ms(), "from b now"),
        ],
    )
    .await;
    assert_eq!(next, "from b now", "and not hours of backlog");
    let since = follows[1].since_time.unwrap();
    let now = k8s_openapi::jiff::Timestamp::now().as_second();
    assert!((now - 40..now).contains(&since), "{since} vs {now}");
}

#[tokio::test]
async fn following_stops_when_the_reader_goes() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let lines = setup
        .kube
        .logs(
            address(Some("grafana"), Some("monitoring")),
            &logs_request(true),
        )
        .await
        .unwrap();
    drop(lines);
    setup.cluster.state().gone.insert("grafana-a".into());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let requests = setup.cluster.state().log_requests.len();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        setup.cluster.state().log_requests.len(),
        requests,
        "no more requests once nobody reads"
    );
}

#[tokio::test]
async fn logs_need_a_service() {
    let setup = Setup::new(PROJECT);
    let err = setup
        .kube
        .logs(address(None, None), &logs_request(false))
        .await
        .unwrap_err();
    assert!(err.contains("name a Service"), "{err}");
}

#[tokio::test]
async fn a_connection_left_open_between_requests_doesnt_keep_a_forward() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let dialer = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    let mut pooled = dialer.dial().await.unwrap();
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
    let mut buf = [0u8; 8];
    assert_eq!(
        pooled.read(&mut buf).await.unwrap(),
        0,
        "stopping ends the connection"
    );
}

/// Sends `text` on the connection and reads it back.
async fn echo(stream: &mut carte::proxy::UpstreamStream, text: &str) -> String {
    stream.write_all(text.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; text.len()];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn a_route_reaches_its_forward_in_process() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let route = setup
        .kube
        .route(target("grafana", Some("monitoring")))
        .await;
    assert!(
        matches!(route, Route::Dialed { .. }),
        "a forward has no port to send requests to"
    );
    let dialer = upstream(route).unwrap();
    let mut open = dialer.dial().await.unwrap();
    assert_eq!(echo(&mut open, "ping").await, "ping");
    assert_eq!(setup.cluster.state().connected_by, vec![1]);
}

#[tokio::test]
async fn a_changed_project_keeps_its_forwards_and_connections() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let first = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    let mut open = first.dial().await.unwrap();
    assert_eq!(echo(&mut open, "one").await, "one");
    std::fs::write(
        setup.paths.project_file("prod"),
        "kube = { context = \"other\" }\nidle_timeout = \"1s\"\n",
    )
    .unwrap();
    setup
        .handle
        .reload(discovery::load_world(&setup.paths.config_dir));
    for _ in 0..50 {
        if setup.handle.world().kube_projects[0].context == "other" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let second = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    assert_eq!(second, first, "the same forward");
    assert_eq!(
        setup.cluster.state().clients,
        2,
        "a client for the new context"
    );
    assert_eq!(get(&second).await, "grafana-a");
    assert_eq!(setup.cluster.state().connected_by, vec![1, 2]);
    assert_eq!(echo(&mut open, "two").await, "two", "the open one goes on");
    assert!(
        setup
            .events()
            .await
            .iter()
            .all(|e| e.kind != EventKind::Stopped)
    );
}

#[tokio::test]
async fn another_current_context_keeps_the_client() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let route = || setup.kube.route(target("grafana", Some("monitoring")));
    let first = upstream(route().await).unwrap();
    let mut open = first.dial().await.unwrap();
    assert_eq!(echo(&mut open, "one").await, "one");
    setup
        .cluster
        .state()
        .env
        .insert("DIRENV_DIFF".into(), "changes on every fetch".into());
    rewrite(&setup.kubeconfig, &kubeconfig("other", SERVER, "one"));
    let second = upstream(route().await).unwrap();
    assert_eq!(second, first);
    assert_eq!(
        setup.cluster.state().clients,
        1,
        "nor does the env without a login plugin"
    );
    assert_eq!(echo(&mut open, "two").await, "two");
    assert_eq!(get(&second).await, "grafana-a");
    assert_eq!(setup.cluster.state().connected_by, vec![1, 1]);
}

#[tokio::test]
async fn a_changed_server_or_user_gives_new_connections_a_new_client() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let route = || setup.kube.route(target("grafana", Some("monitoring")));
    let dialer = upstream(route().await).unwrap();
    let mut open = dialer.dial().await.unwrap();
    assert_eq!(echo(&mut open, "one").await, "one");

    rewrite(
        &setup.kubeconfig,
        &kubeconfig("prod", "https://new.example:6443", "one"),
    );
    assert_eq!(upstream(route().await).unwrap(), dialer);
    assert_eq!(setup.cluster.state().clients, 2);
    assert_eq!(get(&dialer).await, "grafana-a");
    assert_eq!(echo(&mut open, "two").await, "two");

    rewrite(
        &setup.kubeconfig,
        &kubeconfig("prod", "https://new.example:6443", "two"),
    );
    assert_eq!(upstream(route().await).unwrap(), dialer);
    assert_eq!(setup.cluster.state().clients, 3);
    assert_eq!(get(&dialer).await, "grafana-a");
    assert_eq!(echo(&mut open, "three").await, "three");
    assert_eq!(setup.cluster.state().connected_by, vec![1, 2, 3]);
}

#[tokio::test]
async fn a_kubeconfig_caught_mid_rewrite_keeps_the_client() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let route = || setup.kube.route(target("grafana", Some("monitoring")));
    let dialer = upstream(route().await).unwrap();
    for partial in [
        "apiVersion: v1\nkind: Config\nclusters:\n- name: prod\n  cluster: [",
        "",
    ] {
        rewrite(&setup.kubeconfig, partial);
        assert_eq!(upstream(route().await).unwrap(), dialer);
        assert_eq!(get(&dialer).await, "grafana-a");
        assert_eq!(setup.cluster.state().clients, 1, "{partial:?}");
    }
    std::fs::remove_file(&setup.kubeconfig).unwrap();
    assert_eq!(upstream(route().await).unwrap(), dialer);
    assert_eq!(setup.cluster.state().clients, 1);
    let events = setup.events().await;
    assert!(
        events.iter().all(|e| e.kind != EventKind::Failed),
        "{events:?}"
    );

    std::fs::write(&setup.kubeconfig, kubeconfig("other", SERVER, "one")).unwrap();
    upstream(route().await).unwrap();
    assert_eq!(
        setup.cluster.state().clients,
        1,
        "the same context once the rewrite is done"
    );
    rewrite(&setup.kubeconfig, "garbage: [");
    rewrite(&setup.kubeconfig, &kubeconfig("prod", SERVER, "two"));
    upstream(route().await).unwrap();
    assert_eq!(setup.cluster.state().clients, 2);
}

#[tokio::test]
async fn kubeconfig_files_from_the_provider_env_are_watched() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let dir = setup.kubeconfig.parent().unwrap();
    let first = dir.join("first");
    let second = dir.join("second");
    std::fs::write(&first, "apiVersion: v1\nkind: Config\n").unwrap();
    std::fs::write(&second, kubeconfig("prod", SERVER, "two")).unwrap();
    let route = || setup.kube.route(target("grafana", Some("monitoring")));
    upstream(route().await).unwrap();
    let list = std::env::join_paths([&first, &second]).unwrap();
    setup
        .cluster
        .state()
        .env
        .insert("KUBECONFIG".into(), list.to_string_lossy().into_owned());
    upstream(route().await).unwrap();
    assert_eq!(
        setup.cluster.state().clients,
        1,
        "the env is fetched again when a kubeconfig file changes"
    );
    rewrite(&setup.kubeconfig, &kubeconfig("prod", SERVER, "one"));
    upstream(route().await).unwrap();
    assert_eq!(
        setup.cluster.state().clients,
        2,
        "the files the new env names have another user"
    );
    rewrite(&setup.kubeconfig, &kubeconfig("prod", SERVER, "ignored"));
    upstream(route().await).unwrap();
    assert_eq!(
        setup.cluster.state().clients,
        2,
        "~/.kube/config isn't read"
    );
    rewrite(&second, &kubeconfig("prod", SERVER, "three"));
    upstream(route().await).unwrap();
    assert_eq!(setup.cluster.state().clients, 3);
}

#[tokio::test]
async fn disabling_a_service_stops_its_forward() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let dialer = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    let mut open = dialer.dial().await.unwrap();
    assert_eq!(echo(&mut open, "one").await, "one");
    std::fs::write(
        setup.paths.project_file("prod"),
        format!("{PROJECT}[apps.grafana]\npolicy = \"disabled\"\n"),
    )
    .unwrap();
    setup
        .handle
        .reload(discovery::load_world(&setup.paths.config_dir));
    let mut stopped = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if setup
            .events()
            .await
            .iter()
            .any(|e| e.kind == EventKind::Stopped && e.message == "disabled")
        {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "{:?}", setup.events().await);
    let mut buf = [0u8; 8];
    assert_eq!(open.read(&mut buf).await.unwrap(), 0, "the connection ends");
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
async fn a_forward_whose_cluster_stops_answering_fails_in_time() {
    let setup = Setup::with_timeout(PROJECT, Duration::from_millis(300));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let dialer = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    setup.cluster.state().hangs.insert("connect");
    let dialed = tokio::time::timeout(Duration::from_secs(3), dialer.dial())
        .await
        .expect("the dial ends");
    assert!(dialed.is_err());
    let events = setup.events().await;
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::Failed
                && e.message.starts_with("No answer from the cluster")),
        "{events:?}"
    );
}

#[tokio::test]
async fn listing_services_times_out() {
    for call in ["login", "services"] {
        let setup = Setup::with_timeout(PROJECT, Duration::from_millis(300));
        setup.cluster.state().hangs.insert(call);
        let err = tokio::time::timeout(Duration::from_secs(3), setup.kube.services("prod"))
            .await
            .unwrap_or_else(|_| panic!("the listing ends when {call} hangs"))
            .unwrap_err();
        assert!(
            err.starts_with("No answer from the cluster"),
            "{call}: {err}"
        );
    }
}

#[tokio::test]
async fn a_forward_moves_off_a_pod_that_doesnt_answer() {
    let setup = Setup::with_timeout(PROJECT, Duration::from_millis(300));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a", "grafana-b"])
        .await;
    let dialer = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap();
    assert_eq!(get(&dialer).await, "grafana-a");
    {
        let mut state = setup.cluster.state();
        state.hangs.insert("connect grafana-a");
        let slices = state
            .slices
            .get_mut(&("monitoring".into(), "grafana".into()))
            .unwrap();
        slices[0].endpoints[0].ready = false;
    }
    let body = tokio::time::timeout(Duration::from_secs(3), get(&dialer))
        .await
        .expect("the request ends");
    assert_eq!(body, "grafana-b");
}

#[tokio::test]
async fn a_route_that_times_out_says_the_cluster_didnt_answer() {
    let setup = Setup::with_timeout(PROJECT, Duration::from_millis(300));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    setup.cluster.state().hangs.insert("endpoint_slices");
    let (status, text) = upstream(
        setup
            .kube
            .route(target("grafana", Some("monitoring")))
            .await,
    )
    .unwrap_err();
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(
        text.starts_with("The cluster didn't answer\nNo answer from the cluster"),
        "{text}"
    );
}

#[tokio::test]
async fn reading_logs_times_out() {
    let setup = Setup::with_timeout(PROJECT, Duration::from_millis(300));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    for call in ["containers", "logs"] {
        {
            let mut state = setup.cluster.state();
            state.hangs.clear();
            state.hangs.insert(call);
        }
        let err = tokio::time::timeout(
            Duration::from_secs(3),
            setup.kube.logs(
                address(Some("grafana"), Some("monitoring")),
                &logs_request(false),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("reading logs ends when {call} hangs"))
        .unwrap_err();
        assert!(
            err.starts_with("No answer from the cluster"),
            "{call}: {err}"
        );
    }
}

#[tokio::test]
async fn logs_that_stall_end_with_the_lines_read() {
    let setup = Setup::with_timeout(PROJECT, Duration::from_millis(300));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    {
        let mut state = setup.cluster.state();
        state.logs.insert("grafana-a".into(), vec!["one".into()]);
        state.hangs.insert("reading logs");
    }
    let mut lines = setup
        .kube
        .logs(
            address(Some("grafana"), Some("monitoring")),
            &logs_request(false),
        )
        .await
        .unwrap();
    let read = async {
        let mut got = Vec::new();
        while let Some(line) = lines.recv().await {
            got.push(line.text);
        }
        got
    };
    let got = tokio::time::timeout(Duration::from_secs(3), read)
        .await
        .expect("the logs end");
    assert_eq!(got, vec!["one"]);
}

#[tokio::test]
async fn a_stalled_read_is_let_go_while_following() {
    let setup = Setup::with_timeout(PROJECT, Duration::from_millis(300));
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    {
        let mut state = setup.cluster.state();
        state.logs.insert("grafana-a".into(), vec!["one".into()]);
        state.hangs.insert("reading logs");
    }
    let mut lines = setup
        .kube
        .logs(
            address(Some("grafana"), Some("monitoring")),
            &logs_request(true),
        )
        .await
        .unwrap();
    assert_eq!(lines.recv().await.unwrap().text, "one");
    let let_go = async {
        while setup.cluster.hanging() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(3), let_go)
        .await
        .expect("the stalled read is dropped");
}

#[tokio::test]
async fn following_logs_stops_waiting_for_the_cluster_when_the_reader_goes() {
    let setup = Setup::new(PROJECT);
    setup
        .cluster
        .add_service("monitoring", "grafana", &["grafana-a"])
        .await;
    let lines = setup
        .kube
        .logs(
            address(Some("grafana"), Some("monitoring")),
            &logs_request(true),
        )
        .await
        .unwrap();
    {
        let mut state = setup.cluster.state();
        state.hangs.insert("endpoint_slices");
        state.gone.insert("grafana-a".into());
    }
    let waiting = async {
        while setup.cluster.hanging() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the follow looks for the next pod");
    drop(lines);
    let gone = async {
        while setup.cluster.hanging() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(2), gone)
        .await
        .expect("the follow stops waiting once nobody reads");
}
