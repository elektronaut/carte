use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use carte::proxy::{
    ActivityGuard, BoxFuture, ErrorPage, Listener, ListenerKind, ListenerOptions, Proxy, Route,
    RouteRequest, Router,
};
use futures_util::{SinkExt, StreamExt};
use http::header::{ACCEPT, CONNECTION, CONTENT_TYPE, HOST, RETRY_AFTER, SET_COOKIE};
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Activity {
    in_flight: AtomicUsize,
    connections: AtomicUsize,
}

impl Activity {
    fn counts(&self) -> (usize, usize) {
        (
            self.in_flight.load(Ordering::SeqCst),
            self.connections.load(Ordering::SeqCst),
        )
    }
}

struct Guard {
    activity: Arc<Activity>,
    long_lived: bool,
}

impl Guard {
    fn new(activity: Arc<Activity>) -> Self {
        activity.in_flight.fetch_add(1, Ordering::SeqCst);
        Self {
            activity,
            long_lived: false,
        }
    }
}

impl ActivityGuard for Guard {
    fn long_lived(&mut self) {
        if !self.long_lived {
            self.long_lived = true;
            self.activity.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.activity.connections.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let counter = if self.long_lived {
            &self.activity.connections
        } else {
            &self.activity.in_flight
        };
        counter.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
enum Target {
    Port(u16),
    Page(ErrorPage),
    /// Waits for a permit before routing, like a booting instance, and sets the
    /// flag if the routing future is dropped while waiting.
    Booting(u16, Arc<Semaphore>, Arc<AtomicBool>),
}

#[derive(Default)]
struct TestRouter {
    routes: Mutex<HashMap<String, Target>>,
    seen: Mutex<Vec<(String, bool, ListenerKind)>>,
    activity: Arc<Activity>,
}

impl TestRouter {
    fn with(self, host: &str, target: Target) -> Self {
        self.routes.lock().unwrap().insert(host.into(), target);
        self
    }
}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Router for TestRouter {
    fn route<'a>(&'a self, request: RouteRequest<'a>) -> BoxFuture<'a, Route> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .push((request.host.into(), request.tls, request.kind));
            let name = request.host.split(':').next().unwrap();
            let target = self.routes.lock().unwrap().get(name).cloned();
            let port = match target {
                None => {
                    return Route::Error(ErrorPage::new(
                        StatusCode::NOT_FOUND,
                        "Not found",
                        format!("No app at {name}"),
                    ));
                }
                Some(Target::Page(page)) => return Route::Error(page),
                Some(Target::Port(port)) => port,
                Some(Target::Booting(port, gate, dropped)) => {
                    let flag = SetOnDrop(dropped);
                    let _permit = gate.acquire().await.unwrap();
                    std::mem::forget(flag);
                    port
                }
            };
            Route::Upstream {
                port,
                guard: Box::new(Guard::new(self.activity.clone())),
            }
        })
    }
}

struct Running {
    addr: SocketAddr,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

fn spawn(listener: Listener) -> Running {
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(listener.serve(shutdown.clone()));
    Running {
        addr,
        shutdown,
        task,
    }
}

async fn start(router: Arc<TestRouter>, options: ListenerOptions) -> Running {
    let proxy = Proxy::new(router);
    spawn(
        proxy
            .bind("127.0.0.1:0".parse().unwrap(), options)
            .await
            .unwrap(),
    )
}

type TestBody = UnsyncBoxBody<Bytes, Infallible>;

fn text(body: impl Into<String>) -> TestBody {
    Full::new(Bytes::from(body.into())).boxed_unsync()
}

/// Serves HTTP/1.1 on `addr` with `handler`, returning the bound port.
async fn upstream<F>(addr: &str, handler: F) -> u16
where
    F: Fn(Request<Incoming>) -> Response<TestBody> + Clone + Send + Sync + 'static,
{
    let listener = TcpListener::bind(addr).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let response = handler(request);
                    async move { Ok::<_, Infallible>(response) }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    port
}

/// An upstream that answers with the request line and headers it received.
async fn echo_upstream(addr: &str) -> u16 {
    upstream(addr, |request| {
        let mut body = format!("{} {}\n", request.method(), request.uri());
        for (name, value) in request.headers() {
            body.push_str(&format!("{name}: {}\n", value.to_str().unwrap()));
        }
        Response::new(text(body))
    })
    .await
}

async fn send_on<S>(stream: S, request: Request<Empty<Bytes>>) -> Response<Incoming>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection.with_upgrades());
    sender.send_request(request).await.unwrap()
}

