//! Routes proxied requests to instances, booting them through the supervisor.

use std::time::Duration;

use carte_api::{Credentials, ExternalRequest, ProcessState, ProcessStatus, Snapshot};
use carte_core::kube::{self, KubeTarget};
use carte_core::model::{InstanceKey, World, path_match};
use carte_core::naming;
use carte_core::procfile::ProcessSource;
use http::StatusCode;

use crate::kube::Kube;
use crate::proxy::{
    ActivityGuard, BoxFuture, ErrorPage, ListenerKind, Route, RouteRequest, Router, UpstreamError,
};
use crate::supervisor::{Activity, Handle, share};

/// Sends kube projects' hosts to their forwards and everything else to the
/// supervisor. Kube projects are only reachable from the loopback listeners.
pub struct DaemonRouter {
    pub supervisor: SupervisorRouter,
    pub kube: Kube,
}

impl DaemonRouter {
    fn kube_target(
        &self,
        request: &RouteRequest<'_>,
    ) -> Option<Result<KubeTarget, carte_core::naming::NotFound>> {
        if request.kind != ListenerKind::Local {
            return None;
        }
        let world = self.supervisor.supervisor.world();
        kube::resolve_host(&world, request.host)
    }
}

impl Router for DaemonRouter {
    fn route<'a>(&'a self, request: RouteRequest<'a>) -> BoxFuture<'a, Route> {
        let Some(target) = self.kube_target(&request) else {
            return self.supervisor.route(request);
        };
        Box::pin(async move {
            let target = match target {
                Ok(target) => target,
                Err(not_found) => {
                    return Route::Error(ErrorPage::new(
                        StatusCode::NOT_FOUND,
                        "No app here",
                        not_found.to_string(),
                    ));
                }
            };
            let world = self.supervisor.supervisor.world();
            if let Some(from) = foreign_origin(&world, &request) {
                return Route::Error(ErrorPage::new(
                    StatusCode::FORBIDDEN,
                    "Refused",
                    format!(
                        "{from} asked for {}. carte only forwards requests to a cluster that you open yourself or that come from local pages, so other sites can't reach it with your credentials.",
                        target.service
                    ),
                ));
            }
            self.kube.route(target).await
        })
    }

    fn upstream_failed(
        &self,
        request: RouteRequest<'_>,
        port: u16,
        error: &UpstreamError,
    ) -> ErrorPage {
        self.supervisor.upstream_failed(request, port, error)
    }

    fn closing(&self, kind: ListenerKind) -> Option<BoxFuture<'static, ()>> {
        self.supervisor.closing(kind)
    }
}

/// Who sent a request that another site made, as a phrase for the error page; `None`
/// for opening a page yourself (typed, bookmarked, or a link you followed) and for
/// requests from local pages, by their `Origin`, or their `Referer` when the `Origin`
/// is hidden. A frame on another site, or a page it has the browser prefetch or
/// prerender, is a request that site made.
fn foreign_origin(world: &World, request: &RouteRequest<'_>) -> Option<String> {
    let navigation = matches!(*request.method, http::Method::GET | http::Method::HEAD)
        && request.fetch_mode == Some("navigate")
        && request.fetch_dest == Some("document")
        && !request.purpose.is_some_and(|p| p.contains("prefetch"));
    if navigation {
        return None;
    }
    match request.referrer {
        Some(host) if is_local_host(world, host) => None,
        Some(host) => Some(format!("A page on {host}")),
        None if request.opaque_origin => Some("A page that hides its origin".into()),
        None if request.fetch_site == Some("cross-site") => Some("Another site".into()),
        None => None,
    }
}

/// A host under one of the served TLDs, or a loopback address.
fn is_local_host(world: &World, host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => host.split(':').next().unwrap_or(host),
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if name
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
    {
        return true;
    }
    world
        .machine
        .served_tlds()
        .iter()
        .any(|tld| name == *tld || name.ends_with(&format!(".{tld}")))
}

pub struct SupervisorRouter {
    pub supervisor: Handle,
    /// How long a request may wait for a boot before it gets an error page.
    pub hold: Duration,
}

