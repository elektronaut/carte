//! Talking to a cluster: Services, EndpointSlices and port-forward streams.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use carte_core::kube::KubeProject;
use futures_util::AsyncBufReadExt;
use futures_util::stream::{BoxStream, StreamExt};
use k8s_openapi::api::core::v1::{Namespace, Pod, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::{ListParams, LogParams};
use kube::config::{ExecInteractiveMode, KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

use super::lookup::{ContainerInfo, EndpointInfo, PortInfo, ServiceInfo, SliceInfo};
use crate::proxy::BoxFuture;

pub trait PodIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> PodIo for T {}

/// A byte stream to a pod's port.
pub type PodStream = Box<dyn PodIo>;

/// What carte asks of a cluster; tests substitute a fake.
pub trait Cluster: Send + Sync + 'static {
    fn service<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ServiceInfo>, ApiError>>;

    /// Services named `name` in `namespaces`, or in every namespace when it's empty.
    /// Namespaces the cluster won't read are skipped, unless it reads none of them.
    fn services_named<'a>(
        &'a self,
        name: &'a str,
        namespaces: &'a [String],
    ) -> BoxFuture<'a, Result<Readable<ServiceInfo>, ApiError>>;

    fn endpoint_slices<'a>(
        &'a self,
        namespace: &'a str,
        service: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SliceInfo>, ApiError>>;

    /// The Services of one namespace, or of every namespace when `None`.
    fn services<'a>(
        &'a self,
        namespace: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<ServiceInfo>, ApiError>>;

    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<String>, ApiError>>;

    fn containers<'a>(
        &'a self,
        namespace: &'a str,
        pod: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ContainerInfo>, ApiError>>;

    /// A pod's log lines, each starting with its RFC 3339 timestamp.
    fn logs<'a>(
        &'a self,
        namespace: &'a str,
        pod: &'a str,
        options: LogOptions,
    ) -> BoxFuture<'a, Result<LogLines, ApiError>>;

    /// Opens a port-forward stream to `port` of the pod.
    fn connect<'a>(
        &'a self,
        namespace: &'a str,
        pod: &'a str,
        port: u16,
    ) -> BoxFuture<'a, Result<PodStream, ApiError>>;
}

/// What a read of several namespaces found, and the namespaces the cluster refused.
#[derive(Debug, Clone, PartialEq)]
pub struct Readable<T> {
    pub found: Vec<T>,
    pub refused: Vec<String>,
}

/// Reads each namespace in turn, skipping the ones the cluster refuses, unless it
/// refuses all of them. Any other error ends the read.
pub async fn read_each_namespace<'a, T>(
    namespaces: &'a [String],
    mut read: impl FnMut(&'a str) -> BoxFuture<'a, Result<Vec<T>, ApiError>>,
) -> Result<Readable<T>, ApiError> {
    let mut readable = Readable {
        found: Vec::new(),
        refused: Vec::new(),
    };
    let mut forbidden = None;
    for namespace in namespaces {
        match read(namespace).await {
            Ok(found) => readable.found.extend(found),
            Err(ApiError::Forbidden(message)) => {
                readable.refused.push(namespace.clone());
                forbidden = Some(message);
            }
            Err(err) => return Err(err),
        }
    }
    match forbidden {
        Some(message) if readable.refused.len() == namespaces.len() => {
            Err(ApiError::Forbidden(message))
        }
        _ => Ok(readable),
    }
}

/// Which of a container's log lines to read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogOptions {
    pub container: Option<String>,
    pub follow: bool,
    pub tail: Option<i64>,
    pub since_secs: Option<i64>,
}

/// Log lines, invalid UTF-8 replaced; the stream ends with the log or an error.
pub type LogLines = BoxStream<'static, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The kubeconfig's login failed, like an expired SSO session.
    Auth {
        message: String,
        hint: Option<String>,
    },
    Forbidden(String),
    NotFound(String),
    Other(String),
}

