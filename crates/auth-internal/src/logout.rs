//! Non-interactive logout transaction used by the daemon controller and the
//! explicit offline-recovery path.

use daemon_config::consts::PeppyDirs;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::http::HttpClient;
use crate::{client, identity, resolver, storage};

/// One best-effort remote cleanup operation. Messages are intended for
/// sanitized local status; they never contain credentials or certificate PEM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupAttempt {
    NotNeeded,
    Succeeded,
    Failed(String),
}

/// Complete daemon-side logout result. Local cleanup is fail-closed and is
/// reported separately from the two best-effort remote calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutOutcome {
    pub certificate_revocation: CleanupAttempt,
    pub oauth_revocation: CleanupAttempt,
    pub local_cleanup: CleanupAttempt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingLogoutRecovery {
    None,
    Completed,
    Superseded,
}

/// Whether a durable logout commit survived an interrupted process. This is a
/// read-only preflight used by daemon startup so it can fence any exact router
/// orphan before [`recover_pending_local_logout`] deletes identity material.
pub fn pending_local_logout(dirs: &PeppyDirs) -> Result<bool> {
    let maintenance = identity::acquire_identity_maintenance(dirs)?;
    Ok(maintenance.read_logout_intent()?.is_some())
}

/// Logout transaction after best-effort remote revocation, while exclusive
/// certificate-maintenance ownership is still held. The daemon keeps this
/// value alive until the managed router is standalone (or stopped) and only
/// then calls [`PreparedLogout::finish_local_cleanup`].
#[derive(Debug)]
pub struct PreparedLogout {
    maintenance: identity::IdentityMaintenanceGuard,
    expected_session_revision: Option<Uuid>,
    certificate_revocation: CleanupAttempt,
    oauth_revocation: CleanupAttempt,
}

impl PreparedLogout {
    pub fn certificate_revocation(&self) -> &CleanupAttempt {
        &self.certificate_revocation
    }

    pub fn oauth_revocation(&self) -> &CleanupAttempt {
        &self.oauth_revocation
    }

    /// Rechecks the login CAS before deleting anything local. A fresh login
    /// that completed while remote cleanup was in flight always wins.
    pub fn finish_local_cleanup(self) -> Result<LogoutOutcome> {
        let local_cleanup = match self
            .maintenance
            .clear_local_logout(self.expected_session_revision)
        {
            Ok(()) => match self.maintenance.clear_logout_intent() {
                Ok(()) => CleanupAttempt::Succeeded,
                Err(error) => CleanupAttempt::Failed(error.to_string()),
            },
            Err(Error::StaleSessionRevision) => {
                // A newer login wins the CAS. Its state must not inherit this
                // old logout's crash-recovery intent.
                self.maintenance.clear_logout_intent()?;
                return Err(Error::StaleSessionRevision);
            }
            Err(error) => CleanupAttempt::Failed(error.to_string()),
        };
        Ok(LogoutOutcome {
            certificate_revocation: self.certificate_revocation,
            oauth_revocation: self.oauth_revocation,
            local_cleanup,
        })
    }
}

/// Revokes the currently bound enrollment/session when possible, then clears
/// every renewable local field and protected identity generation. An OAuth
/// caller is compare-and-swapped both before remote I/O and immediately before
/// local deletion so a delayed logout cannot erase a newer login.
pub fn logout_current_credential(
    dirs: &PeppyDirs,
    http: &HttpClient,
    expected_session_revision: Option<Uuid>,
) -> Result<LogoutOutcome> {
    prepare_logout_current_credential(dirs, http, expected_session_revision)?.finish_local_cleanup()
}

/// Performs the authenticated, best-effort remote half of logout and returns
/// a transaction that still owns certificate maintenance. It deliberately does
/// not delete local state: the daemon must first de-federate (or stop) its
/// managed router so no live process can retain deleted key material.
pub fn prepare_logout_current_credential(
    dirs: &PeppyDirs,
    http: &HttpClient,
    expected_session_revision: Option<Uuid>,
) -> Result<PreparedLogout> {
    let maintenance = identity::acquire_identity_maintenance(dirs)?;
    prepare_logout_with_maintenance(dirs, http, expected_session_revision, maintenance)
}

