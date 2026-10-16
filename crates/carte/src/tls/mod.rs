pub mod bundle;
pub mod ca;
pub mod resolver;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub use bundle::TrustFiles;
pub use ca::{CaOrigin, CertificateAuthority, CreateReason};
pub use resolver::{CertResolver, server_config};

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("could not {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("certificate generation failed: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("TLS configuration failed: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("invalid TLD {0:?}")]
    InvalidTld(String),
    #[error("no TLDs configured")]
    NoTlds,
    #[error("no system root certificates found")]
    NoSystemRoots,
    #[error("`{command}` failed: {detail}")]
    Command { command: String, detail: String },
    #[error("HOME is not set")]
    NoHome,
}

impl TlsError {
    fn io(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_owned();
        move |source| Self::Io {
            action,
            path,
            source,
        }
    }
}

/// `~/.local/state/carte`
pub fn default_state_dir() -> Result<PathBuf, TlsError> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or(TlsError::NoHome)?;
    Ok(PathBuf::from(home).join(".local/state/carte"))
}

pub(crate) fn ensure_private_dir(dir: &Path) -> Result<(), TlsError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(TlsError::io("create", dir))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(TlsError::io("set permissions on", dir))
}

pub(crate) fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<(), TlsError> {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{file_name}.tmp"));
    let _ = fs::remove_file(&tmp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(TlsError::io("create", &tmp))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(TlsError::io("write", &tmp))?;
    fs::rename(&tmp, path).map_err(TlsError::io("replace", path))
}

pub(crate) fn normalize_tlds<S: AsRef<str>>(tlds: &[S]) -> Result<Vec<String>, TlsError> {
    let mut normalized = tlds
        .iter()
        .map(|tld| {
            let tld = tld.as_ref().trim_matches('.').to_ascii_lowercase();
            rustls::pki_types::DnsName::try_from(tld.as_str())
                .map_err(|_| TlsError::InvalidTld(tld.clone()))?;
            Ok(tld)
        })
        .collect::<Result<Vec<_>, TlsError>>()?;
    normalized.sort();
    normalized.dedup();
    if normalized.is_empty() {
        return Err(TlsError::NoTlds);
    }
    Ok(normalized)
}

pub(crate) fn is_within(host: &str, tld: &str) -> bool {
    host.strip_suffix(tld)
        .is_some_and(|rest| rest.is_empty() || rest.ends_with('.'))
}
