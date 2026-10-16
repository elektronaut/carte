use std::convert::Infallible;
use std::error::Error as _;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use http::header::{CONNECTION, CONTENT_TYPE, HOST, HeaderName, UPGRADE};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode, Uri};
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::body::{GuardedBody, ProxyBody, boxed};
use super::cookies::strip_cookie_domains;
use super::upstream::LoopbackConnector;
use super::{ActivityGuard, ErrorPage, ListenerKind, Route, RouteRequest, Router, UpstreamError};

pub(crate) struct Shared {
    pub(crate) router: Arc<dyn Router>,
    client: Client<LoopbackConnector, Incoming>,
}

impl Shared {
    pub(crate) fn new(router: Arc<dyn Router>) -> Self {
        let client = Client::builder(TokioExecutor::new())
            .set_host(false)
            .build(LoopbackConnector::default());
        Self { router, client }
    }
}

/// What the proxy knows about the connection a request arrived on.
pub(crate) struct Inbound {
    pub(crate) peer: SocketAddr,
    pub(crate) tls: bool,
    pub(crate) kind: ListenerKind,
    pub(crate) share_domain: Option<Arc<str>>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) tracker: TaskTracker,
}

const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub(crate) async fn handle(
    shared: Arc<Shared>,
    inbound: Arc<Inbound>,
    request: Request<Incoming>,
) -> Result<Response<ProxyBody>, Infallible> {
    let Some(host) = request_host(&request) else {
        let page = ErrorPage::new(
            StatusCode::BAD_REQUEST,
            "No host",
            "The request has no Host header, so carte can't tell which app it is for.",
        );
        return Ok(render(&page, request.headers()));
    };
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let authorization = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let method = request.method().clone();
    let route_request = RouteRequest {
        host: &host,
        tls: inbound.tls,
        kind: inbound.kind,
        method: &method,
        path: &path,
        authorization: authorization.as_deref(),
    };
    let route = shared.router.route(route_request).await;
    Ok(match route {
        Route::Error(page) => render(&page, request.headers()),
        Route::Upstream { port, guard } => {
            forward(&shared, &inbound, route_request, port, guard, request).await
        }
    })
}

async fn forward(
    shared: &Shared,
    inbound: &Inbound,
    route_request: RouteRequest<'_>,
    port: u16,
    mut guard: Box<dyn ActivityGuard>,
    mut request: Request<Incoming>,
) -> Response<ProxyBody> {
    let request_headers = request.headers().clone();
    let upgrade = is_upgrade(request.headers());
    let downstream = upgrade.then(|| hyper::upgrade::on(&mut request));

    strip_hop_by_hop(request.headers_mut(), upgrade);
    add_forwarded_headers(request.headers_mut(), inbound, route_request);
    if !request.headers().contains_key(HOST)
        && let Ok(host) = HeaderValue::from_str(route_request.host)
    {
        request.headers_mut().insert(HOST, host);
    }
    *request.uri_mut() = upstream_uri(port, request.uri());

    let mut response = match shared.client.request(request).await {
        Ok(response) => response,
        Err(error) => {
            let error = if error.is_connect() {
                UpstreamError::Connect(connect_error(&error))
            } else {
                UpstreamError::Request(error_chain(&error))
            };
            let page = shared.router.upstream_failed(route_request, port, &error);
            return render(&page, &request_headers);
        }
    };

    if let (Some(downstream), StatusCode::SWITCHING_PROTOCOLS) = (downstream, response.status()) {
        let upstream = hyper::upgrade::on(&mut response);
        strip_hop_by_hop(response.headers_mut(), true);
        guard.long_lived();
        let shutdown = inbound.shutdown.clone();
        inbound.tracker.spawn(async move {
            let _guard = guard;
            let (downstream, upstream) = match tokio::try_join!(downstream, upstream) {
                Ok(pair) => pair,
                Err(error) => {
                    tracing::debug!(%error, "upgrade failed");
                    return;
                }
            };
            let mut downstream = TokioIo::new(downstream);
            let mut upstream = TokioIo::new(upstream);
            tokio::select! {
                _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream) => {}
                () = shutdown.cancelled() => {}
            }
        });
        return response.map(|_| boxed(Empty::new()));
    }

    strip_hop_by_hop(response.headers_mut(), false);
    if let Some(domain) = &inbound.share_domain {
        strip_cookie_domains(response.headers_mut(), domain);
    }
    if is_event_stream(response.headers()) {
        guard.long_lived();
    }
    response.map(|body| boxed(GuardedBody::new(body, guard)))
}

fn render(page: &ErrorPage, request_headers: &HeaderMap) -> Response<ProxyBody> {
    page.render(request_headers).map(boxed)
}

fn request_host<B>(request: &Request<B>) -> Option<String> {
    request
        .uri()
        .authority()
        .map(|authority| authority.as_str().to_string())
        .or_else(|| {
            let host = request.headers().get(HOST)?.to_str().ok()?;
            Some(host.to_string())
        })
        .filter(|host| !host.is_empty())
}

