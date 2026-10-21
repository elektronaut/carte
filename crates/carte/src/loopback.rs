//! Connections to loopback ports.

use std::io;
use std::net::SocketAddr;
use std::os::fd::AsFd;

use nix::sys::socket::{setsockopt, sockopt};

/// Whether a connection is to itself. On Linux, connecting to a loopback port
/// nobody listens on can succeed when the kernel picks that same port as the
/// source port; that isn't a listener. Such a socket is set to close with a
/// reset, so it doesn't leave the port in TIME_WAIT for the app that binds it.
pub fn connected_to_itself(
    socket: &impl AsFd,
    local: io::Result<SocketAddr>,
    peer: io::Result<SocketAddr>,
) -> bool {
    let (Ok(local), Ok(peer)) = (local, peer) else {
        return false;
    };
    if local != peer {
        return false;
    }
    let reset = nix::libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    let _ = setsockopt(socket, sockopt::Linger, &reset);
    true
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, TcpListener};
    use std::time::Duration;

    use super::*;

    /// Connects a socket bound to a free port to that same port. Linux allows it;
    /// macOS refuses, and then there is nothing to test.
    async fn self_connection() -> Option<(u16, tokio::net::TcpStream)> {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket
            .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .unwrap();
        let addr = socket.local_addr().unwrap();
        let port = addr.port();
        let stream = socket.connect(addr).await;
        if cfg!(not(target_os = "linux")) && stream.is_err() {
            return None;
        }
        Some((port, stream.unwrap()))
    }

    #[tokio::test]
    async fn a_connection_to_itself_isnt_a_listener() {
        let Some((_, stream)) = self_connection().await else {
            return;
        };
        assert!(connected_to_itself(
            &stream,
            stream.local_addr(),
            stream.peer_addr()
        ));
    }

    #[tokio::test]
    async fn the_port_is_free_again_once_a_connection_to_itself_is_dropped() {
        let Some((port, stream)) = self_connection().await else {
            return;
        };
        assert!(connected_to_itself(
            &stream,
            stream.local_addr(),
            stream.peer_addr()
        ));
        drop(stream);
        // A child forked by a parallel test can hold the socket for a few
        // milliseconds; a port left in TIME_WAIT would stay taken for a minute.
        let started = std::time::Instant::now();
        loop {
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            socket.set_reuseaddr(true).unwrap();
            let bound = socket
                .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
                .and_then(|()| socket.listen(1));
            match bound {
                Ok(_) => break,
                Err(err) if started.elapsed() < Duration::from_secs(2) => {
                    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(err) => panic!("port {port} still taken: {err}"),
            }
        }
    }

    #[test]
    fn other_connections_are_left_alone() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        assert!(!connected_to_itself(
            &stream,
            stream.local_addr(),
            stream.peer_addr()
        ));
    }
}
