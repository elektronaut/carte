use std::path::{Path, PathBuf};
use std::process::Command;

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use super::{CertificateAuthority, TlsError, write_atomic};

/// Trust files handed to app processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustFiles {
    /// System roots plus the carte CA.
    pub bundle: PathBuf,
    /// The carte CA alone.
    pub ca: PathBuf,
}

impl TrustFiles {
    pub fn env(&self) -> [(&'static str, &Path); 3] {
        [
            ("SSL_CERT_FILE", &self.bundle),
            ("REQUESTS_CA_BUNDLE", &self.bundle),
            ("NODE_EXTRA_CA_CERTS", &self.ca),
        ]
    }
}

/// Rebuilds the bundle from the system's roots; call at daemon start.
pub fn rebuild(state_dir: &Path, ca: &CertificateAuthority) -> Result<TrustFiles, TlsError> {
    write_bundle(state_dir, &system_roots_pem()?, ca)
}

pub fn write_bundle(
    state_dir: &Path,
    system_roots: &str,
    ca: &CertificateAuthority,
) -> Result<TrustFiles, TlsError> {
    let bundle = state_dir.join("ca-bundle.pem");
    write_atomic(
        &bundle,
        build_bundle(system_roots, ca.cert_pem())?.as_bytes(),
        0o644,
    )?;
    Ok(TrustFiles {
        bundle,
        ca: ca.cert_path().to_owned(),
    })
}

fn build_bundle(system_roots: &str, ca_pem: &str) -> Result<String, TlsError> {
    let roots = CertificateDer::pem_slice_iter(system_roots.as_bytes())
        .filter(Result::is_ok)
        .count();
    if roots == 0 {
        return Err(TlsError::NoSystemRoots);
    }
    let mut bundle = String::with_capacity(system_roots.len() + ca_pem.len() + 1);
    bundle.push_str(system_roots.trim_end());
    bundle.push('\n');
    bundle.push_str(ca_pem.trim_end());
    bundle.push('\n');
    Ok(bundle)
}

#[cfg(target_os = "macos")]
pub fn system_roots_pem() -> Result<String, TlsError> {
    let keychain = "/System/Library/Keychains/SystemRootCertificates.keychain";
    let args = ["find-certificate", "-a", "-p", keychain];
    let command = format!("security {}", args.join(" "));
    let output = Command::new("security")
        .args(args)
        .output()
        .map_err(|e| TlsError::Command {
            command: command.clone(),
            detail: e.to_string(),
        })?;
    if !output.status.success() {
        return Err(TlsError::Command {
            command,
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    String::from_utf8(output.stdout).map_err(|e| TlsError::Command {
        command,
        detail: e.to_string(),
    })
}

#[cfg(not(target_os = "macos"))]
pub fn system_roots_pem() -> Result<String, TlsError> {
    [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
    ]
    .iter()
    .find_map(|path| std::fs::read_to_string(path).ok())
    .ok_or(TlsError::NoSystemRoots)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca(dir: &Path) -> CertificateAuthority {
        CertificateAuthority::load_or_create(dir, "carte test CA", &["test"]).unwrap()
    }

    fn fake_root(dir: &Path) -> String {
        ca(dir).cert_pem().to_owned()
    }

    #[test]
    fn combines_system_roots_and_the_ca() {
        let state = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let ca = ca(state.path());
        let root = fake_root(other.path());

        let files = write_bundle(state.path(), &format!("{}\n\n", root.trim_end()), &ca).unwrap();
        assert_eq!(files.bundle, state.path().join("ca-bundle.pem"));
        assert_eq!(files.ca, state.path().join("ca.pem"));

        let bundle = std::fs::read(&files.bundle).unwrap();
        let certs: Vec<_> = CertificateDer::pem_slice_iter(&bundle)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(certs.len(), 2);
        assert_eq!(&certs[1], ca.cert_der());
        assert_eq!(
            files.env().map(|(name, _)| name),
            ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "NODE_EXTRA_CA_CERTS"]
        );
    }

    #[test]
    fn replaces_an_existing_bundle() {
        let state = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let ca = ca(state.path());
        std::fs::write(state.path().join("ca-bundle.pem"), "stale").unwrap();
        let files = write_bundle(state.path(), &fake_root(other.path()), &ca).unwrap();
        assert!(
            !std::fs::read_to_string(files.bundle)
                .unwrap()
                .contains("stale")
        );
        let leftovers: Vec<_> = std::fs::read_dir(state.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn refuses_empty_system_roots() {
        let state = tempfile::tempdir().unwrap();
        let ca = ca(state.path());
        assert!(write_bundle(state.path(), "", &ca).is_err());
        assert!(!state.path().join("ca-bundle.pem").exists());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn reads_the_macos_system_roots() {
        let roots = system_roots_pem().unwrap();
        let count = CertificateDer::pem_slice_iter(roots.as_bytes())
            .filter(Result::is_ok)
            .count();
        assert!(count > 50, "{count}");
    }
}
