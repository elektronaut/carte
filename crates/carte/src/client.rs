//! Talks to the daemon over its Unix socket.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use bytes::{Bytes, BytesMut};
use carte_api::ErrorBody;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

use crate::exit::{Exit, Failure};

pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(socket: &Path) -> Self {
        Client {
            socket: socket.to_path_buf(),
        }
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let response = self.send(Method::GET, path, Bytes::new()).await?;
        decode(response).await
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let response = self
            .send(Method::POST, path, Bytes::from(serde_json::to_vec(body)?))
            .await?;
        decode(response).await
    }

    /// Posts `body` and calls `each` for every line of a newline-delimited JSON
    /// response, until the daemon ends it.
    pub async fn stream<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        mut each: impl FnMut(T),
    ) -> anyhow::Result<()> {
        let bytes = match body {
            Some(body) => Bytes::from(serde_json::to_vec(body)?),
            None => Bytes::new(),
        };
        let response = self.send(method, path, bytes).await?;
        if !response.status().is_success() {
            return decode::<serde_json::Value>(response).await.map(|_| ());
        }
        let mut body = response.into_body();
        let mut buf = BytesMut::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame?.into_data() {
                buf.extend_from_slice(&data);
            }
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line = buf.split_to(pos + 1);
                each(serde_json::from_slice(&line[..pos])?);
            }
        }
        Ok(())
    }

    /// Posts `body` asking to switch the connection to `protocol`, and returns the
    /// connection once the daemon agrees.
    pub async fn upgrade<B: Serialize>(
        &self,
        path: &str,
        protocol: &str,
        body: &B,
    ) -> anyhow::Result<TokioIo<Upgraded>> {
        let stream = self.connect().await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(connection.with_upgrades());
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("host", "carte")
            .header("content-type", "application/json")
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, protocol)
            .body(Full::new(Bytes::from(serde_json::to_vec(body)?)))?;
        let response = sender.send_request(request).await?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return decode::<serde_json::Value>(response).await.and_then(|_| {
                bail!("the daemon didn't switch to {protocol}");
            });
        }
        Ok(TokioIo::new(hyper::upgrade::on(response).await?))
    }

    async fn connect(&self) -> anyhow::Result<UnixStream> {
        UnixStream::connect(&self.socket)
            .await
            .map_err(|err| unreachable(&self.socket, &err).into())
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> anyhow::Result<http::Response<Incoming>> {
        let stream = self.connect().await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(connection);
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "carte")
            .header("content-type", "application/json")
            .body(Full::new(body))?;
        Ok(sender.send_request(request).await?)
    }
}

/// Why the daemon couldn't be reached. A socket that refuses connections belongs
/// to a daemon that is starting or stopping, or to one that crashed.
fn unreachable(socket: &Path, err: &std::io::Error) -> Failure {
    let message = if err.kind() == std::io::ErrorKind::ConnectionRefused && socket.exists() {
        format!(
            "the carte daemon is starting or stopping ({}: {err}); try again in a moment, and if this persists it may have crashed: start it with `carte daemon`",
            socket.display()
        )
    } else {
        format!(
            "the carte daemon isn't running ({}: {err}); start it with `carte daemon`",
            socket.display()
        )
    };
    Failure::new(Exit::DaemonDown, message)
}

async fn decode<T: DeserializeOwned>(response: http::Response<Incoming>) -> anyhow::Result<T> {
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    if status != StatusCode::OK {
        if let Ok(error) = serde_json::from_slice::<ErrorBody>(&bytes) {
            if status == StatusCode::NOT_FOUND {
                return Err(Failure::new(Exit::NotFound, error.error).into());
            }
            bail!("{}", error.error);
        }
        bail!("the daemon answered {status}");
    }
    serde_json::from_slice(&bytes).context("reading the daemon's answer")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_socket_nobody_listens_on_means_starting_or_stopping() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("carte.sock");
        leave_a_socket_nobody_listens_on(&socket);

        let err = Client::new(&socket)
            .get::<serde_json::Value>("/")
            .await
            .unwrap_err();
        let failure = err.downcast_ref::<Failure>().unwrap();
        assert_eq!(failure.exit, Exit::DaemonDown);
        assert!(failure.message.contains("is starting or stopping"), "{err}");

        std::fs::remove_file(&socket).unwrap();
        let err = Client::new(&socket)
            .get::<serde_json::Value>("/")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("isn't running"), "{err}");
    }

    /// Leaves a socket file at `path` that nothing listens on, bound by a forked
    /// child that exits, so no process another test spawns can inherit it.
    fn leave_a_socket_nobody_listens_on(path: &Path) {
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, socket};
        use nix::sys::wait::{WaitStatus, waitpid};
        use nix::unistd::{ForkResult, fork};

        let addr = UnixAddr::new(path).unwrap();
        match unsafe { fork() }.unwrap() {
            ForkResult::Child => {
                let bound = socket(
                    AddressFamily::Unix,
                    SockType::Stream,
                    SockFlag::empty(),
                    None,
                )
                .and_then(|fd| bind(std::os::fd::AsRawFd::as_raw_fd(&fd), &addr));
                unsafe { libc::_exit(i32::from(bound.is_err())) }
            }
            ForkResult::Parent { child } => {
                assert_eq!(waitpid(child, None).unwrap(), WaitStatus::Exited(child, 0));
            }
        }
    }

    #[tokio::test]
    async fn reads_bodies_larger_than_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let big: Vec<String> = (0..20_000).map(|i| format!("item {i}")).collect();
        let body = big.clone();
        let app = axum::Router::new().route(
            "/big",
            axum::routing::get(move || async move { axum::Json(body) }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await });

        let got: Vec<String> = Client::new(&socket).get("/big").await.unwrap();
        assert_eq!(got, big);
    }
}
