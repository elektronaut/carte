//! Routes proxied requests to instances, booting them through the supervisor.

use std::time::Duration;

use carte_api::ExternalRequest;
use carte_core::model::{InstanceKey, World};
use carte_core::naming;
use http::StatusCode;

use crate::proxy::{
    ActivityGuard, BoxFuture, ErrorPage, ListenerKind, Route, RouteRequest, Router,
};
use crate::supervisor::{Activity, Handle, share};

pub struct SupervisorRouter {
    pub supervisor: Handle,
    /// How long a request may wait for a boot before it gets an error page.
    pub hold: Duration,
}

impl Router for SupervisorRouter {
    fn route<'a>(&'a self, request: RouteRequest<'a>) -> BoxFuture<'a, Route> {
        Box::pin(async move {
            let world = self.supervisor.world();
            let key = if request.kind == ListenerKind::Share {
                match self.shared_key(&world, request) {
                    Ok(key) => key,
                    Err(page) => return Route::Error(page),
                }
            } else {
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
            match tokio::time::timeout(self.hold, self.supervisor.ensure_ready(key.clone())).await {
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
