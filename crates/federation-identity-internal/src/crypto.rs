use std::fmt;

use config::namespace::Namespace;
pub use rcgen::KeyPair;
use rcgen::{CertificateParams, DistinguishedName, PublicKeyData};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;
use x509_parser::extensions::GeneralName;
use x509_parser::oid_registry::{
    OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY, OID_SIG_ECDSA_WITH_SHA256,
};
use x509_parser::prelude::FromDer;

use crate::CoreNodeIdentity;

pub const MAX_LEAF_VALIDITY_SECS: i64 = 48 * 60 * 60;
const NOT_BEFORE_CLOCK_SKEW_SECS: i64 = 5 * 60;

/// A validation failure at the certificate/key trust boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptoError(String);

impl CryptoError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CryptoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CryptoError {}

type Result<T> = std::result::Result<T, CryptoError>;

/// Local binding facts associated with the key used for an enrollment CSR.
/// This deliberately contains no bearer credential or HTTP concern.
#[derive(Clone, Copy)]
pub struct EnrollmentRequest<'a> {
    pub api_origin: &'a str,
    pub subject: &'a str,
    pub session_revision: Option<Uuid>,
    pub core_node_name: &'a str,
    pub generation: &'a str,
    pub spki_sha256: &'a str,
}

/// Transport-neutral fields returned by the certificate enrollment endpoint.
#[derive(Clone, Copy)]
pub struct ReturnedCertificate<'a> {
    pub core_node_name: &'a str,
    pub workspace_id: &'a str,
    pub certificate_chain_pem: &'a str,
    pub serial_number: &'a str,
    pub not_before: &'a str,
    pub not_after: &'a str,
    pub renew_after: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedLeaf {
    pub spki_sha256: String,
    pub serial_number: String,
    pub not_before: i64,
    pub not_after: i64,
}

pub fn generate_private_key() -> std::result::Result<KeyPair, rcgen::Error> {
    KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
}

pub fn parse_private_key_pem(pem: &str) -> std::result::Result<KeyPair, rcgen::Error> {
    KeyPair::from_pem(pem)
}

pub fn build_csr(key: &KeyPair) -> Result<String> {
    // Identity/profile extensions are server-controlled. The CSR carries only
    // the P-256 SPKI and proof-of-possession signature.
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    let csr = params
        .serialize_request(key)
        .map_err(|error| CryptoError::new(format!("failed to build core-node CSR: {error}")))?;
    csr.pem()
        .map_err(|error| CryptoError::new(format!("failed to encode core-node CSR: {error}")))
}

pub fn spki_fingerprint(key: &KeyPair) -> String {
    hex_sha256(&key.subject_public_key_info())
}

/// Validates endpoint response fields and the complete returned leaf/issuer
/// chain before constructing durable non-secret identity metadata.
pub fn validate_returned_certificate(
    request: EnrollmentRequest<'_>,
    key: &KeyPair,
    response: ReturnedCertificate<'_>,
    now: i64,
) -> Result<CoreNodeIdentity> {
    if response.core_node_name != request.core_node_name {
        return Err(CryptoError::new(format!(
            "certificate enrollment returned core-node name {:?}, expected {:?}",
            response.core_node_name, request.core_node_name
        )));
    }
    let workspace_id = parse_workspace_id(response.workspace_id).map_err(|error| {
        CryptoError::new(format!(
            "certificate enrollment returned an invalid workspace_id: {error}"
        ))
    })?;
    let not_before = parse_rfc3339("not_before", response.not_before)?;
    let not_after = parse_rfc3339("not_after", response.not_after)?;
    let renew_after = parse_rfc3339("renew_after", response.renew_after)?;
    if not_before > now.saturating_add(NOT_BEFORE_CLOCK_SKEW_SECS)
        || not_after <= now
        || not_after <= not_before
        || not_after.saturating_sub(not_before) > MAX_LEAF_VALIDITY_SECS
        || renew_after <= not_before
        || renew_after >= not_after
    {
        return Err(CryptoError::new(
            "certificate enrollment returned unacceptable validity/renewal timestamps",
        ));
    }

    let expected_uri = identity_uri(&workspace_id, request.core_node_name);
    let inspected = inspect_leaf(
        response.certificate_chain_pem,
        key,
        &expected_uri,
        request.core_node_name,
    )?;
    if inspected.spki_sha256 != request.spki_sha256
        || inspected.not_before != not_before
        || inspected.not_after != not_after
    {
        return Err(CryptoError::new(
            "certificate enrollment response metadata does not match the returned leaf",
        ));
    }
    if normalize_serial(response.serial_number)? != normalize_serial(&inspected.serial_number)? {
        return Err(CryptoError::new(
            "certificate enrollment serial_number does not match the returned leaf",
        ));
    }

    Ok(CoreNodeIdentity {
        api_origin: request.api_origin.to_string(),
        subject: request.subject.to_string(),
        session_revision: request.session_revision,
        workspace_id,
        core_node_name: request.core_node_name.to_string(),
        active_generation: request.generation.to_string(),
        serial_number: response.serial_number.to_string(),
        spki_sha256: request.spki_sha256.to_string(),
        not_before,
        not_after,
        renew_after,
    })
}