async fn send(addr: SocketAddr, request: Request<Empty<Bytes>>) -> Response<Incoming> {
    send_on(TcpStream::connect(addr).await.unwrap(), request).await
}

fn get(host: &str, path: &str) -> http::request::Builder {
    Request::get(path).header(HOST, host)
}

async fn body_text(response: Response<Incoming>) -> String {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(body.to_vec()).unwrap()
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn forwards_requests_with_forwarding_headers() {
    let port = echo_upstream("127.0.0.1:0").await;
    let router = Arc::new(TestRouter::default().with("app.test", Target::Port(port)));
    let proxy = start(router.clone(), ListenerOptions::local()).await;

    let response = send(
        proxy.addr,
        get("app.test", "/path?q=1")
            .header("x-forwarded-for", "10.0.0.7")
            .header("x-forwarded-proto", "https")
            .header(CONNECTION, "keep-alive, x-hop")
            .header("x-hop", "secret")
            .header("keep-alive", "timeout=5")
            .header("x-app", "kept")
            .body(Empty::new())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let echoed = body_text(response).await;

    assert!(echoed.starts_with("GET /path?q=1\n"), "{echoed}");
    assert!(echoed.contains("\nhost: app.test\n"), "{echoed}");
    assert!(
        echoed.contains("\nx-forwarded-for: 10.0.0.7, 127.0.0.1\n"),
        "{echoed}"
    );
    assert!(echoed.contains("\nx-forwarded-proto: http\n"), "{echoed}");
    assert!(
        echoed.contains("\nx-forwarded-host: app.test\n"),
        "{echoed}"
    );
    assert!(echoed.contains("\nx-app: kept\n"), "{echoed}");
    for hop in ["x-hop", "keep-alive", "connection:"] {
        assert!(!echoed.contains(hop), "{hop} in {echoed}");
    }
    assert_eq!(
        router.seen.lock().unwrap().as_slice(),
        [("app.test".to_string(), false, ListenerKind::Local)]
    );
    eventually("the request to end", || router.activity.counts() == (0, 0)).await;
}

#[tokio::test]
async fn falls_back_to_ipv6_upstreams() {
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        return;
    }
    let port = echo_upstream("[::1]:0").await;
    let router = Arc::new(TestRouter::default().with("app.test", Target::Port(port)));
    let proxy = start(router, ListenerOptions::local()).await;

    for _ in 0..2 {
        let response = send(proxy.addr, get("app.test", "/").body(Empty::new()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("host: app.test"));
    }
}

#[tokio::test]
async fn counts_requests_in_flight_until_the_response_completes() {
    let (tx, rx) = mpsc::unbounded_channel::<()>();
    let rx = Arc::new(Mutex::new(Some(rx)));
    let port = upstream("127.0.0.1:0", move |_| {
        let mut rx = rx.lock().unwrap().take().unwrap();
        let stream = futures_util::stream::once(async move {
            rx.recv().await;
            Ok::<_, Infallible>(Frame::data(Bytes::from("done")))
        });
        Response::new(StreamBody::new(stream).boxed_unsync())
    })
    .await;
    let router = Arc::new(TestRouter::default().with("app.test", Target::Port(port)));
    let proxy = start(router.clone(), ListenerOptions::local()).await;

    let response = send(proxy.addr, get("app.test", "/").body(Empty::new()).unwrap()).await;
    assert_eq!(router.activity.counts(), (1, 0));
    tx.send(()).unwrap();
    assert_eq!(body_text(response).await, "done");
    eventually("the request to end", || router.activity.counts() == (0, 0)).await;
}

#[tokio::test]
async fn passes_websocket_upgrades_through() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if message.is_text() {
                socket.send(message).await.unwrap();
            }
        }
    });
    let router = Arc::new(TestRouter::default().with("app.test", Target::Port(port)));
    let proxy = start(router.clone(), ListenerOptions::local()).await;

    let stream = TcpStream::connect(proxy.addr).await.unwrap();
    let (mut socket, response) = tokio_tungstenite::client_async("ws://app.test/cable", stream)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(router.activity.counts(), (0, 1));

    for text in ["ping", "pong"] {
        socket.send(Message::text(text)).await.unwrap();
        let echoed = socket.next().await.unwrap().unwrap();
        assert_eq!(echoed.into_text().unwrap().as_str(), text);
    }
    assert_eq!(router.activity.counts(), (0, 1));

    socket.close(None).await.unwrap();
    drop(socket);
    eventually("the tunnel to close", || router.activity.counts() == (0, 0)).await;
}

