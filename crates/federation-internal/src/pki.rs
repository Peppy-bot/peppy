//! Fleet-CA creation and per-machine identity issuance.

use std::path::Path;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use time::{Duration, OffsetDateTime};

use crate::{CA_CERT_FILE, CA_KEY_FILE, CERT_FILE, Error, KEY_FILE, Result};

const CA_VALIDITY_DAYS: i64 = 365 * 10;
const LEAF_VALIDITY_DAYS: i64 = 365 * 2;
const VALIDITY_BACKDATE_MINUTES: i64 = 5;

/// Creates a new ECDSA P-256 fleet CA. Existing CA material is never
/// overwritten, including a half-present certificate/key pair.
pub fn ca_init(directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    restrict_dir(directory)?;

    let certificate_path = directory.join(CA_CERT_FILE);
    let key_path = directory.join(CA_KEY_FILE);
    if certificate_path.exists() || key_path.exists() {
        return Err(Error::Pki(format!(
            "refusing to overwrite existing fleet CA material in {}; move {} and {} aside before initializing a new CA",
            directory.display(),
            certificate_path.display(),
            key_path.display()
        )));
    }

    let now = OffsetDateTime::now_utc();
    let mut parameters = CertificateParams::new(Vec::<String>::new())
        .map_err(|error| Error::Pki(format!("build CA certificate parameters: {error}")))?;
    parameters.distinguished_name = DistinguishedName::new();
    parameters
        .distinguished_name
        .push(DnType::CommonName, "peppy fleet CA");
    parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    parameters.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    parameters.not_before = checked_sub(now, Duration::minutes(VALIDITY_BACKDATE_MINUTES))?;
    parameters.not_after = checked_add(now, Duration::days(CA_VALIDITY_DAYS))?;

    let key = KeyPair::generate()
        .map_err(|error| Error::Pki(format!("generate fleet CA key: {error}")))?;
    let certificate = parameters
        .self_signed(&key)
        .map_err(|error| Error::Pki(format!("sign fleet CA certificate: {error}")))?;

    publish_private(&key_path, key.serialize_pem().as_bytes())?;
    publish_private(&certificate_path, certificate.pem().as_bytes())?;
    Ok(())
}

/// Issues a dual-purpose server/client certificate for all supplied DNS names
/// and IP addresses. Existing machine identity files are replaced so re-issue
/// is the rotation mechanism.
pub fn issue(ca_directory: &Path, hosts: &[String], output_directory: &Path) -> Result<()> {
    validate_hosts(hosts)?;

    let ca_certificate_path = ca_directory.join(CA_CERT_FILE);
    let ca_key_path = ca_directory.join(CA_KEY_FILE);
    let ca_certificate_pem = std::fs::read_to_string(&ca_certificate_path).map_err(|error| {
        Error::Pki(format!(
            "read fleet CA certificate {}: {error}",
            ca_certificate_path.display()
        ))
    })?;
    let ca_key_pem = std::fs::read_to_string(&ca_key_path).map_err(|error| {
        Error::Pki(format!(
            "read fleet CA private key {}: {error}",
            ca_key_path.display()
        ))
    })?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)
        .map_err(|error| Error::Pki(format!("parse fleet CA private key: {error}")))?;
    let issuer = Issuer::from_ca_cert_pem(&ca_certificate_pem, ca_key)
        .map_err(|error| Error::Pki(format!("parse fleet CA certificate: {error}")))?;

    let now = OffsetDateTime::now_utc();
    let mut parameters = CertificateParams::new(hosts.to_vec())
        .map_err(|error| Error::Pki(format!("invalid certificate host: {error}")))?;
    parameters.distinguished_name = DistinguishedName::new();
    parameters
        .distinguished_name
        .push(DnType::CommonName, hosts[0].as_str());
    parameters.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    parameters.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    parameters.use_authority_key_identifier_extension = true;
    parameters.not_before = checked_sub(now, Duration::minutes(VALIDITY_BACKDATE_MINUTES))?;
    parameters.not_after = checked_add(now, Duration::days(LEAF_VALIDITY_DAYS))?;

    let leaf_key = KeyPair::generate()
        .map_err(|error| Error::Pki(format!("generate machine private key: {error}")))?;
    let leaf_certificate = parameters
        .signed_by(&leaf_key, &issuer)
        .map_err(|error| Error::Pki(format!("sign machine certificate: {error}")))?;

    std::fs::create_dir_all(output_directory)?;
    restrict_dir(output_directory)?;
    publish_private(
        &output_directory.join(KEY_FILE),
        leaf_key.serialize_pem().as_bytes(),
    )?;
    publish_private(
        &output_directory.join(CERT_FILE),
        leaf_certificate.pem().as_bytes(),
    )?;

    // Self-install issues into the CA directory itself. Avoid treating the
    // source CA certificate as a copy destination in that case.
    if !same_directory(ca_directory, output_directory)? {
        publish_private(
            &output_directory.join(CA_CERT_FILE),
            ca_certificate_pem.as_bytes(),
        )?;
    }
    Ok(())
}

