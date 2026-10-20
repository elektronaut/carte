//! Kube projects in the daemon: requests for a cluster's Services go to port-forwards
//! carte opens on demand and stops once nobody uses them.

mod cluster;
mod forward;
mod lookup;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use carte_api::{EventKind, EventSubject};
use carte_core::kube::{KubeProject, KubeTarget};
use carte_core::model::{Policy, World};
use carte_core::{duration, env};
use http::StatusCode;
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub use cluster::{ApiError, Cluster, KubeCluster, PodStream};
use forward::{Forward, Reporter};
pub use lookup::{EndpointInfo, PortInfo, ServiceInfo, SliceInfo};

use crate::proxy::{ActivityGuard, BoxFuture, ErrorPage, Route, UpstreamError};
use crate::supervisor::Handle;

/// How long a short hostname's namespace is remembered.
const NAMESPACE_TTL: Duration = Duration::from_secs(60);

/// How long a request may wait for the cluster before it gets an error page.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);

/// One Service in one namespace of a kube project.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub project: String,
    pub namespace: String,
    pub service: String,
}

impl Key {
    fn subject(&self) -> EventSubject {
        EventSubject {
            project: self.project.clone(),
            stack: Some(self.namespace.clone()),
            app: Some(self.service.clone()),
            process: None,
        }
    }
}

/// Builds a client for a kube project; tests substitute a fake cluster.
pub trait Connector: Send + Sync + 'static {
    fn connect<'a>(
        &'a self,
        project: &'a KubeProject,
    ) -> BoxFuture<'a, Result<Arc<dyn Cluster>, ApiError>>;
}

/// Connects through kube-rs, with login plugins running in the project's provider env.
pub struct KubeConnector;

impl Connector for KubeConnector {
    fn connect<'a>(
        &'a self,
        project: &'a KubeProject,
    ) -> BoxFuture<'a, Result<Arc<dyn Cluster>, ApiError>> {
        Box::pin(async move {
            let kind = project.env_provider;
            let env = tokio::task::spawn_blocking(move || provider_env(kind))
                .await
                .unwrap_or_else(|_| crate::supervisor::base_env());
            let cluster = KubeCluster::connect(project, &env).await?;
            Ok(Arc::new(cluster) as Arc<dyn Cluster>)
        })
    }
}

/// The env of `kind` in the home directory, or the daemon's own env when it fails.
fn provider_env(kind: Option<carte_core::model::EnvProviderKind>) -> BTreeMap<String, String> {
    let base = crate::supervisor::base_env();
    let home = PathBuf::from(base.get("HOME").cloned().unwrap_or_else(|| "/".into()));
    let runner = env::SystemRunner::from_env();
    let kind = env::detect_provider(kind, &home, &runner);
    match env::load_provider(kind, &home, &base, &runner) {
        Ok(env) => env.to_vars(),
        Err(err) => {
            warn!("kube login plugins run with the daemon's env: {err}");
            base
        }
    }
}

/// Routes requests for kube projects' Services. Cheap to clone.
#[derive(Clone)]
pub struct Kube {
    inner: Arc<Inner>,
}

struct Inner {
    supervisor: Handle,
    connector: Arc<dyn Connector>,
    /// Each project's client, behind a lock of its own so that a slow login only holds
    /// up its own project.
    clusters: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<ClusterEntry>>>>>,
    state: Mutex<State>,
}

struct ClusterEntry {
    project: KubeProject,
    /// When the kubeconfig changed last, as of the connect.
    stamp: Option<SystemTime>,
    cluster: Arc<dyn Cluster>,
}

#[derive(Default)]
struct State {
    instances: BTreeMap<Key, Instance>,
    /// The namespace a short hostname resolved to, by project and Service.
    namespaces: HashMap<(String, String), (String, Instant)>,
    /// Why each project's cluster can't be reached, while it can't.
    project_errors: HashMap<String, String>,
}

struct Instance {
    forward: Option<Running>,
    /// Requests and long-lived connections the proxy holds.
    active: usize,
    last_active: Instant,
    last_error: Option<String>,
}

struct Running {
    forward: Arc<Forward>,
    port: u16,
}

impl Instance {
    fn new() -> Self {
        Instance {
            forward: None,
            active: 0,
            last_active: Instant::now(),
            last_error: None,
        }
    }

