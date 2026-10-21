use std::convert::Infallible;
use std::error::Error as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use bytes::Bytes;
use http::header::{CONNECTION, CONTENT_TYPE, COOKIE, HOST, HeaderName, ORIGIN, REFERER, UPGRADE};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri, Version};
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::ext::Protocol;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::body::{ClosingBody, GuardedBody, ProxyBody, boxed};
use super::cookies::strip_cookie_domains;
use super::upstream::{DialConnector, LoopbackConnector};
use super::{
    ActivityGuard, BoxFuture, Dialer, ErrorPage, ListenerKind, Route, RouteRequest, Router,
    UpstreamError,
};

pub(crate) struct Shared {
    pub(crate) router: Arc<dyn Router>,
    client: Client<LoopbackConnector, Incoming>,
    dialers: DialConnector,
    dialed: Client<DialConnector, Incoming>,
}

impl Shared {
    pub(crate) fn new(router: Arc<dyn Router>) -> Self {
        let client = Client::builder(TokioExecutor::new())
            .set_host(false)
            .build(LoopbackConnector::default());
        let dialers = DialConnector::default();
        let dialed = Client::builder(TokioExecutor::new())
            .set_host(false)
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build(dialers.clone());
        Self {
            router,
            client,
            dialers,
            dialed,
        }
    }
}

/// Where a request goes.
enum Upstream<'a> {
    Port(u16),
    Dialed(&'a Dialer),
}

/// What the proxy knows about the connection a request arrived on.
pub(crate) struct Inbound {
    pub(crate) peer: SocketAddr,
    pub(crate) tls: bool,
    pub(crate) kind: ListenerKind,
    pub(crate) share_domain: Option<Arc<str>>,
    /// `CF-Connecting-IP` comes from the tunnel.
    pub(crate) cloudflare: bool,
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
    mut request: Request<Incoming>,
) -> Result<Response<ProxyBody>, Infallible> {
    let Some(host) = request_host(&request) else {
        let page = ErrorPage::new(
            StatusCode::BAD_REQUEST,
            "No host",
            "The request has no Host header, so carte can't tell which app it is for.",
        );
        return Ok(render(&page, inbound.kind, request.headers()));
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
    let referrer = referrer_host(request.headers());
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase)
    };
    let fetch_site = header("sec-fetch-site");
    let fetch_mode = header("sec-fetch-mode");
    let fetch_dest = header("sec-fetch-dest");
    let purpose = header("sec-purpose").or_else(|| header("purpose"));
    let route_request = RouteRequest {
        host: &host,
        tls: inbound.tls,
        kind: inbound.kind,
        method: &method,
        path: &path,
        authorization: authorization.as_deref(),
        referrer: referrer.as_deref(),
        opaque_origin: header("origin").as_deref() == Some("null"),
        fetch_site: fetch_site.as_deref(),
        fetch_mode: fetch_mode.as_deref(),
        fetch_dest: fetch_dest.as_deref(),
        purpose: purpose.as_deref(),
    };
    let route = shared.router.route(route_request).await;
    Ok(match route {
        Route::Error(page) => render(&page, inbound.kind, request.headers()),
        Route::Upstream {
            port,
            guard,
            checked_authorization,
        } => {
            if checked_authorization {
                request.headers_mut().remove(http::header::AUTHORIZATION);
            }
            let upstream = Upstream::Port(port);
            forward(&shared, &inbound, route_request, upstream, guard, request).await
        }
        Route::Dialed { dialer, guard } => {
            let upstream = Upstream::Dialed(&dialer);
            forward(&shared, &inbound, route_request, upstream, guard, request).await
        }
        Route::File { path, guard } => {
            let (closed, body_closed) = (guard.closed(), guard.closed());
            let respond = super::files::respond(&path, request.method(), request.headers(), guard);
            match until_closed(closed, respond).await {
                Ok(response) => closing(response, body_closed),
                Err(page) => render(&page, inbound.kind, request.headers()),
            }
        }
    })
}

