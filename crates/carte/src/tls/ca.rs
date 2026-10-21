use std::fmt;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, PKCS_ECDSA_P256_SHA256, SanType,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

use super::{TlsError, ensure_private_dir, is_within, lock_ca, normalize_tlds, write_atomic};

/// The local CA, constrained to the TLDs it was created for.
pub struct CertificateAuthority {
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    cert_path: PathBuf,
    issuer: Issuer<'static, KeyPair>,
    tlds: Vec<String>,
    origin: CaOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaOrigin {
    Loaded,
    /// Loaded, though it can also sign names under `extra`, TLDs carte no longer serves.
    Wider {
        extra: Vec<String>,
    },
    Created(CreateReason),
}

/// Why a new CA was generated. Anything but `Missing` replaces a CA that may already be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateReason {
    Missing,
    Invalid(String),
    Expiring,
    TldsChanged {
        previous: Vec<String>,
    },
    /// It isn't limited to server authentication, as CAs made before carte trusted them
    /// for TLS only. Only setup replaces such a CA.
    NotTlsOnly,
    /// Not a reason to create one: `ca.pem` doesn't hold the certificate paired with the
    /// key, after a crash between the writes, and loading the CA rewrites it.
    CertFileStale,
}

pub(crate) struct IssuedLeaf {
    pub cert: rcgen::Certificate,
    pub key: KeyPair,
    pub not_after: OffsetDateTime,
}

impl CertificateAuthority {
    /// Loads the CA from `state_dir`, or creates one when it is missing, unreadable, close to
    /// expiry or doesn't cover every TLD. A CA that also covers other TLDs is kept, as
    /// `CaOrigin::Wider`, so a daemon restart doesn't swap out a trusted CA.
    pub fn load_or_create<S: AsRef<str>>(
        state_dir: &Path,
        common_name: &str,
        tlds: &[S],
    ) -> Result<Self, TlsError> {
        Self::load_or_create_matching(state_dir, common_name, tlds, false)
    }

    /// Like `load_or_create`, but also replaces a CA that covers more than `tlds`.
    pub fn load_exact_or_create<S: AsRef<str>>(
        state_dir: &Path,
        common_name: &str,
        tlds: &[S],
    ) -> Result<Self, TlsError> {
        Self::load_or_create_matching(state_dir, common_name, tlds, true)
    }

    fn load_or_create_matching<S: AsRef<str>>(
        state_dir: &Path,
        common_name: &str,
        tlds: &[S],
        exact: bool,
    ) -> Result<Self, TlsError> {
        let tlds = normalize_tlds(tlds)?;
        ensure_private_dir(state_dir)?;
        let _lock = lock_ca(state_dir)?;
        let reason = match Self::load(state_dir, &tlds, exact) {
            Ok((ca, cert_file_current)) => {
                if !cert_file_current {
                    write_atomic(&ca.cert_path, ca.cert_pem.as_bytes(), 0o644)?;
                }
                return Ok(ca);
            }
            Err(reason) => reason,
        };
        Self::create(state_dir, common_name, tlds, reason)
    }

    /// Loads the CA from `state_dir` without creating or replacing anything; the error
    /// says why `load_exact_or_create` would create a new one.
    pub fn load_existing<S: AsRef<str>>(
        state_dir: &Path,
        tlds: &[S],
    ) -> Result<Self, CreateReason> {
        let tlds = normalize_tlds(tlds).map_err(|e| CreateReason::Invalid(e.to_string()))?;
        match Self::load(state_dir, &tlds, true)? {
            (ca, true) => Ok(ca),
            (_, false) => Err(CreateReason::CertFileStale),
        }
    }