    /// No request or long-lived connection is in flight. Connections the proxy keeps
    /// open between requests don't count, so a forward idles out behind its pool.
    fn idle(&self) -> bool {
        self.active == 0
    }
}

impl Kube {
    /// `tick` is how often idle forwards are looked for.
    pub fn new(supervisor: Handle, connector: Arc<dyn Connector>, tick: Duration) -> Self {
        let kube = Kube {
            inner: Arc::new(Inner {
                supervisor,
                connector,
                clusters: Mutex::new(HashMap::new()),
                state: Mutex::new(State::default()),
            }),
        };
        let weak = Arc::downgrade(&kube.inner);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(tick);
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                inner.sweep();
            }
        });
        kube
    }

    /// Resolves the target to a running forward, opening one when needed.
    pub async fn route(&self, target: KubeTarget) -> Route {
        match tokio::time::timeout(LOOKUP_TIMEOUT, self.inner.clone().route(target)).await {
            Ok(Ok(route)) => route,
            Ok(Err(page)) => Route::Error(page),
            Err(_) => Route::Error(
                ErrorPage::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "The cluster didn't answer",
                    format!(
                        "No answer from the cluster within {}s.",
                        LOOKUP_TIMEOUT.as_secs()
                    ),
                )
                .with_retry_after(Duration::from_secs(5)),
            ),
        }
    }

    /// The page for a forward the proxy couldn't use, with what went wrong last.
    pub fn upstream_failed(&self, port: u16, error: &UpstreamError) -> ErrorPage {
        let state = self.inner.state.lock().expect("kube state");
        let found = state
            .instances
            .iter()
            .find(|(_, i)| i.forward.as_ref().is_some_and(|r| r.port == port));
        let (title, detail) = match found {
            Some((key, instance)) => (
                format!("{} didn't answer", key.service),
                instance.last_error.clone().unwrap_or_else(|| match error {
                    UpstreamError::Connect(err) => err.to_string(),
                    UpstreamError::Request(err) | UpstreamError::NotHttp(err) => err.clone(),
                }),
            ),
            None => (
                "The forward is gone".to_string(),
                "It stopped while the request was on its way; reload to open it again.".into(),
            ),
        };
        ErrorPage::new(StatusCode::BAD_GATEWAY, title, detail)
            .with_retry_after(Duration::from_secs(2))
    }
}