impl Router for SupervisorRouter {
    fn route<'a>(&'a self, request: RouteRequest<'a>) -> BoxFuture<'a, Route> {
        Box::pin(async move {
            let world = self.supervisor.world();
            let mut referrer = None;
            let mut foreign = None;
            let mut checked_authorization = false;
            let mut shared = None;
            let key = if request.kind == ListenerKind::Share {
                match self.shared_key(&world, request) {
                    Ok((key, credentials)) => {
                        checked_authorization = credentials.is_some();
                        shared = Some(credentials);
                        key
                    }
                    Err(page) => return Route::Error(page),
                }
            } else {
                foreign = foreign_origin(&world, &request);
                referrer = request
                    .referrer
                    .and_then(|host| naming::resolve_host(&world, host).ok())
                    .map(|resolved| resolved.key);
                match naming::resolve_host(&world, request.host) {
                    Ok(resolved) => resolved.key,
                    Err(not_found) => {
                        return Route::Error(ErrorPage::new(
                            StatusCode::NOT_FOUND,
                            "No app here",
                            not_found.to_string(),
                        ));
                    }
                }
            };
            match served_files(&world, &key, request) {
                Some(Served::File(path)) => {
                    return Route::File {
                        path,
                        guard: Box::new(Guard::new(self.supervisor.clone(), key, None, shared)),
                    };
                }
                Some(Served::Missing) => {
                    return Route::Error(ErrorPage::new(
                        StatusCode::NOT_FOUND,
                        "Not found",
                        format!("{} has no file at {}", key.app, request.path),
                    ));
                }
                None => {}
            }
            let ready = async {
                match &foreign {
                    Some(_) => {
                        self.supervisor
                            .request_running(key.clone(), request.path)
                            .await
                    }
                    None => {
                        self.supervisor
                            .request(key.clone(), request.path, referrer)
                            .await
                    }
                }
            };
            match tokio::time::timeout(self.hold, ready).await {
                Ok(Ok(_))
                    if shared.as_ref().is_some_and(|credentials| {
                        self.supervisor.shared_routes().get(&key) != Some(credentials)
                    }) =>
                {
                    Route::Error(not_shared())
                }
                Ok(Ok(upstream)) => Route::Upstream {
                    port: upstream.port,
                    guard: Box::new(Guard::new(
                        self.supervisor.clone(),
                        key,
                        Some(upstream.boot),
                        shared,
                    )),
                    checked_authorization,
                },
                Ok(Err(failure)) if failure.is_not_running() => {
                    Route::Error(not_started(&key, foreign.as_deref().unwrap_or("A page")))
                }
                Ok(Err(failure)) => {
                    let status = if failure.transient {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::BAD_GATEWAY
                    };
                    let mut page =
                        ErrorPage::new(status, failure.title, failure.detail).with_log(failure.log);
                    if failure.transient {
                        page = page.with_retry_after(Duration::from_secs(2));
                    }
                    Route::Error(page)
                }
                Err(_) => Route::Error(still_starting(
                    &self.supervisor.snapshot(),
                    &key,
                    request.path,
                )),
            }
        })
    }

    fn closing(&self, kind: ListenerKind) -> Option<BoxFuture<'static, ()>> {
        if kind != ListenerKind::Share {
            return None;
        }
        let mut shared = self.supervisor.watch_shared();
        Some(Box::pin(async move {
            let mut routes = shared.borrow_and_update().clone();
            while shared.changed().await.is_ok() {
                let now = shared.borrow_and_update().clone();
                if routes
                    .iter()
                    .any(|(key, credentials)| now.get(key) != Some(credentials))
                {
                    return;
                }
                routes = now;
            }
        }))
    }
}

/// The page for a request another site made for an instance that isn't running.
fn not_started(key: &InstanceKey, from: &str) -> ErrorPage {
    let label = format!("{}@{}", key.app, key.stack.slug());
    ErrorPage::new(
        StatusCode::FORBIDDEN,
        format!("{label} isn't running"),
        format!(
            "{from} asked for {label}, which isn't running. carte only starts apps that you open yourself or that local pages ask for, so other sites can't start them. Open the app yourself to start it."
        ),
    )
}

