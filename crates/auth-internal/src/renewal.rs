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

use crate::client::{self, RouterPeerEnrolled};
use crate::enrollment::{self, Enrollment, EnrollmentDocument, IssuedMaterial};
use crate::error::{Error, Result};
use crate::http::HttpClient;
use crate::resolver::{self, Credential};
use crate::{profile, storage};

/// Renews the certificate of the enrollment under `dirs` and writes the new
/// material. Returns the document of the renewed enrollment. `now_unix` is
/// recorded as the issue time of the new leaf.
///
/// Nothing is written when the platform refuses, when its answer names a
/// different identity, or when the enrollment on disk changed while the
/// platform answered.
pub fn renew(dirs: &PeppyDirs, http: &HttpClient, now_unix: i64) -> Result<EnrollmentDocument> {
    renew_with(dirs, now_unix, |document| {
        let mut credential = credential_for(dirs, http, document)?;
        client::renew_peer(
            http,
            &document.api_url,
            &mut credential,
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
    now_unix: i64,
    ask: impl FnOnce(&EnrollmentDocument) -> Result<RouterPeerEnrolled>,
) -> Result<EnrollmentDocument> {
    let enrolled = load_enrolled(dirs)?;
    let answer = ask(&enrolled.document)?;
    let renewed = IssuedMaterial::for_renewal(&enrolled.document, answer, now_unix)?;

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
    Ok(renewed.document)
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
    use crate::enrollment::{EnrollmentBundle, PEER_CERTIFICATE_FILE, RouterEndpoint};
    use crate::test_support::router_peer;
    use config::namespace::Namespace;
    use pmi::RouterId;

    const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
    const ENROLLED_AT: i64 = 1_700_000_000;

    fn answer(peer_id: &str, leaf: &str) -> RouterPeerEnrolled {
        RouterPeerEnrolled {
            peer: router_peer(peer_id, "robot-7", PeerStatus::Unknown),
            address: RouterEndpoint::parse("rtr-p.example", 7447).unwrap(),
            certificate: leaf.into(),
            chain: "chain".into(),
            trust_anchor: "ca".into(),
            zenoh_id: RouterId::parse("2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5").unwrap(),
            namespace: Namespace::parse(PROJECT).unwrap(),
            zenoh_config: "{}".into(),
        }
    }

    fn enroll(dirs: &PeppyDirs, peer_id: &str) {
        let bundle = EnrollmentBundle {
            peer_key_pem: storage::secret("key".into()),
            issued: IssuedMaterial::for_enrollment(
                "https://api.example.test",
                "ws-1",
                PROJECT,
                answer(peer_id, "first leaf"),
                ENROLLED_AT,
            ),
        };
        enrollment::save(dirs, &bundle).expect("enroll");
    }

    fn leaf_on_disk(dirs: &PeppyDirs) -> String {
        std::fs::read_to_string(dirs.peer_dir().join(PEER_CERTIFICATE_FILE)).unwrap()
    }

    #[test]
    fn the_platform_is_asked_about_the_enrolled_peer_and_the_answer_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        enroll(&dirs, "peer-1");

        let document = renew_with(&dirs, ENROLLED_AT + 100, |document| {
            assert_eq!(document.peer_id, "peer-1");
            Ok(answer("peer-1", "second leaf"))
        })
        .expect("renewed");

        assert_eq!(document.certificate_issued_at, ENROLLED_AT + 100);
        assert_eq!(document.enrolled_at, ENROLLED_AT);
        assert_eq!(leaf_on_disk(&dirs), "second leaf");
        let on_disk = enrollment::load(&dirs).unwrap().expect("enrolled");
        assert_eq!(on_disk.document, document);
    }

    #[test]
    fn a_refusal_of_the_platform_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        enroll(&dirs, "peer-1");
        let before = enrollment::load(&dirs).unwrap();

        let err = renew_with(&dirs, ENROLLED_AT + 100, |_| Err(Error::NotAuthenticated))
            .expect_err("refused");

        assert!(matches!(err, Error::NotAuthenticated));
        assert_eq!(leaf_on_disk(&dirs), "first leaf");
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

        let err = renew_with(&dirs, ENROLLED_AT + 100, |_| {
            enroll(&dirs, "peer-2");
            Ok(answer("peer-1", "second leaf"))
        })
        .expect_err("the enrollment changed");

        assert!(
            err.to_string().contains("changed during the renewal"),
            "{err}"
        );
        assert_eq!(leaf_on_disk(&dirs), "first leaf");
        let on_disk = enrollment::load(&dirs).unwrap().expect("enrolled");
        assert_eq!(on_disk.document.peer_id, "peer-2");
    }

    /// The person removes the enrollment while the platform answers.
    #[test]
    fn an_enrollment_that_was_removed_during_the_renewal_stays_removed() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        enroll(&dirs, "peer-1");

        let err = renew_with(&dirs, ENROLLED_AT + 100, |_| {
            enrollment::remove(&dirs).unwrap();
            Ok(answer("peer-1", "second leaf"))
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

        let err = renew_with(&dirs, ENROLLED_AT, |_| {
            panic!("the platform must not be asked")
        })
        .expect_err("not enrolled");

        assert!(err.to_string().contains("not enrolled"), "{err}");
    }
}