impl Inner {
    async fn route(self: Arc<Self>, target: KubeTarget) -> Result<Route, ErrorPage> {
        let key = self.clone().start_target(target).await?;
        let port = self.acquire(&key).ok_or_else(|| {
            ErrorPage::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The forward stopped",
                "It stopped while the request was on its way; reload to open it again.",
            )
        })?;
        Ok(Route::Upstream {
            port,
            guard: Box::new(Guard { inner: self, key }),
        })
    }

    /// Resolves the target and makes sure its forward runs.
    async fn start_target(self: Arc<Self>, target: KubeTarget) -> Result<Key, ErrorPage> {
        let world = self.supervisor.world();
        let project = world
            .kube_project(&target.project)
            .cloned()
            .ok_or_else(|| {
                ErrorPage::new(
                    StatusCode::NOT_FOUND,
                    "No such project",
                    format!("There's no kube project {}.", target.project),
                )
            })?;
        if project.policy(&target.service) == Policy::Disabled {
            return Err(ErrorPage::new(
                StatusCode::FORBIDDEN,
                "Disabled",
                format!(
                    "{} is disabled in the {} project file.",
                    target.service, project.name
                ),
            ));
        }
        let cluster = self.cluster(&project).await?;
        let namespace = self.namespace(&world, &project, &*cluster, &target).await?;
        let key = Key {
            project: project.name.clone(),
            namespace,
            service: target.service.clone(),
        };
        if !project.allows_namespace(&key.namespace) {
            return Err(ErrorPage::new(
                StatusCode::NOT_FOUND,
                "Not one of the project's namespaces",
                format!(
                    "{} isn't in `namespaces` of the {} project file.",
                    key.namespace, project.name
                ),
            ));
        }
        self.stop_if_stale(&key, &cluster);
        if self.running_port(&key).is_none() {
            self.start(&project, cluster, &key).await?;
        }
        Ok(key)
    }

    fn running_port(&self, key: &Key) -> Option<u16> {
        let state = self.state.lock().expect("kube state");
        state.instances.get(key)?.forward.as_ref().map(|r| r.port)
    }

    /// The running forward's port, counting a request on it so the sweep leaves it be.
    fn acquire(&self, key: &Key) -> Option<u16> {
        let mut state = self.state.lock().expect("kube state");
        let instance = state.instances.get_mut(key)?;
        let port = instance.forward.as_ref()?.port;
        instance.active += 1;
        instance.last_active = Instant::now();
        Some(port)
    }

    /// Stops the forward when it still talks to a client the project replaced, since
    /// its context, kubeconfig or settings changed.
    fn stop_if_stale(&self, key: &Key, cluster: &Arc<dyn Cluster>) {
        let stale = {
            let mut state = self.state.lock().expect("kube state");
            let Some(instance) = state.instances.get_mut(key) else {
                return;
            };
            match &instance.forward {
                Some(running) if !Arc::ptr_eq(&running.forward.cluster, cluster) => {
                    instance.forward.take()
                }
                _ => None,
            }
        };
        if let Some(running) = stale {
            running.forward.cancel.cancel();
            self.supervisor.event(
                EventKind::Stopped,
                "the project changed",
                vec![key.subject()],
            );
        }
    }

    /// The project's client, connecting again when its settings or kubeconfig changed.
    async fn cluster(&self, project: &KubeProject) -> Result<Arc<dyn Cluster>, ErrorPage> {
        let stamp = kubeconfig_stamp(project);
        let slot = self
            .clusters
            .lock()
            .expect("kube clusters")
            .entry(project.name.clone())
            .or_default()
            .clone();
        let mut slot = slot.lock().await;
        if let Some(entry) = slot.as_ref()
            && entry.project == *project
            && entry.stamp == stamp
        {
            return Ok(entry.cluster.clone());
        }
        match self.connector.connect(project).await {
            Ok(cluster) => {
                *slot = Some(ClusterEntry {
                    project: project.clone(),
                    stamp,
                    cluster: cluster.clone(),
                });
                Ok(cluster)
            }
            Err(err) => {
                *slot = None;
                self.project_failed(&project.name, &err);
                Err(api_page(&err))
            }
        }
    }

    /// The target's namespace; for a short hostname, the one namespace whose
    /// Services include the target.
    async fn namespace(
        &self,
        world: &World,
        project: &KubeProject,
        cluster: &dyn Cluster,
        target: &KubeTarget,
    ) -> Result<String, ErrorPage> {
        if let Some(namespace) = &target.namespace {
            return Ok(namespace.clone());
        }
        let cache_key = (project.name.clone(), target.service.clone());
        if let Some((namespace, at)) = self
            .state
            .lock()
            .expect("kube state")
            .namespaces
            .get(&cache_key)
            && at.elapsed() < NAMESPACE_TTL
        {
            return Ok(namespace.clone());
        }
        let found = match cluster
            .services_named(&target.service, &project.namespaces)
            .await
        {
            Ok(found) => found,
            Err(ApiError::Forbidden(message)) => {
                return Err(ErrorPage::new(
                    StatusCode::FORBIDDEN,
                    "Can't look across namespaces",
                    format!(
                        "The cluster doesn't let you list Services in every namespace ({message}). Name the namespace in the hostname, like {}, or list the namespaces you use in `namespaces` in the {} project file.",
                        carte_core::kube::host(
                            &world.machine.tld,
                            &project.name,
                            "<namespace>",
                            &target.service
                        ),
                        project.name
                    ),
                ));
            }
            Err(err) => {
                self.project_failed(&project.name, &err);
                return Err(api_page(&err));
            }
        };
        self.project_reached(&project.name);
        match found.as_slice() {
            [] => Err(ErrorPage::new(
                StatusCode::NOT_FOUND,
                "No such Service",
                format!(
                    "No namespace of {} has a Service named {}.",
                    project.name, target.service
                ),
            )),
            [one] => {
                self.state
                    .lock()
                    .expect("kube state")
                    .namespaces
                    .insert(cache_key, (one.namespace.clone(), Instant::now()));
                Ok(one.namespace.clone())
            }
            many => {
                let hosts: Vec<String> = many
                    .iter()
                    .map(|s| {
                        format!(
                            "https://{}",
                            carte_core::kube::host(
                                &world.machine.tld,
                                &project.name,
                                &s.namespace,
                                &s.name
                            )
                        )
                    })
                    .collect();
                Err(ErrorPage::new(
                    StatusCode::MULTIPLE_CHOICES,
                    "Which namespace?",
                    format!(
                        "Several namespaces have a Service named {}:\n{}\n\nOr give it a `namespace` in [apps.{}] of the {} project file.",
                        target.service,
                        hosts.join("\n"),
                        target.service,
                        project.name
                    ),
                ))
            }
        }
    }

    /// Looks up the Service and a pod behind it, and opens the forward.
    async fn start(
        self: &Arc<Self>,
        project: &KubeProject,
        cluster: Arc<dyn Cluster>,
        key: &Key,
    ) -> Result<u16, ErrorPage> {
        let result = self.prepare(project, cluster, key).await;
        let forward = match result {
            Ok(forward) => forward,
            Err(page) => {
                let message = page.detail.lines().next().unwrap_or_default().to_string();
                self.failed(key, message);
                return Err(page);
            }
        };
        let target = forward.target();
        let port = match forward.clone().listen().await {
            Ok(port) => port,
            Err(err) => {
                let message = format!("couldn't listen on loopback: {err}");
                self.failed(key, message.clone());
                return Err(ErrorPage::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Couldn't open the forward",
                    message,
                ));
            }
        };
        {
            let mut state = self.state.lock().expect("kube state");
            let instance = state
                .instances
                .entry(key.clone())
                .or_insert_with(Instance::new);
            if let Some(running) = &instance.forward {
                forward.cancel.cancel();
                return Ok(running.port);
            }
            instance.forward = Some(Running {
                forward: forward.clone(),
                port,
            });
            instance.last_active = Instant::now();
            instance.last_error = None;
        }
        self.supervisor.event(
            EventKind::Ready,
            format!("pod {} port {}", target.pod, target.port),
            vec![key.subject()],
        );
        Ok(port)
    }

    async fn prepare(
        self: &Arc<Self>,
        project: &KubeProject,
        cluster: Arc<dyn Cluster>,
        key: &Key,
    ) -> Result<Arc<Forward>, ErrorPage> {
        let api = |err: ApiError| {
            self.project_failed(&project.name, &err);
            api_page(&err)
        };
        let service = cluster
            .service(&key.namespace, &key.service)
            .await
            .map_err(api)?
            .ok_or_else(|| {
                ErrorPage::new(
                    StatusCode::NOT_FOUND,
                    "No such Service",
                    format!(
                        "namespace {} has no Service named {}",
                        key.namespace, key.service
                    ),
                )
            })?;
        self.project_reached(&project.name);
        let lookup = |err: lookup::LookupError| {
            ErrorPage::new(
                StatusCode::BAD_GATEWAY,
                err.title(),
                err.detail(&key.service, &project.name),
            )
        };
        let wanted = project
            .service(&key.service)
            .and_then(|s| s.service_port.as_ref());
        let port = lookup::choose_port(&service, wanted).map_err(lookup)?;
        let slices = cluster
            .endpoint_slices(&key.namespace, &key.service)
            .await
            .map_err(api)?;
        let target = lookup::choose_pod(&port, &slices, None).map_err(lookup)?;
        Ok(Arc::new(Forward {
            cluster,
            namespace: key.namespace.clone(),
            service: key.service.clone(),
            port,
            target: Mutex::new(target),
            cancel: CancellationToken::new(),
            reporter: Box::new(InstanceReporter {
                inner: Arc::downgrade(self),
                key: key.clone(),
            }),
        }))
    }

    /// Records why the instance couldn't start; a new reason is also an event.
    fn failed(&self, key: &Key, message: String) {
        let changed = {
            let mut state = self.state.lock().expect("kube state");
            let instance = state
                .instances
                .entry(key.clone())
                .or_insert_with(Instance::new);
            let changed = instance.last_error.as_ref() != Some(&message);
            instance.last_error = Some(message.clone());
            instance.last_active = Instant::now();
            changed
        };
        if changed {
            self.supervisor
                .event(EventKind::Failed, message, vec![key.subject()]);
        }
    }

    fn project_failed(&self, project: &str, err: &ApiError) {
        if !matches!(err, ApiError::Auth { .. } | ApiError::Other(_)) {
            return;
        }
        let summary = err.summary();
        let changed = self
            .state
            .lock()
            .expect("kube state")
            .project_errors
            .insert(project.to_string(), summary.clone())
            .as_ref()
            != Some(&summary);
        if changed {
            self.supervisor.event(
                EventKind::Failed,
                summary,
                vec![EventSubject {
                    project: project.to_string(),
                    stack: None,
                    app: None,
                    process: None,
                }],
            );
        }
    }

    fn project_reached(&self, project: &str) {
        self.state
            .lock()
            .expect("kube state")
            .project_errors
            .remove(project);
    }

    /// Stops forwards nobody used for the idle timeout, and those of projects that
    /// are gone.
    fn sweep(&self) {
        let world = self.supervisor.world();
        let mut stopped = Vec::new();
        {
            let mut state = self.state.lock().expect("kube state");
            state.instances.retain(|key, instance| {
                let Some(project) = world.kube_project(&key.project) else {
                    if let Some(running) = instance.forward.take() {
                        running.forward.cancel.cancel();
                    }
                    return false;
                };
                let timeout = project.idle_timeout.unwrap_or(world.machine.idle_timeout);
                let configured = project
                    .service(&key.service)
                    .is_some_and(|s| s.namespace.as_deref() == Some(key.namespace.as_str()));
                if instance.forward.is_none()
                    && !configured
                    && instance.last_active.elapsed() >= timeout
                {
                    return false;
                }
                if instance.forward.is_some()
                    && instance.idle()
                    && instance.last_active.elapsed() >= timeout
                {
                    if let Some(running) = instance.forward.take() {
                        running.forward.cancel.cancel();
                    }
                    stopped.push((key.clone(), timeout));
                }
                true
            });
            state
                .project_errors
                .retain(|name, _| world.kube_project(name).is_some());
        }
        self.clusters
            .lock()
            .expect("kube clusters")
            .retain(|name, _| world.kube_project(name).is_some());
        for (key, timeout) in stopped {
            self.supervisor.event(
                EventKind::Stopped,
                format!("idle for {}", duration::format(timeout)),
                vec![key.subject()],
            );
        }
    }
}