/// The page for a request that waited too long for its instance: why it isn't
/// ready, and how to answer a debugger prompt that holds it up.
/// Once the instance is ready, the process a request waits for is the one serving
/// its path, unless that one is off, else the HTTP process. While it starts (it
/// has boot progress), the request waits for the whole instance, so any paused
/// or suspended process holds it up.
fn still_starting(snapshot: &Snapshot, key: &InstanceKey, path: &str) -> ErrorPage {
    let label = format!("{}@{}", key.app, key.stack.slug());
    let instance = snapshot
        .projects
        .iter()
        .filter(|p| p.name == key.project)
        .flat_map(|p| &p.stacks)
        .filter(|s| s.name == key.stack.slug())
        .flat_map(|s| &s.instances)
        .find(|i| i.app == key.app);
    let processes: Vec<&ProcessStatus> = instance.into_iter().flat_map(|i| &i.processes).collect();
    let off = |p: &ProcessStatus| matches!(p.state, ProcessState::Stopped | ProcessState::Disabled);
    let waited_for = processes
        .iter()
        .filter(|p| !off(p))
        .filter_map(|p| path_match(&p.paths, path).map(|len| (len, *p)))
        .max_by_key(|(len, _)| *len)
        .map(|(_, p)| p)
        .or_else(|| processes.iter().copied().find(|p| p.http));
    let is_held =
        |p: &&ProcessStatus| matches!(p.state, ProcessState::Paused | ProcessState::Suspended);
    let starting = instance.is_some_and(|i| i.boot.is_some());
    let held = waited_for.filter(is_held).or_else(|| {
        starting
            .then(|| processes.iter().copied().find(is_held))
            .flatten()
    });
    let page = match held {
        Some(process) if process.state == ProcessState::Suspended => ErrorPage::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{label} is suspended"),
            format!(
                "Its {} process was stopped by a signal, such as SIGSTOP. This page will work once it continues; `carte restart {label}` starts it over.",
                process.name
            ),
        ),
        Some(process) => ErrorPage::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{label} is paused at a breakpoint"),
            format!(
                "Its {} process waits at a debugger prompt. Run `carte attach {label}/{}` to answer it; this page will work once it goes on.",
                process.name, process.name
            ),
        ),
        None => ErrorPage::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{label} is still starting"),
            "It is taking a while to boot. This page will work once it is ready.",
        ),
    };
    page.with_retry_after(Duration::from_secs(5))
}

impl SupervisorRouter {
    /// The instance a request from the tunnel is for, once it is known to be shared
    /// and its credentials check out, with the credentials it is shared with. Every
    /// request that names a share is logged.
    fn shared_key(
        &self,
        world: &World,
        request: RouteRequest<'_>,
    ) -> Result<(InstanceKey, Option<Credentials>), ErrorPage> {
        let host = request.host.split(':').next().unwrap_or(request.host);
        let routes = self.supervisor.shared_routes();
        let key = match &world.machine.share {
            Some(share) if share.single_host() && host.eq_ignore_ascii_case(&share.domain) => {
                routes.keys().next().cloned().ok_or_else(not_shared)?
            }
            _ => naming::resolve_public_host(world, host).map_err(|_| not_shared())?,
        };
        let credentials = routes.get(&key).ok_or_else(not_shared)?;
        let refused = credentials
            .as_ref()
            .is_some_and(|c| !share::authorized(request.authorization, c));
        self.supervisor.external_request(
            key.clone(),
            ExternalRequest {
                at_ms: crate::supervisor::runner::now_ms(),
                app: key.app.clone(),
                method: request.method.to_string(),
                path: request.path.to_string(),
                refused,
            },
        );
        if refused {
            return Err(ErrorPage::new(
                StatusCode::UNAUTHORIZED,
                "Credentials needed",
                "This share is protected; ask whoever shared it for the user name and password.",
            )
            .with_basic_challenge("carte"));
        }
        Ok((key, credentials.clone()))
    }
}

