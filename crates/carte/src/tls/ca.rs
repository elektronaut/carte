use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, PKCS_ECDSA_P256_SHA256, SanType,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use time::{Duration, OffsetDateTime};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

use super::{TlsError, ensure_private_dir, is_within, normalize_tlds, write_atomic};

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
    Created(CreateReason),
}

/// Why a new CA was generated. Anything but `Missing` replaces a CA that may already be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateReason {
    Missing,
    Invalid(String),
    Expiring,
    TldsChanged { previous: Vec<String> },
}

pub(crate) struct IssuedLeaf {
    pub cert: rcgen::Certificate,
    pub key: KeyPair,
    pub not_after: OffsetDateTime,
}

impl CertificateAuthority {
    /// Loads the CA from `state_dir`, or creates one when it is missing, unreadable, close to
    /// expiry or constrained to other TLDs.
    pub fn load_or_create<S: AsRef<str>>(
        state_dir: &Path,
        common_name: &str,
        tlds: &[S],
    ) -> Result<Self, TlsError> {
        let tlds = normalize_tlds(tlds)?;
        ensure_private_dir(state_dir)?;
        let reason = match Self::load(state_dir, &tlds) {
            Ok(ca) => return Ok(ca),
            Err(reason) => reason,
        };
        Self::create(state_dir, common_name, tlds, reason)
    }

    pub fn cert_path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("ca.pem")
    }

    fn key_path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("ca-key.pem")
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

    fn load(state_dir: &Path, tlds: &[String]) -> Result<Self, CreateReason> {
        let cert_path = Self::cert_path_in(state_dir);
        let key_path = Self::key_path_in(state_dir);
        let (cert_pem, key_pem) = match (
            fs::read_to_string(&cert_path),
            fs::read_to_string(&key_path),
        ) {
            (Ok(cert), Ok(key)) => (cert, key),
            (Err(e), _) | (_, Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(CreateReason::Missing);
            }
            (Err(e), _) | (_, Err(e)) => return Err(CreateReason::Invalid(e.to_string())),
        };
        let invalid = |what: &str| CreateReason::Invalid(what.to_owned());

        let key = KeyPair::from_pem(&key_pem).map_err(|_| invalid("unreadable key"))?;
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
        if previous != tlds {
            return Err(CreateReason::TldsChanged { previous });
        }

        let issuer = Issuer::from_ca_cert_der(&cert_der, key)
            .map_err(|_| invalid("unusable certificate"))?;
        Ok(Self {
            cert_pem,
            cert_der,
            cert_path,
            issuer,
            tlds: tlds.to_vec(),
            origin: CaOrigin::Loaded,
        })
    }

    fn create(
        state_dir: &Path,
        common_name: &str,
        tlds: Vec<String>,
        reason: CreateReason,
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
        params.not_after = now + Duration::days(3650);
        let cert = params.self_signed(&key)?;

        let cert_path = Self::cert_path_in(state_dir);
        write_atomic(
            &Self::key_path_in(state_dir),
            key.serialize_pem().as_bytes(),
            0o600,
        )?;
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

        fs::write(tmp.path().join("ca.pem"), "garbage").unwrap();
        let third = create(tmp.path());
        assert!(matches!(
            third.origin(),
            CaOrigin::Created(CreateReason::Invalid(_))
        ));

        let other = tempfile::tempdir().unwrap();
        create(other.path());
        fs::copy(
            other.path().join("ca-key.pem"),
            tmp.path().join("ca-key.pem"),
        )
        .unwrap();
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
        for host in ["test", "app.test", "oslo.feature-x.delegatbase.test"] {
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
        let leaf = ca.issue("app.make.test").unwrap();
        let (_, x509) = X509Certificate::from_der(leaf.cert.der()).unwrap();
        let san = x509.subject_alternative_name().unwrap().unwrap();
        assert_eq!(
            san.value.general_names,
            [GeneralName::DNSName("app.make.test")]
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

        let long = format!("{}.make.test", "a".repeat(60));
        let long = ca.issue(&long).unwrap();
        let (_, long) = X509Certificate::from_der(long.cert.der()).unwrap();
        assert_eq!(long.subject().iter_common_name().count(), 0);
        assert!(long.subject_alternative_name().unwrap().unwrap().critical);

        let other = ca.issue("app.make.test").unwrap();
        assert_ne!(other.cert.der(), leaf.cert.der());
        let (_, other) = X509Certificate::from_der(other.cert.der()).unwrap();
        assert_ne!(other.serial, x509.serial);
    }
}
