//! Fixtures for the tests of this crate and of the crates that consume it: a
//! valid enrollment, selection, peer and refusal. A test takes the one it is
//! about and overrides the fields that matter to it with struct update
//! syntax, so one place holds what a valid value looks like.

use daemon_config::consts::PeppyDirs;

use crate::client::{PeerStatus, RouterPeer};
use crate::enrollment::{
    CertificateValidity, ENROLLMENT_VERSION, Enrollment, EnrollmentBundle, EnrollmentDocument,
    IssuedMaterial, RouterEndpoint, save,
};
use crate::error::{Problem, ProblemKind};
use crate::selection::{Named, PlatformSelection, SELECTION_VERSION};
use crate::storage::secret;

pub const WORKSPACE: &str = "ws-1";
pub const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
pub const PEER_ID: &str = "peer-1";
pub const PEER_NAME: &str = "robot-7";
pub const ZID: &str = "2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5";
pub const ROUTER_HOST: &str = "rtr-p.example";
pub const ISSUED_AT: i64 = 1_700_000_000;
pub const EXPIRES_AT: i64 = 2_000_000_000;

/// The record of a machine enrolled in [`PROJECT`] as [`PEER_ID`], under
/// [`ZID`].
pub fn enrollment_document() -> EnrollmentDocument {
    EnrollmentDocument {
        version: ENROLLMENT_VERSION,
        api_url: "https://api.example".into(),
        workspace_id: WORKSPACE.into(),
        project_id: PROJECT.into(),
        peer_id: PEER_ID.into(),
        peer_name: PEER_NAME.into(),
        zenoh_id: pmi::RouterId::parse(ZID).expect("a valid router id"),
        namespace: config::namespace::Namespace::parse(PROJECT).expect("a valid namespace"),
        router: RouterEndpoint::parse(ROUTER_HOST, 7447).expect("a valid endpoint"),
        enrolled_at: ISSUED_AT,
    }
}

/// A leaf issued at [`ISSUED_AT`] that expires at [`EXPIRES_AT`].
pub fn certificate_validity() -> CertificateValidity {
    CertificateValidity {
        not_before: ISSUED_AT,
        not_after: EXPIRES_AT,
    }
}

/// [`enrollment_document`] with [`certificate_validity`] and its material at
/// nominal paths, for a test whose enrollment reader is injected and opens
/// nothing.
pub fn enrollment() -> Enrollment {
    Enrollment {
        document: enrollment_document(),
        certificate: certificate_validity(),
        peer_key: "/peer/peer.key".into(),
        peer_certificate: "/peer/peer.crt".into(),
        trust_anchor: "/peer/ca.crt".into(),
        issuer_chain: "/peer/chain.crt".into(),
    }
}

/// A self-signed leaf for [`PEER_NAME`] valid from `not_before` to
/// `not_after` (unix seconds), PEM. Each call mints a new key, so two leaves
/// never read the same.
pub fn leaf_pem(not_before: i64, not_after: i64) -> String {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("no names");
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, PEER_NAME);
    params.not_before =
        time::OffsetDateTime::from_unix_timestamp(not_before).expect("a date in range");
    params.not_after =
        time::OffsetDateTime::from_unix_timestamp(not_after).expect("a date in range");
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("a key");
    params.self_signed(&key).expect("a self-signed leaf").pem()
}

/// [`leaf_pem`] with [`certificate_validity`].
pub fn leaf() -> String {
    leaf_pem(ISSUED_AT, EXPIRES_AT)
}

/// Enrolls the machine under `dirs` with `document` and `leaf_pem` as its
/// certificate, with placeholder PEM material for the rest, as `peppy
/// platform enroll` would have.
pub fn write_enrollment_with_leaf(dirs: &PeppyDirs, document: EnrollmentDocument, leaf_pem: &str) {
    let bundle = EnrollmentBundle {
        peer_key_pem: secret("key".into()),
        issued: IssuedMaterial {
            document,
            certificate: CertificateValidity::parse_pem(leaf_pem).expect("a certificate"),
            peer_certificate_pem: leaf_pem.into(),
            trust_anchor_pem: "ca".into(),
            chain_pem: "chain".into(),
            platform_zenoh_config: "{}".into(),
        },
    };
    save(dirs, &bundle).expect("write the enrollment");
}

/// [`write_enrollment_with_leaf`] with [`leaf`].
pub fn write_enrollment(dirs: &PeppyDirs, document: EnrollmentDocument) {
    write_enrollment_with_leaf(dirs, document, &leaf());
}

/// One peer of a router as the platform lists it.
pub fn router_peer(id: &str, name: &str, status: PeerStatus) -> RouterPeer {
    RouterPeer {
        id: id.into(),
        name: name.into(),
        certificate_cn: name.into(),
        status,
        certificate_expires_at: "2027-01-01T00:00:00Z".parse().expect("a valid time"),
        created_at: "2026-10-03T00:00:00Z".parse().expect("a valid time"),
    }
}