impl ApiError {
    pub fn title(&self) -> &'static str {
        match self {
            ApiError::Auth { .. } => "Login to the cluster failed",
            ApiError::Forbidden(_) => "The cluster refused",
            ApiError::NotFound(_) => "Not found in the cluster",
            ApiError::Other(_) => "Couldn't reach the cluster",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            ApiError::Auth {
                message,
                hint: Some(hint),
            } => format!("{hint}\n\n{message}"),
            ApiError::Auth {
                message,
                hint: None,
            }
            | ApiError::Forbidden(message)
            | ApiError::NotFound(message)
            | ApiError::Other(message) => message.clone(),
        }
    }

    /// A one-line summary for status lines.
    pub fn summary(&self) -> String {
        match self {
            ApiError::Auth {
                hint: Some(hint), ..
            } => format!("login failed: {hint}"),
            ApiError::Auth { message, .. } => format!(
                "login failed: {}",
                message.lines().next().unwrap_or_default()
            ),
            other => other
                .detail()
                .lines()
                .next()
                .unwrap_or_default()
                .to_string(),
        }
    }

    fn from_kube(err: kube::Error, plugins: &[String]) -> Self {
        if let kube::Error::Api(status) = &err {
            return match status.code {
                401 => ApiError::Auth {
                    message: status.message.clone(),
                    hint: None,
                },
                403 => ApiError::Forbidden(status.message.clone()),
                404 => ApiError::NotFound(status.message.clone()),
                _ => ApiError::Other(status.message.clone()),
            };
        }
        if let kube::Error::UpgradeConnection(
            kube::client::UpgradeConnectionError::ProtocolSwitch(code),
        ) = &err
        {
            return match code.as_u16() {
                401 => ApiError::Auth {
                    message: err.to_string(),
                    hint: None,
                },
                403 => ApiError::Forbidden(
                    "port-forwarding needs the pods/portforward permission".into(),
                ),
                404 => ApiError::NotFound(err.to_string()),
                _ => ApiError::Other(err.to_string()),
            };
        }
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&err);
        while let Some(current) = source {
            if let Some(auth) = current.downcast_ref::<kube::client::AuthError>() {
                return auth_error(auth, plugins);
            }
            source = current.source();
        }
        ApiError::Other(err.to_string())
    }
}

fn auth_error(err: &kube::client::AuthError, plugins: &[String]) -> ApiError {
    let message = match err {
        kube::client::AuthError::AuthExecRun { cmd, status, out } => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            format!("{cmd} exited with {status}:\n{}", stderr.trim())
        }
        other => other.to_string(),
    };
    ApiError::Auth {
        hint: login_hint(plugins),
        message,
    }
}

/// What to run to log in again, for the login plugins carte knows.
fn login_hint(plugins: &[String]) -> Option<String> {
    plugins.iter().find_map(|plugin| {
        let name = Path::new(plugin).file_name()?.to_string_lossy();
        match name.as_ref() {
            "aws" | "aws-iam-authenticator" => Some("Run `aws sso login`, then reload.".into()),
            "gke-gcloud-auth-plugin" | "gcloud" => {
                Some("Run `gcloud auth login`, then reload.".into())
            }
            "kubelogin" => Some("Run `az login`, then reload.".into()),
            _ => None,
        }
    })
}

/// A cluster reached through kube-rs.
pub struct KubeCluster {
    client: Client,
    /// The login plugins of the kubeconfig, for hints when a login fails.
    plugins: Vec<String>,
}

