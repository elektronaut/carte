use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use http::StatusCode;
use tokio::io::{AsyncRead, AsyncWrite};

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
    /// `Origin: null`: a page that hides where it is, like a sandboxed frame.
    pub opaque_origin: bool,
    /// `Sec-Fetch-Site`, `Sec-Fetch-Mode` and `Sec-Fetch-Dest`, lowercased, from
    /// browsers that send them.
    pub fetch_site: Option<&'a str>,
    pub fetch_mode: Option<&'a str>,
    pub fetch_dest: Option<&'a str>,
    /// `Sec-Purpose`, or the older `Purpose`, lowercased: `prefetch` for a page the
    /// browser loads before you open it.
    pub purpose: Option<&'a str>,
    /// Where the router notes what it found out, for the request log.
    pub trace: &'a Trace,
}

/// What a router found out about a request while routing it.
#[derive(Debug, Default)]
pub struct Resolution {
    /// The instance or Service it is for, and the process that answers it.
    pub subject: Option<carte_api::EventSubject>,
    /// `main` or the stack of the checkout the instance runs from.
    pub checkout: Option<String>,
    pub via_alias: bool,
    /// Apps the request started ahead.
    pub warmed: Vec<String>,
}

/// A request's [`Resolution`], filled in by the router.
#[derive(Debug, Default)]
pub struct Trace(std::sync::Mutex<Resolution>);

impl Trace {
    pub const fn new() -> Self {
        Trace(std::sync::Mutex::new(Resolution {
            subject: None,
            checkout: None,
            via_alias: false,
            warmed: Vec::new(),
        }))
    }

    pub fn note(&self, note: impl FnOnce(&mut Resolution)) {
        note(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// What the router noted, leaving the trace empty.
    pub fn take(&self) -> Resolution {
        std::mem::take(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// A request the proxy answered, for [`Router::handled`].
#[derive(Debug)]
pub struct Handled {
    pub at: std::time::SystemTime,
    pub kind: ListenerKind,
    /// The visitor's address, for a request through a share.
    pub visitor: Option<String>,
    pub method: http::Method,
    pub host: String,
    pub path: String,
    /// `None` when the client went away before the response's head.
    pub status: Option<StatusCode>,
    /// How long routing took, waiting for a boot included.
    pub held: std::time::Duration,
    /// How long the upstream took to send its response's head.
    pub upstream: Option<std::time::Duration>,
    pub websocket: bool,
    /// `None` when the client went away before the response's head.
    pub served: Option<carte_api::Served>,
    pub resolution: Resolution,
    pub request_headers: http::HeaderMap,
    pub response_headers: http::HeaderMap,
    /// Header values were cut short or left out to keep the entry small.
    pub headers_truncated: bool,
}

pub enum Route {
    /// Forward to `port` on loopback. `guard` lives as long as the request is in
    /// flight, or as long as the connection if it becomes long-lived. With
    /// `checked_authorization`, the router took the `Authorization` header as its own
    /// credentials, and the app doesn't get it.
    Upstream {
        port: u16,
        guard: Box<dyn ActivityGuard>,
        checked_authorization: bool,
    },
    /// Forward over connections `dialer` opens inside the daemon, for an upstream with
    /// no listener of its own.
    Dialed {
        dialer: Dialer,
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

    /// Resolves once the request may no longer use its route, as when the share it
    /// came through ends, with the page for a request that has no response yet.
    /// The proxy then fails the request, or cuts its response or upgraded
    /// connection short. Each call watches anew; `None` for a route that stays
    /// open.
    fn closed(&self) -> Option<BoxFuture<'static, ErrorPage>> {
        None
    }
}

impl ActivityGuard for () {
    fn long_lived(&mut self) {}
}

pub trait UpstreamIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> UpstreamIo for T {}

/// A connection to an upstream that a [`Dial`] opened.
pub type UpstreamStream = Box<dyn UpstreamIo>;

/// Opens connections to an upstream inside the daemon.
pub trait Dial: Send + Sync + 'static {
    fn dial(&self) -> BoxFuture<'_, io::Result<UpstreamStream>>;

    /// The page shown when no connection could be opened or the request failed.
    fn failed(&self, error: &UpstreamError) -> ErrorPage;
}

/// A [`Dial`] with an identity of its own: the proxy keeps idle connections per
/// dialer, and clones share them.
#[derive(Clone)]
pub struct Dialer {
    id: u64,
    dial: Arc<dyn Dial>,
}

impl Dialer {
    pub fn new(dial: Arc<dyn Dial>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Dialer {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            dial,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn dial(&self) -> BoxFuture<'_, io::Result<UpstreamStream>> {
        self.dial.dial()
    }

    pub fn failed(&self, error: &UpstreamError) -> ErrorPage {
        self.dial.failed(error)
    }

    pub(crate) fn downgrade(&self) -> std::sync::Weak<dyn Dial> {
        Arc::downgrade(&self.dial)
    }
}

impl PartialEq for Dialer {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl std::fmt::Debug for Dialer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dialer").field("id", &self.id).finish()
    }
}

#[derive(Debug)]
pub enum UpstreamError {
    /// No connection could be made: on neither `127.0.0.1` nor `::1`, or by the
    /// dialer.
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

    /// Called with what the proxy knows about a request once it has the head of its
    /// response, or once its client went away before that. Runs on the request's
    /// path.
    fn handled(&self, request: Handled) {
        let _ = request;
    }

    /// Resolves when a connection opened now on a listener of `kind` should close,
    /// once its requests in flight are done, because a route it could have used
    /// closed. `None` keeps connections open until the client or shutdown closes
    /// them.
    fn closing(&self, kind: ListenerKind) -> Option<BoxFuture<'static, ()>> {
        let _ = kind;
        None
    }

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