/// A selection of `user-123` at `https://api.example`: project `Field`
/// (`p-2`) in workspace `Robotics lab` (`ws-2`).
pub fn platform_selection() -> PlatformSelection {
    PlatformSelection {
        version: SELECTION_VERSION,
        api_origin: "https://api.example".into(),
        subject: "user-123".into(),
        workspace: Named {
            id: "ws-2".into(),
            name: "Robotics lab".into(),
        },
        project: Some(Named {
            id: "p-2".into(),
            name: "Field".into(),
        }),
        selected_at: ISSUED_AT,
    }
}

/// The common name of the certificate signing request in the body of an
/// enrollment (`{ "csr": "<pem>" }`), or `None` when the body carries no
/// request that parses and verifies.
pub fn enrollment_request_common_name(body: &[u8]) -> Option<String> {
    let body: serde_json::Value = serde_json::from_slice(body).ok()?;
    let request = rcgen::CertificateSigningRequestParams::from_pem(body["csr"].as_str()?).ok()?;
    request
        .params
        .distinguished_name
        .iter()
        .find_map(|(kind, value)| match (kind, value) {
            (rcgen::DnType::CommonName, rcgen::DnValue::Utf8String(name)) => Some(name.clone()),
            _ => None,
        })
}

/// A refusal of `kind` with `status`, titled `Refused`, with no detail and
/// no delay.
pub fn problem(kind: ProblemKind, status: u16) -> Problem {
    Problem {
        kind,
        status,
        title: "Refused".into(),
        detail: None,
        retry_after_secs: None,
        pending_removals: None,
    }
}

/// A CA that mints the machine certificates of a test federation: the CA of
/// an operator, or the CA of a project of the platform with an issuer under
/// it.
pub struct TestMachineCa {
    issuer: rcgen::CertifiedIssuer<'static, rcgen::KeyPair>,
    /// The certificates above `issuer`, nearest first: empty for a root.
    above: Vec<String>,
}

/// The files of a machine certificate: the leaf then the certificates above
/// it, its private key, and the CA that the machines of the federation trust.
#[derive(Debug, Clone)]
pub struct MachineFiles {
    pub certificate: std::path::PathBuf,
    pub private_key: std::path::PathBuf,
    pub trust_anchor: std::path::PathBuf,
}

impl TestMachineCa {
    /// A root CA named `name`.
    pub fn root(name: &str) -> Self {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("a key");
        let issuer = rcgen::CertifiedIssuer::self_signed(ca_params(name), key).expect("a root CA");
        Self {
            issuer,
            above: Vec::new(),
        }
    }

    /// An issuing CA named `name` under this CA, as the platform gives each
    /// peer of a project its own issuer under the CA of the project.
    pub fn issuer(&self, name: &str) -> Self {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("a key");
        let issuer = rcgen::CertifiedIssuer::signed_by(ca_params(name), key, &self.issuer)
            .expect("an issuing CA");
        let mut above = vec![self.issuer.pem()];
        above.extend(self.above.iter().cloned());
        Self { issuer, above }
    }

    /// The certificate of the root of this CA, PEM: the one that the machines
    /// of the federation trust.
    pub fn trust_anchor_pem(&self) -> String {
        self.above
            .last()
            .cloned()
            .unwrap_or_else(|| self.issuer.pem())
    }

    /// A leaf for client authentication whose common name is `common_name`,
    /// valid from `not_before` to `not_after` (unix seconds), and its key, PEM.
    pub fn leaf_pem(&self, common_name: &str, not_before: i64, not_after: i64) -> (String, String) {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("no names");
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        params.not_before =
            time::OffsetDateTime::from_unix_timestamp(not_before).expect("a date in range");
        params.not_after =
            time::OffsetDateTime::from_unix_timestamp(not_after).expect("a date in range");
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("a key");
        let leaf = params.signed_by(&key, &self.issuer).expect("a leaf");
        (leaf.pem(), key.serialize_pem())
    }

    /// The certificates above a leaf of this CA, nearest first, PEM.
    pub fn chain_pem(&self) -> String {
        let mut chain = vec![self.issuer.pem()];
        chain.extend(self.above.iter().cloned());
        chain.concat()
    }

    /// Writes, in `directory`, the machine certificate of `common_name`
    /// (valid from [`ISSUED_AT`] to [`EXPIRES_AT`]) with the chain above it,
    /// its key, and the CA, and gives their paths.
    pub fn write_machine(&self, directory: &std::path::Path, common_name: &str) -> MachineFiles {
        let (leaf, key) = self.leaf_pem(common_name, ISSUED_AT, EXPIRES_AT);
        let files = MachineFiles {
            certificate: directory.join(format!("{common_name}.crt")),
            private_key: directory.join(format!("{common_name}.key")),
            trust_anchor: directory.join("ca.crt"),
        };
        std::fs::write(&files.certificate, format!("{leaf}{}", self.chain_pem()))
            .expect("write the machine certificate");
        std::fs::write(&files.private_key, key).expect("write the machine key");
        std::fs::write(&files.trust_anchor, self.trust_anchor_pem()).expect("write the CA");
        files
    }
}

fn ca_params(name: &str) -> rcgen::CertificateParams {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("no names");
    params.distinguished_name.push(rcgen::DnType::CommonName, name);
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    params.not_before = time::OffsetDateTime::from_unix_timestamp(ISSUED_AT).expect("a date");
    params.not_after = time::OffsetDateTime::from_unix_timestamp(EXPIRES_AT).expect("a date");
    params
}