/// Explicit daemon-down recovery. The caller must already hold
/// [`identity::acquire_identity_owner`]. A valid OAuth session receives the
/// same best-effort remote cleanup as normal logout; malformed credentials are
/// intentionally reset so orphaned local identity material can still be
/// removed.
pub fn logout_offline_recovery(dirs: &PeppyDirs, http: &HttpClient) -> Result<LogoutOutcome> {
    let maintenance = identity::acquire_identity_maintenance(dirs)?;
    let credentials_path = storage::credentials_path(dirs);
    let expected_session_revision = match storage::load(&credentials_path) {
        Ok(credentials) => credentials
            .session
            .as_ref()
            .map(|session| session.session_revision),
        Err(_) => {
            let local_cleanup = match maintenance.clear_local_logout_offline() {
                Ok(()) => CleanupAttempt::Succeeded,
                Err(error) => CleanupAttempt::Failed(error.to_string()),
            };
            return Ok(LogoutOutcome {
                certificate_revocation: CleanupAttempt::NotNeeded,
                oauth_revocation: CleanupAttempt::NotNeeded,
                local_cleanup,
            });
        }
    };
    prepare_logout_with_maintenance(dirs, http, expected_session_revision, maintenance)?
        .finish_local_cleanup()
}

/// Completes a logout whose durable intent survived a process crash. Daemon
/// startup calls this before constructing a router, so revoked credentials or
/// certificate material can never be reused by a later generation.
pub fn recover_pending_local_logout(dirs: &PeppyDirs) -> Result<PendingLogoutRecovery> {
    let maintenance = identity::acquire_identity_maintenance(dirs)?;
    let Some(intent) = maintenance.read_logout_intent()? else {
        return Ok(PendingLogoutRecovery::None);
    };
    let credentials_path = storage::credentials_path(dirs);
    let cleanup = match storage::load(&credentials_path) {
        Ok(_) => maintenance.complete_pending_local_logout(intent.expected_session_revision),
        Err(_) => maintenance.clear_local_logout_offline(),
    };
    match cleanup {
        Ok(()) => {
            maintenance.clear_logout_intent()?;
            Ok(PendingLogoutRecovery::Completed)
        }
        Err(Error::StaleSessionRevision) => {
            maintenance.clear_logout_intent()?;
            Ok(PendingLogoutRecovery::Superseded)
        }
        Err(error) => Err(error),
    }
}

fn prepare_logout_with_maintenance(
    dirs: &PeppyDirs,
    http: &HttpClient,
    expected_session_revision: Option<Uuid>,
    maintenance: identity::IdentityMaintenanceGuard,
) -> Result<PreparedLogout> {
    let credentials_path = storage::credentials_path(dirs);
    let credentials = storage::load(&credentials_path)?;
    ensure_expected_session(&credentials, expected_session_revision)?;
    // This is the logout commit point. It precedes every remote side effect;
    // startup recovery completes local fail-closed cleanup if this process is
    // killed at any later instruction.
    maintenance.write_logout_intent(expected_session_revision)?;

    let session = credentials.session.as_ref();
    let identity_metadata_result = identity::load_identity_metadata(dirs);
    let identity_metadata = identity_metadata_result
        .as_ref()
        .ok()
        .cloned()
        .flatten()
        .or_else(|| credentials.core_node_identity.clone());

    let mut certificate_revocation = identity_metadata_result
        .err()
        .map_or(CleanupAttempt::NotNeeded, |error| {
            CleanupAttempt::Failed(error.to_string())
        });
    let mut oauth_revocation = CleanupAttempt::NotNeeded;

    if let Some(profile) = session {
        let mut credential = resolver::session_credential(&credentials_path, profile);
        if let Some(identity) = identity_metadata.as_ref() {
            certificate_revocation = match identity::normalize_api_origin(&profile.api_url) {
                Ok(origin) if origin == identity.api_origin => {
                    match client::delete_core_node_certificate(
                        http,
                        &profile.api_url,
                        &mut credential,
                        &identity.core_node_name,
                    ) {
                        Ok(204) => CleanupAttempt::Succeeded,
                        Ok(status) => CleanupAttempt::Failed(format!(
                            "certificate revocation returned HTTP {status}"
                        )),
                        Err(error) => CleanupAttempt::Failed(error.to_string()),
                    }
                }
                Ok(_) => CleanupAttempt::Failed(
                    "OAuth session and certificate belong to different API origins".into(),
                ),
                Err(error) => CleanupAttempt::Failed(error.to_string()),
            };
        }

        oauth_revocation = match client::logout(http, &profile.api_url, &credential) {
            Ok(202) | Ok(401) => CleanupAttempt::Succeeded,
            Ok(status) => {
                CleanupAttempt::Failed(format!("OAuth revocation returned HTTP {status}"))
            }
            Err(error) => CleanupAttempt::Failed(error.to_string()),
        };
    } else if identity_metadata.is_some() {
        certificate_revocation = CleanupAttempt::Failed(
            "no OAuth session is available for orphaned certificate revocation".into(),
        );
    }

    Ok(PreparedLogout {
        maintenance,
        expected_session_revision,
        certificate_revocation,
        oauth_revocation,
    })
}