fn not_shared() -> ErrorPage {
    ErrorPage::new(
        StatusCode::NOT_FOUND,
        "Not shared",
        "Nothing is shared under this name.",
    )
}

enum Served {
    File(std::path::PathBuf),
    /// The app only serves files, and this one isn't there.
    Missing,
}

/// A GET or HEAD for a file in the app's `serve_files` folder, in the checkout of the
/// request's stack. `None` sends the request on to the app, as for paths a process
/// serves.
fn served_files(world: &World, key: &InstanceKey, request: RouteRequest<'_>) -> Option<Served> {
    if !matches!(*request.method, http::Method::GET | http::Method::HEAD) {
        return None;
    }
    let state = world.project(&key.project)?;
    let app = state.project.app(&key.app)?;
    let app = state.app_at(app, &key.stack);
    let dir = app.settings.serve_files.as_ref()?;
    let checkout = state.checkout(app, &key.stack)?;
    let list = state.process_list(app, &key.stack);
    if let Some(Ok(list)) = list
        && list
            .processes
            .iter()
            .any(|p| p.path_match(request.path).is_some())
    {
        return None;
    }
    if let Some(path) = crate::proxy::files::find(&checkout.path().join(dir), request.path) {
        return Some(Served::File(path));
    }
    let files_only = matches!(list, Some(Ok(list)) if list.source == ProcessSource::ServeFiles);
    files_only.then_some(Served::Missing)
}

struct Guard {
    supervisor: Handle,
    key: InstanceKey,
    /// The start of the instance it goes to; `None` for a file carte serves.
    boot: Option<u64>,
    long_lived: bool,
    /// For a request from the tunnel, the credentials its instance was shared with.
    shared: Option<Option<Credentials>>,
}

impl Guard {
    fn new(
        supervisor: Handle,
        key: InstanceKey,
        boot: Option<u64>,
        shared: Option<Option<Credentials>>,
    ) -> Self {
        supervisor.activity(key.clone(), boot, Activity::RequestStarted);
        Guard {
            supervisor,
            key,
            boot,
            long_lived: false,
            shared,
        }
    }
}

impl ActivityGuard for Guard {
    fn long_lived(&mut self) {
        if !self.long_lived {
            self.long_lived = true;
            self.supervisor
                .activity(self.key.clone(), self.boot, Activity::RequestEnded);
            self.supervisor
                .activity(self.key.clone(), self.boot, Activity::ConnectionOpened);
        }
    }

