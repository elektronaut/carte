//! A loopback listener per Service: every connection to it opens its own port-forward
//! stream to the pod, so nothing long-lived breaks when the laptop sleeps or a pod is
//! replaced.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use carte_api::EventKind;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use super::cluster::{ApiError, Cluster, PodStream};
use super::lookup::{self, PodTarget, PortInfo};

/// Reports what a forward does, to the event log and the instance's status.
pub trait Reporter: Send + Sync + 'static {
    fn event(&self, kind: EventKind, message: String);
    /// Why the last connection failed, or `None` once one works; a new reason is
    /// also an event.
    fn error(&self, error: Option<String>);
}

/// Forwards one Service's connections to a pod behind it.
pub struct Forward {
    pub cluster: Arc<dyn Cluster>,
    pub namespace: String,
    pub service: String,
    pub port: PortInfo,
    pub target: Mutex<PodTarget>,
    pub cancel: CancellationToken,
    pub reporter: Box<dyn Reporter>,
}

impl Forward {
    pub fn target(&self) -> PodTarget {
        self.target.lock().expect("target lock").clone()
    }

    /// Binds a loopback port and serves it until cancelled.
    pub async fn listen(self: Arc<Self>) -> std::io::Result<u16> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let port = listener.local_addr()?.port();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = self.cancel.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else {
                    continue;
                };
                let forward = self.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = forward.cancel.cancelled() => {}
                        _ = forward.serve(stream) => {}
                    }
                });
            }
        });
        Ok(port)
    }

    async fn serve(&self, mut local: TcpStream) {
        let mut pod = match self.open().await {
            Ok(pod) => pod,
            Err(err) => {
                self.reporter.error(Some(err.summary()));
                let _ = local.shutdown().await;
                return;
            }
        };
        self.reporter.error(None);
        let _ = tokio::io::copy_bidirectional(&mut local, &mut pod).await;
    }

    /// A stream to the current pod; when that fails, to another ready pod.
    async fn open(&self) -> Result<PodStream, ApiError> {
        let target = self.target();
        let first = match self.connect(&target).await {
            Ok(stream) => return Ok(stream),
            Err(err) => err,
        };
        if matches!(first, ApiError::Auth { .. } | ApiError::Forbidden(_)) {
            return Err(first);
        }
        let slices = self
            .cluster
            .endpoint_slices(&self.namespace, &self.service)
            .await?;
        let next = lookup::choose_pod(&self.port, &slices, None)
            .map_err(|err| ApiError::Other(err.title().to_string()))?;
        if next.pod != target.pod {
            self.reporter.event(
                EventKind::Restarted,
                format!("pod {} is gone; now pod {}", target.pod, next.pod),
            );
            *self.target.lock().expect("target lock") = next.clone();
        }
        self.connect(&next).await
    }

    async fn connect(&self, target: &PodTarget) -> Result<PodStream, ApiError> {
        let stream = self
            .cluster
            .connect(&self.namespace, &target.pod, target.port)
            .await?;
        if !target.tls {
            return Ok(stream);
        }
        let name = ServerName::try_from(self.service.clone())
            .unwrap_or_else(|_| ServerName::try_from("localhost").expect("valid name"));
        let tls = tls_connector()
            .connect(name, stream)
            .await
            .map_err(|err| ApiError::Other(format!("TLS to pod {}: {err}", target.pod)))?;
        Ok(Box::new(tls))
    }
}

fn tls_connector() -> tokio_rustls::TlsConnector {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate))
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// Accepts whatever certificate a pod presents: the connection already runs inside the
/// API server's authenticated port-forward, and pods' certificates rarely match.
#[derive(Debug)]
struct AnyCertificate;

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