pub fn inspect_leaf(
    chain_pem: &str,
    key: &KeyPair,
    expected_uri: &str,
    expected_common_name: &str,
) -> Result<InspectedLeaf> {
    let blocks = pem::parse_many(chain_pem)
        .map_err(|error| CryptoError::new(format!("invalid certificate chain PEM: {error}")))?;
    if blocks.len() < 2 {
        return Err(CryptoError::new(
            "certificate chain must contain a leaf and at least one issuing CA certificate",
        ));
    }
    if blocks.iter().any(|block| block.tag() != "CERTIFICATE") {
        return Err(CryptoError::new(
            "certificate chain contains a non-certificate PEM block",
        ));
    }
    let certificates = blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            x509_parser::certificate::X509Certificate::from_der(block.contents())
                .and_then(|(remainder, certificate)| {
                    if remainder.is_empty() {
                        Ok((remainder, certificate))
                    } else {
                        Err(x509_parser::asn1_rs::Err::Error(
                            x509_parser::error::X509Error::InvalidCertificate,
                        ))
                    }
                })
                .map(|(_, certificate)| certificate)
                .map_err(|error| {
                    CryptoError::new(format!(
                        "invalid certificate at position {} in returned chain: {error}",
                        index + 1
                    ))
                })
        })
        .collect::<Result<Vec<_>>>()?;
    for (index, pair) in certificates.windows(2).enumerate() {
        let certificate = &pair[0];
        let issuer = &pair[1];
        if certificate.issuer() != issuer.subject() {
            return Err(CryptoError::new(format!(
                "certificate chain issuer/subject mismatch between positions {} and {}",
                index + 1,
                index + 2
            )));
        }
        if certificate.signature_algorithm.algorithm != OID_SIG_ECDSA_WITH_SHA256
            || certificate.signature_algorithm.parameters.is_some()
            || certificate.tbs_certificate.signature.algorithm != OID_SIG_ECDSA_WITH_SHA256
            || certificate.tbs_certificate.signature.parameters.is_some()
        {
            return Err(CryptoError::new(format!(
                "certificate at chain position {} must be signed with ecdsa-with-SHA256",
                index + 1
            )));
        }
        let issuer_spki = issuer.public_key();
        let issuer_uses_p256 = issuer_spki.algorithm.algorithm == OID_KEY_TYPE_EC_PUBLIC_KEY
            && issuer_spki
                .algorithm
                .parameters
                .as_ref()
                .and_then(|parameters| parameters.as_oid().ok())
                .is_some_and(|curve| curve == OID_EC_P256);
        if !issuer_uses_p256 {
            return Err(CryptoError::new(format!(
                "certificate at chain position {} must use an EC prime256v1 signing key",
                index + 2
            )));
        }
        let issuer_basic = issuer
            .basic_constraints()
            .map_err(|error| {
                CryptoError::new(format!("invalid issuer Basic Constraints: {error}"))
            })?
            .ok_or_else(|| {
                CryptoError::new(format!(
                    "certificate at chain position {} is missing CA Basic Constraints",
                    index + 2
                ))
            })?;
        if !issuer_basic.critical || !issuer_basic.value.ca {
            return Err(CryptoError::new(format!(
                "certificate at chain position {} must have critical CA Basic Constraints",
                index + 2
            )));
        }
        let issuer_usage = issuer
            .key_usage()
            .map_err(|error| CryptoError::new(format!("invalid issuer Key Usage: {error}")))?
            .ok_or_else(|| {
                CryptoError::new(format!(
                    "certificate at chain position {} is missing Key Usage",
                    index + 2
                ))
            })?;
        if !issuer_usage.critical || !issuer_usage.value.key_cert_sign() {
            return Err(CryptoError::new(format!(
                "certificate at chain position {} must have critical keyCertSign usage",
                index + 2
            )));
        }
        if issuer.validity().not_before.timestamp() > certificate.validity().not_before.timestamp()
            || issuer.validity().not_after.timestamp()
                < certificate.validity().not_after.timestamp()
        {
            return Err(CryptoError::new(format!(
                "certificate at chain position {} does not contain its child's validity interval",
                index + 2
            )));
        }
        certificate
            .verify_signature(Some(issuer.public_key()))
            .map_err(|error| {
                CryptoError::new(format!(
                    "certificate chain signature verification failed between positions {} and {}: {error}",
                    index + 1,
                    index + 2
                ))
            })?;
    }
    let leaf = &certificates[0];

    if !is_valid_positive_der_serial(leaf.raw_serial()) {
        return Err(CryptoError::new(
            "returned leaf serial number is not a canonical positive RFC 5280 serial",
        ));
    }

    if leaf.public_key().raw != key.subject_public_key_info() {
        return Err(CryptoError::new(
            "returned leaf certificate does not match the locally generated private key",
        ));
    }
    let basic = leaf
        .basic_constraints()
        .map_err(|error| CryptoError::new(format!("invalid Basic Constraints: {error}")))?
        .ok_or_else(|| CryptoError::new("returned leaf is missing Basic Constraints"))?;
    if !basic.critical || basic.value.ca {
        return Err(CryptoError::new(
            "returned leaf Basic Constraints must be critical with CA=false",
        ));
    }
    let usage = leaf
        .key_usage()
        .map_err(|error| CryptoError::new(format!("invalid Key Usage: {error}")))?
        .ok_or_else(|| CryptoError::new("returned leaf is missing Key Usage"))?;
    if !usage.critical || usage.value.flags != 1 || !usage.value.digital_signature() {
        return Err(CryptoError::new(
            "returned leaf Key Usage must be critical and restricted to digitalSignature",
        ));
    }
    let eku = leaf
        .extended_key_usage()
        .map_err(|error| CryptoError::new(format!("invalid Extended Key Usage: {error}")))?
        .ok_or_else(|| CryptoError::new("returned leaf is missing Extended Key Usage"))?;
    let eku = eku.value;
    if !eku.client_auth
        || eku.any
        || eku.server_auth
        || eku.code_signing
        || eku.email_protection
        || eku.time_stamping
        || eku.ocsp_signing
        || !eku.other.is_empty()
    {
        return Err(CryptoError::new(
            "returned leaf Extended Key Usage must be restricted to clientAuth",
        ));
    }
    let san = leaf
        .subject_alternative_name()
        .map_err(|error| CryptoError::new(format!("invalid Subject Alternative Name: {error}")))?
        .ok_or_else(|| CryptoError::new("returned leaf is missing Subject Alternative Name"))?;
    if !matches!(
        san.value.general_names.as_slice(),
        [GeneralName::URI(uri)] if *uri == expected_uri
    ) {
        return Err(CryptoError::new(format!(
            "returned leaf SAN must contain only the exact server-controlled core-node identity URI `{expected_uri}`"
        )));
    }
    let common_names = leaf.subject().iter_common_name().collect::<Vec<_>>();
    if common_names.len() != 1
        || common_names[0]
            .as_str()
            .map(|common_name| common_name != expected_common_name)
            .unwrap_or(true)
    {
        return Err(CryptoError::new(format!(
            "returned leaf subject must contain exactly one common name equal to `{expected_common_name}`"
        )));
    }

    Ok(InspectedLeaf {
        spki_sha256: hex_sha256(leaf.public_key().raw),
        serial_number: leaf.raw_serial_as_string(),
        not_before: leaf.validity().not_before.timestamp(),
        not_after: leaf.validity().not_after.timestamp(),
    })
}