#[tokio::test]
async fn counts_event_streams_as_connections() {
    let (tx, rx) = mpsc::unbounded_channel::<&'static str>();
    let rx = Arc::new(Mutex::new(Some(rx)));
    let port = upstream("127.0.0.1:0", move |_| {
        let rx = rx.lock().unwrap().take().unwrap();
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            let event = rx.recv().await?;
            Some((Ok::<_, Infallible>(Frame::data(Bytes::from(event))), rx))
        });
        Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(StreamBody::new(stream).boxed_unsync())
            .unwrap()
    })
    .await;
    let router = Arc::new(TestRouter::default().with("app.test", Target::Port(port)));
    let proxy = start(router.clone(), ListenerOptions::local()).await;

    let response = send(
        proxy.addr,
        get("app.test", "/events").body(Empty::new()).unwrap(),
    )
    .await;
    assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
    assert_eq!(router.activity.counts(), (0, 1));

    let mut body = response.into_body();
    tx.send("data: one\n\n").unwrap();
    let frame = body.frame().await.unwrap().unwrap();
    assert_eq!(frame.into_data().unwrap(), "data: one\n\n");
    assert_eq!(router.activity.counts(), (0, 1));

    drop(tx);
    while body.frame().await.is_some() {}
    eventually("the stream to end", || router.activity.counts() == (0, 0)).await;
}

#[tokio::test]
async fn survives_clients_that_disconnect_while_booting() {
    let port = echo_upstream("127.0.0.1:0").await;
    let gate = Arc::new(Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let router = Arc::new(
        TestRouter::default()
            .with(
                "slow.test",
                Target::Booting(port, gate.clone(), dropped.clone()),
            )
            .with("app.test", Target::Port(port)),
    );
    let proxy = start(router.clone(), ListenerOptions::local()).await;

    let (mut sender, connection) = hyper::client::conn::http1::handshake::<_, Empty<Bytes>>(
        TokioIo::new(TcpStream::connect(proxy.addr).await.unwrap()),
    )
    .await
    .unwrap();
    let connection = tokio::spawn(connection);
    let pending =
        tokio::spawn(sender.send_request(get("slow.test", "/").body(Empty::new()).unwrap()));
    eventually("the request to reach the router", || {
        !router.seen.lock().unwrap().is_empty()
    })
    .await;
    pending.abort();
    connection.abort();
    drop(sender);

    eventually("the routing future to be dropped", || {
        dropped.load(Ordering::SeqCst)
    })
    .await;
    gate.add_permits(1);

    let response = send(proxy.addr, get("app.test", "/").body(Empty::new()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::OK);
    body_text(response).await;

    let response = send(
        proxy.addr,
        get("slow.test", "/").body(Empty::new()).unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    body_text(response).await;
    eventually("all requests to end", || router.activity.counts() == (0, 0)).await;
}

fn self_signed(host: &str) -> (Arc<rustls::ServerConfig>, rustls::RootCertStore) {
    let certified = rcgen::generate_simple_self_signed([host.to_string()]).unwrap();
    let cert = certified.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.into())
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    (Arc::new(config), roots)
}

#[tokio::test]
async fn terminates_tls() {
    let port = echo_upstream("127.0.0.1:0").await;
    let router = Arc::new(TestRouter::default().with("app.test", Target::Port(port)));
    let (server_config, roots) = self_signed("app.test");
    let proxy = start(router.clone(), ListenerOptions::local().tls(server_config)).await;

    let mut client_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let stream = TcpStream::connect(proxy.addr).await.unwrap();
    let stream = connector
        .connect("app.test".try_into().unwrap(), stream)
        .await
        .unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));

    let response = send_on(stream, get("app.test", "/").body(Empty::new()).unwrap()).await;
    let echoed = body_text(response).await;
    assert!(echoed.contains("\nx-forwarded-proto: https\n"), "{echoed}");
    assert_eq!(
        router.seen.lock().unwrap().as_slice(),
        [("app.test".to_string(), true, ListenerKind::Local)]
    );
}

async fn cookie_upstream() -> u16 {
    upstream("127.0.0.1:0", |request| {
        let forwarded = format!(
            "{} {}",
            request.headers()["x-forwarded-proto"].to_str().unwrap(),
            request.headers()["x-forwarded-host"].to_str().unwrap(),
        );
        Response::builder()
            .header(SET_COOKIE, "a=1; Domain=share.example; Path=/; HttpOnly")
            .header(SET_COOKIE, "b=2; domain=.SHARE.example")
            .header(SET_COOKIE, "c=3; Domain=other.example; Secure")
            .body(text(forwarded))
            .unwrap()
    })
    .await
}

