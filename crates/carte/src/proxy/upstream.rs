use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use http::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use super::{BoxFuture, Dial, Dialer, UpstreamStream};

/// Connects to the port of a URI on `127.0.0.1` or `::1`, trying first the
/// address that last worked for that port.
#[derive(Clone, Default)]
pub(crate) struct LoopbackConnector {
    preferred: Arc<Mutex<HashMap<u16, IpAddr>>>,
}

impl LoopbackConnector {
    async fn connect(self, port: u16) -> io::Result<TcpStream> {
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let first = self.preferred().get(&port).copied().unwrap_or(v4);
        let second = if first == v4 { v6 } else { v4 };
        let mut first_error = None;
        for ip in [first, second] {
            match TcpStream::connect(SocketAddr::new(ip, port)).await {
                Ok(stream)
                    if crate::loopback::connected_to_itself(
                        &stream,
                        stream.local_addr(),
                        stream.peer_addr(),
                    ) =>
                {
                    first_error.get_or_insert(io::ErrorKind::ConnectionRefused.into());
                }
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    if ip != first {
                        self.preferred().insert(port, ip);
                    }
                    return Ok(stream);
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        Err(first_error.expect("both addresses were tried"))
    }

    fn preferred(&self) -> std::sync::MutexGuard<'_, HashMap<u16, IpAddr>> {
        self.preferred
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl tower_service::Service<Uri> for LoopbackConnector {
    type Response = TokioIo<TcpStream>;
    type Error = io::Error;
    type Future = BoxFuture<'static, io::Result<Self::Response>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connector = self.clone();
        Box::pin(async move {
            let port = uri.port_u16().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "upstream URI without a port")
            })?;
            connector.connect(port).await.map(TokioIo::new)
        })
    }
}

/// Connects through the dialer a URI's host names, among the dialers of requests
/// in flight or with idle connections.
#[derive(Clone, Default)]
pub(crate) struct DialConnector {
    dialers: Arc<Mutex<HashMap<u64, Weak<dyn Dial>>>>,
}

impl DialConnector {
    /// The URI of `path` through `dialer`.
    pub(crate) fn uri(&self, dialer: &Dialer, original: &Uri) -> Uri {
        let mut dialers = self.dialers();
        dialers.retain(|_, dial| dial.strong_count() > 0);
        dialers
            .entry(dialer.id())
            .or_insert_with(|| dialer.downgrade());
        let path = original.path_and_query().map_or("/", |pq| pq.as_str());
        format!("http://dial-{}{path}", dialer.id())
            .parse()
            .expect("a dialer URI with a valid path is valid")
    }

    fn dialers(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Weak<dyn Dial>>> {
        self.dialers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl tower_service::Service<Uri> for DialConnector {
    type Response = TokioIo<Dialed>;
    type Error = io::Error;
    type Future = BoxFuture<'static, io::Result<Self::Response>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let dial = uri
            .host()
            .and_then(|host| host.strip_prefix("dial-")?.parse::<u64>().ok())
            .and_then(|id| self.dialers().get(&id)?.upgrade());
        Box::pin(async move {
            let dial = dial.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "gone"))?;
            let stream = dial.dial().await?;
            Ok(TokioIo::new(Dialed(stream)))
        })
    }
}

/// A connection a dialer opened.
pub(crate) struct Dialed(UpstreamStream);

impl Connection for Dialed {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl AsyncRead for Dialed {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Dialed {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn falls_back_to_ipv6_and_remembers_it() {
        let Ok(listener) = TcpListener::bind("[::1]:0").await else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let connector = LoopbackConnector::default();

        let stream = connector.clone().connect(port).await.unwrap();
        assert!(stream.peer_addr().unwrap().is_ipv6());
        assert_eq!(
            connector.preferred().get(&port),
            Some(&IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
    }

    #[tokio::test]
    async fn reports_refusal_when_neither_answers() {
        let mut kinds = Vec::new();
        for _ in 0..5 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            match LoopbackConnector::default().connect(port).await {
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => return,
                Err(error) => kinds.push(error.kind()),
                Ok(_) => kinds.push(io::ErrorKind::AddrInUse),
            }
        }
        panic!("never refused: {kinds:?}");
    }
}
