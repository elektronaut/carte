//! Routes proxied requests to instances, booting them through the supervisor.

use std::time::Duration;

use carte_api::ExternalRequest;
use carte_core::kube::{self, KubeTarget};
use carte_core::model::{InstanceKey, World};
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
}

/// Who sent a request that another site made, as a phrase for the error page; `None`
/// for opening a page yourself (typed, bookmarked, or a link you followed) and for
/// requests from local pages. A frame on another site is a request that site made.
fn foreign_origin(world: &World, request: &RouteRequest<'_>) -> Option<String> {
    let navigation = matches!(*request.method, http::Method::GET | http::Method::HEAD)
        && request.fetch_mode == Some("navigate")
        && request.fetch_dest == Some("document");
    if navigation {
        return None;
    }
    if request.opaque_origin {
        return Some("A page that hides its origin".into());
    }
    match request.referrer {
        Some(host) if is_local_host(world, host) => None,
        Some(host) => Some(format!("A page on {host}")),
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
            let key = if request.kind == ListenerKind::Share {
                match self.shared_key(&world, request) {
                    Ok(key) => key,
                    Err(page) => return Route::Error(page),
                }
            } else {
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
                        guard: Box::new(Guard::new(self.supervisor.clone(), key)),
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
            let ready = self.supervisor.request(key.clone(), referrer);
            match tokio::time::timeout(self.hold, ready).await {
                Ok(Ok(upstream)) => Route::Upstream {
                    port: upstream.port,
                    guard: Box::new(Guard::new(self.supervisor.clone(), key)),
                },
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
                Err(_) => Route::Error(
                    ErrorPage::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("{}@{} is still starting", key.app, key.stack.slug()),
                        "It is taking a while to boot. This page will work once it is ready.",
                    )
                    .with_retry_after(Duration::from_secs(5)),
                ),
            }
        })
    }
}

impl SupervisorRouter {
    /// The instance a request from the tunnel is for, once it is known to be shared
    /// and its credentials check out. Every request that names a share is logged.
    fn shared_key(
        &self,
        world: &World,
        request: RouteRequest<'_>,
    ) -> Result<InstanceKey, ErrorPage> {
        let not_shared = || {
            ErrorPage::new(
                StatusCode::NOT_FOUND,
                "Not shared",
                "Nothing is shared under this name.",
            )
        };
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
        Ok(key)
    }
}

enum Served {
    File(std::path::PathBuf),
    /// The app only serves files, and this one isn't there.
    Missing,
}

/// A GET or HEAD for a file in the app's `serve_files` folder, in the checkout of the
/// request's stack. `None` sends the request on to the app.
fn served_files(world: &World, key: &InstanceKey, request: RouteRequest<'_>) -> Option<Served> {
    if !matches!(*request.method, http::Method::GET | http::Method::HEAD) {
        return None;
    }
    let state = world.project(&key.project)?;
    let app = state.project.app(&key.app)?;
    let app = state.app_at(app, &key.stack);
    let dir = app.settings.serve_files.as_ref()?;
    let checkout = state.checkout(app, &key.stack)?;
    if let Some(path) = crate::proxy::files::find(&checkout.path().join(dir), request.path) {
        return Some(Served::File(path));
    }
    let files_only = matches!(
        state.process_list(app, &key.stack),
        Some(Ok(list)) if list.source == ProcessSource::ServeFiles
    );
    files_only.then_some(Served::Missing)
}

struct Guard {
    supervisor: Handle,
    key: InstanceKey,
    long_lived: bool,
}

impl Guard {
    fn new(supervisor: Handle, key: InstanceKey) -> Self {
        supervisor.activity(key.clone(), Activity::RequestStarted);
        Guard {
            supervisor,
            key,
            long_lived: false,
        }
    }
}

impl ActivityGuard for Guard {
    fn long_lived(&mut self) {
        if !self.long_lived {
            self.long_lived = true;
            self.supervisor
                .activity(self.key.clone(), Activity::RequestEnded);
            self.supervisor
                .activity(self.key.clone(), Activity::ConnectionOpened);
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let activity = if self.long_lived {
            Activity::ConnectionClosed
        } else {
            Activity::RequestEnded
        };
        self.supervisor.activity(self.key.clone(), activity);
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
                None,
                false,
                Some("same-origin"),
                Some("cors"),
                Some("empty"),
            ),
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
}