    pub fn cert_path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("ca.pem")
    }

    fn key_path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("ca-key.pem")
    }

    /// The CAs this state dir has had, one `<kind> <SHA-256>` line each: `created` for one
    /// made here, `replaced` for one found here and replaced.
    pub fn created_path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("ca-created")
    }

    /// The uppercase SHA-256 hashes of the CAs recorded in `state_dir` and of the one in
    /// it now, which covers a CA made before carte kept the record.
    pub fn created_hashes(state_dir: &Path) -> Vec<String> {
        let recorded = fs::read_to_string(Self::created_path_in(state_dir)).unwrap_or_default();
        let mut hashes: Vec<String> = recorded_hashes(&recorded)
            .chain(current_hash(state_dir))
            .collect();
        hashes.sort();
        hashes.dedup();
        hashes
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    pub fn tlds(&self) -> &[String] {
        &self.tlds
    }

    pub fn origin(&self) -> &CaOrigin {
        &self.origin
    }

    /// Whether `host` (lowercase, no trailing dot) is at or below one of the CA's TLDs.
    pub fn covers(&self, host: &str) -> bool {
        self.tlds.iter().any(|tld| is_within(host, tld))
    }

    /// The CA, and whether `ca.pem` holds its certificate. `ca-key.pem` holds the key and,
    /// since carte wrote both in one file, the certificate too, which then wins over a
    /// `ca.pem` left behind by a crash.
    fn load(state_dir: &Path, tlds: &[String], exact: bool) -> Result<(Self, bool), CreateReason> {
        let cert_path = Self::cert_path_in(state_dir);
        let key_path = Self::key_path_in(state_dir);
        let read = |path: &Path| match fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CreateReason::Invalid(e.to_string())),
        };
        let key_text = read(&key_path)?.ok_or(CreateReason::Missing)?;
        let cert_file = read(&cert_path)?;
        let cert_pem = match pem_block(&key_text, "CERTIFICATE") {
            Some(paired) => format!("{paired}\n"),
            None => cert_file.clone().ok_or(CreateReason::Missing)?,
        };
        let cert_file_current = cert_file.as_deref().map(str::trim) == Some(cert_pem.trim());
        let invalid = |what: &str| CreateReason::Invalid(what.to_owned());

        let key_pem =
            pem_block(&key_text, "PRIVATE KEY").ok_or_else(|| invalid("unreadable key"))?;
        let key = KeyPair::from_pem(key_pem).map_err(|_| invalid("unreadable key"))?;
        let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|_| invalid("unreadable certificate"))?;
        let (_, x509) =
            X509Certificate::from_der(&cert_der).map_err(|_| invalid("unparsable certificate"))?;

        if x509.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
            return Err(invalid("key does not match certificate"));
        }
        let is_ca = x509
            .basic_constraints()
            .ok()
            .flatten()
            .is_some_and(|ext| ext.value.ca);
        if !is_ca {
            return Err(invalid("not a CA certificate"));
        }
        let renew_at = OffsetDateTime::now_utc() + Duration::days(30);
        if x509.validity().not_after.to_datetime() < renew_at {
            return Err(CreateReason::Expiring);
        }
        let previous = permitted_dns_names(&x509);
        let extra: Vec<String> = previous
            .iter()
            .filter(|tld| !tlds.contains(tld))
            .cloned()
            .collect();
        let covers_all = tlds.iter().all(|tld| previous.contains(tld));
        if !covers_all || (exact && !extra.is_empty()) {
            return Err(CreateReason::TldsChanged { previous });
        }
        if exact && !tls_only(&x509) {
            return Err(CreateReason::NotTlsOnly);
        }
        let origin = if extra.is_empty() {
            CaOrigin::Loaded
        } else {
            CaOrigin::Wider { extra }
        };

        let issuer = Issuer::from_ca_cert_der(&cert_der, key)
            .map_err(|_| invalid("unusable certificate"))?;
        let ca = Self {
            cert_pem,
            cert_der,
            cert_path,
            issuer,
            tlds: tlds.to_vec(),
            origin,
        };
        Ok((ca, cert_file_current))
    }

    fn create(
        state_dir: &Path,
        common_name: &str,
        tlds: Vec<String>,
        reason: CreateReason,
    ) -> Result<Self, TlsError> {
        let lifetime = Duration::days(3650);
        Self::create_for(state_dir, common_name, tlds, reason, lifetime)
    }

    /// Writes a CA that expires after `lifetime`; tests use a short one.
    pub(crate) fn create_for(
        state_dir: &Path,
        common_name: &str,
        tlds: Vec<String>,
        reason: CreateReason,
        lifetime: Duration,
    ) -> Result<Self, TlsError> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let now = OffsetDateTime::now_utc();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params
            .distinguished_name
            .push(DnType::OrganizationName, "carte");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.name_constraints = Some(NameConstraints {
            permitted_subtrees: tlds
                .iter()
                .map(|tld| GeneralSubtree::DnsName(tld.clone()))
                .collect(),
            excluded_subtrees: vec![
                GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])),
                GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16])),
            ],
        });
        params.not_before = now - Duration::days(1);
        params.not_after = now + lifetime;
        let cert = params.self_signed(&key)?;

        let cert_path = Self::cert_path_in(state_dir);
        let mut record = current_hash(state_dir)
            .map(|old| format!("replaced {old}\n"))
            .unwrap_or_default();
        record.push_str(&format!("created {}\n", sha256_hex(cert.der())));
        let created = Self::created_path_in(state_dir);
        fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o644)
            .open(&created)
            .and_then(|mut file| file.write_all(record.as_bytes()))
            .map_err(TlsError::io("write", &created))?;
        let pair = format!("{}{}", key.serialize_pem(), cert.pem());
        write_atomic(&Self::key_path_in(state_dir), pair.as_bytes(), 0o600)?;
        write_atomic(&cert_path, cert.pem().as_bytes(), 0o644)?;

        Ok(Self {
            cert_pem: cert.pem(),
            cert_der: cert.der().clone(),
            cert_path,
            issuer: Issuer::new(params, key),
            tlds,
            origin: CaOrigin::Created(reason),
        })
    }

    pub(crate) fn issue(&self, host: &str) -> Result<IssuedLeaf, TlsError> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let now = OffsetDateTime::now_utc();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        if host.len() <= 64 {
            params.distinguished_name.push(DnType::CommonName, host);
        }
        params.subject_alt_names = vec![SanType::DnsName(host.try_into()?)];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        params.not_before = now - Duration::hours(1);
        params.not_after = now + Duration::days(365);
        let cert = params.signed_by(&key, &self.issuer)?;
        Ok(IssuedLeaf {
            cert,
            key,
            not_after: params.not_after,
        })
    }
}