/// When the kubeconfig carte reads by default last changed.
fn kubeconfig_stamp(project: &KubeProject) -> Option<SystemTime> {
    let path = match &project.kubeconfig {
        Some(path) => path.clone(),
        None => Path::new(&std::env::var_os("HOME")?).join(".kube/config"),
    };
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn api_page(err: &ApiError) -> ErrorPage {
    let status = match err {
        ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
        ApiError::NotFound(_) => StatusCode::NOT_FOUND,
        ApiError::Auth { .. } | ApiError::Other(_) => StatusCode::BAD_GATEWAY,
    };
    ErrorPage::new(status, err.title(), err.detail())
}

struct InstanceReporter {
    inner: Weak<Inner>,
    key: Key,
}

impl Reporter for InstanceReporter {
    fn event(&self, kind: EventKind, message: String) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .supervisor
                .event(kind, message, vec![self.key.subject()]);
        }
    }

    fn error(&self, error: Option<String>) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let changed = {
            let mut state = inner.state.lock().expect("kube state");
            let Some(instance) = state.instances.get_mut(&self.key) else {
                return;
            };
            let changed = instance.last_error != error;
            instance.last_error = error.clone();
            changed
        };
        if let (true, Some(message)) = (changed, error) {
            inner
                .supervisor
                .event(EventKind::Failed, message, vec![self.key.subject()]);
        }
    }
}

/// Keeps an instance from idling out while the proxy holds a request or connection.
struct Guard {
    inner: Arc<Inner>,
    key: Key,
}

impl ActivityGuard for Guard {
    fn long_lived(&mut self) {}
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(instance) = self
            .inner
            .state
            .lock()
            .expect("kube state")
            .instances
            .get_mut(&self.key)
        {
            instance.active = instance.active.saturating_sub(1);
            instance.last_active = Instant::now();
        }
    }
}
