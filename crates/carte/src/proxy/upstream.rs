use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use http::Uri;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use super::BoxFuture;

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