async fn forward(
    shared: &Shared,
    inbound: &Inbound,
    route_request: RouteRequest<'_>,
    upstream: Upstream<'_>,
    mut guard: Box<dyn ActivityGuard>,
    mut request: Request<Incoming>,
) -> Response<ProxyBody> {
    let request_headers = request.headers().clone();
    let h2_websocket = is_h2_websocket(&request);
    let upgrade = h2_websocket || is_upgrade(request.headers());
    let downstream = upgrade.then(|| hyper::upgrade::on(&mut request));

    strip_hop_by_hop(request.headers_mut(), upgrade);
    drop_next_dev_origin(
        request.headers_mut(),
        route_request.host,
        route_request.path,
    );
    if h2_websocket {
        websocket_over_http1(&mut request);
    }
    if request.version() == Version::HTTP_2 {
        join_cookies(request.headers_mut());
        *request.version_mut() = Version::HTTP_11;
    }
    add_forwarded_headers(request.headers_mut(), inbound, route_request);
    if let Ok(host) = HeaderValue::from_str(route_request.host) {
        request.headers_mut().insert(HOST, host);
    }
    let sent = match upstream {
        Upstream::Port(port) => {
            *request.uri_mut() = upstream_uri(port, request.uri());
            shared.client.request(request)
        }
        Upstream::Dialed(dialer) => {
            *request.uri_mut() = shared.dialers.uri(dialer, request.uri());
            shared.dialed.request(request)
        }
    };

    let sent = match until_closed(guard.closed(), sent).await {
        Ok(sent) => sent,
        Err(page) => return render(&page, inbound.kind, &request_headers),
    };
    let mut response = match sent {
        Ok(response) => response,
        Err(error) => {
            let error = if error.is_connect() {
                UpstreamError::Connect(connect_error(&error))
            } else if is_parse_error(&error) {
                UpstreamError::NotHttp(error_chain(&error))
            } else {
                UpstreamError::Request(error_chain(&error))
            };
            let page = match upstream {
                Upstream::Port(port) => shared.router.upstream_failed(route_request, port, &error),
                Upstream::Dialed(dialer) => dialer.failed(&error),
            };
            return render(&page, inbound.kind, &request_headers);
        }
    };

    if let (Some(downstream), StatusCode::SWITCHING_PROTOCOLS) = (downstream, response.status()) {
        let upstream = hyper::upgrade::on(&mut response);
        strip_hop_by_hop(response.headers_mut(), !h2_websocket);
        if let Some(domain) = &inbound.share_domain {
            strip_cookie_domains(response.headers_mut(), domain);
        }
        if h2_websocket {
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().remove("sec-websocket-accept");
        }
        guard.long_lived();
        let shutdown = inbound.shutdown.clone();
        let closed = guard.closed();
        inbound.tracker.spawn(async move {
            let _guard = guard;
            let closed = async {
                match closed {
                    Some(closed) => {
                        closed.await;
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(closed);
            let (downstream, upstream) = tokio::select! {
                pair = async { tokio::try_join!(downstream, upstream) } => match pair {
                    Ok(pair) => pair,
                    Err(error) => {
                        tracing::debug!(%error, "upgrade failed");
                        return;
                    }
                },
                () = &mut closed => return,
            };
            let mut downstream = TokioIo::new(downstream);
            let mut upstream = TokioIo::new(upstream);
            tokio::select! {
                _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream) => {}
                () = shutdown.cancelled() => {}
                () = &mut closed => {}
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
    let small = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
        .is_some_and(|length| length <= 8192);
    if response.status() == StatusCode::FORBIDDEN && small {
        let (parts, body) = response.into_parts();
        let bytes = match until_closed(guard.closed(), body.collect()).await {
            Ok(collected) => collected
                .map(|collected| collected.to_bytes())
                .unwrap_or_default(),
            Err(page) => return render(&page, inbound.kind, &request_headers),
        };
        if let Some(page) = host_check_page(&bytes, route_request.host) {
            return render(&page, inbound.kind, &request_headers);
        }
        return Response::from_parts(parts, boxed(GuardedBody::new(Full::new(bytes), guard)));
    }
    let closed = guard.closed();
    closing(
        response.map(|body| boxed(GuardedBody::new(body, guard))),
        closed,
    )
}

/// Runs `work` unless `closed` resolves first, which gives the page to answer with.
async fn until_closed<T>(
    closed: Option<BoxFuture<'static, ErrorPage>>,
    work: impl Future<Output = T>,
) -> Result<T, ErrorPage> {
    match closed {
        Some(closed) => tokio::select! {
            biased;
            page = closed => Err(page),
            done = work => Ok(done),
        },
        None => Ok(work.await),
    }
}

/// Ends the response's body with an error once `closed` resolves.
fn closing(
    response: Response<ProxyBody>,
    closed: Option<BoxFuture<'static, ErrorPage>>,
) -> Response<ProxyBody> {
    match closed {
        Some(closed) => response.map(|body| boxed(ClosingBody::new(body, closed))),
        None => response,
    }
}

/// carte's page for a dev server's refusal of the request's hostname, saying which
/// setting to add it to.
fn host_check_page(body: &Bytes, host: &str) -> Option<ErrorPage> {
    let body = std::str::from_utf8(body).ok()?;
    let setting = if body.contains("angular.json") {
        "`allowedHosts` under the `serve` target's options in angular.json"
    } else if body.contains("Blocked request: disallowed or invalid Host header") {
        "`server.allowedHosts` in the Rsbuild config"
    } else if body.contains("Blocked request. This host") {
        "`server.allowedHosts` in vite.config"
    } else if body.trim() == "Invalid Host header" {
        "`devServer.allowedHosts` in the webpack config"
    } else {
        return None;
    };
    let name = host.rsplit_once(':').map_or(host, |(name, _)| name);
    let domain = name.split_once('.').map_or(name, |(_, rest)| rest);
    Some(ErrorPage::new(
        StatusCode::FORBIDDEN,
        "The app refused this hostname",
        format!(
            "The dev server doesn't accept requests for {name}. Add it, or `.{domain}` for \
             every name under it, to {setting}, then restart the app."
        ),
    ))
}

/// Renders carte's own page; a share's visitors get its public form.
fn render(
    page: &ErrorPage,
    kind: ListenerKind,
    request_headers: &HeaderMap,
) -> Response<ProxyBody> {
    match kind {
        ListenerKind::Local => page.render(request_headers).map(boxed),
        ListenerKind::Share => page.public().render(request_headers).map(boxed),
    }
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

/// The host of the page a request came from: its `Origin`, or else its `Referer`.
fn referrer_host(headers: &HeaderMap) -> Option<String> {
    [ORIGIN, REFERER].iter().find_map(|name| {
        let uri: Uri = headers.get(name)?.to_str().ok()?.parse().ok()?;
        uri.scheme()?;
        uri.host().map(str::to_string)
    })
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

/// An HTTP/2 extended CONNECT for a WebSocket (RFC 8441).
fn is_h2_websocket<B>(request: &Request<B>) -> bool {
    request.method() == Method::CONNECT
        && request
            .extensions()
            .get::<Protocol>()
            .is_some_and(|protocol| protocol.as_str().eq_ignore_ascii_case("websocket"))
}

/// Turns an HTTP/2 WebSocket CONNECT into the HTTP/1.1 upgrade an app expects.
fn websocket_over_http1<B>(request: &mut Request<B>) {
    *request.method_mut() = Method::GET;
    request.extensions_mut().remove::<Protocol>();
    let headers = request.headers_mut();
    headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert("sec-websocket-key", websocket_key());
}

/// A fresh `Sec-WebSocket-Key`: 16 bytes, base64-encoded.
fn websocket_key() -> HeaderValue {
    use base64::Engine;
    use std::hash::BuildHasher;

    let state = std::collections::hash_map::RandomState::new();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&state.hash_one(1u8).to_le_bytes());
    bytes[8..].copy_from_slice(&state.hash_one(2u8).to_le_bytes());
    let key = base64::engine::general_purpose::STANDARD.encode(bytes);
    HeaderValue::from_str(&key).expect("base64 is a valid header value")
}

/// HTTP/2 may split `Cookie` into several fields; HTTP/1.1 needs one.
fn join_cookies(headers: &mut HeaderMap) {
    let values: Vec<&str> = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    if values.len() < 2 {
        return;
    }
    if let Ok(joined) = HeaderValue::from_str(&values.join("; ")) {
        headers.insert(COOKIE, joined);
    }
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

/// Forwarding headers that carte doesn't set, removed from requests through a share.
const VISITOR_FORWARDING: [&str; 12] = [
    "forwarded",
    "x-forwarded-port",
    "x-forwarded-prefix",
    "x-forwarded-scheme",
    "x-forwarded-ssl",
    "x-real-ip",
    "client-ip",
    "true-client-ip",
    "x-cluster-client-ip",
    "fastly-client-ip",
    "x-original-host",
    "x-host",
];

fn add_forwarded_headers(headers: &mut HeaderMap, inbound: &Inbound, request: RouteRequest<'_>) {
    let forwarded_for = match inbound.kind {
        ListenerKind::Local => {
            let peer = inbound.peer.ip().to_canonical().to_string();
            headers
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|value| value.to_str().ok())
                .chain([peer.as_str()])
                .collect::<Vec<_>>()
                .join(", ")
        }
        ListenerKind::Share => {
            for name in VISITOR_FORWARDING {
                headers.remove(name);
            }
            if !inbound.cloudflare {
                headers.remove("cf-connecting-ip");
            }
            visitor_chain(headers)
        }
    };
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

/// The `X-Forwarded-For` addresses a tunnel sent, without local or malformed ones;
/// `192.0.2.1` (TEST-NET-1) stands in for the visitor when none are left.
fn visitor_chain(headers: &HeaderMap) -> String {
    let addresses: Vec<String> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|address| address.trim().parse::<IpAddr>().ok())
        .filter(|ip| !is_local_address(*ip))
        .map(|ip| ip.to_string())
        .collect();
    if addresses.is_empty() {
        "192.0.2.1".into()
    } else {
        addresses.join(", ")
    }
}

/// Whether `ip` is loopback or unspecified, also as the IPv4 address inside an
/// IPv4-mapped, IPv4-compatible, IPv4-translated or NAT64 (`64:ff9b::/96`) one.
fn is_local_address(ip: IpAddr) -> bool {
    let local = |ip: IpAddr| ip.is_loopback() || ip.is_unspecified();
    let ip = ip.to_canonical();
    local(ip)
        || matches!(ip, IpAddr::V6(v6) if embedded_ipv4(v6).is_some_and(|v4| local(v4.into())))
}

fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = ip.segments();
    let embeds = matches!(
        segments[..6],
        [0, 0, 0, 0, 0, 0] | [0, 0, 0, 0, 0xffff, 0] | [0x64, 0xff9b, 0, 0, 0, 0]
    );
    embeds.then(|| {
        let [.., a, b, c, d] = ip.octets();
        Ipv4Addr::new(a, b, c, d)
    })
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

fn is_parse_error(error: &hyper_util::client::legacy::Error) -> bool {
    let mut source = error.source();
    while let Some(current) = source {
        if current
            .downcast_ref::<hyper::Error>()
            .is_some_and(hyper::Error::is_parse)
        {
            return true;
        }
        source = current.source();
    }
    false
}

/// Removes `Origin` from same-origin requests for Next.js's dev resources. Next refuses
/// them, its hot reloading included, when the origin isn't in `allowedDevOrigins`, but
/// lets requests without one through.
fn drop_next_dev_origin(headers: &mut HeaderMap, host: &str, path: &str) {
    let path = path.split('?').next().unwrap_or_default();
    if !(path.contains("/_next/") || path.contains("/__nextjs")) {
        return;
    }
    let same_origin = headers
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(|origin| origin.split_once("://"))
        .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host));
    if same_origin {
        headers.remove(ORIGIN);
    }
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
    fn the_referrer_is_the_origin_or_the_referer() {
        let both = headers(&[
            ("referer", "https://web.shop.localhost/cart?x=1"),
            ("origin", "https://billing.shop.localhost:8443"),
        ]);
        assert_eq!(
            referrer_host(&both).as_deref(),
            Some("billing.shop.localhost")
        );
        let referer = headers(&[
            ("origin", "null"),
            ("referer", "https://web.shop.localhost/cart"),
        ]);
        assert_eq!(
            referrer_host(&referer).as_deref(),
            Some("web.shop.localhost")
        );
        let origin = headers(&[("origin", "https://web.nova.localhost")]);
        assert_eq!(
            referrer_host(&origin).as_deref(),
            Some("web.nova.localhost")
        );
        assert_eq!(referrer_host(&headers(&[])), None);
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
    fn explains_a_dev_servers_host_check() {
        let page = |body: &'static str| {
            host_check_page(&Bytes::from_static(body.as_bytes()), "web.acme.test:8443")
                .map(|page| page.detail)
        };
        assert_eq!(
            page("Invalid Host header").unwrap(),
            "The dev server doesn't accept requests for web.acme.test. Add it, or `.acme.test` \
             for every name under it, to `devServer.allowedHosts` in the webpack config, then \
             restart the app."
        );
        assert!(
            page("Blocked request. This host (\"web.acme.test\") is not allowed.")
                .unwrap()
                .contains("vite.config")
        );
        assert!(page("<p>add it to <code>allowedHosts</code> under the <code>serve</code> target in <code>angular.json</code></p> Blocked request. This host").unwrap().contains("angular.json"));
        assert!(page("Forbidden").is_none());
    }

    #[test]
    fn drops_a_same_origin_origin_for_next_dev_resources() {
        let origin = |value: &'static str, host: &str, path: &str| {
            let mut headers = headers(&[("origin", value)]);
            drop_next_dev_origin(&mut headers, host, path);
            headers.contains_key(ORIGIN)
        };
        assert!(!origin(
            "https://web.acme.test",
            "web.acme.test",
            "/_next/hmr"
        ));
        assert!(!origin(
            "https://Web.Acme.test",
            "web.acme.test",
            "/_next/hmr?id=1"
        ));
        assert!(!origin(
            "https://web.acme.test:8443",
            "web.acme.test:8443",
            "/docs/_next/static/a.js"
        ));
        assert!(!origin(
            "https://shop.share.example",
            "shop.share.example",
            "/__nextjs_original-stack-frames"
        ));
        assert!(origin(
            "https://evil.example",
            "web.acme.test",
            "/_next/hmr"
        ));
        assert!(origin(
            "https://web.acme.test:8443",
            "web.acme.test",
            "/_next/hmr"
        ));
        assert!(origin("https://web.acme.test", "web.acme.test", "/cable"));
        assert!(origin(
            "https://web.acme.test",
            "web.acme.test",
            "/?next=/_next/"
        ));
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
    fn joins_cookies_split_by_http2() {
        let mut split = headers(&[("cookie", "a=1"), ("cookie", "b=2")]);
        join_cookies(&mut split);
        assert_eq!(
            split.get_all(COOKIE).iter().collect::<Vec<_>>(),
            ["a=1; b=2"]
        );
    }

    #[test]
    fn turns_http2_websocket_connects_into_upgrades() {
        let mut request = Request::connect("https://app.test/cable")
            .version(Version::HTTP_2)
            .body(())
            .unwrap();
        assert!(!is_h2_websocket(&request));
        request
            .extensions_mut()
            .insert(Protocol::from_static("websocket"));
        assert!(is_h2_websocket(&request));

        websocket_over_http1(&mut request);
        assert_eq!(request.method(), Method::GET);
        assert!(is_upgrade(request.headers()));
        let key = request.headers()["sec-websocket-key"].to_str().unwrap();
        assert_eq!(key.len(), 24);
        assert_ne!(key, websocket_key().to_str().unwrap());
    }

    #[test]
    fn keeps_only_visitor_addresses_from_the_tunnel() {
        let chain = |pairs: &[(&'static str, &'static str)]| visitor_chain(&headers(pairs));
        assert_eq!(chain(&[]), "192.0.2.1");
        assert_eq!(
            chain(&[("x-forwarded-for", "127.0.0.1, ::1, localhost, 0.0.0.0, ::")]),
            "192.0.2.1"
        );
        assert_eq!(
            chain(&[(
                "x-forwarded-for",
                "::ffff:0.0.0.0, ::127.0.0.1, ::ffff:0:127.0.0.1, 64:ff9b::7f00:1"
            )]),
            "192.0.2.1"
        );
        assert_eq!(
            chain(&[("x-forwarded-for", "64:ff9b::cb00:7109, ::ffff:0:a00:7")]),
            "64:ff9b::cb00:7109, ::ffff:0:a00:7"
        );
        assert_eq!(
            chain(&[("x-forwarded-for", "::ffff:127.0.0.2, 10.0.0.7")]),
            "10.0.0.7"
        );
        assert_eq!(
            chain(&[
                ("x-forwarded-for", "127.0.0.1, 198.51.100.4"),
                ("x-forwarded-for", "2001:db8::1")
            ]),
            "198.51.100.4, 2001:db8::1"
        );
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