impl IssuedLeaf {
    pub fn key_der(&self) -> PrivateKeyDer<'static> {
        PrivatePkcs8KeyDer::from(self.key.serialize_der()).into()
    }
}

impl fmt::Debug for CertificateAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertificateAuthority")
            .field("cert_path", &self.cert_path)
            .field("tlds", &self.tlds)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// `carte local CA (<user>@<host>)`
pub fn default_common_name() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    let host = host.strip_suffix(".local").unwrap_or(&host);
    let mut name = format!("carte local CA ({user}@{host})");
    if name.len() > 64 {
        let cut = (0..=63)
            .rev()
            .find(|&i| name.is_char_boundary(i))
            .unwrap_or(0);
        name.truncate(cut);
        name.push(')');
    }
    name
}

/// A certificate's SHA-256 hash in uppercase hex, as `security` prints it.
pub fn sha256_hex(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

/// The hashes on `created` and `replaced` lines; lines of other kinds are skipped.
fn recorded_hashes(record: &str) -> impl Iterator<Item = String> + '_ {
    record.lines().filter_map(|line| {
        let (kind, hash) = line.trim().split_once(' ')?;
        let hash = hash.trim();
        let known = matches!(kind, "created" | "replaced")
            && !hash.is_empty()
            && hash.chars().all(|c| c.is_ascii_hexdigit());
        known.then(|| hash.to_ascii_uppercase())
    })
}

/// The SHA-256 hash of the CA certificate in `state_dir`, when it can be read: the one
/// paired with the key, or for an older state dir the one in `ca.pem`.
pub fn current_hash(state_dir: &Path) -> Option<String> {
    let paired = fs::read_to_string(CertificateAuthority::key_path_in(state_dir))
        .ok()
        .and_then(|text| pem_block(&text, "CERTIFICATE").map(str::to_owned));
    let pem = match paired {
        Some(pem) => pem,
        None => fs::read_to_string(CertificateAuthority::cert_path_in(state_dir)).ok()?,
    };
    let der = CertificateDer::from_pem_slice(pem.as_bytes()).ok()?;
    Some(sha256_hex(&der))
}

