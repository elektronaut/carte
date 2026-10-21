//! The daemon: supervisor, proxy, DNS and the API socket, until a signal stops it.

mod api;
mod launchd;
mod notifications;
mod router;
pub mod tunnel;
pub mod watch;

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use carte_core::discovery;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::dns::{DnsServer, Responder};
use crate::kube::{Kube, KubeConnector};
use crate::paths::Paths;
use crate::process::Spawner;
use crate::proxy::{ListenerOptions, Proxy};
use crate::supervisor::{self, Options, groups};
use crate::tls::{self, CaOrigin, CertResolver, CertificateAuthority, CreateReason};

pub async fn run(paths: Paths) -> anyhow::Result<()> {
    std::fs::create_dir_all(&paths.state_dir)
        .with_context(|| format!("creating {}", paths.state_dir.display()))?;
    let socket = paths.socket();
    if UnixStream::connect(&socket).await.is_ok() {
        bail!("a carte daemon is already running on {}", socket.display());
    }
    let _ = std::fs::remove_file(&socket);

    match groups::kill_leftovers(&paths.run_dir().join("groups")) {
        Ok(0) => {}
        Ok(n) => warn!("killed {n} process groups left behind by a previous daemon"),
        Err(err) => warn!("couldn't read the previous daemon's process groups: {err}"),
    }

    let world = discovery::load_world(&paths.config_dir);
    for problem in &world.problems {
        warn!(project = ?problem.project, app = ?problem.app, "{}", problem.message);
    }
    let machine = world.machine.clone();

    let tlds = machine.served_tlds();
    let ca = CertificateAuthority::load_or_create(
        &paths.state_dir,
        &tls::ca::default_common_name(),
        &tlds,
    )
    .context("loading the local CA")?;
    if let CaOrigin::Created(reason) = ca.origin() {
        let why = match reason {
            CreateReason::Missing => "none existed".to_string(),
            CreateReason::Invalid(err) => format!("the old one was invalid: {err}"),
            CreateReason::Expiring => "the old one was about to expire".to_string(),
            CreateReason::TldsChanged { previous } => {
                format!("the TLD changed from {}", previous.join(", "))
            }
        };
        warn!("created a new local CA because {why}; run `carte setup` to trust it");
    }
    if let Err(err) = tls::bundle::rebuild(&paths.state_dir, &ca) {
        warn!("couldn't build the CA bundle for apps: {err}");
    }
    let tls_config = Arc::new(tls::server_config(Arc::new(CertResolver::new(Arc::new(
        ca,
    ))))?);

    let options = Options::new(&paths);
    let hold = options.boot_timeout + Duration::from_secs(10);
    let supervisor = supervisor::start(world, paths.clone(), Spawner::current()?, options);

    let loopback = |port| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let loopbacks = |port| {
        [
            loopback(port),
            SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        ]
    };
    let dns = DnsServer::bind(loopback(machine.dns_port), Responder::new(&tlds)?)
        .await
        .with_context(|| format!("binding DNS on {}", loopback(machine.dns_port)))?;
    tokio::spawn(dns.run());

    let kube = Kube::new(
        supervisor.clone(),
        Arc::new(KubeConnector),
        Duration::from_secs(15),
        crate::kube::LOOKUP_TIMEOUT,
    );
    let proxy = Proxy::new(Arc::new(router::DaemonRouter {
        supervisor: router::SupervisorRouter {
            supervisor: supervisor.clone(),
            hold,
        },
        kube: kube.clone(),
    }));
    let bind_hint = |port| {
        format!(
            "binding {} (ports below 1024 need `carte setup`; for a trial, set http_port and https_port in {})",
            loopback(port),
            paths.machine_config().display()
        )
    };
    let http = launchd::proxy_listeners(
        &proxy,
        "http",
        &loopbacks(machine.http_port),
        ListenerOptions::local(),
    )
    .await
    .with_context(|| bind_hint(machine.http_port))?;
    let https = launchd::proxy_listeners(
        &proxy,
        "https",
        &loopbacks(machine.https_port),
        ListenerOptions::local().tls(tls_config),
    )
    .await
    .with_context(|| bind_hint(machine.https_port))?;
    let mut share_listener = Vec::new();
    if let Some(share) = &machine.share {
        let listener = proxy
            .bind(
                loopback(share.port),
                ListenerOptions::share(share.domain.clone()),
            )
            .await
            .with_context(|| format!("binding the share listener on {}", loopback(share.port)))?;
        share_listener.push(listener);
        tokio::spawn(tunnel::run(
            share.clone(),
            supervisor.clone(),
            Arc::new(Spawner::current()?),
            paths.state_dir.clone(),
        ));
    }
    let shutdown = CancellationToken::new();
    let proxy_tasks: Vec<_> = http
        .into_iter()
        .chain(https)
        .chain(share_listener)
        .map(|listener| tokio::spawn(listener.serve(shutdown.clone())))
        .collect();

    tokio::spawn(notifications::run(supervisor.clone()));
    if let Some(watching) = watch::run(
        paths.config_dir.clone(),
        supervisor.clone(),
        Duration::from_secs(60),
    ) {
        tokio::spawn(watching);
    }
    let listener = bind_api(&socket).with_context(|| format!("binding {}", socket.display()))?;
    let reload_supervisor = supervisor.clone();
    let config_dir = paths.config_dir.clone();
    let api = api::Api {
        supervisor: supervisor.clone(),
        kube,
        paths: paths.clone(),
        reload: Arc::new(move || {
            let world = discovery::load_world(&config_dir);
            let problems = world
                .problems
                .iter()
                .map(|p| carte_api::Problem {
                    project: p.project.clone(),
                    app: p.app.clone(),
                    message: p.message.clone(),
                })
                .collect();
            reload_supervisor.reload(world);
            problems
        }),
    };
    let api_shutdown = shutdown.clone();
    let api_task = tokio::spawn(async move {
        axum::serve(listener, api::router(api))
            .with_graceful_shutdown(api_shutdown.cancelled_owned())
            .await
    });

    info!(
        "carte is running: http {}, https {}, dns {}, tld .{}",
        machine.http_port, machine.https_port, machine.dns_port, machine.tld
    );

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => info!("stopping on SIGTERM"),
        _ = int.recv() => info!("stopping on SIGINT"),
    }

    let _ = std::fs::remove_file(&socket);
    shutdown.cancel();
    let grace = Options::new(&paths).grace * 2 + Duration::from_secs(5);
    if tokio::time::timeout(grace, supervisor.shutdown())
        .await
        .is_err()
    {
        warn!("some processes didn't stop in time");
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        for task in proxy_tasks {
            let _ = task.await;
        }
        let _ = api_task.await;
    })
    .await;
    Ok(())
}

/// Binds the API socket under a temporary name and moves it into place once it
/// listens, so the socket doesn't appear before the daemon accepts connections.
/// The daemon removes it as soon as it starts stopping.
fn bind_api(socket: &Path) -> std::io::Result<UnixListener> {
    let mut pending = socket.as_os_str().to_owned();
    pending.push(".new");
    let pending = PathBuf::from(pending);
    let _ = std::fs::remove_file(&pending);
    let listener = UnixListener::bind(&pending)?;
    std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&pending, socket)?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_api_socket_appears_listening_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("carte.sock");
        let _listener = bind_api(&socket).unwrap();
        UnixStream::connect(&socket).await.unwrap();
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!dir.path().join("carte.sock.new").exists());
    }
}
