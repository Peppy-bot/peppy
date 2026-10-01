//! Renewal of this machine's peer certificate.
//!
//! The platform signs again the request the peer enrolled with, so a renewal
//! sends no key and no request: it names the peer, and the answer carries a
//! new leaf for the key that is on disk. The identity of the peer (its id, its
//! zenoh id, its namespace and the router it dials) stays as it is, so the
//! daemon's router takes the new leaf without a restart: zenoh reads
//! `peer.crt` each time it opens the link to the cloud router.
//!
//! The platform accepts a renewal from a signed-in person only. [`renew`] uses
//! the cached session of this machine, and that session must be one of the
//! platform API the machine is enrolled at. With no session the renewal fails
//! with [`Error::NotAuthenticated`], and the certificate stays as it is.

use daemon_config::consts::PeppyDirs;

use crate::client::{PlatformApi, RouterPeerEnrolled};
use crate::enrollment::{self, Enrollment, EnrollmentDocument, IssuedMaterial};
use crate::error::{Error, Result};
use crate::http::HttpClient;
use crate::resolver::{self, Credential};
use crate::{profile, storage};

/// Renews the certificate of the enrollment under `dirs` and writes the new
/// material. Returns the renewed enrollment, with the validity of the new
/// leaf.
///
/// Nothing is written when the platform refuses, when its answer names a
/// different identity or carries a leaf that is not a certificate, or when
/// the enrollment on disk changed while the platform answered.
pub fn renew(dirs: &PeppyDirs, http: &HttpClient) -> Result<Enrollment> {
    renew_with(dirs, |document| {
        let credential = credential_for(dirs, http, document)?;
        PlatformApi::new(http, &document.api_url, credential).renew_peer(
            &document.workspace_id,
            &document.project_id,
            &document.peer_id,
        )
    })
}

/// [`renew`] with the call to the platform given by the caller: `ask` gets the
/// document of the enrollment and returns the platform's answer.
fn renew_with(
    dirs: &PeppyDirs,
    ask: impl FnOnce(&EnrollmentDocument) -> Result<RouterPeerEnrolled>,
) -> Result<Enrollment> {
    let enrolled = load_enrolled(dirs)?;
    let answer = ask(&enrolled.document)?;
    let renewed = IssuedMaterial::for_renewal(&enrolled.document, answer)?;

    // `peppy platform enroll --replace` and `unenroll` write the same files.
    // The new leaf belongs to the enrollment this renewal started from, so it
    // is written only while that enrollment is the one on disk.
    if enrollment::load(dirs)?.as_ref() != Some(&enrolled) {
        return Err(Error::Auth(
            "the enrollment changed during the renewal, so the renewed certificate was not \
             written"
                .to_string(),
        ));
    }
    enrollment::save_renewal(dirs, &renewed)?;
    Ok(Enrollment {
        document: renewed.document,
        certificate: renewed.certificate,
        ..enrolled
    })
}

fn load_enrolled(dirs: &PeppyDirs) -> Result<Enrollment> {
    enrollment::load(dirs)?.ok_or_else(|| {
        Error::Auth("this machine is not enrolled, so there is no certificate to renew".to_string())
    })
}

