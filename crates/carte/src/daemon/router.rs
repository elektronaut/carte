//! Routes proxied requests to instances, booting them through the supervisor.

use std::time::Duration;

use carte_core::model::InstanceKey;
use carte_core::naming;
use http::StatusCode;

use crate::proxy::{
    ActivityGuard, BoxFuture, ErrorPage, ListenerKind, Route, RouteRequest, Router,
};
use crate::supervisor::{Activity, Handle};

pub struct SupervisorRouter {
    pub supervisor: Handle,
    /// How long a request may wait for a boot before it gets an error page.
    pub hold: Duration,
}

impl Router for SupervisorRouter {
    fn route<'a>(&'a self, request: RouteRequest<'a>) -> BoxFuture<'a, Route> {
        Box::pin(async move {
            if request.kind == ListenerKind::Share {
                return Route::Error(ErrorPage::new(
                    StatusCode::NOT_FOUND,
                    "Not shared",
                    "Nothing is shared under this name.",
                ));
            }
            let world = self.supervisor.world();
            let resolved = match naming::resolve_host(&world, request.host) {
                Ok(resolved) => resolved,
                Err(not_found) => {
                    return Route::Error(ErrorPage::new(
                        StatusCode::NOT_FOUND,
                        "No app here",
                        not_found.to_string(),
                    ));
                }
            };
            let key = resolved.key;
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
