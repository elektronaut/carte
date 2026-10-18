use std::future::Future;
use std::io;
use std::pin::Pin;

use http::StatusCode;

use super::ErrorPage;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Which listener a request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerKind {
    /// The loopback listeners on ports 80 and 443.
    Local,
    /// The listener the tunnel connects to; it must only reach shared routes.
    Share,
}

#[derive(Debug, Clone, Copy)]
pub struct RouteRequest<'a> {
    /// The `Host` header (or the authority of an absolute-form URI) as received,
    /// port included.
    pub host: &'a str,
    /// The request came in over TLS.
    pub tls: bool,
    pub kind: ListenerKind,
    pub method: &'a http::Method,
    /// The path and query.
    pub path: &'a str,
    /// The `Authorization` header, for routers that check credentials themselves.
    pub authorization: Option<&'a str>,
    /// The host of the page the request came from, by its `Origin` or `Referer`.
    pub referrer: Option<&'a str>,
}

pub enum Route {
    /// Forward to `port` on loopback. `guard` lives as long as the request is in
    /// flight, or as long as the connection if it becomes long-lived.
    Upstream {
        port: u16,
        guard: Box<dyn ActivityGuard>,
    },
    /// Answer with a file from the app's `serve_files` folder.
    File {
        path: std::path::PathBuf,
        guard: Box<dyn ActivityGuard>,
    },
    Error(ErrorPage),
}

/// Activity of one request, held by the proxy. Dropping it ends the activity.
pub trait ActivityGuard: Send + 'static {
    /// The request became a long-lived connection (an upgraded tunnel or an
    /// event stream); the guard is now held until that connection closes.
    fn long_lived(&mut self);
}

impl ActivityGuard for () {
    fn long_lived(&mut self) {}
}

#[derive(Debug)]
pub enum UpstreamError {
    /// No connection could be made on either `127.0.0.1` or `::1`.
    Connect(io::Error),
    /// The connection was made but the request failed, e.g. the app closed it
    /// before responding.
    Request(String),
    /// The app answered with something that doesn't parse as HTTP.
    NotHttp(String),
}

/// Decides where requests go. Shared by all listeners of a [`super::Proxy`].
pub trait Router: Send + Sync + 'static {
    /// Resolves a request to an upstream, waiting for the instance to boot if
    /// needed.
    ///
    /// The returned future is dropped when the client disconnects while it is
    /// pending, so it must not own the boot: spawn the boot separately and only
    /// await its readiness here.
    fn route<'a>(&'a self, request: RouteRequest<'a>) -> BoxFuture<'a, Route>;

    /// The page shown when the upstream returned by [`Router::route`] can't be
    /// reached or fails before responding.
    fn upstream_failed(
        &self,
        request: RouteRequest<'_>,
        port: u16,
        error: &UpstreamError,
    ) -> ErrorPage {
        let _ = request;
        match error {
            UpstreamError::Connect(error) => ErrorPage::new(
                StatusCode::BAD_GATEWAY,
                "App not reachable",
                format!("Nothing answered on port {port}: {error}"),
            ),
            UpstreamError::Request(error) => ErrorPage::new(
                StatusCode::BAD_GATEWAY,
                "App failed to respond",
                format!("The request to port {port} failed: {error}"),
            ),
            UpstreamError::NotHttp(error) => ErrorPage::new(
                StatusCode::BAD_GATEWAY,
                "App sent an invalid response",
                format!(
                    "The app on port {port} answered with something that isn't HTTP ({error}). \
                     Dev servers do this when they refuse a request, such as Next.js for a \
                     host that isn't in its allowedDevOrigins."
                ),
            ),
        }
    }
}