fn cookies(headers: &HeaderMap) -> Vec<&str> {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect()
}

#[tokio::test]
async fn share_listener_forwards_as_https_and_strips_cookie_domains() {
    let port = cookie_upstream().await;
    let host = "app--feature-x--nova.share.example";
    let router = Arc::new(TestRouter::default().with(host, Target::Port(port)));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = spawn(
        Proxy::new(router.clone())
            .listen(listener, ListenerOptions::share("share.example"))
            .unwrap(),
    );

    let response = send(proxy.addr, get(host, "/").body(Empty::new()).unwrap()).await;
    assert_eq!(
        cookies(response.headers()),
        [
            "a=1; Path=/; HttpOnly",
            "b=2",
            "c=3; Domain=other.example; Secure"
        ]
    );
    assert_eq!(body_text(response).await, format!("https {host}"));
    assert_eq!(
        router.seen.lock().unwrap().as_slice(),
        [(host.to_string(), false, ListenerKind::Share)]
    );
}

#[tokio::test]
async fn local_listener_leaves_cookies_alone() {
    let port = cookie_upstream().await;
    let router = Arc::new(TestRouter::default().with("app.test", Target::Port(port)));
    let proxy = start(router, ListenerOptions::local()).await;

    let response = send(proxy.addr, get("app.test", "/").body(Empty::new()).unwrap()).await;
    assert_eq!(
        cookies(response.headers()),
        [
            "a=1; Domain=share.example; Path=/; HttpOnly",
            "b=2; domain=.SHARE.example",
            "c=3; Domain=other.example; Secure"
        ]
    );
}

#[tokio::test]
async fn renders_router_error_pages_by_content_type() {
    let page = ErrorPage::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "<app> timed out",
        "web didn't pass its readiness check",
    )
    .with_log(["<boot> 1 & 2"])
    .with_retry_after(Duration::from_secs(5));
    let router = Arc::new(TestRouter::default().with("app.test", Target::Page(page)));
    let proxy = start(router, ListenerOptions::local()).await;

    let response = send(
        proxy.addr,
        get("app.test", "/")
            .header(ACCEPT, "text/html,*/*;q=0.8")
            .body(Empty::new())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[RETRY_AFTER], "5");
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    let html = body_text(response).await;
    assert!(html.contains("<h1>&lt;app&gt; timed out</h1>"), "{html}");
    assert!(
        html.contains("<pre>&lt;boot&gt; 1 &amp; 2\n</pre>"),
        "{html}"
    );

    let response = send(proxy.addr, get("app.test", "/").body(Empty::new()).unwrap()).await;
    assert_eq!(
        response.headers()[CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert_eq!(
        body_text(response).await,
        "503 <app> timed out\n\nweb didn't pass its readiness check\n\n<boot> 1 & 2\n"
    );
}

#[tokio::test]
async fn reports_unreachable_upstreams_through_the_router() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let router = Arc::new(TestRouter::default().with("app.test", Target::Port(port)));
    let proxy = start(router.clone(), ListenerOptions::local()).await;

    let response = send(proxy.addr, get("app.test", "/").body(Empty::new()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = body_text(response).await;
    assert!(body.starts_with("502 App not reachable\n"), "{body}");
    assert!(body.contains(&format!("port {port}")), "{body}");
    eventually("the request to end", || router.activity.counts() == (0, 0)).await;
}

#[tokio::test]
async fn shuts_down_gracefully() {
    let port = echo_upstream("127.0.0.1:0").await;
    let gate = Arc::new(Semaphore::new(0));
    let router = Arc::new(TestRouter::default().with(
        "app.test",
        Target::Booting(port, gate.clone(), Arc::default()),
    ));
    let proxy = start(router.clone(), ListenerOptions::local()).await;

    let pending = tokio::spawn(send(
        proxy.addr,
        get("app.test", "/").body(Empty::new()).unwrap(),
    ));
    eventually("the request to reach the router", || {
        !router.seen.lock().unwrap().is_empty()
    })
    .await;
    proxy.shutdown.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!proxy.task.is_finished());
    assert!(TcpStream::connect(proxy.addr).await.is_err());

    gate.add_permits(1);
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_text(response).await;
    tokio::time::timeout(Duration::from_secs(2), proxy.task)
        .await
        .unwrap()
        .unwrap();
}