/// The first PEM block labelled `label`, from its BEGIN line to its END line.
fn pem_block<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text.find(&begin)?;
    let stop = start + text[start..].find(&end)? + end.len();
    Some(&text[start..stop])
}

/// Whether the certificate's extended key usage is server authentication alone.
fn tls_only(x509: &X509Certificate<'_>) -> bool {
    x509.extended_key_usage().ok().flatten().is_some_and(|ext| {
        let eku = &ext.value;
        eku.server_auth
            && !eku.any
            && !eku.client_auth
            && !eku.code_signing
            && !eku.email_protection
            && !eku.time_stamping
            && !eku.ocsp_signing
            && eku.other.is_empty()
    })
}

fn permitted_dns_names(x509: &X509Certificate<'_>) -> Vec<String> {
    let mut names: Vec<String> = x509
        .name_constraints()
        .ok()
        .flatten()
        .and_then(|ext| ext.value.permitted_subtrees.as_ref())
        .into_iter()
        .flatten()
        .filter_map(|subtree| match subtree.base {
            GeneralName::DNSName(name) => Some(name.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn create(dir: &Path) -> CertificateAuthority {
        CertificateAuthority::load_or_create(dir, "carte test CA", &["test"]).unwrap()
    }

    #[test]
    fn creates_a_constrained_ca_with_private_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state/carte");
        let ca = create(&dir);
        assert_eq!(ca.origin(), &CaOrigin::Created(CreateReason::Missing));
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("ca-key.pem")), 0o600);
        assert_eq!(ca.cert_path(), dir.join("ca.pem"));
        assert_eq!(fs::read_to_string(ca.cert_path()).unwrap(), ca.cert_pem());
        assert!(!format!("{ca:?}").contains("PRIVATE"));

        let (_, x509) = X509Certificate::from_der(ca.cert_der()).unwrap();
        assert_eq!(x509.subject().to_string(), "CN=carte test CA, O=carte");
        let constraints = x509.basic_constraints().unwrap().unwrap();
        assert!(constraints.critical);
        assert!(constraints.value.ca);
        assert_eq!(constraints.value.path_len_constraint, Some(0));
        let usage = x509.key_usage().unwrap().unwrap();
        assert!(usage.critical);
        assert!(usage.value.key_cert_sign() && usage.value.crl_sign());
        let eku = x509.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.server_auth);
        assert!(!eku.email_protection && !eku.client_auth && !eku.code_signing);
        assert!(eku.other.is_empty());
        assert!(x509.name_constraints().unwrap().unwrap().critical);
        assert_eq!(permitted_dns_names(&x509), ["test"]);
        let lifetime =
            x509.validity().not_after.to_datetime() - x509.validity().not_before.to_datetime();
        assert!(lifetime >= Duration::days(3650));
    }

    #[test]
    fn loads_an_existing_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let created = create(tmp.path());
        let loaded = create(tmp.path());
        assert_eq!(loaded.origin(), &CaOrigin::Loaded);
        assert_eq!(loaded.cert_der(), created.cert_der());
    }

    #[test]
    fn tightens_loose_directory_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        create(tmp.path());
        assert_eq!(mode(tmp.path()), 0o700);
    }

    #[test]
    fn regenerates_a_missing_or_corrupt_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let first = create(tmp.path());

        fs::remove_file(tmp.path().join("ca-key.pem")).unwrap();
        let second = create(tmp.path());
        assert_eq!(second.origin(), &CaOrigin::Created(CreateReason::Missing));
        assert_ne!(second.cert_der(), first.cert_der());

        fs::write(tmp.path().join("ca-key.pem"), "garbage").unwrap();
        let third = create(tmp.path());
        assert!(matches!(
            third.origin(),
            CaOrigin::Created(CreateReason::Invalid(_))
        ));

        let other = tempfile::tempdir().unwrap();
        create(other.path());
        let other_key = fs::read_to_string(other.path().join("ca-key.pem")).unwrap();
        let other_key = pem_block(&other_key, "PRIVATE KEY").unwrap();
        fs::write(tmp.path().join("ca-key.pem"), other_key).unwrap();
        let fourth = create(tmp.path());
        assert_eq!(
            fourth.origin(),
            &CaOrigin::Created(CreateReason::Invalid(
                "key does not match certificate".into()
            ))
        );
        assert_eq!(create(tmp.path()).origin(), &CaOrigin::Loaded);
    }

    #[test]
    fn a_crash_between_the_writes_keeps_the_new_ca_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let old = create(tmp.path());
        let other = tempfile::tempdir().unwrap();
        let new = create(other.path());
        fs::copy(
            other.path().join("ca-key.pem"),
            tmp.path().join("ca-key.pem"),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(old.cert_path()).unwrap(), old.cert_pem());

        assert_eq!(
            CertificateAuthority::load_existing(tmp.path(), &["test"]).unwrap_err(),
            CreateReason::CertFileStale
        );
        assert_eq!(current_hash(tmp.path()), Some(sha256_hex(new.cert_der())));
        let loaded = create(tmp.path());
        assert_eq!(loaded.origin(), &CaOrigin::Loaded);
        assert_eq!(loaded.cert_der(), new.cert_der());
        assert_eq!(
            fs::read_to_string(loaded.cert_path()).unwrap(),
            new.cert_pem()
        );
        assert!(CertificateAuthority::load_existing(tmp.path(), &["test"]).is_ok());
    }

    #[test]
    fn an_older_carte_still_reads_the_key_file() {
        let tmp = tempfile::tempdir().unwrap();
        create(tmp.path());
        let pair = fs::read_to_string(tmp.path().join("ca-key.pem")).unwrap();
        assert!(pem_block(&pair, "CERTIFICATE").is_some());
        assert!(KeyPair::from_pem(&pair).is_ok());
    }

    #[test]
    fn loads_a_ca_from_before_the_key_file_held_the_certificate() {
        let tmp = tempfile::tempdir().unwrap();
        let ca = create(tmp.path());
        let pair = fs::read_to_string(tmp.path().join("ca-key.pem")).unwrap();
        let key = pem_block(&pair, "PRIVATE KEY").unwrap();
        fs::write(tmp.path().join("ca-key.pem"), format!("{key}\n")).unwrap();
        let loaded = create(tmp.path());
        assert_eq!(loaded.origin(), &CaOrigin::Loaded);
        assert_eq!(loaded.cert_der(), ca.cert_der());
        assert_eq!(current_hash(tmp.path()), Some(sha256_hex(ca.cert_der())));
    }

    #[test]
    fn records_the_hash_of_every_ca_it_creates() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(CertificateAuthority::created_hashes(tmp.path()).is_empty());
        let first = sha256_hex(create(tmp.path()).cert_der());
        assert_eq!(
            CertificateAuthority::created_hashes(tmp.path()),
            std::slice::from_ref(&first)
        );
        create(tmp.path());
        fs::write(tmp.path().join("ca-key.pem"), "garbage").unwrap();
        let second = sha256_hex(create(tmp.path()).cert_der());
        let mut both = vec![first, second];
        both.sort();
        assert_eq!(CertificateAuthority::created_hashes(tmp.path()), both);
    }

    #[test]
    fn records_a_ca_made_before_the_record_when_replacing_it() {
        let tmp = tempfile::tempdir().unwrap();
        let old = sha256_hex(create(tmp.path()).cert_der());
        fs::remove_file(CertificateAuthority::created_path_in(tmp.path())).unwrap();
        fs::write(tmp.path().join("ca-key.pem"), "garbage").unwrap();
        let new = sha256_hex(create(tmp.path()).cert_der());
        let hashes = CertificateAuthority::created_hashes(tmp.path());
        assert!(hashes.contains(&old) && hashes.contains(&new), "{hashes:?}");
        let record = fs::read_to_string(CertificateAuthority::created_path_in(tmp.path())).unwrap();
        assert_eq!(record, format!("replaced {old}\ncreated {new}\n"));
    }

    #[test]
    fn without_a_record_only_the_ca_in_the_state_dir_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let current = sha256_hex(create(tmp.path()).cert_der());
        fs::remove_file(CertificateAuthority::created_path_in(tmp.path())).unwrap();
        assert_eq!(CertificateAuthority::created_hashes(tmp.path()), [current]);
    }

    #[test]
    fn reads_only_the_record_lines_it_knows() {
        let record = "created 1d2d\nreplaced C544\nsigned 9F00\nlegacy\ncreated not-hex\n\n";
        assert_eq!(
            recorded_hashes(record).collect::<Vec<_>>(),
            ["1D2D", "C544"]
        );
    }

    #[test]
    fn hashes_like_security_does() {
        assert_eq!(
            sha256_hex(b"carte"),
            "D2C12420B9815E07B29B1256FDD6E39A94B69ED192009F0C5D7AFFFC99DE44E9"
        );
    }

    #[test]
    fn concurrent_creators_leave_a_usable_ca() {
        use nix::sys::wait::{WaitStatus, waitpid};
        use nix::unistd::{ForkResult, fork};

        let tmp = tempfile::tempdir().unwrap();
        let dirs: Vec<PathBuf> = (0..40).map(|i| tmp.path().join(i.to_string())).collect();
        let children: Vec<_> = (0..2)
            .map(|_| match unsafe { fork() }.unwrap() {
                ForkResult::Child => {
                    let failed = dirs.iter().any(|dir| {
                        CertificateAuthority::load_or_create(dir, "x", &["test"]).is_err()
                    });
                    unsafe { libc::_exit(i32::from(failed)) }
                }
                ForkResult::Parent { child } => child,
            })
            .collect();
        for child in children {
            assert_eq!(waitpid(child, None).unwrap(), WaitStatus::Exited(child, 0));
        }
        for dir in &dirs {
            let loaded = CertificateAuthority::load_existing(dir, &["test"]);
            assert!(loaded.is_ok(), "{}: {:?}", dir.display(), loaded.err());
            let record = fs::read_to_string(CertificateAuthority::created_path_in(dir)).unwrap();
            let hash = sha256_hex(loaded.unwrap().cert_der());
            assert_eq!(record, format!("created {hash}\n"), "{}", dir.display());
        }
    }

    /// A CA as carte made them before it limited them to server authentication.
    fn write_ca_without_eku(dir: &Path) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, "x");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.name_constraints = Some(NameConstraints {
            permitted_subtrees: vec![GeneralSubtree::DnsName("localhost".into())],
            excluded_subtrees: Vec::new(),
        });
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::days(1);
        params.not_after = now + Duration::days(3650);
        let cert = params.self_signed(&key).unwrap();
        fs::write(dir.join("ca.pem"), cert.pem()).unwrap();
        fs::write(dir.join("ca-key.pem"), key.serialize_pem()).unwrap();
    }

    #[test]
    fn only_setup_replaces_a_ca_not_limited_to_tls() {
        let tmp = tempfile::tempdir().unwrap();
        write_ca_without_eku(tmp.path());
        let kept = CertificateAuthority::load_or_create(tmp.path(), "x", &["localhost"]).unwrap();
        assert_eq!(kept.origin(), &CaOrigin::Loaded);
        assert_eq!(
            CertificateAuthority::load_existing(tmp.path(), &["localhost"]).unwrap_err(),
            CreateReason::NotTlsOnly
        );
        let ca =
            CertificateAuthority::load_exact_or_create(tmp.path(), "x", &["localhost"]).unwrap();
        assert_eq!(ca.origin(), &CaOrigin::Created(CreateReason::NotTlsOnly));
        assert!(CertificateAuthority::load_existing(tmp.path(), &["localhost"]).is_ok());
    }

    #[test]
    fn keeps_a_ca_made_for_more_tlds_unless_asked_for_an_exact_one() {
        let tmp = tempfile::tempdir().unwrap();
        let wide =
            CertificateAuthority::load_or_create(tmp.path(), "x", &["dev", "localhost"]).unwrap();
        let kept = CertificateAuthority::load_or_create(tmp.path(), "x", &["localhost"]).unwrap();
        assert_eq!(
            kept.origin(),
            &CaOrigin::Wider {
                extra: vec!["dev".into()]
            }
        );
        assert_eq!(kept.cert_der(), wide.cert_der());
        assert_eq!(kept.tlds(), ["localhost"]);
        assert!(!kept.covers("app.dev"));
        assert_eq!(
            CertificateAuthority::load_existing(tmp.path(), &["localhost"]).unwrap_err(),
            CreateReason::TldsChanged {
                previous: vec!["dev".into(), "localhost".into()]
            }
        );

        let ca =
            CertificateAuthority::load_exact_or_create(tmp.path(), "x", &["localhost"]).unwrap();
        assert_eq!(
            ca.origin(),
            &CaOrigin::Created(CreateReason::TldsChanged {
                previous: vec!["dev".into(), "localhost".into()]
            })
        );
        let (_, x509) = X509Certificate::from_der(ca.cert_der()).unwrap();
        assert_eq!(permitted_dns_names(&x509), ["localhost"]);
    }

    #[test]
    fn regenerates_when_the_tlds_change() {
        let tmp = tempfile::tempdir().unwrap();
        create(tmp.path());
        let ca =
            CertificateAuthority::load_or_create(tmp.path(), "x", &["test", "Localhost."]).unwrap();
        assert_eq!(
            ca.origin(),
            &CaOrigin::Created(CreateReason::TldsChanged {
                previous: vec!["test".into()]
            })
        );
        assert_eq!(ca.tlds(), ["localhost", "test"]);
        let ca =
            CertificateAuthority::load_or_create(tmp.path(), "x", &["localhost", "test"]).unwrap();
        assert_eq!(ca.origin(), &CaOrigin::Loaded);
    }

    #[test]
    fn covers_names_at_or_below_its_tlds() {
        let tmp = tempfile::tempdir().unwrap();
        let ca = create(tmp.path());
        for host in ["test", "app.test", "oslo.feature-x.eventportal.test"] {
            assert!(ca.covers(host), "{host}");
        }
        for host in ["example.com", "contest", "test.com", "app.test.com"] {
            assert!(!ca.covers(host), "{host}");
        }
    }

    #[test]
    fn rejects_invalid_tlds() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(CertificateAuthority::load_or_create::<&str>(tmp.path(), "x", &[]).is_err());
        assert!(CertificateAuthority::load_or_create(tmp.path(), "x", &["not a tld"]).is_err());
    }

    #[test]
    fn names_the_ca_after_user_and_host() {
        let name = default_common_name();
        assert!(name.starts_with("carte local CA ("), "{name}");
        assert!(name.contains('@'), "{name}");
    }

    #[test]
    fn issues_leaves_for_a_single_host() {
        let tmp = tempfile::tempdir().unwrap();
        let ca = create(tmp.path());
        let leaf = ca.issue("app.nova.test").unwrap();
        let (_, x509) = X509Certificate::from_der(leaf.cert.der()).unwrap();
        let san = x509.subject_alternative_name().unwrap().unwrap();
        assert_eq!(
            san.value.general_names,
            [GeneralName::DNSName("app.nova.test")]
        );
        assert!(
            x509.extended_key_usage()
                .unwrap()
                .unwrap()
                .value
                .server_auth
        );
        assert!(!x509.is_ca());
        let lifetime =
            x509.validity().not_after.to_datetime() - x509.validity().not_before.to_datetime();
        assert!(lifetime <= Duration::days(398));
        assert_eq!(
            x509.issuer(),
            X509Certificate::from_der(ca.cert_der())
                .unwrap()
                .1
                .subject()
        );

        let long = format!("{}.nova.test", "a".repeat(60));
        let long = ca.issue(&long).unwrap();
        let (_, long) = X509Certificate::from_der(long.cert.der()).unwrap();
        assert_eq!(long.subject().iter_common_name().count(), 0);
        assert!(long.subject_alternative_name().unwrap().unwrap().critical);

        let other = ca.issue("app.nova.test").unwrap();
        assert_ne!(other.cert.der(), leaf.cert.der());
        let (_, other) = X509Certificate::from_der(other.cert.der()).unwrap();
        assert_ne!(other.serial, x509.serial);
    }
}
