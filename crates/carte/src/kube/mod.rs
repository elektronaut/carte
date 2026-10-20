//! Kube projects in the daemon: requests for a cluster's Services go to port-forwards
//! carte opens on demand and stops once nobody uses them.

mod cluster;
mod forward;
mod lookup;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use carte_api::{
    Action, ControlResponse, EventKind, EventSubject, Hold, InstanceState, InstanceStatus,
    KubeInstanceInfo, KubeProjectInfo, KubeServiceEntry, LogLine, LogsRequest, ProjectStatus,
    StackStatus, StartFailure,
};
use carte_core::kube::{KubeAddress, KubeProject, KubeTarget};
use carte_core::model::{Policy, World};
use carte_core::{duration, env};
use futures_util::StreamExt;
use http::StatusCode;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub use cluster::{
    ApiError, Cluster, KubeCluster, LogLines, LogOptions, PodStream, Readable, read_each_namespace,
};
use forward::{Forward, Reporter};
pub use kube::config::Kubeconfig;
use lookup::PodTarget;
pub use lookup::{ContainerInfo, EndpointInfo, PortInfo, ServiceInfo, SliceInfo};

use crate::proxy::{
    ActivityGuard, BoxFuture, Dial, Dialer, ErrorPage, Route, UpstreamError, UpstreamStream,
};
use crate::supervisor::Handle;

/// How long a short hostname's namespace is remembered.
const NAMESPACE_TTL: Duration = Duration::from_secs(60);

/// The most lines read back to search through or to show since a time.
const MAX_LOG_LINES: i64 = 10_000;

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
    /// `service.project@namespace`, as the CLI takes it.
    pub fn label(&self) -> String {
        format!("{}.{}@{}", self.service, self.project, self.namespace)
    }

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
    /// The env login plugins run with. Its `KUBECONFIG` names the kubeconfig files of
    /// a project that names none.
    fn env<'a>(&'a self, project: &'a KubeProject) -> BoxFuture<'a, BTreeMap<String, String>>;

    /// A client for the project's context in `kubeconfig`.
    fn connect<'a>(
        &'a self,
        project: &'a KubeProject,
        kubeconfig: Kubeconfig,
        env: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Arc<dyn Cluster>, ApiError>>;
}

/// Connects through kube-rs, with login plugins running in the project's provider env.
pub struct KubeConnector;

impl Connector for KubeConnector {
    fn env<'a>(&'a self, project: &'a KubeProject) -> BoxFuture<'a, BTreeMap<String, String>> {
        Box::pin(async move {
            let kind = project.env_provider;
            tokio::task::spawn_blocking(move || provider_env(kind))
                .await
                .unwrap_or_else(|_| crate::supervisor::base_env())
        })
    }