impl KubeCluster {
    /// Builds a client for the project's context in `kubeconfig`. Login plugins run
    /// with `env`, the project's env provider's environment, since the daemon's own
    /// `PATH` rarely reaches them.
    pub async fn connect(
        project: &KubeProject,
        mut kubeconfig: Kubeconfig,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, ApiError> {
        let plugins = prepare_plugins(&mut kubeconfig, env);
        let options = KubeConfigOptions {
            context: Some(project.context.clone()),
            ..KubeConfigOptions::default()
        };
        let config = Config::from_custom_kubeconfig(kubeconfig, &options)
            .await
            .map_err(|err| match err {
                kube::config::KubeconfigError::LoadContext(_) => ApiError::Other(format!(
                    "the kubeconfig has no context {:?}",
                    project.context
                )),
                other => ApiError::Other(other.to_string()),
            })?;
        let client = tokio::task::spawn_blocking(move || Client::try_from(config))
            .await
            .map_err(|err| ApiError::Other(err.to_string()))?
            .map_err(|err| ApiError::from_kube(err, &plugins))?;
        Ok(KubeCluster { client, plugins })
    }

    fn error(&self, err: kube::Error) -> ApiError {
        ApiError::from_kube(err, &self.plugins)
    }
}

/// The project's kubeconfig, else the files in `KUBECONFIG`, else `~/.kube/config`.
/// Missing files in `KUBECONFIG` are skipped, as kubectl does, unless all are missing.
pub fn kubeconfig_paths(project: &KubeProject, env: &BTreeMap<String, String>) -> Vec<PathBuf> {
    match (&project.kubeconfig, env.get("KUBECONFIG")) {
        (Some(path), _) => vec![path.clone()],
        (None, Some(list)) if !list.is_empty() => {
            let listed: Vec<PathBuf> = std::env::split_paths(list)
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
            let present: Vec<PathBuf> = listed.iter().filter(|p| p.exists()).cloned().collect();
            if present.is_empty() { listed } else { present }
        }
        _ => {
            let home = env
                .get("HOME")
                .cloned()
                .or_else(|| std::env::var("HOME").ok())
                .unwrap_or_default();
            vec![Path::new(&home).join(".kube/config")]
        }
    }
}

/// The files of [`kubeconfig_paths`], merged.
pub fn read_kubeconfig(
    project: &KubeProject,
    env: &BTreeMap<String, String>,
) -> Result<Kubeconfig, ApiError> {
    let mut merged: Option<Kubeconfig> = None;
    for path in kubeconfig_paths(project, env) {
        let config =
            Kubeconfig::read_from(&path).map_err(|err| ApiError::Other(err.to_string()))?;
        merged = Some(match merged {
            Some(base) => base
                .merge(config)
                .map_err(|err| ApiError::Other(err.to_string()))?,
            None => config,
        });
    }
    merged.ok_or_else(|| ApiError::Other("no kubeconfig file".into()))
}

/// A digest of what `context` resolves to: its entry, its cluster's and its user's,
/// with the env its login plugin runs with. Other contexts, `current-context` and,
/// without a login plugin, the env leave it be.
pub fn context_digest(
    kubeconfig: &Kubeconfig,
    context: &str,
    env: &BTreeMap<String, String>,
) -> Result<[u8; 32], ApiError> {
    let mut kubeconfig = kubeconfig.clone();
    prepare_plugins(&mut kubeconfig, env);
    let missing = || ApiError::Other(format!("the kubeconfig has no context {context:?}"));
    let named = kubeconfig
        .contexts
        .iter()
        .find(|c| c.name == context)
        .ok_or_else(missing)?;
    let entry = named.context.as_ref().ok_or_else(missing)?;
    let cluster = kubeconfig.clusters.iter().find(|c| c.name == entry.cluster);
    let user = entry
        .user
        .as_ref()
        .and_then(|user| kubeconfig.auth_infos.iter().find(|a| a.name == *user));
    let json = serde_json::to_vec(&(named, cluster, user))
        .map_err(|err| ApiError::Other(err.to_string()))?;
    Ok(Sha256::digest(json).into())
}

/// Makes login plugins run without a terminal, so their errors are captured, with the
/// provider's env under the kubeconfig's own, and found on the provider's `PATH`.
/// Returns the plugins' commands.
fn prepare_plugins(kubeconfig: &mut Kubeconfig, env: &BTreeMap<String, String>) -> Vec<String> {
    let search: Vec<PathBuf> = env
        .get("PATH")
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    let mut plugins = Vec::new();
    for named in &mut kubeconfig.auth_infos {
        let Some(exec) = named.auth_info.as_mut().and_then(|a| a.exec.as_mut()) else {
            continue;
        };
        exec.interactive_mode = Some(ExecInteractiveMode::Never);
        let own = exec.env.take().unwrap_or_default();
        let mut vars: Vec<HashMap<String, String>> = env
            .iter()
            .map(|(name, value)| {
                HashMap::from([
                    ("name".into(), name.clone()),
                    ("value".into(), value.clone()),
                ])
            })
            .collect();
        vars.extend(own);
        exec.env = Some(vars);
        if let Some(command) = exec.command.as_mut() {
            if !command.contains('/')
                && let Some(found) = search
                    .iter()
                    .map(|dir| dir.join(&*command))
                    .find(|path| path.is_file())
            {
                *command = found.to_string_lossy().into_owned();
            }
            plugins.push(command.clone());
        }
    }
    plugins
}

fn service_info(service: Service) -> ServiceInfo {
    let spec = service.spec.unwrap_or_default();
    ServiceInfo {
        namespace: service.metadata.namespace.unwrap_or_default(),
        name: service.metadata.name.unwrap_or_default(),
        external: spec.type_.as_deref() == Some("ExternalName"),
        ports: spec
            .ports
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.protocol.as_deref().unwrap_or("TCP") == "TCP")
            .filter_map(|p| {
                Some(PortInfo {
                    name: p.name.filter(|n| !n.is_empty()),
                    port: u16::try_from(p.port).ok()?,
                    app_protocol: p.app_protocol,
                })
            })
            .collect(),
    }
}

