use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::ListenerKind;
use super::forward::{Inbound, Shared, handle};

/// How a listener treats its connections.
#[derive(Clone)]
pub struct ListenerOptions {
    kind: ListenerKind,
    share_domain: Option<Arc<str>>,
    cloudflare: bool,
    tls: Option<Arc<ServerConfig>>,
}

impl ListenerOptions {
    /// A listener for local traffic.
    pub fn local() -> Self {
        Self {
            kind: ListenerKind::Local,
            share_domain: None,
            cloudflare: false,
            tls: None,
        }
    }

    /// A listener for tunnel traffic: requests are forwarded as `https`, and
    /// `Domain=<share_domain>` is stripped from `Set-Cookie`.
    pub fn share(share_domain: impl Into<String>) -> Self {
        Self {
            kind: ListenerKind::Share,
            share_domain: Some(share_domain.into().into()),
            cloudflare: false,
            tls: None,
        }
    }

    /// The tunnel is Cloudflare's, which sets `CF-Connecting-IP` itself, so that
    /// header is passed on.
    pub fn cloudflare(mut self) -> Self {
        self.cloudflare = true;
        self
    }

    /// Terminates TLS with `config`, offering HTTP/2 and HTTP/1.1 through ALPN in
    /// place of the config's own protocols.
    pub fn tls(mut self, config: Arc<ServerConfig>) -> Self {
        let mut config = Arc::unwrap_or_clone(config);
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        self.tls = Some(Arc::new(config));
        self
    }
}

/// A bound listener, ready to [`serve`](Listener::serve).
pub struct Listener {
    listener: TcpListener,
    options: ListenerOptions,
    shared: Arc<Shared>,
}

impl Listener {
    pub(crate) fn new(
        listener: TcpListener,
        options: ListenerOptions,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            listener,
            options,
            shared,
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts connections until `shutdown` is cancelled, then lets requests in
    /// flight finish and returns once every connection has closed. Upgraded
    /// tunnels are closed on shutdown.
    pub async fn serve(self, shutdown: CancellationToken) {
        let tracker = TaskTracker::new();
        let acceptor = self.options.tls.clone().map(TlsAcceptor::from);
        loop {
            let accepted = tokio::select! {
                () = shutdown.cancelled() => break,
                accepted = self.listener.accept() => accepted,
            };
            let (stream, peer) = match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%error, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if !is_loopback(peer.ip()) {
                tracing::warn!(%peer, "refused a connection from outside loopback");
                continue;
            }
            let inbound = Arc::new(Inbound {
                peer,
                tls: acceptor.is_some(),
                kind: self.options.kind,
                share_domain: self.options.share_domain.clone(),
                cloudflare: self.options.cloudflare,
                shutdown: shutdown.clone(),
                tracker: tracker.clone(),
            });
            tracker.spawn(serve_connection(
                stream,
                acceptor.clone(),
                inbound,
                self.shared.clone(),
            ));
        }
        drop(self.listener);
        tracker.close();
        tracker.wait().await;
    }
}

/// Whether a peer address is on loopback: `127.0.0.0/8`, `::1`, or
/// `::ffff:127.0.0.0/104` from a dual-stack socket.
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

async fn serve_connection(
    stream: TcpStream,
    acceptor: Option<TlsAcceptor>,
    inbound: Arc<Inbound>,
    shared: Arc<Shared>,
) {
    let _ = stream.set_nodelay(true);
    let Some(acceptor) = acceptor else {
        return serve_http(TokioIo::new(stream), inbound, shared).await;
    };
    let handshake = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream));
    let stream = tokio::select! {
        () = inbound.shutdown.cancelled() => return,
        handshake = handshake => match handshake {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                tracing::debug!(%error, peer = %inbound.peer, "TLS handshake failed");
                return;
            }
            Err(_) => return,
        },
    };
    if stream.get_ref().1.alpn_protocol() == Some(b"h2") {
        serve_h2(TokioIo::new(stream), inbound, shared).await;
    } else {
        serve_http(TokioIo::new(stream), inbound, shared).await;
    }
}

async fn serve_http<I>(io: I, inbound: Arc<Inbound>, shared: Arc<Shared>)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let shutdown = inbound.shutdown.clone();
    let service = service_fn(move |request| handle(shared.clone(), inbound.clone(), request));
    let mut connection = pin!(
        http1::Builder::new()
            .timer(TokioTimer::new())
            .serve_connection(io, service)
            .with_upgrades()
    );
    let result = tokio::select! {
        result = connection.as_mut() => result,
        () = shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    };
    if let Err(error) = result {
        tracing::debug!(%error, "connection ended with an error");
    }
}

async fn serve_h2<I>(io: I, inbound: Arc<Inbound>, shared: Arc<Shared>)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let shutdown = inbound.shutdown.clone();
    let service = service_fn(move |request| handle(shared.clone(), inbound.clone(), request));
    let mut builder = http2::Builder::new(TokioExecutor::new());
    builder.timer(TokioTimer::new()).enable_connect_protocol();
    let mut connection = pin!(builder.serve_connection(io, service));
    let result = tokio::select! {
        result = connection.as_mut() => result,
        () = shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    };
    if let Err(error) = result {
        tracing::debug!(%error, "connection ended with an error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_loopback_peers() {
        for ip in ["127.0.0.1", "127.1.2.3", "::1", "::ffff:127.0.0.1"] {
            assert!(is_loopback(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "192.168.1.10",
            "10.0.0.1",
            "0.0.0.0",
            "128.0.0.1",
            "::",
            "::2",
            "fe80::1",
            "::ffff:192.168.1.10",
            "2001:db8::1",
        ] {
            assert!(!is_loopback(ip.parse().unwrap()), "{ip}");
        }
    }
}