fn validate_hosts(hosts: &[String]) -> Result<()> {
    if hosts.is_empty() {
        return Err(Error::Pki(
            "at least one DNS name or IP address is required".to_string(),
        ));
    }
    for host in hosts {
        if host.is_empty() || host.trim() != host {
            return Err(Error::Pki(format!(
                "invalid certificate host {host:?}: hosts must be non-empty and contain no leading or trailing whitespace"
            )));
        }
    }
    Ok(())
}

fn checked_sub(time: OffsetDateTime, duration: Duration) -> Result<OffsetDateTime> {
    time.checked_sub(duration).ok_or_else(|| {
        Error::Pki("certificate validity start is outside the supported range".into())
    })
}

fn checked_add(time: OffsetDateTime, duration: Duration) -> Result<OffsetDateTime> {
    time.checked_add(duration)
        .ok_or_else(|| Error::Pki("certificate validity end is outside the supported range".into()))
}

fn same_directory(left: &Path, right: &Path) -> Result<bool> {
    let left = std::fs::canonicalize(left)?;
    let right = std::fs::canonicalize(right)?;
    Ok(left == right)
}

fn publish_private(path: &Path, content: &[u8]) -> Result<()> {
    daemon_config::atomic_write::publish_atomic(path, |temporary| {
        std::fs::write(temporary, content)?;
        restrict_file(temporary)
    })?;
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::extensions::GeneralName;

    #[test]
    fn ca_and_leaf_have_required_x509_properties() {
        let temporary = tempfile::tempdir().unwrap();
        let ca_directory = temporary.path().join("fleet");
        let output_directory = temporary.path().join("machine");
        ca_init(&ca_directory).unwrap();
        issue(
            &ca_directory,
            &["robot.example".into(), "192.0.2.10".into()],
            &output_directory,
        )
        .unwrap();

        let ca_bytes = std::fs::read(ca_directory.join(CA_CERT_FILE)).unwrap();
        let (_, ca_pem) = x509_parser::pem::parse_x509_pem(&ca_bytes).unwrap();
        let ca = ca_pem.parse_x509().unwrap();
        assert!(ca.basic_constraints().unwrap().unwrap().value.ca);
        assert_eq!(
            ca.public_key().algorithm.algorithm.to_id_string(),
            "1.2.840.10045.2.1"
        );
        let ca_validity_days =
            (ca.validity().not_after.timestamp() - ca.validity().not_before.timestamp()) / 86_400;
        assert!((CA_VALIDITY_DAYS..=CA_VALIDITY_DAYS + 1).contains(&ca_validity_days));

        let leaf_bytes = std::fs::read(output_directory.join(CERT_FILE)).unwrap();
        let (_, leaf_pem) = x509_parser::pem::parse_x509_pem(&leaf_bytes).unwrap();
        let leaf = leaf_pem.parse_x509().unwrap();
        let usages = leaf.extended_key_usage().unwrap().unwrap().value;
        assert!(usages.server_auth);
        assert!(usages.client_auth);

        let names = &leaf
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names;
        assert!(names.contains(&GeneralName::DNSName("robot.example")));
        assert!(names.contains(&GeneralName::IPAddress(&[192, 0, 2, 10])));
        assert_eq!(
            std::fs::read(output_directory.join(CA_CERT_FILE)).unwrap(),
            ca_bytes
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [
                ca_directory.join(CA_KEY_FILE),
                output_directory.join(KEY_FILE),
            ] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn ca_init_refuses_to_overwrite_any_existing_material() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("fleet");
        ca_init(&directory).unwrap();
        let certificate_before = std::fs::read(directory.join(CA_CERT_FILE)).unwrap();
        let key_before = std::fs::read(directory.join(CA_KEY_FILE)).unwrap();

        let error = ca_init(&directory).unwrap_err().to_string();
        assert!(error.contains("refusing to overwrite"));
        assert_eq!(
            std::fs::read(directory.join(CA_CERT_FILE)).unwrap(),
            certificate_before
        );
        assert_eq!(
            std::fs::read(directory.join(CA_KEY_FILE)).unwrap(),
            key_before
        );
    }

    #[test]
    fn issue_supports_self_install_without_replacing_the_ca() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("fleet");
        ca_init(&directory).unwrap();
        let ca_before = std::fs::read(directory.join(CA_CERT_FILE)).unwrap();

        issue(&directory, &["127.0.0.1".into()], &directory).unwrap();

        assert!(directory.join(CERT_FILE).is_file());
        assert!(directory.join(KEY_FILE).is_file());
        assert_eq!(
            std::fs::read(directory.join(CA_CERT_FILE)).unwrap(),
            ca_before
        );
    }

    #[test]
    fn issue_rejects_missing_or_whitespace_hosts() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("fleet");
        ca_init(&directory).unwrap();

        assert!(issue(&directory, &[], temporary.path()).is_err());
        assert!(issue(&directory, &[" bad.example".into()], temporary.path()).is_err());
    }
}