fn slice_info(slice: EndpointSlice) -> SliceInfo {
    SliceInfo {
        ports: slice
            .ports
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.protocol.as_deref().unwrap_or("TCP") == "TCP")
            .filter_map(|p| {
                Some(PortInfo {
                    name: p.name.filter(|n| !n.is_empty()),
                    port: u16::try_from(p.port?).ok()?,
                    app_protocol: p.app_protocol,
                })
            })
            .collect(),
        endpoints: slice
            .endpoints
            .into_iter()
            .map(|e| EndpointInfo {
                pod: e
                    .target_ref
                    .filter(|r| r.kind.as_deref() == Some("Pod"))
                    .and_then(|r| r.name),
                ready: e.conditions.and_then(|c| c.ready).unwrap_or(true),
            })
            .collect(),
    }
}

impl Cluster for KubeCluster {
    fn service<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ServiceInfo>, ApiError>> {
        Box::pin(async move {
            let api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
            match api.get_opt(name).await {
                Ok(service) => Ok(service.map(service_info)),
                Err(err) => Err(self.error(err)),
            }
        })
    }

    fn services_named<'a>(
        &'a self,
        name: &'a str,
        namespaces: &'a [String],
    ) -> BoxFuture<'a, Result<Readable<ServiceInfo>, ApiError>> {
        Box::pin(async move {
            if namespaces.is_empty() {
                let api: Api<Service> = Api::all(self.client.clone());
                let params = ListParams::default().fields(&format!("metadata.name={name}"));
                let list = api.list(&params).await.map_err(|err| self.error(err))?;
                return Ok(Readable {
                    found: list.items.into_iter().map(service_info).collect(),
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
            let api: Api<EndpointSlice> = Api::namespaced(self.client.clone(), namespace);
            let params =
                ListParams::default().labels(&format!("kubernetes.io/service-name={service}"));
            let list = api.list(&params).await.map_err(|err| self.error(err))?;
            Ok(list.items.into_iter().map(slice_info).collect())
        })
    }

    fn services<'a>(
        &'a self,
        namespace: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<ServiceInfo>, ApiError>> {
        Box::pin(async move {
            let api: Api<Service> = match namespace {
                Some(namespace) => Api::namespaced(self.client.clone(), namespace),
                None => Api::all(self.client.clone()),
            };
            let list = api
                .list(&ListParams::default())
                .await
                .map_err(|err| self.error(err))?;
            Ok(list.items.into_iter().map(service_info).collect())
        })
    }

    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<String>, ApiError>> {
        Box::pin(async move {
            let api: Api<Namespace> = Api::all(self.client.clone());
            let list = api
                .list(&ListParams::default())
                .await
                .map_err(|err| self.error(err))?;
            Ok(list
                .items
                .into_iter()
                .filter_map(|n| n.metadata.name)
                .collect())
        })
    }

    fn containers<'a>(
        &'a self,
        namespace: &'a str,
        pod: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ContainerInfo>, ApiError>> {
        Box::pin(async move {
            let api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
            let pod = api.get(pod).await.map_err(|err| self.error(err))?;
            Ok(pod
                .spec
                .map(|s| s.containers)
                .unwrap_or_default()
                .into_iter()
                .map(|c| ContainerInfo {
                    ports: c
                        .ports
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|p| u16::try_from(p.container_port).ok())
                        .collect(),
                    name: c.name,
                })
                .collect())
        })
    }

    fn logs<'a>(
        &'a self,
        namespace: &'a str,
        pod: &'a str,
        options: LogOptions,
    ) -> BoxFuture<'a, Result<LogLines, ApiError>> {
        Box::pin(async move {
            let api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
            let params = LogParams {
                container: options.container,
                follow: options.follow,
                tail_lines: options.tail,
                since_seconds: options.since_secs,
                timestamps: true,
                ..LogParams::default()
            };
            let reader =
                api.log_stream(pod, &params)
                    .await
                    .map_err(|err| match self.error(err) {
                        ApiError::Forbidden(_) => {
                            ApiError::Forbidden("reading logs needs the pods/log permission".into())
                        }
                        other => other,
                    })?;
            let lines = futures_util::stream::unfold(Box::pin(reader), |mut reader| async move {
                let mut line = Vec::new();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => None,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line);
                        Some((text.trim_end_matches(['\n', '\r']).to_string(), reader))
                    }
                }
            });
            Ok(lines.boxed())
        })
    }

    fn connect<'a>(
        &'a self,
        namespace: &'a str,
        pod: &'a str,
        port: u16,
    ) -> BoxFuture<'a, Result<PodStream, ApiError>> {
        Box::pin(async move {
            let api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
            let mut forwarder = api
                .portforward(pod, &[port])
                .await
                .map_err(|err| self.error(err))?;
            let stream = forwarder
                .take_stream(port)
                .ok_or_else(|| ApiError::Other("the port-forward has no stream".into()))?;
            if let Some(error) = forwarder.take_error(port) {
                let pod = pod.to_string();
                tokio::spawn(async move {
                    if let Some(message) = error.await {
                        tracing::debug!(pod, "port-forward error: {message}");
                    }
                });
            }
            tokio::spawn(async move {
                if let Err(err) = forwarder.join().await {
                    tracing::debug!("port-forward ended: {err}");
                }
            });
            Ok(Box::new(stream) as PodStream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KUBECONFIG: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: prod
  cluster:
    server: https://127.0.0.1:6443
contexts:
- name: prod
  context:
    cluster: prod
    user: prod
users:
- name: prod
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: aws
      args: ["eks", "get-token"]
      env:
      - name: AWS_PROFILE
        value: prod
"#;

    #[test]
    fn login_plugins_get_the_provider_env_and_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("aws"), "#!/bin/sh\n").unwrap();
        let mut kubeconfig = Kubeconfig::from_yaml(KUBECONFIG).unwrap();
        let env = BTreeMap::from([
            ("PATH".to_string(), format!("/nowhere:{}", bin.display())),
            ("AWS_PROFILE".to_string(), "default".to_string()),
        ]);
        let plugins = prepare_plugins(&mut kubeconfig, &env);
        let aws = bin.join("aws").to_string_lossy().into_owned();
        assert_eq!(plugins, vec![aws.clone()]);
        let exec = kubeconfig.auth_infos[0]
            .auth_info
            .as_ref()
            .unwrap()
            .exec
            .as_ref()
            .unwrap();
        assert_eq!(exec.command.as_deref(), Some(aws.as_str()));
        assert_eq!(exec.interactive_mode, Some(ExecInteractiveMode::Never));
        let vars: Vec<(String, String)> = exec
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|v| (v["name"].clone(), v["value"].clone()))
            .collect();
        assert_eq!(
            vars.last(),
            Some(&("AWS_PROFILE".to_string(), "prod".to_string())),
            "the kubeconfig's own env comes last and wins"
        );
        assert!(vars.iter().any(|(k, _)| k == "PATH"));
    }

    #[test]
    fn the_env_counts_for_a_context_with_a_login_plugin() {
        let mut kubeconfig = Kubeconfig::from_yaml(KUBECONFIG).unwrap();
        let env = |value: &str| BTreeMap::from([("DIRENV_DIFF".to_string(), value.to_string())]);
        let digest = |kubeconfig: &Kubeconfig, value: &str| {
            context_digest(kubeconfig, "prod", &env(value)).unwrap()
        };
        assert_ne!(digest(&kubeconfig, "a"), digest(&kubeconfig, "b"));
        let user = kubeconfig.auth_infos[0].auth_info.as_mut().unwrap();
        user.exec = None;
        user.token = Some("secret".to_string().into());
        assert_eq!(digest(&kubeconfig, "a"), digest(&kubeconfig, "b"));
    }

    #[test]
    fn hints_for_known_plugins() {
        assert_eq!(
            login_hint(&["/opt/homebrew/bin/aws".into()]).as_deref(),
            Some("Run `aws sso login`, then reload.")
        );
        assert_eq!(
            login_hint(&["gke-gcloud-auth-plugin".into()]).as_deref(),
            Some("Run `gcloud auth login`, then reload.")
        );
        assert_eq!(login_hint(&["/usr/local/bin/other".into()]), None);
    }

    #[test]
    fn reads_the_project_kubeconfig() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config");
        std::fs::write(&path, KUBECONFIG).unwrap();
        let project = KubeProject {
            name: "prod".into(),
            config_path: tmp.path().join("prod.toml"),
            folder: None,
            context: "prod".into(),
            kubeconfig: Some(path),
            namespaces: Vec::new(),
            env_provider: None,
            idle_timeout: None,
            services: BTreeMap::new(),
        };
        let kubeconfig = read_kubeconfig(&project, &BTreeMap::new()).unwrap();
        assert_eq!(kubeconfig.contexts[0].name, "prod");
        let missing = KubeProject {
            kubeconfig: Some(tmp.path().join("missing")),
            ..project
        };
        assert!(matches!(
            read_kubeconfig(&missing, &BTreeMap::new()),
            Err(ApiError::Other(_))
        ));
    }

    #[test]
    fn skips_missing_files_in_kubeconfig_env() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config");
        std::fs::write(&path, KUBECONFIG).unwrap();
        let missing = tmp.path().join("missing");
        let project = KubeProject {
            name: "prod".into(),
            config_path: tmp.path().join("prod.toml"),
            folder: None,
            context: "prod".into(),
            kubeconfig: None,
            namespaces: Vec::new(),
            env_provider: None,
            idle_timeout: None,
            services: BTreeMap::new(),
        };
        let list = std::env::join_paths([&missing, &path]).unwrap();
        let env = BTreeMap::from([(
            "KUBECONFIG".to_string(),
            list.to_string_lossy().into_owned(),
        )]);
        let kubeconfig = read_kubeconfig(&project, &env).unwrap();
        assert_eq!(kubeconfig.contexts[0].name, "prod");
        let env = BTreeMap::from([(
            "KUBECONFIG".to_string(),
            missing.to_string_lossy().into_owned(),
        )]);
        assert!(matches!(
            read_kubeconfig(&project, &env),
            Err(ApiError::Other(_))
        ));
    }

    fn namespaces(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn read(namespace: &str) -> BoxFuture<'_, Result<Vec<String>, ApiError>> {
        Box::pin(async move {
            match namespace {
                "secret" | "private" => {
                    Err(ApiError::Forbidden(format!("{namespace} is forbidden")))
                }
                "broken" => Err(ApiError::Other("connection refused".into())),
                _ => Ok(vec![format!("web.{namespace}")]),
            }
        })
    }

    #[tokio::test]
    async fn reading_each_namespace_skips_refusals() {
        let listed = namespaces(&["blue", "secret", "green"]);
        let readable = read_each_namespace(&listed, read).await.unwrap();
        assert_eq!(readable.found, vec!["web.blue", "web.green"]);
        assert_eq!(readable.refused, vec!["secret"]);
    }

    #[tokio::test]
    async fn reading_each_namespace_fails_when_all_refuse() {
        let listed = namespaces(&["secret", "private"]);
        assert!(matches!(
            read_each_namespace(&listed, read).await,
            Err(ApiError::Forbidden(message)) if message == "private is forbidden"
        ));
    }

    #[tokio::test]
    async fn reading_each_namespace_stops_at_other_errors() {
        let listed = namespaces(&["blue", "broken", "green"]);
        assert!(matches!(
            read_each_namespace(&listed, read).await,
            Err(ApiError::Other(_))
        ));
    }
}
