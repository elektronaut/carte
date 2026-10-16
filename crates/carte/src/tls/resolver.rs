use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use time::{Duration, OffsetDateTime};

use super::{CertificateAuthority, TlsError};

/// Issues and caches a leaf certificate per SNI host under the CA's TLDs.
pub struct CertResolver {
    ca: Arc<CertificateAuthority>,
    provider: Arc<CryptoProvider>,
    cache: Mutex<LruCache<String, CachedLeaf>>,
}

#[derive(Clone)]
struct CachedLeaf {
    key: Arc<CertifiedKey>,
    renew_at: OffsetDateTime,
}

impl CertResolver {
    pub fn new(ca: Arc<CertificateAuthority>) -> Self {
        Self {
            ca,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(4096).unwrap())),
        }
    }

    /// The certificate for `host`, issued on first use. `None` outside the CA's TLDs.
    pub fn certified_key(&self, host: &str) -> Option<Arc<CertifiedKey>> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if !self.ca.covers(&host) {
            return None;
        }
        let now = OffsetDateTime::now_utc();
        if let Some(cached) = self.cache().get(&host)
            && cached.renew_at > now
        {
            return Some(cached.key.clone());
        }
        let key = match self.issue(&host) {
            Ok(key) => key,
            Err(e) => {
                tracing::error!(%host, error = %e, "tls: could not issue certificate");
                return None;
            }
        };
        self.cache().put(host, key.clone());
        Some(key.key)
    }

    fn issue(&self, host: &str) -> Result<CachedLeaf, TlsError> {
        let leaf = self.ca.issue(host)?;
        let signing_key = self
            .provider
            .key_provider
            .load_private_key(leaf.key_der())?;
        let key = CertifiedKey::new(vec![leaf.cert.der().clone()], signing_key);
        Ok(CachedLeaf {
            key: Arc::new(key),
            renew_at: leaf.not_after - Duration::days(30),
        })
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, LruCache<String, CachedLeaf>> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.certified_key(client_hello.server_name()?)
    }
}

impl fmt::Debug for CertResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertResolver")
            .field("tlds", &self.ca.tlds())
            .finish_non_exhaustive()
    }
}

/// A server config for the HTTPS proxy, with ALPN `http/1.1`.
pub fn server_config(resolver: Arc<CertResolver>) -> Result<ServerConfig, TlsError> {
    let mut config = ServerConfig::builder_with_provider(resolver.provider.clone())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, RootCertStore};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    fn resolver(dir: &std::path::Path) -> Arc<CertResolver> {
        let ca = CertificateAuthority::load_or_create(dir, "carte test CA", &["test"]).unwrap();
        Arc::new(CertResolver::new(Arc::new(ca)))
    }

    fn client_config(resolver: &CertResolver) -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        roots.add(resolver.ca.cert_der().clone()).unwrap();
        let mut config = ClientConfig::builder_with_provider(resolver.provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(config)
    }

    async fn serve(resolver: Arc<CertResolver>) -> std::net::SocketAddr {
        let acceptor = TlsAcceptor::from(Arc::new(server_config(resolver).unwrap()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(stream).await {
                        let _ = tls.write_all(b"hello").await;
                        let _ = tls.shutdown().await;
                    }
                });
            }
        });
        addr
    }

    async fn connect(
        addr: std::net::SocketAddr,
        config: Arc<ClientConfig>,
        host: &str,
    ) -> std::io::Result<Vec<u8>> {
        let stream = TcpStream::connect(addr).await?;
        let name = ServerName::try_from(host.to_owned()).unwrap();
        let mut tls = TlsConnector::from(config).connect(name, stream).await?;
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
        let mut body = Vec::new();
        tls.read_to_end(&mut body).await?;
        Ok(body)
    }

    #[tokio::test]
    async fn completes_handshakes_for_hosts_under_the_tld() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = resolver(tmp.path());
        let config = client_config(&resolver);
        let addr = serve(resolver).await;
        for host in ["app.make.test", "oslo.feature-x.delegatbase.test"] {
            let body = connect(addr, config.clone(), host).await.unwrap();
            assert_eq!(body, b"hello", "{host}");
        }
    }

    #[tokio::test]
    async fn fails_handshakes_outside_the_tld() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = resolver(tmp.path());
        let config = client_config(&resolver);
        let addr = serve(resolver).await;
        assert!(connect(addr, config, "example.com").await.is_err());
    }

    #[test]
    fn caches_leaves_per_host() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = resolver(tmp.path());
        let first = resolver.certified_key("app.make.test").unwrap();
        let again = resolver.certified_key("App.Make.Test.").unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        let other = resolver.certified_key("api.make.test").unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
        assert!(resolver.certified_key("example.com").is_none());
        assert_eq!(resolver.cache().len(), 2);
    }

    #[test]
    fn reissues_leaves_close_to_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = resolver(tmp.path());
        let first = resolver.certified_key("app.make.test").unwrap();
        resolver.cache().get_mut("app.make.test").unwrap().renew_at =
            OffsetDateTime::now_utc() - Duration::seconds(1);
        let second = resolver.certified_key("app.make.test").unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
    }
}
