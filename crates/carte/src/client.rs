//! Talks to the daemon over its Unix socket.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use bytes::{Buf, Bytes, BytesMut};
use carte_api::ErrorBody;
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

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

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> anyhow::Result<http::Response<Incoming>> {
        let stream = UnixStream::connect(&self.socket).await.map_err(|err| {
            anyhow::anyhow!(
                "the carte daemon isn't running ({}: {err}); start it with `carte daemon`",
                self.socket.display()
            )
        })?;
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

async fn decode<T: DeserializeOwned>(response: http::Response<Incoming>) -> anyhow::Result<T> {
    let status = response.status();
    let body = response.into_body().collect().await?.aggregate();
    let bytes = body.chunk().to_vec();
    if status != StatusCode::OK {
        if let Ok(error) = serde_json::from_slice::<ErrorBody>(&bytes) {
            bail!("{}", error.error);
        }
        bail!("the daemon answered {status}");
    }
    serde_json::from_slice(&bytes).context("reading the daemon's answer")
}