/// The bearer of the cached session, when that session is one of the platform
/// API the machine is enrolled at. A token of another platform cannot renew
/// the peer, so it is not sent there.
fn credential_for(
    dirs: &PeppyDirs,
    http: &HttpClient,
    document: &EnrollmentDocument,
) -> Result<Credential> {
    let creds_path = storage::credentials_path(dirs);
    let session = storage::load(&creds_path)?
        .session
        .ok_or(Error::NotAuthenticated)?;
    let session_origin = profile::normalize_api_origin(&session.api_url)?;
    let enrolled_origin = profile::normalize_api_origin(&document.api_url)?;
    if session_origin != enrolled_origin {
        return Err(Error::Auth(format!(
            "the session is one of {session_origin}, and this machine is enrolled at \
             {enrolled_origin}; run `peppy platform login --api-url {enrolled_origin}`"
        )));
    }
    resolver::resolve(&creds_path, http)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::PeerStatus;
    use crate::enrollment::{
        CertificateValidity, EnrollmentBundle, PEER_CERTIFICATE_FILE, RouterEndpoint,
    };
    use crate::test_support::{EXPIRES_AT, ISSUED_AT, PROJECT, ZID, leaf, leaf_pem, router_peer};
    use config::namespace::Namespace;
    use pmi::RouterId;

    const DAY: i64 = 24 * 60 * 60;

    fn answer(peer_id: &str, leaf: &str) -> RouterPeerEnrolled {
        RouterPeerEnrolled {
            peer: router_peer(peer_id, "robot-7", PeerStatus::Unknown),
            address: RouterEndpoint::parse("rtr-p.example", 7447).unwrap(),
            certificate: leaf.into(),
            chain: "chain".into(),
            trust_anchor: "ca".into(),
            zenoh_id: RouterId::parse(ZID).unwrap(),
            namespace: Namespace::parse(PROJECT).unwrap(),
            zenoh_config: "{}".into(),
        }
    }

    /// Enrolls `peer_id` with the fixture leaf, and returns that leaf.
    fn enroll(dirs: &PeppyDirs, peer_id: &str) -> String {
        let first_leaf = leaf();
        let bundle = EnrollmentBundle {
            peer_key_pem: storage::secret("key".into()),
            issued: IssuedMaterial::for_enrollment(
                "https://api.example.test",
                "ws-1",
                PROJECT,
                answer(peer_id, &first_leaf),
                ISSUED_AT,
            )
            .expect("a certificate"),
        };
        enrollment::save(dirs, &bundle).expect("enroll");
        first_leaf
    }

    fn leaf_on_disk(dirs: &PeppyDirs) -> String {
        std::fs::read_to_string(dirs.peer_dir().join(PEER_CERTIFICATE_FILE)).unwrap()
    }

    #[test]
    fn the_platform_is_asked_about_the_enrolled_peer_and_the_answer_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        enroll(&dirs, "peer-1");
        let second_leaf = leaf_pem(ISSUED_AT + 60 * DAY, EXPIRES_AT + 60 * DAY);

        let renewed = renew_with(&dirs, |document| {
            assert_eq!(document.peer_id, "peer-1");
            Ok(answer("peer-1", &second_leaf))
        })
        .expect("renewed");

        assert_eq!(
            renewed.certificate,
            CertificateValidity {
                not_before: ISSUED_AT + 60 * DAY,
                not_after: EXPIRES_AT + 60 * DAY,
            },
            "the validity is the new leaf's"
        );
        assert_eq!(renewed.document.enrolled_at, ISSUED_AT);
        assert_eq!(leaf_on_disk(&dirs), second_leaf);
        let on_disk = enrollment::load(&dirs).unwrap().expect("enrolled");
        assert_eq!(on_disk, renewed);
    }

    #[test]
    fn a_refusal_of_the_platform_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        let first_leaf = enroll(&dirs, "peer-1");
        let before = enrollment::load(&dirs).unwrap();

        let err = renew_with(&dirs, |_| Err(Error::NotAuthenticated)).expect_err("refused");

        assert!(matches!(err, Error::NotAuthenticated));
        assert_eq!(leaf_on_disk(&dirs), first_leaf);
        assert_eq!(enrollment::load(&dirs).unwrap(), before);
    }

    /// The person enrolls the machine again while the platform answers the
    /// renewal. The leaf of the renewal belongs to the peer that is gone, so
    /// the material of the new enrollment stays.
    #[test]
    fn an_enrollment_that_changed_during_the_renewal_keeps_its_material() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        enroll(&dirs, "peer-1");
        let mut second_enrollment_leaf = String::new();

        let err = renew_with(&dirs, |_| {
            second_enrollment_leaf = enroll(&dirs, "peer-2");
            Ok(answer("peer-1", &leaf()))
        })
        .expect_err("the enrollment changed");

        assert!(
            err.to_string().contains("changed during the renewal"),
            "{err}"
        );
        assert_eq!(leaf_on_disk(&dirs), second_enrollment_leaf);
        let on_disk = enrollment::load(&dirs).unwrap().expect("enrolled");
        assert_eq!(on_disk.document.peer_id, "peer-2");
    }

    /// The person removes the enrollment while the platform answers.
    #[test]
    fn an_enrollment_that_was_removed_during_the_renewal_stays_removed() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        enroll(&dirs, "peer-1");

        let err = renew_with(&dirs, |_| {
            enrollment::remove(&dirs).unwrap();
            Ok(answer("peer-1", &leaf()))
        })
        .expect_err("the enrollment is gone");

        assert!(
            err.to_string().contains("changed during the renewal"),
            "{err}"
        );
        assert!(!dirs.peer_dir().exists());
    }

    #[test]
    fn a_machine_that_is_not_enrolled_does_not_ask_the_platform() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());

        let err = renew_with(&dirs, |_| panic!("the platform must not be asked"))
            .expect_err("not enrolled");

        assert!(err.to_string().contains("not enrolled"), "{err}");
    }
}