    fn closed(&self) -> Option<BoxFuture<'static, ErrorPage>> {
        let credentials = self.shared.clone()?;
        let key = self.key.clone();
        let mut shared = self.supervisor.watch_shared();
        Some(Box::pin(async move {
            let _ = shared
                .wait_for(|routes| routes.get(&key) != Some(&credentials))
                .await;
            not_shared()
        }))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let activity = if self.long_lived {
            Activity::ConnectionClosed
        } else {
            Activity::RequestEnded
        };
        self.supervisor
            .activity(self.key.clone(), self.boot, activity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_requests_other_sites_make() {
        let mut world = World::default();
        world.machine.tld = "test".into();
        let get = http::Method::GET;
        let post = http::Method::POST;
        let request = |method: &'static http::Method,
                       referrer: Option<&'static str>,
                       opaque_origin: bool,
                       fetch_site: Option<&'static str>,
                       fetch_mode: Option<&'static str>,
                       fetch_dest: Option<&'static str>| RouteRequest {
            host: "grafana.monitoring.prod.test",
            tls: true,
            kind: ListenerKind::Local,
            method,
            path: "/",
            authorization: None,
            referrer,
            opaque_origin,
            fetch_site,
            fetch_mode,
            fetch_dest,
            purpose: None,
        };
        let get: &'static http::Method = Box::leak(Box::new(get));
        let post: &'static http::Method = Box::leak(Box::new(post));
        let allowed = [
            request(get, None, false, None, None, None),
            request(
                get,
                Some("app.slack.com"),
                false,
                Some("cross-site"),
                Some("navigate"),
                Some("document"),
            ),
            request(
                post,
                Some("shop.test"),
                false,
                Some("cross-site"),
                Some("cors"),
                Some("empty"),
            ),
            request(
                get,
                Some("shop.test"),
                false,
                Some("same-site"),
                Some("navigate"),
                Some("iframe"),
            ),
            request(get, Some("localhost"), false, None, None, None),
            request(
                post,
                Some("shop.test"),
                true,
                Some("cross-site"),
                Some("navigate"),
                Some("document"),
            ),
            request(
                post,
                None,
                false,
                Some("same-origin"),
                Some("cors"),
                Some("empty"),
            ),
            RouteRequest {
                purpose: Some("prefetch"),
                ..request(
                    get,
                    Some("shop.test"),
                    false,
                    Some("same-site"),
                    Some("navigate"),
                    Some("document"),
                )
            },
            RouteRequest {
                purpose: Some("prefetch;prerender"),
                ..request(
                    get,
                    None,
                    false,
                    Some("none"),
                    Some("navigate"),
                    Some("document"),
                )
            },
        ];
        for r in allowed {
            assert_eq!(
                foreign_origin(&world, &r),
                None,
                "{:?} {:?}",
                r.referrer,
                r.fetch_site
            );
        }
        let refused = [
            request(
                post,
                Some("evil.example"),
                false,
                Some("cross-site"),
                Some("navigate"),
                Some("document"),
            ),
            request(
                get,
                Some("evil.example"),
                false,
                Some("cross-site"),
                Some("no-cors"),
                Some("image"),
            ),
            request(
                get,
                Some("evil.example"),
                false,
                Some("cross-site"),
                Some("navigate"),
                Some("iframe"),
            ),
            request(
                get,
                None,
                false,
                Some("cross-site"),
                Some("navigate"),
                Some("iframe"),
            ),
            request(
                post,
                None,
                true,
                Some("cross-site"),
                Some("navigate"),
                Some("document"),
            ),
            request(
                get,
                None,
                false,
                Some("cross-site"),
                Some("no-cors"),
                Some("script"),
            ),
            request(get, Some("evil.example"), false, None, None, None),
            request(
                post,
                Some("evil.example"),
                true,
                Some("cross-site"),
                Some("navigate"),
                Some("document"),
            ),
            RouteRequest {
                purpose: Some("prefetch"),
                ..request(
                    get,
                    Some("evil.example"),
                    false,
                    Some("cross-site"),
                    Some("navigate"),
                    Some("document"),
                )
            },
            RouteRequest {
                purpose: Some("prefetch"),
                ..request(
                    get,
                    None,
                    false,
                    Some("cross-site"),
                    Some("navigate"),
                    Some("document"),
                )
            },
            RouteRequest {
                purpose: Some("prefetch;prerender"),
                ..request(
                    get,
                    Some("evil.example"),
                    false,
                    Some("cross-site"),
                    Some("navigate"),
                    Some("document"),
                )
            },
        ];
        for r in refused {
            assert!(
                foreign_origin(&world, &r).is_some(),
                "{:?} {:?}",
                r.referrer,
                r.fetch_site
            );
        }
    }

    #[test]
    fn local_hosts_are_under_the_served_tlds_or_loopback() {
        let mut world = World::default();
        world.machine.tld = "test".into();
        for host in [
            "grafana.prod.test",
            "shop.localhost:8443",
            "localhost",
            "127.0.0.1:3000",
            "[::1]:3000",
            "Shop.Test.",
        ] {
            assert!(is_local_host(&world, host), "{host}");
        }
        for host in ["evil.example", "test.example", "eviltest", "10.0.0.1"] {
            assert!(!is_local_host(&world, host), "{host}");
        }
    }

