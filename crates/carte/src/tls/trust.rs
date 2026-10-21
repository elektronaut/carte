use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};

use super::{CertificateAuthority, TlsError};

/// A command to run, kept as data so it can be shown before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandLine {
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|&arg| arg.to_owned()).collect(),
        }
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command
    }
}

impl fmt::Display for CommandLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let words = std::iter::once(&self.program).chain(&self.args);
        for (i, word) in words.enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            let plain = !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:@+,".contains(c));
            if plain {
                f.write_str(word)?;
            } else {
                write!(f, "'{}'", word.replace('\'', r"'\''"))?;
            }
        }
        Ok(())
    }
}

/// Adds the CA to the macOS system keychain as a trusted root. Needs root.
pub fn trust_command(ca_cert: &Path) -> CommandLine {
    let ca_cert = ca_cert.to_string_lossy();
    CommandLine::new(
        "security",
        &[
            "add-trusted-cert",
            "-d",
            "-r",
            "trustRoot",
            "-k",
            SYSTEM_KEYCHAIN,
            &ca_cert,
        ],
    )
}

pub const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";

/// A carte CA in the system keychain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainCa {
    pub hash: String,
    pub label: String,
}

/// carte's CAs in the system keychain, every user's and old ones included.
pub fn keychain_cas() -> Vec<KeychainCa> {
    CommandLine::new(
        "security",
        &[
            "find-certificate",
            "-a",
            "-c",
            "carte local CA",
            "-Z",
            SYSTEM_KEYCHAIN,
        ],
    )
    .command()
    .stdin(Stdio::null())
    .stderr(Stdio::null())
    .output()
    .map(|output| parse_listing(&String::from_utf8_lossy(&output.stdout)))
    .unwrap_or_default()
}

fn parse_listing(listing: &str) -> Vec<KeychainCa> {
    let mut cas: Vec<KeychainCa> = Vec::new();
    for line in listing.lines() {
        if let Some(hash) = line.strip_prefix("SHA-256 hash: ") {
            cas.push(KeychainCa {
                hash: hash.trim().to_ascii_uppercase(),
                label: String::new(),
            });
        } else if let Some(label) = line.trim_start().strip_prefix("\"labl\"<blob>=")
            && let Some(ca) = cas.last_mut()
        {
            ca.label = label.trim().trim_matches('"').to_owned();
        }
    }
    cas
}

/// The keychain CAs this install created: those whose hashes `created` lists, or, when
/// the state dir has no record yet, those named exactly `common_name`.
pub fn own_cas(
    listed: Vec<KeychainCa>,
    created: Option<&[String]>,
    common_name: &str,
) -> Vec<KeychainCa> {
    listed
        .into_iter()
        .filter(|ca| match created {
            Some(hashes) => hashes.contains(&ca.hash),
            None => ca.label == common_name,
        })
        .collect()
}

/// Whether macOS trusts a leaf issued by the CA, checked read-only with `security verify-cert`.
pub fn is_trusted(ca: &CertificateAuthority) -> Result<bool, TlsError> {
    verify_leaf(ca, &format!("carte-trust-check.{}", ca.tlds()[0]), None)
}

fn verify_leaf(
    ca: &CertificateAuthority,
    host: &str,
    extra_root: Option<&Path>,
) -> Result<bool, TlsError> {
    let leaf = ca.issue(host)?;
    let dir = tempfile::tempdir().map_err(TlsError::io("create", &std::env::temp_dir()))?;
    let leaf_path = dir.path().join("leaf.pem");
    std::fs::write(&leaf_path, leaf.cert.pem()).map_err(TlsError::io("write", &leaf_path))?;

    let leaf_path = leaf_path.to_string_lossy();
    let ca_path = ca.cert_path().to_string_lossy();
    let mut args = vec![
        "verify-cert",
        "-q",
        "-L",
        "-p",
        "ssl",
        "-n",
        host,
        "-c",
        &leaf_path,
        "-c",
        &ca_path,
    ];
    let extra_root = extra_root.map(Path::to_string_lossy);
    if let Some(root) = &extra_root {
        args.extend(["-r", root]);
    }
    let command = CommandLine::new("security", &args);
    let status = command
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| TlsError::Command {
            command: command.to_string(),
            detail: e.to_string(),
        })?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn describes_the_trust_command() {
        let command = trust_command(&PathBuf::from("/Users/me/.local/state/carte/ca.pem"));
        assert_eq!(
            command.to_string(),
            "security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain /Users/me/.local/state/carte/ca.pem"
        );
        let spaced = trust_command(&PathBuf::from("/Users/O'Brien/My State/ca.pem"));
        assert!(
            spaced
                .to_string()
                .ends_with(r"'/Users/O'\''Brien/My State/ca.pem'")
        );
        assert_eq!(
            spaced.args.last().unwrap(),
            "/Users/O'Brien/My State/ca.pem"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn reports_an_untrusted_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let ca =
            CertificateAuthority::load_or_create(tmp.path(), "carte test CA", &["test"]).unwrap();
        assert!(!is_trusted(&ca).unwrap());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_honours_the_name_constraints() {
        let tmp = tempfile::tempdir().unwrap();
        let ca =
            CertificateAuthority::load_or_create(tmp.path(), "carte test CA", &["test"]).unwrap();
        let root = Some(ca.cert_path());
        assert!(verify_leaf(&ca, "app.nova.test", root).unwrap());
        assert!(verify_leaf(&ca, "oslo.feature-x.eventportal.test", root).unwrap());
        assert!(!verify_leaf(&ca, "example.com", root).unwrap());
    }

    const LISTING: &str = "SHA-256 hash: 1D2D\nSHA-1 hash: 34FB\nkeychain: \"/Library/Keychains/System.keychain\"\nattributes:\n    \"alis\"<blob>=\"carte local CA (alice@mac)\"\n    \"labl\"<blob>=\"carte local CA (alice@Mac)\"\nSHA-256 hash: c544\nSHA-1 hash: 0112\nattributes:\n    \"labl\"<blob>=\"carte local CA (bob@Mac)\"\n";

    fn ca(hash: &str, user: &str) -> KeychainCa {
        KeychainCa {
            hash: hash.into(),
            label: format!("carte local CA ({user}@Mac)"),
        }
    }

    #[test]
    fn reads_hashes_and_labels_from_a_keychain_listing() {
        assert_eq!(
            parse_listing(LISTING),
            [ca("1D2D", "alice"), ca("C544", "bob")]
        );
    }

    #[test]
    fn keeps_only_the_cas_this_install_created() {
        let listed = parse_listing(LISTING);
        let alice = "carte local CA (alice@Mac)";
        let created = ["1D2D".to_string(), "9F00".to_string()];
        assert_eq!(
            own_cas(listed.clone(), Some(&created), alice),
            [ca("1D2D", "alice")]
        );
        assert_eq!(
            own_cas(listed.clone(), Some(&["C544".to_string()]), alice),
            [ca("C544", "bob")]
        );
        assert_eq!(own_cas(listed.clone(), None, alice), [ca("1D2D", "alice")]);
        assert!(own_cas(listed, None, "carte local CA (alice@Ma").is_empty());
    }
}