/// RFC 5280 serials are positive DER INTEGERs of at most 20 content octets.
/// `raw_serial` is the INTEGER content, so enforce sign and DER minimality even
/// when the certificate parser accepted a non-conforming value.
pub fn is_valid_positive_der_serial(raw_serial: &[u8]) -> bool {
    if raw_serial.is_empty() || raw_serial.len() > 20 {
        return false;
    }
    if raw_serial[0] & 0x80 != 0 || raw_serial.iter().all(|byte| *byte == 0) {
        return false;
    }
    if raw_serial.len() > 1 && raw_serial[0] == 0 && raw_serial[1] & 0x80 == 0 {
        return false;
    }
    true
}

pub fn normalize_serial(serial: &str) -> Result<String> {
    if serial.is_empty() || serial.trim() != serial {
        return Err(CryptoError::new(
            "certificate serial number must be non-empty hexadecimal",
        ));
    }
    let compact = if serial.contains(':') {
        let parts = serial.split(':').collect::<Vec<_>>();
        if parts
            .iter()
            .any(|part| part.len() != 2 || !part.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(CryptoError::new(
                "certificate serial number must use two-digit hexadecimal bytes separated by colons",
            ));
        }
        parts.concat()
    } else {
        if !serial.len().is_multiple_of(2) || !serial.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(CryptoError::new(
                "certificate serial number must be an even-length hexadecimal string",
            ));
        }
        serial.to_string()
    };
    let normalized = compact.to_ascii_lowercase();
    let without_zeroes = normalized.trim_start_matches('0');
    Ok(if without_zeroes.is_empty() {
        "0".into()
    } else {
        without_zeroes.into()
    })
}