fn ensure_expected_session(
    credentials: &storage::Credentials,
    expected_session_revision: Option<Uuid>,
) -> Result<()> {
    match (expected_session_revision, credentials.session.as_ref()) {
        (Some(expected), Some(session)) if session.session_revision == expected => Ok(()),
        (None, None) => Ok(()),
        (Some(_), _) | (None, Some(_)) => Err(Error::StaleSessionRevision),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn revision_guard_distinguishes_same_subject_sessions() {
        let mut credentials = storage::Credentials::default();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        credentials.session = Some(storage::ProfileCreds::with_tokens(
            first,
            "https://api.example".into(),
            "https://issuer.example".into(),
            "client".into(),
            "same-subject".into(),
            "user".into(),
            &crate::device::TokenSet {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: storage::now_unix() + 300,
                token_type: "Bearer".into(),
                scope: "openid".into(),
            },
        ));

        assert!(ensure_expected_session(&credentials, first.into()).is_ok());
        assert!(matches!(
            ensure_expected_session(&credentials, second.into()),
            Err(Error::StaleSessionRevision)
        ));
    }

    #[test]
    fn final_local_cleanup_cannot_erase_a_replacement_login() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temp.path());
        let credentials_path = storage::credentials_path(&dirs);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let session = |revision| {
            storage::ProfileCreds::with_tokens(
                revision,
                "http://127.0.0.1:9".into(),
                "http://127.0.0.1:9".into(),
                "client".into(),
                "same-subject".into(),
                "user".into(),
                &crate::device::TokenSet {
                    access_token: "access".into(),
                    refresh_token: "refresh".into(),
                    expires_at: storage::now_unix() + 300,
                    token_type: "Bearer".into(),
                    scope: "openid".into(),
                },
            )
        };
        storage::save(
            &credentials_path,
            &storage::Credentials {
                session: Some(session(first)),
                ..Default::default()
            },
        )
        .unwrap();
        let prepared = prepare_logout_current_credential(
            &dirs,
            &HttpClient::with_timeout(Duration::from_millis(10)),
            Some(first),
        )
        .unwrap();
        storage::update(&credentials_path, |credentials| {
            credentials.session = Some(session(second));
            Ok(())
        })
        .unwrap();

        assert!(matches!(
            prepared.finish_local_cleanup(),
            Err(Error::StaleSessionRevision)
        ));
        assert_eq!(
            storage::load(&credentials_path)
                .unwrap()
                .session
                .unwrap()
                .session_revision,
            second
        );
        assert_eq!(
            recover_pending_local_logout(&dirs).unwrap(),
            PendingLogoutRecovery::None,
            "the replacement login must not inherit the older logout intent"
        );
    }

    #[test]
    fn replacement_published_during_logout_cannot_recover_the_old_identity_after_crash() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temp.path());
        let credentials_path = storage::credentials_path(&dirs);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let session = |revision| {
            storage::ProfileCreds::with_tokens(
                revision,
                "http://127.0.0.1:9".into(),
                "http://127.0.0.1:9".into(),
                "client".into(),
                "same-subject".into(),
                "user".into(),
                &crate::device::TokenSet {
                    access_token: "access".into(),
                    refresh_token: "refresh".into(),
                    expires_at: storage::now_unix() + 300,
                    token_type: "Bearer".into(),
                    scope: "openid".into(),
                },
            )
        };
        storage::save(
            &credentials_path,
            &storage::Credentials {
                session: Some(session(first)),
                ..Default::default()
            },
        )
        .unwrap();
        let old_generation = identity::identity_root(&dirs)
            .join("generations")
            .join("old-identity-still-loaded-by-router");
        std::fs::create_dir_all(&old_generation).unwrap();
        identity::arm_binding_incomplete(&dirs).unwrap();

        let maintenance = identity::acquire_identity_maintenance(&dirs).unwrap();
        maintenance.write_logout_intent(Some(first)).unwrap();
        let replacement_path = credentials_path.clone();
        let replacement = session(second);
        identity::after_logout_credentials_cleared_for_test(move || {
            storage::update(&replacement_path, |credentials| {
                credentials.session = Some(replacement);
                Ok(())
            })
            .unwrap();
        });

        maintenance.clear_local_logout(Some(first)).unwrap();
        drop(maintenance); // Simulate a crash before the durable intent is removed.

        assert!(!identity::identity_root(&dirs).exists());
        assert!(!identity::binding_incomplete(&dirs).unwrap());
        assert_eq!(
            storage::load(&credentials_path)
                .unwrap()
                .session
                .unwrap()
                .session_revision,
            second
        );
        assert_eq!(
            recover_pending_local_logout(&dirs).unwrap(),
            PendingLogoutRecovery::Superseded
        );
        assert!(!identity::identity_root(&dirs).exists());
        assert_eq!(
            storage::load(&credentials_path)
                .unwrap()
                .session
                .unwrap()
                .session_revision,
            second,
            "startup may keep the replacement session but cannot resurrect the deleted old identity"
        );
    }

    #[test]
    fn startup_recovery_completes_a_crashed_logout_exactly_once() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temp.path());
        let credentials_path = storage::credentials_path(&dirs);
        let revision = Uuid::new_v4();
        storage::save(
            &credentials_path,
            &storage::Credentials {
                session: Some(storage::ProfileCreds::with_tokens(
                    revision,
                    "http://127.0.0.1:9".into(),
                    "http://127.0.0.1:9".into(),
                    "client".into(),
                    "subject".into(),
                    "user".into(),
                    &crate::device::TokenSet {
                        access_token: "access".into(),
                        refresh_token: "refresh".into(),
                        expires_at: storage::now_unix() + 300,
                        token_type: "Bearer".into(),
                        scope: "openid".into(),
                    },
                )),
                ..Default::default()
            },
        )
        .unwrap();
        let prepared = prepare_logout_current_credential(
            &dirs,
            &HttpClient::with_timeout(Duration::from_millis(10)),
            Some(revision),
        )
        .unwrap();
        drop(prepared);

        assert_eq!(
            recover_pending_local_logout(&dirs).unwrap(),
            PendingLogoutRecovery::Completed
        );
        let credentials = storage::load(&credentials_path).unwrap();
        assert!(credentials.session.is_none());
        assert!(credentials.router.is_none());
        assert!(credentials.core_node_identity.is_none());
        assert_eq!(
            recover_pending_local_logout(&dirs).unwrap(),
            PendingLogoutRecovery::None
        );
    }

    #[test]
    fn offline_recovery_clears_malformed_credentials_without_authentication() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temp.path());
        let credentials_path = storage::credentials_path(&dirs);
        std::fs::create_dir_all(credentials_path.parent().unwrap()).unwrap();
        std::fs::write(&credentials_path, "{ malformed").unwrap();
        let intent_path = dirs
            .root()
            .join("conf")
            .join(".platform-logout-pending.json5");
        std::fs::write(&intent_path, "{ malformed").unwrap();
        let _owner = identity::acquire_identity_owner(&dirs).unwrap();

        let outcome =
            logout_offline_recovery(&dirs, &HttpClient::with_timeout(Duration::from_millis(10)))
                .unwrap();

        assert_eq!(outcome.local_cleanup, CleanupAttempt::Succeeded);
        let reset = storage::load(&credentials_path).unwrap();
        assert_eq!(reset.version, storage::CREDENTIALS_VERSION);
        assert!(reset.session.is_none());
        assert!(reset.router.is_none());
        assert!(reset.core_node_identity.is_none());
        assert!(!intent_path.exists());
    }
}