fn upstream_uri(port: u16, original: &Uri) -> Uri {
    let path = original.path_and_query().map_or("/", |pq| pq.as_str());
    format!("http://127.0.0.1:{port}{path}")
        .parse()
        .expect("a loopback URI with a valid path is valid")
}

fn connection_tokens(headers: &HeaderMap) -> impl Iterator<Item = String> + '_ {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
}

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(UPGRADE) && connection_tokens(headers).any(|token| token == "upgrade")
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

/// Removes hop-by-hop headers, including those listed in `Connection`. With
/// `keep_upgrade`, the `Upgrade` header survives along with `Connection: upgrade`.
fn strip_hop_by_hop(headers: &mut HeaderMap, keep_upgrade: bool) {
    let listed: Vec<HeaderName> = connection_tokens(headers)
        .filter(|token| token != "host")
        .filter_map(|token| HeaderName::from_bytes(token.as_bytes()).ok())
        .collect();
    let upgrade: Vec<HeaderValue> = if keep_upgrade {
        headers.get_all(UPGRADE).iter().cloned().collect()
    } else {
        Vec::new()
    };
    for name in listed {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    if !upgrade.is_empty() {
        headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        for value in upgrade {
            headers.append(UPGRADE, value);
        }
    }
}

fn add_forwarded_headers(headers: &mut HeaderMap, inbound: &Inbound, request: RouteRequest<'_>) {
    let peer = inbound.peer.ip().to_canonical().to_string();
    let forwarded_for = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .chain([peer.as_str()])
        .collect::<Vec<_>>()
        .join(", ");
    let proto = match (inbound.kind, inbound.tls) {
        (ListenerKind::Share, _) | (ListenerKind::Local, true) => "https",
        (ListenerKind::Local, false) => "http",
    };
    if let Ok(value) = HeaderValue::from_str(&forwarded_for) {
        headers.insert("x-forwarded-for", value);
    }
    headers.insert("x-forwarded-proto", HeaderValue::from_static(proto));
    if let Ok(value) = HeaderValue::from_str(request.host) {
        headers.insert("x-forwarded-host", value);
    }
}

fn connect_error(error: &hyper_util::client::legacy::Error) -> io::Error {
    let mut source = error.source();
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<io::Error>() {
            return io::Error::new(io.kind(), io.to_string());
        }
        source = current.source();
    }
    io::Error::other(error_chain(error))
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(current) = source {
        message.push_str(": ");
        message.push_str(&current.to_string());
        source = current.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn strips_hop_by_hop_and_listed_headers() {
        let mut headers = headers(&[
            ("connection", "keep-alive, X-Secret"),
            ("keep-alive", "timeout=5"),
            ("te", "trailers"),
            ("transfer-encoding", "chunked"),
            ("proxy-authorization", "Basic x"),
            ("x-secret", "1"),
            ("upgrade", "websocket"),
            ("accept", "*/*"),
        ]);
        strip_hop_by_hop(&mut headers, false);
        let names: Vec<_> = headers.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, ["accept"]);
    }

    #[test]
    fn keeps_upgrade_headers_for_upgrades() {
        let mut headers = headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "websocket"),
            ("keep-alive", "timeout=5"),
            ("sec-websocket-key", "abc"),
        ]);
        assert!(is_upgrade(&headers));
        strip_hop_by_hop(&mut headers, true);
        assert_eq!(headers[CONNECTION], "upgrade");
        assert_eq!(headers[UPGRADE], "websocket");
        assert_eq!(headers["sec-websocket-key"], "abc");
        assert!(!headers.contains_key("keep-alive"));
    }

    #[test]
    fn never_strips_host_through_connection() {
        let mut headers = headers(&[("connection", "host"), ("host", "app.test")]);
        strip_hop_by_hop(&mut headers, false);
        assert_eq!(headers[HOST], "app.test");
    }

    #[test]
    fn detects_upgrades_and_event_streams() {
        assert!(!is_upgrade(&headers(&[("upgrade", "websocket")])));
        assert!(!is_upgrade(&headers(&[("connection", "upgrade")])));
        assert!(is_event_stream(&headers(&[(
            "content-type",
            "Text/Event-Stream; charset=utf-8"
        )])));
        assert!(!is_event_stream(&headers(&[("content-type", "text/html")])));
    }

    #[test]
    fn builds_loopback_uris() {
        let uri: Uri = "/a/b?c=d".parse().unwrap();
        assert_eq!(
            upstream_uri(3000, &uri).to_string(),
            "http://127.0.0.1:3000/a/b?c=d"
        );
        let uri: Uri = "http://app.test/x".parse().unwrap();
        assert_eq!(
            upstream_uri(3000, &uri).to_string(),
            "http://127.0.0.1:3000/x"
        );
    }
}