pub fn identity_uri(workspace: &Namespace, core_node_name: &str) -> String {
    format!(
        "peppy://platform/workspaces/{}/core-nodes/{core_node_name}",
        workspace.as_str()
    )
}

fn parse_workspace_id(raw: &str) -> Result<Namespace> {
    let parsed = uuid::Uuid::parse_str(raw)
        .map_err(|error| CryptoError::new(format!("invalid workspace_id {raw:?}: {error}")))?;
    if parsed.hyphenated().to_string() != raw {
        return Err(CryptoError::new(format!(
            "workspace_id {raw:?} is not a canonical lower-case hyphenated UUID"
        )));
    }
    Namespace::parse(raw)
        .map_err(|error| CryptoError::new(format!("invalid workspace_id {raw:?}: {error}")))
}

fn parse_rfc3339(field: &str, value: &str) -> Result<i64> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|time| time.unix_timestamp())
        .map_err(|error| {
            CryptoError::new(format!(
                "invalid certificate {field} timestamp {value:?}: {error}"
            ))
        })
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyUsagePurpose, SanType,
    };
    use time::Duration;

    const CORE_NODE: &str = "core-node-test-0001";
    const WORKSPACE: &str = "550e8400-e29b-41d4-a716-446655440000";
    const NOT_BEFORE: &str = "2026-07-19T00:00:00Z";
    const RENEW_AFTER: &str = "2026-07-19T12:00:00Z";
    const NOT_AFTER: &str = "2026-07-20T00:00:00Z";
    const DIFFERENT_FINGERPRINT: &str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    struct ValidationFixture {
        key: KeyPair,
        chain: String,
        fingerprint: String,
    }

    impl ValidationFixture {
        fn new() -> Self {
            let key = generate_private_key().unwrap();
            let chain = issued_chain(&key, CORE_NODE);
            let fingerprint = spki_fingerprint(&key);
            Self {
                key,
                chain,
                fingerprint,
            }
        }

        fn request(&self) -> EnrollmentRequest<'_> {
            EnrollmentRequest {
                api_origin: "https://api.peppy.bot",
                subject: "user-test-subject",
                session_revision: None,
                core_node_name: CORE_NODE,
                generation: &self.fingerprint,
                spki_sha256: &self.fingerprint,
            }
        }

        fn response(&self) -> ReturnedCertificate<'_> {
            ReturnedCertificate {
                core_node_name: CORE_NODE,
                workspace_id: WORKSPACE,
                certificate_chain_pem: &self.chain,
                serial_number: "019abcde",
                not_before: NOT_BEFORE,
                not_after: NOT_AFTER,
                renew_after: RENEW_AFTER,
            }
        }

        fn now() -> i64 {
            parse_rfc3339("now", RENEW_AFTER).unwrap()
        }
    }

    fn issued_chain(key: &KeyPair, core_node_name: &str) -> String {
        let not_before = OffsetDateTime::parse(NOT_BEFORE, &Rfc3339).unwrap();
        let not_after = OffsetDateTime::parse(NOT_AFTER, &Rfc3339).unwrap();
        let ca_key = generate_private_key().unwrap();

        let mut ca_params = CertificateParams::default();
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
        ];
        ca_params.not_before = not_before;
        ca_params.not_after = not_after + Duration::days(1);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::from_params(&ca_params, &ca_key);

        let workspace = Namespace::parse(WORKSPACE).unwrap();
        let mut leaf_params = CertificateParams::default();
        leaf_params.distinguished_name = DistinguishedName::new();
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, core_node_name);
        leaf_params.is_ca = IsCa::ExplicitNoCa;
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        leaf_params.subject_alt_names = vec![SanType::URI(
            identity_uri(&workspace, core_node_name).try_into().unwrap(),
        )];
        leaf_params.serial_number = Some(vec![0x01, 0x9a, 0xbc, 0xde].into());
        leaf_params.not_before = not_before;
        leaf_params.not_after = not_after;
        let leaf = leaf_params.signed_by(key, &issuer).unwrap();

        format!("{}{}", leaf.pem(), ca.pem())
    }

    #[test]
    fn certificate_serial_requires_positive_canonical_der() {
        assert!(is_valid_positive_der_serial(&[1]));
        assert!(is_valid_positive_der_serial(&[0, 0x80]));
        assert!(is_valid_positive_der_serial(&[0x7f; 20]));

        assert!(!is_valid_positive_der_serial(&[]));
        assert!(!is_valid_positive_der_serial(&[0]));
        assert!(!is_valid_positive_der_serial(&[0x80]));
        assert!(!is_valid_positive_der_serial(&[0, 0x7f]));
        assert!(!is_valid_positive_der_serial(&[1; 21]));
    }

    #[test]
    fn generated_private_key_round_trips_with_the_same_spki() {
        let key = generate_private_key().unwrap();
        let pem = key.serialize_pem();
        let reparsed = parse_private_key_pem(&pem).unwrap();

        assert_eq!(spki_fingerprint(&key), spki_fingerprint(&reparsed));
        assert!(
            build_csr(&reparsed)
                .unwrap()
                .starts_with("-----BEGIN CERTIFICATE REQUEST-----")
        );
    }

    #[test]
    fn returned_p256_ecdsa_chain_profile_is_accepted() {
        let key = generate_private_key().unwrap();
        let chain = issued_chain(&key, CORE_NODE);

        inspect_leaf(
            &chain,
            &key,
            &identity_uri(&Namespace::parse(WORKSPACE).unwrap(), CORE_NODE),
            CORE_NODE,
        )
        .unwrap();
    }

    #[test]
    fn returned_certificate_constructs_the_exact_bound_identity() {
        let fixture = ValidationFixture::new();

        let identity = validate_returned_certificate(
            fixture.request(),
            &fixture.key,
            fixture.response(),
            ValidationFixture::now(),
        )
        .unwrap();

        assert_eq!(identity.api_origin, "https://api.peppy.bot");
        assert_eq!(identity.subject, "user-test-subject");
        assert_eq!(identity.workspace_id.as_str(), WORKSPACE);
        assert_eq!(identity.core_node_name, CORE_NODE);
        assert_eq!(identity.active_generation, fixture.fingerprint);
        assert_eq!(identity.serial_number, "019abcde");
    }

    #[test]
    fn returned_certificate_rejects_a_different_core_node_name() {
        let fixture = ValidationFixture::new();
        let mut response = fixture.response();
        response.core_node_name = "some-other-core-node";

        let error = validate_returned_certificate(
            fixture.request(),
            &fixture.key,
            response,
            ValidationFixture::now(),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("returned core-node name"),
            "{error}"
        );
    }

    #[test]
    fn returned_certificate_rejects_a_noncanonical_workspace_uuid() {
        let fixture = ValidationFixture::new();
        let mut response = fixture.response();
        response.workspace_id = "550E8400-E29B-41D4-A716-446655440000";

        let error = validate_returned_certificate(
            fixture.request(),
            &fixture.key,
            response,
            ValidationFixture::now(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("not a canonical"), "{error}");
    }

    #[test]
    fn returned_certificate_rejects_an_invalid_renewal_window() {
        let fixture = ValidationFixture::new();
        let mut response = fixture.response();
        response.renew_after = NOT_BEFORE;

        let error = validate_returned_certificate(
            fixture.request(),
            &fixture.key,
            response,
            ValidationFixture::now(),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("unacceptable validity/renewal"),
            "{error}"
        );
    }

    #[test]
    fn returned_certificate_rejects_spki_or_leaf_time_mismatch() {
        let fixture = ValidationFixture::new();
        let mut request = fixture.request();
        request.spki_sha256 = DIFFERENT_FINGERPRINT;

        let spki_error = validate_returned_certificate(
            request,
            &fixture.key,
            fixture.response(),
            ValidationFixture::now(),
        )
        .unwrap_err();
        assert!(spki_error.to_string().contains("metadata does not match"));

        let mut response = fixture.response();
        response.not_before = "2026-07-19T00:00:01Z";
        let time_error = validate_returned_certificate(
            fixture.request(),
            &fixture.key,
            response,
            ValidationFixture::now(),
        )
        .unwrap_err();
        assert!(time_error.to_string().contains("metadata does not match"));
    }

    #[test]
    fn returned_certificate_rejects_a_serial_mismatch() {
        let fixture = ValidationFixture::new();
        let mut response = fixture.response();
        response.serial_number = "019abcdf";

        let error = validate_returned_certificate(
            fixture.request(),
            &fixture.key,
            response,
            ValidationFixture::now(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("serial_number does not match"));
    }

    #[test]
    fn returned_leaf_is_rejected_for_the_wrong_identity_uri() {
        let key = generate_private_key().unwrap();
        let chain = issued_chain(&key, "some-other-core-node");

        let error = inspect_leaf(
            &chain,
            &key,
            &identity_uri(&Namespace::parse(WORKSPACE).unwrap(), CORE_NODE),
            CORE_NODE,
        )
        .unwrap_err();

        assert!(error.to_string().contains("identity URI"), "{error}");
    }

    #[test]
    fn returned_leaf_without_its_issuer_is_rejected() {
        let key = generate_private_key().unwrap();
        let chain = issued_chain(&key, CORE_NODE);
        let leaf_only = pem::parse_many(&chain).unwrap().remove(0).to_string();

        let error = inspect_leaf(
            &leaf_only,
            &key,
            &identity_uri(&Namespace::parse(WORKSPACE).unwrap(), CORE_NODE),
            CORE_NODE,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("at least one issuing CA"),
            "{error}"
        );
    }

    #[test]
    fn serial_normalization_preserves_numeric_identity() {
        assert_eq!(normalize_serial("00:01:9A:BC").unwrap(), "19abc");
        assert_eq!(normalize_serial("00019abc").unwrap(), "19abc");
        assert!(normalize_serial("1").is_err());
        assert!(normalize_serial("01:9a:b").is_err());
    }
}