    /// An instance of the sample with a web process and a `cable` process serving
    /// `/cable`, in `state`, booting or not.
    fn cable_app(state: carte_api::InstanceState, booting: bool) -> (Snapshot, InstanceKey) {
        let mut snapshot = crate::tui::fixtures::sample();
        let instance = &mut snapshot.projects[0].stacks[0].instances[0];
        instance.state = state;
        instance.boot = booting.then(|| carte_api::BootProgress {
            elapsed_ms: 1000,
            waiting_for: "cable to listen on port 51234".into(),
        });
        let cable = &mut instance.processes[1];
        cable.name = "cable".into();
        cable.paths = vec!["/cable".into()];
        let key = InstanceKey {
            project: "nova".into(),
            app: "nova-billing".into(),
            stack: carte_core::model::Stack::Main,
        };
        (snapshot, key)
    }

    fn set(snapshot: &mut Snapshot, web: ProcessState, cable: ProcessState) {
        let processes = &mut snapshot.projects[0].stacks[0].instances[0].processes;
        processes[0].state = web;
        processes[1].state = cable;
    }

    #[test]
    fn the_waiting_page_names_the_paused_process_the_request_waits_for() {
        use carte_api::InstanceState;
        let attach = |page: &ErrorPage| {
            assert_eq!(page.title, "nova-billing@main is paused at a breakpoint");
            page.detail
                .split('`')
                .nth(1)
                .unwrap_or_default()
                .to_string()
        };

        let (snapshot, key) = cable_app(InstanceState::Ready, false);
        let page = still_starting(&snapshot, &key, "/");
        assert_eq!(page.title, "nova-billing@main is still starting");

        let (mut snapshot, key) = cable_app(InstanceState::Paused, false);
        set(&mut snapshot, ProcessState::Paused, ProcessState::Running);
        let page = still_starting(&snapshot, &key, "/");
        assert_eq!(attach(&page), "carte attach nova-billing@main/web");

        let (mut snapshot, key) = cable_app(InstanceState::Paused, false);
        set(&mut snapshot, ProcessState::Starting, ProcessState::Paused);
        let page = still_starting(&snapshot, &key, "/cable");
        assert_eq!(attach(&page), "carte attach nova-billing@main/cable");
        let page = still_starting(&snapshot, &key, "/");
        assert_eq!(
            page.title, "nova-billing@main is still starting",
            "once ready, a request for / waits for web, which is restarting"
        );

        let (mut snapshot, key) = cable_app(InstanceState::Paused, true);
        set(&mut snapshot, ProcessState::Running, ProcessState::Paused);
        let page = still_starting(&snapshot, &key, "/");
        assert_eq!(
            attach(&page),
            "carte attach nova-billing@main/cable",
            "while booting, a request for / waits for cable too"
        );

        let (mut snapshot, key) = cable_app(InstanceState::Paused, false);
        set(&mut snapshot, ProcessState::Paused, ProcessState::Stopped);
        let page = still_starting(&snapshot, &key, "/cable");
        assert_eq!(
            attach(&page),
            "carte attach nova-billing@main/web",
            "an off path process leaves its paths to web"
        );
    }

    #[test]
    fn the_waiting_page_names_the_suspended_process_the_request_waits_for() {
        use carte_api::InstanceState;
        let (mut snapshot, key) = cable_app(InstanceState::Suspended, false);
        set(
            &mut snapshot,
            ProcessState::Running,
            ProcessState::Suspended,
        );
        let page = still_starting(&snapshot, &key, "/cable");
        assert_eq!(page.title, "nova-billing@main is suspended");
        assert!(
            page.detail.starts_with("Its cable process"),
            "{}",
            page.detail
        );
        assert!(
            page.detail.contains("`carte restart nova-billing@main`"),
            "{}",
            page.detail
        );
        let page = still_starting(&snapshot, &key, "/");
        assert_eq!(page.title, "nova-billing@main is still starting");

        let (mut snapshot, key) = cable_app(InstanceState::Suspended, true);
        set(
            &mut snapshot,
            ProcessState::Starting,
            ProcessState::Suspended,
        );
        let page = still_starting(&snapshot, &key, "/");
        assert_eq!(
            page.title, "nova-billing@main is suspended",
            "while booting, a request for / waits for cable too"
        );
    }
}
