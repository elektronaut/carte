//! The reverse proxy: accepts HTTP(S) on loopback (HTTP/2 over TLS when the
//! client offers it), asks a [`Router`] where each request goes, and forwards it
//! as HTTP/1.1 to an app on `127.0.0.1` or `::1`, including WebSocket upgrades.

mod body;
mod cookies;
mod error_page;
pub mod files;
mod forward;
mod listener;
mod router;
mod upstream;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

pub use error_page::ErrorPage;
pub use listener::{Listener, ListenerOptions, is_loopback};
pub use router::{
    ActivityGuard, BoxFuture, ListenerKind, Route, RouteRequest, Router, UpstreamError,
};

/// A router and a pool of upstream connections, shared by any number of
/// listeners.
#[derive(Clone)]
pub struct Proxy {
    shared: Arc<forward::Shared>,
}

impl Proxy {
    pub fn new(router: Arc<dyn Router>) -> Self {
        Self {
            shared: Arc::new(forward::Shared::new(router)),
        }
    }

    pub async fn bind(&self, addr: SocketAddr, options: ListenerOptions) -> io::Result<Listener> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(Listener::new(listener, options, self.shared.clone()))
    }

    /// Serves on a socket that is already bound and listening, such as one
    /// handed over by launchd or systemd.
    pub fn listen(
        &self,
        listener: std::net::TcpListener,
        options: ListenerOptions,
    ) -> io::Result<Listener> {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        Ok(Listener::new(listener, options, self.shared.clone()))
    }
}