    fn connect<'a>(
        &'a self,
        project: &'a KubeProject,
        kubeconfig: Kubeconfig,
        env: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Arc<dyn Cluster>, ApiError>> {
        Box::pin(async move {
            let cluster = KubeCluster::connect(project, kubeconfig, env).await?;
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
    /// Bumped when what `status` shows changes.
    changes: watch::Sender<u64>,
}

struct ClusterEntry {
    project: KubeProject,
    env: BTreeMap<String, String>,
    /// When each kubeconfig file changed last, as of the last read.
    stamps: Vec<Option<SystemTime>>,
    /// What the project's context resolved to.
    context: [u8; 32],
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
    dialer: Dialer,
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
                changes: watch::channel(0).0,
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

    /// How many forwards run.
    pub fn running(&self) -> u32 {
        self.inner.running_count()
    }

    /// Wakes up when what `status` shows changes.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    /// Every kube project with the Services carte knows of: configured ones with a
    /// namespace, and those requested since the daemon started.
    pub fn status(&self, world: &World) -> Vec<ProjectStatus> {
        let state = self.inner.state.lock().expect("kube state");
        world
            .kube_projects
            .iter()
            .map(|project| {
                let mut stacks: BTreeMap<String, Vec<InstanceStatus>> = BTreeMap::new();
                for (key, instance) in state
                    .instances
                    .iter()
                    .filter(|(k, _)| k.project == project.name)
                {
                    stacks
                        .entry(key.namespace.clone())
                        .or_default()
                        .push(instance_status(world, project, key, Some(instance)));
                }
                for (service, settings) in &project.services {
                    let Some(namespace) = &settings.namespace else {
                        continue;
                    };
                    let key = Key {
                        project: project.name.clone(),
                        namespace: namespace.clone(),
                        service: service.clone(),
                    };
                    if !state.instances.contains_key(&key) {
                        stacks
                            .entry(namespace.clone())
                            .or_default()
                            .push(instance_status(world, project, &key, None));
                    }
                }
                ProjectStatus {
                    name: project.name.clone(),
                    folder: project.folder.clone(),
                    apps: Vec::new(),
                    stacks: stacks
                        .into_iter()
                        .map(|(name, mut instances)| {
                            instances.sort_by(|a, b| a.app.cmp(&b.app));
                            StackStatus {
                                name,
                                branch: None,
                                instances,
                            }
                        })
                        .collect(),
                    kube: Some(KubeProjectInfo {
                        context: project.context.clone(),
                        error: state.project_errors.get(&project.name).cloned(),
                    }),
                }
            })
            .collect()
    }

    /// Starts, stops or restarts what the address names.
    pub async fn control(
        &self,
        address: KubeAddress,
        action: Action,
    ) -> Result<ControlResponse, String> {
        let world = self.inner.supervisor.world();
        let project = world
            .kube_project(&address.project)
            .cloned()
            .ok_or_else(|| format!("no kube project {}", address.project))?;
        let mut response = ControlResponse {
            affected: Vec::new(),
            messages: Vec::new(),
            failures: Vec::new(),
        };
        let to_start: Vec<KubeTarget> = match action {
            Action::Up => {
                let service = address.service.clone().ok_or_else(|| {
                    format!(
                        "name a Service to start, like `carte up grafana.{}`",
                        project.name
                    )
                })?;
                vec![KubeTarget {
                    project: project.name.clone(),
                    service,
                    namespace: address.namespace.clone(),
                    via_alias: false,
                }]
            }
            Action::Down => {
                for key in self.inner.stop_matching(&address, "stopped on request") {
                    response.affected.push(key.label());
                }
                self.inner.forget_failures(&address);
                return Ok(response);
            }
            Action::Restart => {
                let stopped = self.inner.stop_matching(&address, "restarting");
                let mut targets: Vec<KubeTarget> = stopped
                    .into_iter()
                    .map(|key| KubeTarget {
                        project: key.project,
                        service: key.service,
                        namespace: Some(key.namespace),
                        via_alias: false,
                    })
                    .collect();
                if targets.is_empty()
                    && let Some(service) = &address.service
                {
                    targets.push(KubeTarget {
                        project: project.name.clone(),
                        service: service.clone(),
                        namespace: address.namespace.clone(),
                        via_alias: false,
                    });
                }
                targets
            }
            Action::Pin { .. } | Action::Unpin { .. } => {
                return Err("kube projects' Services can't be pinned; idling costs nothing".into());
            }
            Action::Disable { .. } | Action::Enable { .. } => {
                return Err(format!(
                    "set `policy = \"disabled\"` in [apps.<service>] of the {} project file instead",
                    project.name
                ));
            }
        };
        for target in to_start {
            let label = match &target.namespace {
                Some(namespace) => format!("{}.{}@{namespace}", target.service, target.project),
                None => format!("{}.{}", target.service, target.project),
            };
            response.affected.push(label.clone());
            match tokio::time::timeout(LOOKUP_TIMEOUT, self.inner.clone().start_target(target))
                .await
            {
                Ok(Ok(key)) => {
                    response.affected.pop();
                    response.affected.push(key.label());
                }
                Ok(Err(page)) => response.failures.push(StartFailure {
                    instance: label,
                    title: page.title,
                    detail: page.detail,
                    log: Vec::new(),
                }),
                Err(_) => response.failures.push(StartFailure {
                    instance: label,
                    title: "The cluster didn't answer".into(),
                    detail: format!("No answer within {}s.", LOOKUP_TIMEOUT.as_secs()),
                    log: Vec::new(),
                }),
            }
        }
        Ok(response)
    }

    /// Every Service of the project, from the cluster. When the cluster doesn't let
    /// you list Services across namespaces, lists them per namespace, and when it
    /// doesn't let you list namespaces either, in the project's `namespaces`.
    pub async fn services(&self, project: &str) -> Result<Vec<KubeServiceEntry>, String> {
        let world = self.inner.supervisor.world();
        let project = world
            .kube_project(project)
            .cloned()
            .ok_or_else(|| format!("no kube project {project}"))?;
        let cluster = self
            .inner
            .cluster(&project)
            .await
            .map_err(|page| page.detail)?;
        let failed = |err: ApiError| {
            self.inner.project_failed(&project.name, &err);
            err.summary()
        };
        let mut services = if project.namespaces.is_empty() {
            match cluster.services(None).await {
                Ok(services) => services,
                Err(ApiError::Forbidden(_)) => {
                    let namespaces = match cluster.namespaces().await {
                        Ok(namespaces) => namespaces,
                        Err(ApiError::Forbidden(_)) => {
                            return Err(format!(
                                "The cluster doesn't let you list namespaces, or Services across them. List the namespaces you use in `namespaces` in the {} project file.",
                                project.name
                            ));
                        }
                        Err(err) => return Err(failed(err)),
                    };
                    each_namespace(&*cluster, &namespaces)
                        .await
                        .map_err(failed)?
                }
                Err(err) => return Err(failed(err)),
            }
        } else {
            each_namespace(&*cluster, &project.namespaces)
                .await
                .map_err(failed)?
        };
        self.inner.project_reached(&project.name);
        services.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
        Ok(services
            .into_iter()
            .map(|s| KubeServiceEntry {
                url: carte_core::kube::url(&world.machine, &project.name, &s.namespace, &s.name),
                namespace: s.namespace,
                name: s.name,
            })
            .collect())
    }

    /// The logs of the pod the Service's forward uses, opening the forward when
    /// needed. Following, they continue on the next pod when that one goes away.
    pub async fn logs(
        &self,
        address: KubeAddress,
        request: &LogsRequest,
    ) -> Result<mpsc::Receiver<LogLine>, String> {
        if request.rank.is_some() {
            return Err("kube projects' Services can't be ranked".into());
        }
        let service = address.service.clone().ok_or_else(|| {
            format!(
                "name a Service to see its logs, like `carte logs grafana.{}`",
                address.project
            )
        })?;
        let target = KubeTarget {
            project: address.project.clone(),
            service,
            namespace: address.namespace.clone(),
            via_alias: false,
        };
        let page = |page: ErrorPage| format!("{}: {}", page.title, page.detail);
        let found = tokio::time::timeout(LOOKUP_TIMEOUT, async {
            let (project, cluster, key) = self.inner.resolve(&target).await?;
            let running = self
                .inner
                .state
                .lock()
                .expect("kube state")
                .instances
                .get(&key)
                .and_then(|i| i.forward.as_ref())
                .filter(|r| Arc::ptr_eq(&r.forward.cluster(), &cluster))
                .map(|r| (r.forward.port.clone(), r.forward.target()));
            let (port, pod) = match running {
                Some(found) => found,
                None => self.inner.lookup_pod(&project, &*cluster, &key).await?,
            };
            Ok::<_, ErrorPage>((cluster, key, port, pod))
        })
        .await
        .map_err(|_| "the cluster didn't answer".to_string())?;
        let (cluster, key, port, pod) = found.map_err(page)?;
        let containers = cluster
            .containers(&key.namespace, &pod.pod)
            .await
            .map_err(|err| err.detail())?;
        let container = lookup::choose_container(&containers, pod.port).map(|c| c.name.clone());
        let since_secs = request
            .since_ms
            .map(|ms| (crate::clock::now_ms().saturating_sub(ms) / 1000 + 1) as i64);
        let filtered = request.search.is_some() || since_secs.is_some();
        let tail = request.tail.unwrap_or(100);
        let initial = LogOptions {
            container: container.clone(),
            tail: Some(if filtered {
                MAX_LOG_LINES
            } else {
                tail.max(1) as i64
            }),
            since_secs,
            ..LogOptions::default()
        };
        let initial = cluster
            .logs(&key.namespace, &pod.pod, initial)
            .await
            .map_err(|err| err.detail())?;
        let (tx, rx) = mpsc::channel(256);
        let lines = PodLines {
            cluster,
            key,
            port,
            pod: pod.pod,
            container,
            search: request.search.clone(),
            tail: (since_secs.is_none()).then_some(tail),
            since_secs,
            last_ns: None,
            catching_up: false,
            tx,
        };
        tokio::spawn(lines.run(initial, request.follow));
        Ok(rx)
    }

    /// Stops every forward, or only those without requests or connections; answers
    /// with `service.project@namespace` for each.
    pub fn stop_all(&self, idle_only: bool) -> Vec<String> {
        let world = self.inner.supervisor.world();
        let keys: Vec<Key> = {
            let state = self.inner.state.lock().expect("kube state");
            state
                .instances
                .iter()
                .filter(|(_, i)| i.forward.is_some() && (!idle_only || i.idle()))
                .map(|(k, _)| k.clone())
                .collect()
        };
        keys.into_iter()
            .filter(|key| world.kube_project(&key.project).is_some())
            .flat_map(|key| {
                let address = KubeAddress {
                    project: key.project.clone(),
                    service: Some(key.service.clone()),
                    namespace: Some(key.namespace.clone()),
                };
                self.inner.stop_matching(&address, "stopped on request")
            })
            .map(|key| key.label())
            .collect()
    }
}

impl Inner {
    async fn route(self: Arc<Self>, target: KubeTarget) -> Result<Route, ErrorPage> {
        let key = self.clone().start_target(target).await?;
        let dialer = self.acquire(&key).ok_or_else(|| {
            ErrorPage::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The forward stopped",
                "It stopped while the request was on its way; reload to open it again.",
            )
        })?;
        Ok(Route::Dialed {
            dialer,
            guard: Box::new(Guard { inner: self, key }),
        })
    }

    /// Resolves the target and makes sure its forward runs.
    async fn start_target(self: Arc<Self>, target: KubeTarget) -> Result<Key, ErrorPage> {
        let (project, cluster, key) = self.resolve(&target).await?;
        if !self.running_with(&key, &cluster) {
            self.start(&project, cluster, &key).await?;
        }
        Ok(key)
    }

    /// The target's project, client and namespace, without opening anything.
    async fn resolve(
        &self,
        target: &KubeTarget,
    ) -> Result<(KubeProject, Arc<dyn Cluster>, Key), ErrorPage> {
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
        let namespace = self.namespace(&world, &project, &*cluster, target).await?;
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
        Ok((project, cluster, key))
    }

    /// Stops the running forwards the address names.
    fn stop_matching(&self, address: &KubeAddress, why: &str) -> Vec<Key> {
        let mut stopped = Vec::new();
        {
            let mut state = self.state.lock().expect("kube state");
            for (key, instance) in state.instances.iter_mut() {
                let matches = key.project == address.project
                    && address.service.as_ref().is_none_or(|s| *s == key.service)
                    && address
                        .namespace
                        .as_ref()
                        .is_none_or(|n| *n == key.namespace);
                if let (true, Some(running)) = (matches, instance.forward.take()) {
                    running.forward.cancel.cancel();
                    stopped.push(key.clone());
                }
            }
        }
        for key in &stopped {
            self.supervisor
                .event(EventKind::Stopped, why, vec![key.subject()]);
        }
        if !stopped.is_empty() {
            self.changed();
        }
        stopped
    }

    /// Forgets why the instances the address names couldn't start, for those not
    /// running.
    fn forget_failures(&self, address: &KubeAddress) {
        let mut state = self.state.lock().expect("kube state");
        let before = state.instances.len();
        state.instances.retain(|key, instance| {
            let matches = key.project == address.project
                && address.service.as_ref().is_none_or(|s| *s == key.service)
                && address
                    .namespace
                    .as_ref()
                    .is_none_or(|n| *n == key.namespace);
            !(matches && instance.forward.is_none())
        });
        let forgot = state.instances.len() != before;
        drop(state);
        if forgot {
            self.changed();
        }
    }

    /// Forwards running now.
    fn running_count(&self) -> u32 {
        let state = self.state.lock().expect("kube state");
        state
            .instances
            .values()
            .filter(|i| i.forward.is_some())
            .count() as u32
    }

    fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    /// Whether the key's forward runs; when it does, its new connections use
    /// `cluster`, which replaced the client of a changed project or kubeconfig.
    fn running_with(&self, key: &Key, cluster: &Arc<dyn Cluster>) -> bool {
        let state = self.state.lock().expect("kube state");
        let Some(running) = state.instances.get(key).and_then(|i| i.forward.as_ref()) else {
            return false;
        };
        if !Arc::ptr_eq(&running.forward.cluster(), cluster) {
            running.forward.set_cluster(cluster.clone());
        }
        true
    }

    /// The running forward's dialer, counting a request on it so the sweep leaves it
    /// be.
    fn acquire(&self, key: &Key) -> Option<Dialer> {
        let mut state = self.state.lock().expect("kube state");
        let instance = state.instances.get_mut(key)?;
        let dialer = instance.forward.as_ref()?.dialer.clone();
        instance.active += 1;
        instance.last_active = Instant::now();
        Some(dialer)
    }

    /// The project's client. When a kubeconfig file changed, the provider env is
    /// fetched and the kubeconfig read again, and a new client built only when what
    /// the project's context resolves to, with its login plugin's env, changed.
    async fn cluster(&self, project: &KubeProject) -> Result<Arc<dyn Cluster>, ErrorPage> {
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
            && kubeconfig_stamps(project, &entry.env) == entry.stamps
        {
            return Ok(entry.cluster.clone());
        }
        let env = self.connector.env(project).await;
        match self.reconnect(project, env, &mut slot).await {
            Ok(cluster) => Ok(cluster),
            Err(err) => {
                *slot = None;
                self.project_failed(&project.name, &err);
                Err(api_page(&err))
            }
        }
    }

    /// Reads the kubeconfig and keeps the slot's client while what the context
    /// resolves to, with its login plugin's env, is the same, else connects again.
    async fn reconnect(
        &self,
        project: &KubeProject,
        env: BTreeMap<String, String>,
        slot: &mut Option<ClusterEntry>,
    ) -> Result<Arc<dyn Cluster>, ApiError> {
        let stamps = kubeconfig_stamps(project, &env);
        let kubeconfig = cluster::read_kubeconfig(project, &env)?;
        let context = cluster::context_digest(&kubeconfig, &project.context, &env)?;
        if let Some(entry) = slot.as_mut()
            && entry.project == *project
            && entry.context == context
        {
            entry.env = env;
            entry.stamps = stamps;
            return Ok(entry.cluster.clone());
        }
        let cluster = self.connector.connect(project, kubeconfig, &env).await?;
        *slot = Some(ClusterEntry {
            project: project.clone(),
            env,
            stamps,
            context,
            cluster: cluster.clone(),
        });
        Ok(cluster)
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
        let named = match cluster
            .services_named(&target.service, &project.namespaces)
            .await
        {
            Ok(named) => named,
            Err(ApiError::Forbidden(message)) if !project.namespaces.is_empty() => {
                return Err(ErrorPage::new(
                    StatusCode::FORBIDDEN,
                    "Can't read the namespaces",
                    format!(
                        "The cluster doesn't let you read Services in {} ({message}).",
                        project.namespaces.join(", ")
                    ),
                ));
            }
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
        match named.found.as_slice() {
            [] => {
                let refused = if named.refused.is_empty() {
                    String::new()
                } else {
                    format!(" (couldn't read {})", named.refused.join(", "))
                };
                Err(ErrorPage::new(
                    StatusCode::NOT_FOUND,
                    "No such Service",
                    format!(
                        "No namespace of {} has a Service named {}{refused}.",
                        project.name, target.service
                    ),
                ))
            }
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
                        carte_core::kube::url(&world.machine, &project.name, &s.namespace, &s.name)
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
    ) -> Result<(), ErrorPage> {
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
        let dialer = Dialer::new(Arc::new(ForwardDial {
            forward: forward.clone(),
            inner: Arc::downgrade(self),
            key: key.clone(),
        }));
        {
            let mut state = self.state.lock().expect("kube state");
            let instance = state
                .instances
                .entry(key.clone())
                .or_insert_with(Instance::new);
            if instance.forward.is_some() {
                forward.cancel.cancel();
                return Ok(());
            }
            instance.forward = Some(Running {
                forward: forward.clone(),
                dialer,
            });
            instance.last_active = Instant::now();
            instance.last_error = None;
        }
        self.supervisor.event(
            EventKind::Ready,
            format!("pod {} port {}", target.pod, target.port),
            vec![key.subject()],
        );
        self.changed();
        Ok(())
    }

    async fn prepare(
        self: &Arc<Self>,
        project: &KubeProject,
        cluster: Arc<dyn Cluster>,
        key: &Key,
    ) -> Result<Arc<Forward>, ErrorPage> {
        let (port, target) = self.lookup_pod(project, &*cluster, key).await?;
        Ok(Arc::new(Forward {
            cluster: Mutex::new(cluster),
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

    /// The Service's port and a ready pod behind it.
    async fn lookup_pod(
        &self,
        project: &KubeProject,
        cluster: &dyn Cluster,
        key: &Key,
    ) -> Result<(PortInfo, PodTarget), ErrorPage> {
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
        Ok((port, target))
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
            self.changed();
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
            self.changed();
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
        let removed = self
            .state
            .lock()
            .expect("kube state")
            .project_errors
            .remove(project);
        if removed.is_some() {
            self.changed();
        }
    }

    /// Stops forwards nobody used for the idle timeout, those of disabled Services,
    /// and those of projects that are gone.
    fn sweep(&self) {
        let world = self.supervisor.world();
        let mut stopped = Vec::new();
        let running = {
            let mut state = self.state.lock().expect("kube state");
            state.instances.retain(|key, instance| {
                let Some(project) = world.kube_project(&key.project) else {
                    if let Some(running) = instance.forward.take() {
                        running.forward.cancel.cancel();
                    }
                    return false;
                };
                if project.policy(&key.service) == Policy::Disabled
                    && let Some(running) = instance.forward.take()
                {
                    running.forward.cancel.cancel();
                    stopped.push((key.clone(), "disabled".to_string()));
                }
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
                    stopped.push((
                        key.clone(),
                        format!("idle for {}", duration::format(timeout)),
                    ));
                }
                true
            });
            state
                .project_errors
                .retain(|name, _| world.kube_project(name).is_some());
            state.instances.values().any(|i| i.forward.is_some())
        };
        self.clusters
            .lock()
            .expect("kube clusters")
            .retain(|name, _| world.kube_project(name).is_some());
        if running || !stopped.is_empty() {
            self.changed();
        }
        for (key, why) in stopped {
            self.supervisor
                .event(EventKind::Stopped, why, vec![key.subject()]);
        }
    }
}

fn instance_status(
    world: &World,
    project: &KubeProject,
    key: &Key,
    instance: Option<&Instance>,
) -> InstanceStatus {
    let running = instance.and_then(|i| i.forward.as_ref());
    let last_error = instance.and_then(|i| i.last_error.clone());
    let state = match (running, &last_error) {
        (Some(_), _) => InstanceState::Ready,
        (None, Some(_)) => InstanceState::Crashed,
        (None, None) => InstanceState::Stopped,
    };
    let mut holds = Vec::new();
    let mut idle_in_secs = None;
    if let (Some(instance), Some(_)) = (instance, running) {
        if instance.active > 0 {
            holds.push(Hold::Requests {
                count: instance.active as u32,
            });
        }
        if holds.is_empty() {
            let timeout = project.idle_timeout.unwrap_or(world.machine.idle_timeout);
            idle_in_secs = Some(
                timeout
                    .saturating_sub(instance.last_active.elapsed())
                    .as_secs(),
            );
        }
    }
    let target = running.map(|r| r.forward.target());
    InstanceStatus {
        app: key.service.clone(),
        stack: key.namespace.clone(),
        checkout: String::new(),
        checkout_path: PathBuf::new(),
        url: carte_core::kube::url(&world.machine, &key.project, &key.namespace, &key.service),
        public_url: None,
        state,
        processes: Vec::new(),
        holds,
        idle_in_secs,
        memory_bytes: 0,
        cpu_percent: 0.0,
        last_error,
        env_changed: false,
        last_event: None,
        boot: None,
        warms: Vec::new(),
        kube: Some(KubeInstanceInfo {
            pod: target.as_ref().map(|t| t.pod.clone()),
            port: target.map(|t| t.port),
        }),
    }
}

/// Sends a Service's log lines on: the ones it has, then, following, new ones from
/// its pod and then the next.
struct PodLines {
    cluster: Arc<dyn Cluster>,
    key: Key,
    port: PortInfo,
    pod: String,
    container: Option<String>,
    search: Option<String>,
    /// How many of the lines it has to send; all with `since`.
    tail: Option<usize>,
    /// How far back the lines it has go, in seconds; all of them when `None`.
    since_secs: Option<i64>,
    /// The latest timestamp of the lines read from this pod, in nanoseconds since the
    /// Unix epoch.
    last_ns: Option<u64>,
    /// Skipping the lines a reopened stream repeats, up to the first one after
    /// `last_ns`.
    catching_up: bool,
    tx: mpsc::Sender<LogLine>,
}

impl PodLines {
    async fn run(mut self, initial: LogLines, follow: bool) {
        let mut lines = Vec::new();
        let mut initial = initial;
        let mut read_any = false;
        loop {
            let next = tokio::select! {
                _ = self.tx.closed() => return,
                next = initial.next() => next,
            };
            let Some(line) = next else {
                break;
            };
            read_any = true;
            self.fresh(&line);
            lines.extend(self.line(&line));
        }
        let skip = self.tail.map_or(0, |tail| lines.len().saturating_sub(tail));
        for line in lines.into_iter().skip(skip) {
            if self.tx.send(line).await.is_err() {
                return;
            }
        }
        if !follow {
            return;
        }
        let first = match (self.last_ns, read_any) {
            (Some(_), _) => self.after_last(),
            (None, false) => LogOptions {
                since_secs: self.since_secs,
                ..LogOptions::default()
            },
            (None, true) => LogOptions {
                tail: Some(0),
                ..LogOptions::default()
            },
        };
        self.catching_up = self.last_ns.is_some();
        let mut stream = self.open(first).await;
        loop {
            if let Some(stream) = stream.take()
                && !self.send_all(stream).await
            {
                return;
            }
            let ended = Instant::now();
            let ended_ms = crate::clock::now_ms();
            while stream.is_none() {
                tokio::select! {
                    _ = self.tx.closed() => return,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                let Ok(slices) = self
                    .cluster
                    .endpoint_slices(&self.key.namespace, &self.key.service)
                    .await
                else {
                    continue;
                };
                let Ok(next) = lookup::choose_pod(&self.port, &slices, Some(&self.pod)) else {
                    continue;
                };
                let options = match self.last_ns {
                    Some(last_ns) if next.pod != self.pod => {
                        let last = (last_ns / 1_000_000_000) as i64;
                        let ended = (ended_ms / 1000) as i64 - 30;
                        LogOptions {
                            since_time: Some(last.max(ended)),
                            ..LogOptions::default()
                        }
                    }
                    Some(_) => self.after_last(),
                    None => LogOptions {
                        since_secs: Some(ended.elapsed().as_secs() as i64 + 1),
                        ..LogOptions::default()
                    },
                };
                if next.pod == self.pod {
                    self.catching_up = self.last_ns.is_some();
                } else {
                    self.pod = next.pod;
                    self.last_ns = None;
                    self.catching_up = false;
                }
                stream = self.open(options).await;
            }
        }
    }

    /// From the second of the last line read; `fresh` skips the lines up to it on the
    /// same pod.
    fn after_last(&self) -> LogOptions {
        LogOptions {
            since_time: self.last_ns.map(|ns| (ns / 1_000_000_000) as i64),
            ..LogOptions::default()
        }
    }

    async fn open(&self, options: LogOptions) -> Option<LogLines> {
        let options = LogOptions {
            container: self.container.clone(),
            follow: true,
            ..options
        };
        self.cluster
            .logs(&self.key.namespace, &self.pod, options)
            .await
            .ok()
    }

    /// Sends every new line of the stream; `false` once the receiver is gone.
    async fn send_all(&mut self, mut stream: LogLines) -> bool {
        loop {
            let next = tokio::select! {
                _ = self.tx.closed() => return false,
                next = stream.next() => next,
            };
            let Some(line) = next else {
                return true;
            };
            if !self.fresh(&line) {
                continue;
            }
            if let Some(line) = self.line(&line)
                && self.tx.send(line).await.is_err()
            {
                return false;
            }
        }
    }

    /// Whether to send the line: every line, except while catching up those at or
    /// before `last_ns`.
    fn fresh(&mut self, line: &str) -> bool {
        let at_ns = lookup::split_timestamp(line).0;
        if self.catching_up {
            if let (Some(at_ns), Some(last)) = (at_ns, self.last_ns)
                && at_ns <= last
            {
                return false;
            }
            self.catching_up = false;
        }
        if let Some(at_ns) = at_ns {
            self.last_ns = Some(self.last_ns.map_or(at_ns, |last| last.max(at_ns)));
        }
        true
    }

    /// The log line for a line of output, unless the search leaves it out.
    fn line(&self, line: &str) -> Option<LogLine> {
        let (at_ns, text) = lookup::split_timestamp(line);
        if self
            .search
            .as_ref()
            .is_some_and(|s| !text.contains(s.as_str()))
        {
            return None;
        }
        Some(LogLine {
            project: self.key.project.clone(),
            app: self.key.service.clone(),
            stack: self.key.namespace.clone(),
            process: self.container.clone().unwrap_or_default(),
            at_ms: at_ns.map_or_else(crate::clock::now_ms, |ns| ns / 1_000_000),
            text: text.to_string(),
            marker: false,
        })
    }
}

/// The Services of each namespace, skipping namespaces the cluster won't list, unless
/// it lists none of them.
async fn each_namespace(
    cluster: &dyn Cluster,
    namespaces: &[String],
) -> Result<Vec<ServiceInfo>, ApiError> {
    read_each_namespace(namespaces, |namespace| cluster.services(Some(namespace)))
        .await
        .map(|readable| readable.found)
}

/// When each kubeconfig file of the project last changed.
fn kubeconfig_stamps(
    project: &KubeProject,
    env: &BTreeMap<String, String>,
) -> Vec<Option<SystemTime>> {
    cluster::kubeconfig_paths(project, env)
        .into_iter()
        .map(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
        .collect()
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
        if changed {
            inner.changed();
        }
        if let (true, Some(message)) = (changed, error) {
            inner
                .supervisor
                .event(EventKind::Failed, message, vec![self.key.subject()]);
        }
    }
}

/// Opens connections to a Service's pod for the proxy.
struct ForwardDial {
    forward: Arc<Forward>,
    inner: Weak<Inner>,
    key: Key,
}

impl Dial for ForwardDial {
    fn dial(&self) -> BoxFuture<'_, std::io::Result<UpstreamStream>> {
        Box::pin(async move {
            let stream = self.forward.dial().await?;
            Ok(Box::new(stream) as UpstreamStream)
        })
    }

    /// The page for a request the forward couldn't serve, with what went wrong last.
    fn failed(&self, error: &UpstreamError) -> ErrorPage {
        let last_error = self.inner.upgrade().and_then(|inner| {
            let state = inner.state.lock().expect("kube state");
            let instance = state.instances.get(&self.key)?;
            let running = instance.forward.as_ref()?;
            Arc::ptr_eq(&running.forward, &self.forward).then(|| instance.last_error.clone())
        });
        let (title, detail) = match last_error {
            Some(last_error) => (
                format!("{} didn't answer", self.key.service),
                last_error.unwrap_or_else(|| match error {
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
