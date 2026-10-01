//! Fixtures for the tests of this crate and of the crates that consume it: a
//! valid enrollment, context, peer and refusal. A test takes the one it is
//! about and overrides the fields that matter to it with struct update
//! syntax, so one place holds what a valid value looks like.

use daemon_config::consts::PeppyDirs;

use crate::client::{PeerStatus, RouterPeer};
use crate::context::{CONTEXT_VERSION, Named, PlatformContext};
use crate::enrollment::{
    CertificateValidity, ENROLLMENT_VERSION, Enrollment, EnrollmentBundle, EnrollmentDocument,
    IssuedMaterial, RouterEndpoint, save,
};
use crate::error::{Problem, ProblemKind};
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

/// A context of `user-123` at `https://api.example`: project `Field` (`p-2`)
/// in workspace `Robotics lab` (`ws-2`).
pub fn platform_context() -> PlatformContext {
    PlatformContext {
        version: CONTEXT_VERSION,
        api_origin: "https://api.example".into(),
        subject: "user-123".into(),
        workspace: Named {
            id: "ws-2".into(),
            name: "Robotics lab".into(),
        },
        project: Named {
            id: "p-2".into(),
            name: "Field".into(),
        },
        selected_at: ISSUED_AT,
    }
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
