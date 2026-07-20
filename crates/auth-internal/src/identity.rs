//! Production per-core-node client identity enrollment and protected storage.
//!
//! The daemon identity is exactly its already-resolved `core_node_name`; this
//! module never creates a second device identifier. Each enrollment stages a
//! locally-generated ECDSA P-256 PKCS#8 key in an immutable generation, sends
//! only a proof-of-possession CSR, validates the returned client certificate,
//! and atomically publishes non-secret metadata after the generation is fully
//! durable. A stable advisory lock serializes CLI enrollment, daemon renewal,
//! rollback, and logout cleanup across processes.

use std::path::PathBuf;

use daemon_config::consts::PeppyDirs;
use federation_identity::{
    CryptoError, EnrollmentRequest, IdentityError, IdentityLock, IdentityOwnerGuard, IdentityStore,
    KeyPair, PendingEnrollment, RecoveryTarget, ReturnedCertificate, RotationLease, RotationRecord,
    build_csr, validate_returned_certificate,
};

use crate::client::CoreNodeCertificateResponse;
use crate::error::{Error, Result};
use crate::{client, http::HttpClient, resolver, resolver::Credential, storage};

#[cfg(test)]
const PENDING_FILE: &str = "pending.json5";
#[cfg(test)]
const UNVERIFIED_FILE: &str = "unverified-rotation.json5";
#[cfg(test)]
const GENERATIONS_DIR: &str = "generations";

pub use federation_identity::{CoreNodeIdentity, IdentityPaths};

/// An activated rotation retained until the caller verifies the real mTLS
/// link. A failed apply/probe can restore the prior still-valid generation.
#[derive(Debug)]
pub struct IdentityRotation {
    dirs: PeppyDirs,
    record: RotationRecord,
    activated: CoreNodeIdentity,
    lease: Option<RotationLease>,
    armed: bool,
}

/// Rejected generation retained after metadata rollback. The running router
/// may still have these paths open/configured until its prior link has been
/// re-applied and probed, so deletion is an explicit post-restore operation.
#[derive(Debug)]
pub struct RejectedIdentityGeneration {
    dirs: PeppyDirs,
    generation: Option<String>,
    restored_previous: bool,
    /// Transferred from the rejected rotation and held until the caller has
    /// finished reapplying/probing the prior router state (or drops this token).
    _lease: RotationLease,
}

/// Exclusive guard for logout or other destructive identity maintenance.
/// Acquiring it before any remote side effect guarantees a concurrent daemon
/// rotation cannot make local cleanup fail after the bearer/enrollment has
/// already been revoked.
#[derive(Debug)]
pub struct IdentityMaintenanceGuard {
    dirs: PeppyDirs,
    _lease: RotationLease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogoutCleanupFence {
    Exact(Option<uuid::Uuid>),
    PendingRecovery(Option<uuid::Uuid>),
}

fn identity_store(dirs: &PeppyDirs) -> IdentityStore {
    IdentityStore::new(dirs.root())
}

fn identity_error(error: IdentityError) -> Error {
    match error {
        IdentityError::Io(error) => Error::Io(error),
        IdentityError::Invalid(message) => Error::Auth(message),
    }
}

impl IdentityMaintenanceGuard {
    pub fn write_logout_intent(&self, expected_session_revision: Option<uuid::Uuid>) -> Result<()> {
        identity_store(&self.dirs)
            .write_logout_intent(&federation_identity::LogoutIntent {
                version: 1,
                expected_session_revision,
            })
            .map_err(identity_error)
    }

    pub fn read_logout_intent(&self) -> Result<Option<federation_identity::LogoutIntent>> {
        identity_store(&self.dirs)
            .read_logout_intent()
            .map_err(identity_error)
    }

    pub fn clear_logout_intent(&self) -> Result<()> {
        identity_store(&self.dirs)
            .clear_logout_intent()
            .map_err(identity_error)
    }

    pub fn prepare_pat_login(&self) -> Result<()> {
        prepare_pat_login_with_lease(&self.dirs)
    }

    pub fn enroll_and_activate(
        self,
        http: &HttpClient,
        api_url: &str,
        credential: &mut Credential,
        subject: &str,
        core_node_name: &str,
    ) -> Result<IdentityRotation> {
        let Self {
            dirs,
            _lease: lease,
        } = self;
        enroll_and_activate_with_lease(
            &dirs,
            http,
            api_url,
            credential,
            subject,
            core_node_name,
            lease,
        )
    }

    /// Removes all protected generations and their non-secret credentials
    /// mirror while this operation remains the unique rotation owner.
    pub fn clear_local_identity(&self) -> Result<()> {
        clear_local_identity_with_lease(&self.dirs, None)
    }

    /// Durably clears every renewable/authenticated local field in one first
    /// transaction, then removes key material. A crash may leave orphaned key
    /// files, but can never leave a refresh session capable of re-enrollment.
    pub fn clear_local_logout(&self, expected_session_revision: Option<uuid::Uuid>) -> Result<()> {
        clear_local_identity_with_lease(
            &self.dirs,
            Some(LogoutCleanupFence::Exact(expected_session_revision)),
        )
    }

    pub fn complete_pending_local_logout(
        &self,
        expected_session_revision: Option<uuid::Uuid>,
    ) -> Result<()> {
        clear_local_identity_with_lease(
            &self.dirs,
            Some(LogoutCleanupFence::PendingRecovery(
                expected_session_revision,
            )),
        )
    }

    /// Explicit offline recovery for a credentials document that cannot be
    /// parsed. The caller must hold the lifetime identity-owner lock and have
    /// proven the daemon is stopped. Renewable credentials are reset before
    /// orphaned key material is removed.
    pub fn clear_local_logout_offline(&self) -> Result<()> {
        let store = identity_store(&self.dirs);
        let lock = store.acquire_lock().map_err(identity_error)?;
        storage::reset_for_offline_recovery(&storage::credentials_path(&self.dirs))?;
        store.clear_identity(&lock).map_err(identity_error)?;
        // Keep the fail-closed marker armed until all old key material is gone.
        // A crash can leave a harmless marker, never a marker-free reusable
        // identity generation.
        store
            .clear_binding_incomplete(&lock)
            .map_err(identity_error)?;
        // Explicit recovery is allowed to remove an unreadable intent without
        // parsing it; otherwise malformed state could permanently block the
        // next daemon startup even though all renewable state was reset.
        store.clear_logout_intent().map_err(identity_error)
    }
}

impl RejectedIdentityGeneration {
    /// Whether rollback restored a still-valid, fully-validated prior identity.
    /// `false` requires the router restore to apply intentional standalone;
    /// callers must never infer eligibility from stale pre-rotation state.
    pub fn restored_previous(&self) -> bool {
        self.restored_previous
    }

    /// Deletes the rejected immutable generation only after the caller has
    /// confirmed that Zenoh is using the restored prior generation or is
    /// intentionally standalone.
    pub fn cleanup_after_router_restore(self) -> Result<()> {
        let Some(generation) = self.generation else {
            return Ok(());
        };
        let store = identity_store(&self.dirs);
        let lock = store.acquire_lock().map_err(identity_error)?;
        store
            .remove_generation_if_inactive(&lock, &generation)
            .map_err(identity_error)
    }
}

impl IdentityRotation {
    pub fn activated(&self) -> &CoreNodeIdentity {
        &self.activated
    }

    fn publish_pat_precedence(&self) -> Result<()> {
        prepare_pat_login_with_lease(&self.dirs)
    }

    fn clear_logout_intent(&self) -> Result<()> {
        identity_store(&self.dirs)
            .clear_logout_intent()
            .map_err(identity_error)
    }

    /// Keeps the activated generation and removes superseded generations only
    /// after the managed router has passed its mTLS probe.
    pub fn commit_after_probe(mut self) -> Result<()> {
        // The router has verified this exact generation. Disarm automatic
        // rollback before finalizing the durable receipt: if finalization
        // fails, the controller explicitly applies standalone and startup
        // recovery retains enough state to settle the transaction.
        let credentials_path = storage::credentials_path(&self.dirs);
        let store = identity_store(&self.dirs);
        let lock = store.acquire_lock().map_err(identity_error)?;
        storage::inspect_locked(&credentials_path, |credentials| {
            ensure_rotation_session_current(credentials, self.activated.session_revision)?;
            self.armed = false;
            store
                .finalize_rotation(&lock, &self.record)
                .map_err(identity_error)?;
            store
                .clear_binding_incomplete_if_matches(&lock, self.activated.session_revision)
                .map_err(identity_error)?;
            if let Err(error) =
                store.prune_generations(&lock, &self.record.activated().active_generation)
            {
                // Receipt removal and transition clearing already made the
                // commit unambiguous. Superseded immutable files are safe
                // cleanup debt.
                tracing::warn!(
                    error = %error,
                    event = "identity_generation_prune_failed",
                    "core-node identity: committed rotation left superseded generations"
                );
            }
            Ok(())
        })
    }

    /// Completes a locally valid rotation for an operator-managed router.
    /// Superseded immutable generations are deliberately retained because
    /// Peppy cannot know which path the external router still consumes.
    pub fn commit_for_operator_managed_router(mut self) -> Result<()> {
        let credentials_path = storage::credentials_path(&self.dirs);
        let store = identity_store(&self.dirs);
        let lock = store.acquire_lock().map_err(identity_error)?;
        storage::inspect_locked(&credentials_path, |credentials| {
            ensure_rotation_session_current(credentials, self.activated.session_revision)?;
            self.armed = false;
            store
                .commit_operator_managed_rotation(&lock, &self.record)
                .map_err(identity_error)?;
            store
                .clear_binding_incomplete_if_matches(&lock, self.activated.session_revision)
                .map_err(identity_error)
        })
    }

    /// Restores the previous metadata pointer while retaining the rejected
    /// generation. Callers that do not own router restoration can safely leave
    /// the unreferenced files for the next verified commit/prune.
    pub fn rollback(mut self) -> Result<()> {
        let result = self.rollback_inner().map(drop);
        if result.is_ok() {
            self.armed = false;
        }
        result
    }

    /// Restores prior metadata and returns a cleanup token for the rejected
    /// generation. The daemon consumes it only after prior-link reapply/probe
    /// or an intentional standalone apply has succeeded.
    pub fn rollback_for_router_restore(mut self) -> Result<RejectedIdentityGeneration> {
        let result = self.rollback_inner();
        if result.is_ok() {
            self.armed = false;
        }
        result
    }

    /// Releases this in-process owner so the federation resolver can recover
    /// the same durable receipt. The explicit-login transition remains armed
    /// until router apply/probe commits the recovered rotation.
    pub fn handoff_to_resolver(mut self) -> Result<()> {
        let store = identity_store(&self.dirs);
        let lock = store.acquire_lock().map_err(identity_error)?;
        store
            .retain_rotation(&lock, &self.record)
            .map_err(identity_error)?;
        self.armed = false;
        Ok(())
    }

    /// Keeps the activated pointer across a namespace-generation restart while
    /// retaining the durable unverified receipt. The matching explicit-login
    /// transition is cleared so the next daemon generation can recover the
    /// receipt through ordinary startup reconciliation, force a real probe,
    /// and only then prune.
    pub fn retain_for_restart(mut self) -> Result<()> {
        let store = identity_store(&self.dirs);
        let lock = store.acquire_lock().map_err(identity_error)?;
        store
            .retain_rotation(&lock, &self.record)
            .map_err(identity_error)?;
        store
            .clear_binding_incomplete_if_matches(&lock, self.activated.session_revision)
            .map_err(identity_error)?;
        self.armed = false;
        Ok(())
    }

    fn rollback_inner(&mut self) -> Result<RejectedIdentityGeneration> {
        let store = identity_store(&self.dirs);
        let lock = store.acquire_lock().map_err(identity_error)?;
        let publication = store
            .prepare_rollback(&lock, &self.record, storage::now_unix())
            .map_err(identity_error)?;
        let previous = publication.previous_owned();
        storage::update(&storage::credentials_path(&self.dirs), |creds| {
            creds.core_node_identity = previous.clone();
            creds.router = None;
            Ok(())
        })?;
        store
            .finish_rollback(&lock, &self.record, &publication)
            .map_err(identity_error)?;
        let generation = publication.rejected_generation().map(str::to_owned);
        let lease = self.lease.take().ok_or_else(|| {
            Error::Auth("core-node rotation lease ownership disappeared before rollback".into())
        })?;
        Ok(RejectedIdentityGeneration {
            dirs: self.dirs.clone(),
            generation,
            restored_previous: previous.is_some(),
            _lease: lease,
        })
    }
}

fn ensure_rotation_session_current(
    credentials: &storage::Credentials,
    expected: Option<uuid::Uuid>,
) -> Result<()> {
    match (expected, credentials.session.as_ref()) {
        (Some(expected), Some(session)) if session.session_revision == expected => Ok(()),
        (None, None) => Ok(()),
        _ => Err(Error::StaleSessionRevision),
    }
}

impl Drop for IdentityRotation {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = self.rollback_inner()
        {
            tracing::error!(
                error = %error,
                generation = %self.activated.active_generation,
                "core-node identity: could not roll back an abandoned unverified generation"
            );
        }
    }
}

/// Acquires the stable process-lifetime identity owner lock. The daemon holds
/// it above its in-process restart loop; explicit offline maintenance acquires
/// the same lock and therefore cannot overlap daemon recovery or renewal.
pub fn acquire_identity_owner(dirs: &PeppyDirs) -> Result<IdentityOwnerGuard> {
    identity_store(dirs)
        .try_acquire_owner()
        .map_err(identity_error)
}

/// Whether this binary requires an enrolled production identity. Debug builds
/// retain the isolated committed development certificate path.
pub const fn production_identity_required() -> bool {
    !cfg!(debug_assertions)
}

/// Durably marks an authentication change whose exact core-node binding is not
/// ready for router use yet. The daemon treats presence (or an unreadable/
/// unsafe marker) as intentional standalone and must not maintain or reuse a
/// prior identity until login completes the handoff.
pub fn arm_binding_incomplete(dirs: &PeppyDirs) -> Result<()> {
    arm_binding_incomplete_for_session(dirs, None)
}

/// Arms a transition owned by the exact OAuth revision that will be published
/// after Prepare succeeds. A later Prepare atomically supersedes this owner.
pub fn arm_binding_incomplete_for_session(
    dirs: &PeppyDirs,
    expected_session_revision: Option<uuid::Uuid>,
) -> Result<()> {
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    store
        .arm_binding_incomplete(&lock, expected_session_revision)
        .map_err(identity_error)
}

/// Removes the binding transition only after login has handed off a usable
/// identity, or as part of durable logout cleanup. Parent-directory fsync makes
/// the removal crash durable.
pub fn clear_binding_incomplete(dirs: &PeppyDirs) -> Result<()> {
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    store
        .clear_binding_incomplete(&lock)
        .map_err(identity_error)
}

/// Rejects enrollment from a superseded Prepare before it can touch the router
/// or certificate store.
pub fn ensure_binding_transition_current(
    dirs: &PeppyDirs,
    expected_session_revision: uuid::Uuid,
) -> Result<()> {
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    match store.binding_transition(&lock).map_err(identity_error)? {
        Some(transition)
            if transition.expected_session_revision == Some(expected_session_revision) =>
        {
            Ok(())
        }
        _ => Err(Error::StaleSessionRevision),
    }
}

/// Publishes a freshly authorized OAuth session only while its exact Prepare
/// still owns the durable transition. The identity lock spans the marker CAS
/// and credentials rename, so a newer Prepare or completed logout cannot be
/// followed by resurrection from an older paused CLI.
pub fn publish_oauth_session(dirs: &PeppyDirs, session: storage::ProfileCreds) -> Result<()> {
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    match store.binding_transition(&lock).map_err(identity_error)? {
        Some(transition)
            if transition.expected_session_revision == Some(session.session_revision) => {}
        _ => return Err(Error::StaleSessionRevision),
    }
    storage::update(&storage::credentials_path(dirs), move |credentials| {
        credentials.session = Some(session);
        credentials.router = None;
        Ok(())
    })
}

/// Whether a login binding transition is still incomplete. Protected-file
/// ownership, type, symlink, and mode checks run before reporting presence.
pub fn binding_incomplete(dirs: &PeppyDirs) -> Result<bool> {
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    Ok(store
        .binding_transition(&lock)
        .map_err(identity_error)?
        .is_some())
}

/// Status-only transition-marker read. Unsafe modes are reported without
/// chmodding the marker or its parent.
pub fn binding_incomplete_read_only(dirs: &PeppyDirs) -> Result<bool> {
    identity_store(dirs)
        .binding_incomplete_read_only()
        .map_err(identity_error)
}

pub fn identity_root(dirs: &PeppyDirs) -> PathBuf {
    identity_store(dirs).identity_root()
}

#[cfg(test)]
fn identity_path(dirs: &PeppyDirs) -> PathBuf {
    identity_store(dirs).identity_path()
}

#[cfg(test)]
fn pending_path(dirs: &PeppyDirs) -> PathBuf {
    identity_root(dirs).join(PENDING_FILE)
}

#[cfg(test)]
fn unverified_path(dirs: &PeppyDirs) -> PathBuf {
    identity_root(dirs).join(UNVERIFIED_FILE)
}

#[cfg(test)]
fn generation_dir(dirs: &PeppyDirs, generation: &str) -> PathBuf {
    identity_root(dirs).join(GENERATIONS_DIR).join(generation)
}

/// Normalizes and validates a platform API origin for identity binding.
pub fn normalize_api_origin(api_url: &str) -> Result<String> {
    federation_identity::normalize_api_origin(api_url).map_err(identity_error)
}

/// Makes a PAT login the sole active authentication mode without ever writing
/// the PAT itself. Credentials parsing remains fail-closed; a valid v1
/// document's identity mirror is reconciled to the protected canonical pointer.
pub fn prepare_pat_login(dirs: &PeppyDirs) -> Result<()> {
    let _lease = identity_store(dirs)
        .try_acquire_rotation_lease()
        .map_err(identity_error)?;
    prepare_pat_login_with_lease(dirs)
}

fn prepare_pat_login_with_lease(dirs: &PeppyDirs) -> Result<()> {
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    let pointer = match store.read_pointer(&lock) {
        Ok(pointer) => pointer,
        Err(_) => {
            store.clear_identity(&lock).map_err(identity_error)?;
            None
        }
    };
    storage::update(&storage::credentials_path(dirs), |creds| {
        creds.session = None;
        creds.router = None;
        creds.core_node_identity = pointer;
        Ok(())
    })?;
    // A successfully authenticated PAT login supersedes any older logout
    // intent. PATs have no persisted revision, so this unlink is the durable
    // epoch boundary that prevents recovery from deleting the new identity.
    store.clear_logout_intent().map_err(identity_error)
}

/// Ensures the production daemon has a usable certificate for its immutable
/// startup name, rotating at `renew_after` and automatically re-binding a
/// same-owner identity after a name/API/workspace change. A different
/// authenticated subject is never allowed to take over the stored name.
///
/// Returns an activated rotation that must be committed only after the managed
/// router applies the generation and its real mTLS probe succeeds. On a
/// transient enrollment failure the current still-valid generation is left
/// untouched for the caller to continue using until the next maintenance wake.
pub fn maintain_identity(
    dirs: &PeppyDirs,
    http: &HttpClient,
    api_url: &str,
    pat: Option<String>,
    core_node_name: &str,
) -> Result<Option<IdentityRotation>> {
    maintain_identity_inner(dirs, http, api_url, pat, core_node_name, false, None, None)
}

/// Forces a same-owner rotation after typed federation discovery reports that
/// the backend workspace no longer matches the active certificate binding.
pub fn rotate_identity_for_binding_change(
    dirs: &PeppyDirs,
    http: &HttpClient,
    api_url: &str,
    pat: Option<String>,
    core_node_name: &str,
) -> Result<Option<IdentityRotation>> {
    maintain_identity_inner(dirs, http, api_url, pat, core_node_name, true, None, None)
}

/// Forces enrollment using the credential currently owned by the daemon. An
/// OAuth handoff is compare-and-swapped against the revision supplied by the
/// CLI; a configured daemon PAT is ambient and therefore has no revision.
pub fn enroll_current_credential(
    dirs: &PeppyDirs,
    http: &HttpClient,
    api_url: &str,
    daemon_pat: Option<String>,
    core_node_name: &str,
    expected_session_revision: Option<uuid::Uuid>,
    expected_pat_subject: Option<String>,
) -> Result<IdentityRotation> {
    let daemon_pat = daemon_pat.filter(|pat| !pat.is_empty());
    let required_session_revision = match (
        expected_session_revision,
        daemon_pat.as_ref(),
        expected_pat_subject.as_deref(),
    ) {
        // The request shape, not ambient precedence, selects the operation.
        // An OAuth handoff must never be consumed as a PAT login or erase the
        // exact session the CLI just published.
        (Some(_), Some(_), _) => return Err(Error::PatActive),
        (Some(expected), None, None) => Some(expected),
        (None, Some(_), Some(subject)) if !subject.is_empty() => None,
        (None, Some(_), _) => {
            return Err(Error::Auth(
                "PAT enrollment requires the validated CLI principal".into(),
            ));
        }
        (None, None, _) => return Err(Error::PatNotConfigured),
        (Some(_), None, Some(_)) => {
            return Err(Error::Auth(
                "an OAuth enrollment cannot carry a PAT principal".into(),
            ));
        }
    };
    let rotation = maintain_identity_inner(
        dirs,
        http,
        api_url,
        daemon_pat,
        core_node_name,
        true,
        required_session_revision,
        expected_pat_subject.as_deref(),
    )?
    .ok_or_else(|| Error::Auth("forced core-node enrollment produced no rotation".into()))?;
    // A successfully enrolled explicit login supersedes an older logout
    // transaction under the same rotation lease. This also covers OAuth,
    // whose fresh revision otherwise has no chance to clear the old marker.
    rotation.clear_logout_intent()?;
    Ok(rotation)
}

/// Recovers only the durable receipt that can complete an interrupted login
/// handoff. It never enrolls a new generation: without an exact receipt the
/// binding-incomplete gate remains fail-closed until login is retried.
pub fn recover_incomplete_binding_rotation(
    dirs: &PeppyDirs,
    api_url: &str,
    pat_active: bool,
    core_node_name: &str,
) -> Result<Option<IdentityRotation>> {
    validate_core_node_name(core_node_name)?;
    let api_origin = normalize_api_origin(api_url)?;
    let credentials = storage::load(&storage::credentials_path(dirs))?;
    let expected_revision = if pat_active {
        None
    } else {
        credentials
            .session
            .as_ref()
            .map(|session| session.session_revision)
    };
    let expected_subject = if pat_active {
        None
    } else {
        credentials
            .session
            .as_ref()
            .map(|session| session.subject.as_str())
    };
    let Some(rotation) = recover_unverified_rotation(dirs)? else {
        return Ok(None);
    };
    // A durable receipt is necessary but not sufficient while a login
    // transition exists: a later OAuth Prepare may already have superseded the
    // receipt's owner without publishing its credentials yet. Never apply the
    // older receipt under the newer transition marker.
    let transition = {
        let store = identity_store(dirs);
        let lock = store.acquire_lock().map_err(identity_error)?;
        store.binding_transition(&lock).map_err(identity_error)?
    };
    if transition
        .as_ref()
        .is_some_and(|transition| transition.expected_session_revision != expected_revision)
    {
        rotation.rollback()?;
        return Err(Error::StaleSessionRevision);
    }
    let exact = rotation.activated.api_origin == api_origin
        && rotation.activated.core_node_name == core_node_name
        && rotation.activated.session_revision == expected_revision
        && expected_subject.is_none_or(|subject| rotation.activated.subject == subject);
    if exact {
        return Ok(Some(rotation));
    }
    rotation.rollback()?;
    Err(Error::Auth(
        "the interrupted enrollment receipt does not match the current authentication binding"
            .into(),
    ))
}

/// Compare-and-swap fence used by the daemon immediately before router apply
/// and again before rotation commit. PAT identities carry no revision and are
/// validated through the daemon's ambient service credential instead.
pub fn ensure_session_revision_current(
    dirs: &PeppyDirs,
    expected_session_revision: Option<uuid::Uuid>,
) -> Result<()> {
    let Some(expected) = expected_session_revision else {
        return Ok(());
    };
    let credentials = storage::load(&storage::credentials_path(dirs))?;
    if credentials
        .session
        .as_ref()
        .is_some_and(|session| session.session_revision == expected)
    {
        Ok(())
    } else {
        Err(Error::StaleSessionRevision)
    }
}

#[allow(clippy::too_many_arguments)]
fn maintain_identity_inner(
    dirs: &PeppyDirs,
    http: &HttpClient,
    api_url: &str,
    pat: Option<String>,
    core_node_name: &str,
    force_rotation: bool,
    required_session_revision: Option<uuid::Uuid>,
    expected_pat_subject: Option<&str>,
) -> Result<Option<IdentityRotation>> {
    validate_core_node_name(core_node_name)?;
    let api_origin = normalize_api_origin(api_url)?;
    let creds_path = storage::credentials_path(dirs);
    let stored_credentials = storage::load(&creds_path)?;
    let pat_configured = pat.as_ref().is_some_and(|pat| !pat.is_empty());
    let stored_session_revision = stored_credentials
        .session
        .as_ref()
        .map(|session| session.session_revision);
    if let Some(expected) = required_session_revision
        && stored_session_revision != Some(expected)
    {
        return Err(Error::StaleSessionRevision);
    }
    // Validate the ambient PAT without mutating local ownership. Precedence is
    // published only after the authenticated subject passes the existing
    // certificate-owner check below.
    let validated_pat = if pat_configured {
        let mut credential = resolver::resolve(&creds_path, http, pat.clone())?;
        let principal = client::get_me(http, api_url, &mut credential)?;
        if expected_pat_subject.is_some_and(|expected| principal.sub != expected) {
            return Err(Error::PatPrincipalMismatch);
        }
        Some((credential, principal))
    } else {
        None
    };
    // Prepare owns the OAuth transition before the session is published. Do
    // not re-arm it here: that would let an older enrollment overwrite a newer
    // concurrent Prepare. Require the exact persisted transition owner instead.
    if let Some(expected) = required_session_revision {
        ensure_binding_transition_current(dirs, expected)?;
    }
    let active_session_revision = if pat_configured {
        None
    } else {
        required_session_revision.or(stored_session_revision)
    };
    let active_subject = validated_pat
        .as_ref()
        .map(|(_, principal)| principal.sub.as_str())
        .or_else(|| {
            stored_credentials
                .session
                .as_ref()
                .map(|session| session.subject.as_str())
        });
    if let Some(rotation) = recover_unverified_rotation(dirs)? {
        if rotation.activated.api_origin == api_origin
            && rotation.activated.core_node_name == core_node_name
            && rotation.activated.session_revision == active_session_revision
            && active_subject.is_none_or(|subject| rotation.activated.subject == subject)
        {
            if pat_configured {
                rotation.publish_pat_precedence()?;
            }
            return Ok(Some(rotation));
        }
        // This process cannot apply an abandoned rotation for another binding.
        // Its armed drop restores the prior valid generation before continuing.
        rotation.rollback()?;
    }
    let metadata = load_identity_metadata(dirs)?;
    if let Some((_, principal)) = validated_pat.as_ref() {
        ensure_same_identity_owner(metadata.as_ref(), &principal.sub)?;
        prepare_pat_login(dirs)?;
        if force_rotation {
            arm_binding_incomplete(dirs)?;
        }
    }
    let stored_subject = active_subject
        .map(str::to_owned)
        .filter(|subject| !subject.is_empty());
    let exact_binding = metadata.as_ref().is_some_and(|identity| {
        identity.api_origin == api_origin
            && identity.core_node_name == core_node_name
            && identity.session_revision == active_session_revision
    });
    if !force_rotation
        && exact_binding
        && let Some(identity) = metadata.as_ref()
        && !identity.renewal_due(storage::now_unix())
        && load_active_identity(dirs, api_url, stored_subject.as_deref(), core_node_name).is_ok()
    {
        return Ok(None);
    }

    let (mut credential, principal) = if let Some(validated) = validated_pat {
        validated
    } else {
        let mut credential = resolver::resolve(&creds_path, http, pat)?;
        if let Some(expected) = required_session_revision
            && credential.session_revision() != Some(expected)
        {
            return Err(Error::StaleSessionRevision);
        }
        let principal = client::get_me(http, api_url, &mut credential)?;
        (credential, principal)
    };
    ensure_same_identity_owner(metadata.as_ref(), &principal.sub)?;

    // A restarted daemon under a new fixed name is a new federation identity.
    // Revoke the old enrollment best-effort while the same authenticated owner
    // is available, but do not let cleanup failure prevent enrollment of the
    // current captured name. Cross-origin revocation is deliberately skipped:
    // a token minted for the new API must not be forwarded to the old origin.
    if let Some(previous) = metadata.as_ref()
        && previous.core_node_name != core_node_name
        && previous.api_origin == api_origin
    {
        match client::delete_core_node_certificate(
            http,
            api_url,
            &mut credential,
            &previous.core_node_name,
        ) {
            Ok(204) => {}
            Ok(status) => tracing::warn!(
                old_core_node_name = %previous.core_node_name,
                new_core_node_name = %core_node_name,
                status,
                "core-node identity: previous-name revocation returned a non-success status; continuing with current-name enrollment"
            ),
            Err(error) => tracing::warn!(
                old_core_node_name = %previous.core_node_name,
                new_core_node_name = %core_node_name,
                error = %error,
                "core-node identity: could not revoke the previous-name enrollment; continuing with current-name enrollment"
            ),
        }
    }

    enroll_and_activate(
        dirs,
        http,
        api_url,
        &mut credential,
        &principal.sub,
        core_node_name,
    )
    .map(Some)
}

/// Generates/reuses a pending key, enrolls it, validates the returned leaf, and
/// atomically activates the new immutable generation. OAuth/PAT material is
/// never written by this function; only the supplied bearer is used in memory.
pub fn enroll_and_activate(
    dirs: &PeppyDirs,
    http: &HttpClient,
    api_url: &str,
    credential: &mut Credential,
    subject: &str,
    core_node_name: &str,
) -> Result<IdentityRotation> {
    let store = identity_store(dirs);
    enroll_and_activate_with_lease(
        dirs,
        http,
        api_url,
        credential,
        subject,
        core_node_name,
        store.try_acquire_rotation_lease().map_err(identity_error)?,
    )
}

fn enroll_and_activate_with_lease(
    dirs: &PeppyDirs,
    http: &HttpClient,
    api_url: &str,
    credential: &mut Credential,
    subject: &str,
    core_node_name: &str,
    initial_lease: RotationLease,
) -> Result<IdentityRotation> {
    if subject.is_empty() {
        return Err(Error::Auth(
            "cannot enroll a core-node certificate without an authenticated subject".into(),
        ));
    }
    validate_core_node_name(core_node_name)?;
    let api_origin = normalize_api_origin(api_url)?;
    let session_revision = credential.session_revision();
    let lease = match recover_unverified_rotation_with_lease(dirs, initial_lease)? {
        RecoveredRotation::Active(rotation)
            if rotation.activated.api_origin == api_origin
                && rotation.activated.subject == subject
                && rotation.activated.session_revision == session_revision
                && rotation.activated.core_node_name == core_node_name =>
        {
            return Ok(*rotation);
        }
        RecoveredRotation::Active(rotation) => {
            // Resolve the old receipt under its unique lease before creating a
            // new binding. Its rejected files stay until a later verified prune.
            rotation.rollback()?;
            identity_store(dirs)
                .try_acquire_rotation_lease()
                .map_err(identity_error)?
        }
        RecoveredRotation::Clean(lease) => lease,
    };
    // Logout may have won while a daemon resolved `/me` and waited for the
    // rotation lease. Revalidate the exact OAuth session before recreating the
    // identity root or staging any pending private key. PAT credentials are a
    // no-op here and remain environment-only.
    crate::resolver::ensure_session_credential_current(credential)?;
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    store.ensure_private_layout(&lock).map_err(identity_error)?;

    let previous = load_metadata_locked(dirs, &store, &lock, true)?;
    // Login uses this direct enrollment path after resolving its principal.
    // Enforce the same local ownership invariant as daemon maintenance before
    // staging a key or sending a CSR: a backend conflict response is defense in
    // depth, not authorization to transfer a globally reserved core-node name.
    ensure_same_identity_owner(previous.as_ref(), subject)?;
    let pending = store
        .prepare_generation(&lock, &api_origin, subject, core_node_name)
        .map_err(identity_error)?;
    let csr_pem = build_csr(pending.key()).map_err(crypto_error)?;
    let response = match client::enroll_core_node_certificate(
        http,
        api_url,
        credential,
        core_node_name,
        &csr_pem,
    ) {
        Ok(response) => response,
        Err(error @ (Error::CoreNodeRevoked(_) | Error::CoreNodeKeyAlreadyUsed(_))) => {
            // These stable conflicts can never succeed with the same retained
            // CSR/SPKI. Discard only the pending, non-active generation so the
            // next retry produces a fresh P-256 key instead of wedging forever.
            store
                .discard_pending_generation(
                    &lock,
                    &pending.enrollment().generation,
                    previous.as_ref(),
                )
                .map_err(identity_error)?;
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    if let Err(error) = crate::resolver::ensure_session_credential_current(credential) {
        store
            .discard_pending_generation(&lock, &pending.enrollment().generation, previous.as_ref())
            .map_err(identity_error)?;
        return Err(error);
    }
    let activated = validate_enrollment_response(
        &api_origin,
        subject,
        session_revision,
        core_node_name,
        pending.enrollment(),
        pending.key(),
        &response,
    )?;

    store
        .write_certificate_chain(
            &lock,
            &pending.enrollment().generation,
            &response.certificate_chain_pem,
        )
        .map_err(identity_error)?;

    // Persist rollback intent before publishing the new pointer. If the
    // process is cancelled, crashes, or restarts for a namespace change, a
    // later daemon generation can either probe+commit or restore `previous`.
    let record = store
        .begin_rotation(&lock, previous.clone(), activated.clone())
        .map_err(identity_error)?;
    let rotation = IdentityRotation {
        dirs: dirs.clone(),
        record,
        activated,
        lease: Some(lease),
        armed: true,
    };

    // Publish the canonical pointer first, then mirror it into credentials v1
    // while the cross-process lock excludes every reader/writer in this module.
    let creds_path = storage::credentials_path(dirs);
    if let Err(error) = storage::update(&creds_path, |creds| {
        creds.core_node_identity = Some(rotation.activated.clone());
        creds.router = None;
        Ok(())
    }) {
        // The atomic rename may already be visible even though its parent
        // fsync failed. From the moment begin_rotation succeeds, the armed
        // guard owns every exit: release the short store lock, then restore the
        // prior pointer/mirror through the ordinary receipt-CAS rollback.
        drop(lock);
        return Err(rollback_after_publication_error(rotation, error));
    }
    if let Err(error) = store.finish_rotation_publication(&lock, &rotation.record) {
        // Pending-file cleanup is still inside the unverified transaction.
        // Never return a plain error while the activated pointer is visible:
        // otherwise the caller could resolve and apply it without owning the
        // receipt. Roll it back synchronously while the armed guard retains
        // the cross-process lease.
        drop(lock);
        return Err(rollback_after_publication_error(
            rotation,
            identity_error(error),
        ));
    }

    Ok(rotation)
}

fn rollback_after_publication_error(rotation: IdentityRotation, error: Error) -> Error {
    match rotation.rollback() {
        Ok(()) => error,
        Err(rollback) => Error::Auth(format!(
            "{error}; the activated core-node generation could not be rolled back immediately: {rollback}"
        )),
    }
}

fn recover_unverified_rotation(dirs: &PeppyDirs) -> Result<Option<IdentityRotation>> {
    let lease = identity_store(dirs)
        .try_acquire_rotation_lease()
        .map_err(identity_error)?;
    match recover_unverified_rotation_with_lease(dirs, lease)? {
        RecoveredRotation::Clean(_) => Ok(None),
        RecoveredRotation::Active(rotation) => Ok(Some(*rotation)),
    }
}

enum RecoveredRotation {
    Clean(RotationLease),
    Active(Box<IdentityRotation>),
}

fn recover_unverified_rotation_with_lease(
    dirs: &PeppyDirs,
    lease: RotationLease,
) -> Result<RecoveredRotation> {
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    let Some(plan) = store.recovery_plan(&lock).map_err(identity_error)? else {
        return Ok(RecoveredRotation::Clean(lease));
    };
    let mirrored = plan.mirror_identity().cloned();
    let target = plan.target();
    let record = plan.rotation().clone();
    tracing::info!(
        event = "identity_rotation_recovery",
        target = match target {
            RecoveryTarget::Activated => "activated",
            RecoveryTarget::Previous => "previous",
        },
        "core-node identity: recovering an interrupted rotation"
    );
    let creds_path = storage::credentials_path(dirs);
    if target == RecoveryTarget::Activated {
        let activated = record.activated().clone();
        let rotation = IdentityRotation {
            dirs: dirs.clone(),
            record,
            activated,
            lease: Some(lease),
            armed: true,
        };
        if let Err(error) = storage::update(&creds_path, |creds| {
            creds.core_node_identity = mirrored.clone();
            creds.router = None;
            Ok(())
        }) {
            drop(lock);
            return Err(rollback_after_publication_error(rotation, error));
        }
        if let Err(error) = store.finish_recovery(&lock, &plan) {
            drop(lock);
            return Err(rollback_after_publication_error(
                rotation,
                identity_error(error),
            ));
        }
        return Ok(RecoveredRotation::Active(Box::new(rotation)));
    }

    storage::update(&creds_path, |creds| {
        creds.core_node_identity = mirrored.clone();
        creds.router = None;
        Ok(())
    })?;
    store
        .finish_recovery(&lock, &plan)
        .map_err(identity_error)?;
    // A crash before activation or midway through rollback leaves the
    // rejected generation ambiguous; a later verified prune removes it.
    Ok(RecoveredRotation::Clean(lease))
}

/// Whether durable state still contains an unresolved rotation receipt. A
/// caller that does not own the corresponding armed guard must remain
/// standalone even if the activated metadata pointer is readable.
pub fn unverified_rotation_pending(dirs: &PeppyDirs) -> Result<bool> {
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    store
        .unverified_rotation_pending(&lock)
        .map_err(identity_error)
}

fn ensure_same_identity_owner(
    previous: Option<&CoreNodeIdentity>,
    authenticated_subject: &str,
) -> Result<()> {
    if let Some(previous) = previous
        && previous.subject != authenticated_subject
    {
        return Err(Error::Auth(format!(
            "the stored core-node certificate belongs to a different platform account; refusing to transfer `{}` implicitly. Release it with the owning account or run `peppy platform logout` before logging in as another account",
            previous.core_node_name
        )));
    }
    Ok(())
}

/// Rolls back a durable unverified rotation when no managed daemon owns the
/// apply. Used by login only after it has established that the daemon is not
/// running; live-daemon failures are resolved by the daemon-owned receipt to
/// avoid racing a timed-out but still-running apply.
pub fn rollback_unverified_rotation(dirs: &PeppyDirs) -> Result<bool> {
    let Some(rotation) = recover_unverified_rotation(dirs)? else {
        return Ok(false);
    };
    rotation
        .rollback_for_router_restore()?
        .cleanup_after_router_restore()?;
    Ok(true)
}

/// Loads and fully validates the active production identity for this exact API
/// origin, authenticated subject (when known), workspace, and core-node name.
/// Missing, malformed, mismatched, not-yet-valid, or expired material is an
/// error; callers must not render a certificate-less upstream.
pub fn load_active_identity(
    dirs: &PeppyDirs,
    api_url: &str,
    subject: Option<&str>,
    core_node_name: &str,
) -> Result<(CoreNodeIdentity, IdentityPaths)> {
    let api_origin = normalize_api_origin(api_url)?;
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    let metadata = load_metadata_locked(dirs, &store, &lock, true)?.ok_or_else(|| {
        Error::Auth("core-node certificate identity is missing; run `peppy platform login`".into())
    })?;
    if metadata.api_origin != api_origin {
        return Err(Error::Auth(format!(
            "core-node certificate belongs to API origin {}, not {}; run `peppy platform login`",
            metadata.api_origin, api_origin
        )));
    }
    if metadata.core_node_name != core_node_name {
        return Err(Error::Auth(format!(
            "core-node certificate belongs to `{}`, but the running daemon is `{core_node_name}`; run `peppy platform login`",
            metadata.core_node_name
        )));
    }
    if let Some(subject) = subject
        && metadata.subject != subject
    {
        return Err(Error::Auth(
            "core-node certificate belongs to a different platform account; run `peppy platform login`".into(),
        ));
    }
    let paths = store
        .validate_stored_material(&metadata, storage::now_unix())
        .map_err(identity_error)?;
    Ok((metadata, paths))
}

/// Validates that metadata points to an intact immutable key/certificate
/// generation without conflating material integrity with the current wall
/// clock. Status uses this to distinguish expired/not-yet-valid certificates
/// from missing or corrupt files.
pub fn validate_identity_material(
    dirs: &PeppyDirs,
    metadata: &CoreNodeIdentity,
) -> Result<IdentityPaths> {
    let store = identity_store(dirs);
    let _lock = store.acquire_lock().map_err(identity_error)?;
    store
        .validate_stored_material(metadata, metadata.not_before)
        .map_err(identity_error)
}

/// Status-only material validation. Unlike the daemon's repairing load, this
/// never creates a lock or changes directory/file modes.
pub fn validate_identity_material_read_only(
    dirs: &PeppyDirs,
    metadata: &CoreNodeIdentity,
) -> Result<IdentityPaths> {
    identity_store(dirs)
        .validate_stored_material_read_only(metadata, metadata.not_before)
        .map_err(identity_error)
}

/// Removes all local key/certificate generations and identity metadata. The
/// stable lock file remains so concurrent/future operations keep one inode.
pub fn clear_local_identity(dirs: &PeppyDirs) -> Result<()> {
    acquire_identity_maintenance(dirs)?.clear_local_identity()
}

/// Acquires exclusive identity maintenance before logout begins remote
/// revocation. This is deliberately fail-fast: when a daemon apply/probe owns
/// a rotation, logout changes nothing remotely or locally and can be retried.
pub fn acquire_identity_maintenance(dirs: &PeppyDirs) -> Result<IdentityMaintenanceGuard> {
    let store = identity_store(dirs);
    Ok(IdentityMaintenanceGuard {
        dirs: dirs.clone(),
        _lease: store.try_acquire_rotation_lease().map_err(identity_error)?,
    })
}

fn clear_local_identity_with_lease(
    dirs: &PeppyDirs,
    logout_fence: Option<LogoutCleanupFence>,
) -> Result<()> {
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    let creds_path = storage::credentials_path(dirs);
    storage::update(&creds_path, |creds| {
        if let Some(fence) = logout_fence {
            let expected = match fence {
                LogoutCleanupFence::Exact(expected)
                | LogoutCleanupFence::PendingRecovery(expected) => expected,
            };
            let current = creds
                .session
                .as_ref()
                .map(|session| session.session_revision);
            let matches = current == expected
                || matches!(fence, LogoutCleanupFence::PendingRecovery(Some(_)))
                    && current.is_none();
            if !matches {
                return Err(Error::StaleSessionRevision);
            }
        }
        creds.core_node_identity = None;
        creds.router = None;
        if logout_fence.is_some() {
            creds.session = None;
        }
        Ok(())
    })?;
    #[cfg(test)]
    run_after_logout_credentials_cleared_hook();
    // The transition marker is the crash fence for old material. Remove the
    // generation tree first; if deletion fails or the process dies, the marker
    // remains and startup is forced standalone.
    store.clear_identity(&lock).map_err(identity_error)?;
    if logout_fence.is_some() {
        store
            .clear_binding_incomplete(&lock)
            .map_err(identity_error)?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static AFTER_LOGOUT_CREDENTIALS_CLEARED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn after_logout_credentials_cleared_for_test(hook: impl FnOnce() + 'static) {
    AFTER_LOGOUT_CREDENTIALS_CLEARED.with(|slot| slot.replace(Some(Box::new(hook))));
}

#[cfg(test)]
fn run_after_logout_credentials_cleared_hook() {
    AFTER_LOGOUT_CREDENTIALS_CLEARED.with(|slot| {
        if let Some(hook) = slot.take() {
            hook();
        }
    });
}

/// Reads the non-secret identity metadata for logout/status without exposing
/// the private layout. An interrupted credentials mirror is repaired while the
/// cross-process identity lock is held.
pub fn load_identity_metadata(dirs: &PeppyDirs) -> Result<Option<CoreNodeIdentity>> {
    let store = identity_store(dirs);
    let lock = store.acquire_lock().map_err(identity_error)?;
    load_metadata_locked(dirs, &store, &lock, true)
}

/// Reads non-secret identity metadata without repairing either durable mirror.
/// CLI status paths use this boundary so a read can never republish a
/// generation while daemon-owned receipt recovery or rollback is incomplete.
pub fn load_identity_metadata_read_only(dirs: &PeppyDirs) -> Result<Option<CoreNodeIdentity>> {
    let store = identity_store(dirs);
    let pointer = store.read_pointer_read_only().map_err(identity_error)?;
    let credentials = storage::load_read_only(&storage::credentials_path(dirs))?;
    if let Some(identity) = pointer.as_ref() {
        federation_identity::validate_identity_metadata_shape(identity).map_err(identity_error)?;
    }
    if let Some(identity) = credentials.core_node_identity.as_ref() {
        federation_identity::validate_identity_metadata_shape(identity).map_err(identity_error)?;
    }
    match (&pointer, &credentials.core_node_identity) {
        (Some(left), Some(right)) if left != right => Err(Error::Auth(
            "core-node identity metadata is inconsistent; run `peppy platform logout`, then login again"
                .into(),
        )),
        (Some(identity), _) | (_, Some(identity)) => Ok(Some(identity.clone())),
        (None, None) => Ok(None),
    }
}

/// Removes superseded generations after an mTLS probe has verified the active
/// identity. A pending retry generation is retained.
pub fn prune_generations(dirs: &PeppyDirs) -> Result<()> {
    let store = identity_store(dirs);
    let _lease = store.try_acquire_rotation_lease().map_err(identity_error)?;
    let lock = store.acquire_lock().map_err(identity_error)?;
    let Some(active) = load_metadata_locked(dirs, &store, &lock, true)? else {
        return Ok(());
    };
    store
        .prune_generations(&lock, &active.active_generation)
        .map_err(identity_error)
}

fn crypto_error(error: CryptoError) -> Error {
    Error::Auth(error.to_string())
}

fn validate_enrollment_response(
    api_origin: &str,
    subject: &str,
    session_revision: Option<uuid::Uuid>,
    core_node_name: &str,
    pending: &PendingEnrollment,
    key: &KeyPair,
    response: &CoreNodeCertificateResponse,
) -> Result<CoreNodeIdentity> {
    validate_returned_certificate(
        EnrollmentRequest {
            api_origin,
            subject,
            session_revision,
            core_node_name,
            generation: &pending.generation,
            spki_sha256: &pending.spki_sha256,
        },
        key,
        ReturnedCertificate {
            core_node_name: &response.core_node_name,
            workspace_id: &response.workspace_id,
            certificate_chain_pem: &response.certificate_chain_pem,
            serial_number: &response.serial_number,
            not_before: &response.not_before,
            not_after: &response.not_after,
            renew_after: &response.renew_after,
        },
        storage::now_unix(),
    )
    .map_err(crypto_error)
}

fn load_metadata_locked(
    dirs: &PeppyDirs,
    store: &IdentityStore,
    lock: &IdentityLock,
    repair_mirror: bool,
) -> Result<Option<CoreNodeIdentity>> {
    let pointer = store.read_pointer(lock).map_err(identity_error)?;
    let creds_path = storage::credentials_path(dirs);
    let creds = storage::load(&creds_path)?;
    if let Some(identity) = pointer.as_ref() {
        federation_identity::validate_identity_metadata_shape(identity).map_err(identity_error)?;
    }
    if let Some(identity) = creds.core_node_identity.as_ref() {
        federation_identity::validate_identity_metadata_shape(identity).map_err(identity_error)?;
    }
    match (&pointer, &creds.core_node_identity) {
        (Some(left), Some(right)) if left != right => Err(Error::Auth(
            "core-node identity metadata is inconsistent; run `peppy platform logout`, then login again"
                .into(),
        )),
        (Some(identity), None) if repair_mirror => {
            storage::update(&creds_path, |current| match current.core_node_identity.as_ref() {
                None => {
                    current.core_node_identity = Some(identity.clone());
                    Ok(())
                }
                Some(current_identity) if current_identity == identity => Ok(()),
                Some(_) => Err(Error::Auth(
                    "core-node identity metadata changed while repairing its credentials mirror"
                        .into(),
                )),
            })?;
            Ok(Some(identity.clone()))
        }
        (None, Some(identity)) if repair_mirror => {
            store
                .publish_pointer(lock, Some(identity))
                .map_err(identity_error)?;
            Ok(Some(identity.clone()))
        }
        (Some(identity), _) | (_, Some(identity)) => Ok(Some(identity.clone())),
        (None, None) => Ok(None),
    }
}

fn validate_core_node_name(name: &str) -> Result<()> {
    federation_identity::validate_core_node_name(name).map_err(identity_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use config::namespace::Namespace;
    use federation_identity::{
        identity_uri, inspect_leaf, is_valid_positive_der_serial, spki_fingerprint,
    };
    use httpmock::prelude::*;
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
        IsCa, Issuer, KeyUsagePurpose, SanType,
    };
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use serde::{Deserialize, Serialize, de::DeserializeOwned};
    use serde_json::json;
    use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
    use x509_parser::prelude::FromDer;

    const CORE_NODE: &str = "core-node-test-0001";
    const SUBJECT: &str = "user-test-subject";
    const WORKSPACE: &str = "550e8400-e29b-41d4-a716-446655440000";
    const BINDING_INCOMPLETE_FILE: &str = ".platform-binding-incomplete";
    const ROTATION_RECEIPT_VERSION: u32 = 1;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct PersistedRotation {
        receipt_version: u32,
        receipt_id: String,
        previous: Option<CoreNodeIdentity>,
        activated: CoreNodeIdentity,
    }

    fn validate_persisted_receipt(receipt: &PersistedRotation) -> Result<()> {
        let canonical_id = uuid::Uuid::parse_str(&receipt.receipt_id)
            .map(|id| id.hyphenated().to_string() == receipt.receipt_id)
            .unwrap_or(false);
        if receipt.receipt_version != ROTATION_RECEIPT_VERSION || !canonical_id {
            return Err(Error::Auth("invalid test rotation receipt".into()));
        }
        federation_identity::validate_identity_metadata_shape(&receipt.activated)
            .map_err(identity_error)?;
        if let Some(previous) = receipt.previous.as_ref() {
            federation_identity::validate_identity_metadata_shape(previous)
                .map_err(identity_error)?;
        }
        Ok(())
    }

    fn ensure_private_layout(dirs: &PeppyDirs) -> Result<()> {
        let store = identity_store(dirs);
        let lock = store.acquire_lock().map_err(identity_error)?;
        store.ensure_private_layout(&lock).map_err(identity_error)
    }

    fn publish_identity_locked(
        dirs: &PeppyDirs,
        identity: Option<&CoreNodeIdentity>,
    ) -> Result<()> {
        let store = identity_store(dirs);
        let lock = store.acquire_lock().map_err(identity_error)?;
        store
            .publish_pointer(&lock, identity)
            .map_err(identity_error)
    }

    fn paths_for(dirs: &PeppyDirs, identity: &CoreNodeIdentity) -> IdentityPaths {
        identity_store(dirs).paths_for(identity).unwrap()
    }

    fn read_optional_json5<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json5::from_str(&contents)
                .map(Some)
                .map_err(|error| Error::Auth(error.to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn write_json5_private_durable<T: Serialize>(path: &Path, value: &T) -> Result<()> {
        let contents = json5_pretty::to_string_pretty(value)
            .map_err(|error| Error::Auth(error.to_string()))?;
        std::fs::write(path, contents)?;
        Ok(())
    }

    #[test]
    fn binding_incomplete_marker_round_trips_as_a_private_durable_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(tmp.path());
        assert!(!binding_incomplete(&dirs).unwrap());

        arm_binding_incomplete(&dirs).unwrap();
        assert!(binding_incomplete(&dirs).unwrap());
        let marker: federation_identity::BindingTransition =
            read_optional_json5(&dirs.conf_dir().join(BINDING_INCOMPLETE_FILE))
                .unwrap()
                .unwrap();
        assert_eq!(marker.version, 1);
        assert_eq!(marker.expected_session_revision, None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dirs.conf_dir().join(BINDING_INCOMPLETE_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        clear_binding_incomplete(&dirs).unwrap();
        assert!(!binding_incomplete(&dirs).unwrap());
    }

    #[test]
    fn later_oauth_prepare_supersedes_earlier_transition_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(tmp.path());
        let older = uuid::Uuid::new_v4();
        let newer = uuid::Uuid::new_v4();

        arm_binding_incomplete_for_session(&dirs, Some(older)).unwrap();
        ensure_binding_transition_current(&dirs, older).unwrap();
        arm_binding_incomplete_for_session(&dirs, Some(newer)).unwrap();

        assert!(matches!(
            ensure_binding_transition_current(&dirs, older),
            Err(Error::StaleSessionRevision)
        ));
        ensure_binding_transition_current(&dirs, newer).unwrap();
        let store = identity_store(&dirs);
        let lock = store.acquire_lock().unwrap();
        assert_eq!(
            store
                .binding_transition(&lock)
                .unwrap()
                .unwrap()
                .expected_session_revision,
            Some(newer)
        );
    }

    fn oauth_session(revision: uuid::Uuid, access_token: &str) -> storage::ProfileCreds {
        storage::ProfileCreds::with_tokens(
            revision,
            "https://api.peppy.bot".into(),
            "https://issuer.peppy.bot".into(),
            "client".into(),
            SUBJECT.into(),
            "user".into(),
            &crate::device::TokenSet {
                access_token: access_token.into(),
                refresh_token: format!("refresh-{access_token}"),
                expires_at: storage::now_unix() + 300,
                token_type: "Bearer".into(),
                scope: "openid".into(),
            },
        )
    }

    #[test]
    fn oauth_publication_cas_rejects_superseded_or_logged_out_prepare() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(tmp.path());
        let older = uuid::Uuid::new_v4();
        let newer = uuid::Uuid::new_v4();

        arm_binding_incomplete_for_session(&dirs, Some(older)).unwrap();
        arm_binding_incomplete_for_session(&dirs, Some(newer)).unwrap();
        assert!(matches!(
            publish_oauth_session(&dirs, oauth_session(older, "older")),
            Err(Error::StaleSessionRevision)
        ));
        assert!(
            storage::load(&storage::credentials_path(&dirs))
                .unwrap()
                .session
                .is_none(),
            "a superseded CLI must not publish its session"
        );

        publish_oauth_session(&dirs, oauth_session(newer, "newer")).unwrap();
        assert_eq!(
            storage::load(&storage::credentials_path(&dirs))
                .unwrap()
                .session
                .unwrap()
                .session_revision,
            newer
        );

        clear_binding_incomplete(&dirs).unwrap();
        storage::update(&storage::credentials_path(&dirs), |credentials| {
            credentials.session = None;
            Ok(())
        })
        .unwrap();
        assert!(matches!(
            publish_oauth_session(&dirs, oauth_session(older, "resurrected")),
            Err(Error::StaleSessionRevision)
        ));
        assert!(
            storage::load(&storage::credentials_path(&dirs))
                .unwrap()
                .session
                .is_none(),
            "a completed logout must not be followed by stale session resurrection"
        );
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
    fn rotation_receipt_rejects_path_traversal_generation_ids() {
        let receipt = PersistedRotation {
            receipt_version: ROTATION_RECEIPT_VERSION,
            receipt_id: "44444444-4444-4444-8444-444444444444".into(),
            previous: None,
            activated: metadata_for_generation("../outside"),
        };
        assert!(validate_persisted_receipt(&receipt).is_err());
    }

    fn issued_response(key: &KeyPair, core_node_name: &str) -> CoreNodeCertificateResponse {
        let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        issued_response_with_ca(key, core_node_name, &ca_key)
    }

    fn issued_response_with_ca(
        key: &KeyPair,
        core_node_name: &str,
        ca_key: &KeyPair,
    ) -> CoreNodeCertificateResponse {
        let now = OffsetDateTime::now_utc();
        let not_before = now - Duration::minutes(1);
        let not_after = now + Duration::hours(24);
        let renew_after = now + Duration::hours(12);

        let mut ca_params = CertificateParams::default();
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
        ];
        ca_params.not_before = not_before;
        ca_params.not_after = not_after + Duration::days(1);
        let ca = ca_params.self_signed(ca_key).unwrap();
        let issuer = Issuer::from_params(&ca_params, ca_key);

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

        let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(leaf.der()).unwrap();
        CoreNodeCertificateResponse {
            core_node_name: core_node_name.into(),
            workspace_id: WORKSPACE.into(),
            certificate_chain_pem: format!("{}{}", leaf.pem(), ca.pem()),
            serial_number: parsed.raw_serial_as_string(),
            not_before: not_before.format(&Rfc3339).unwrap(),
            not_after: not_after.format(&Rfc3339).unwrap(),
            renew_after: renew_after.format(&Rfc3339).unwrap(),
        }
    }

    fn enroll_test_rotation<'a>(
        server: &'a MockServer,
        dirs: &PeppyDirs,
    ) -> (IdentityRotation, httpmock::Mock<'a>) {
        let api_origin = normalize_api_origin(&server.base_url()).unwrap();
        let response = {
            let store = identity_store(dirs);
            let lock = store.acquire_lock().unwrap();
            let pending = store
                .prepare_generation(&lock, &api_origin, SUBJECT, CORE_NODE)
                .unwrap();
            issued_response(pending.key(), CORE_NODE)
        };
        let enrollment = server.mock(|when, then| {
            when.method(POST)
                .path("/me/cli/core-node-certificates")
                .header("authorization", "Bearer test-pat");
            then.status(200).json_body(json!({
                "core_node_name": response.core_node_name,
                "workspace_id": response.workspace_id,
                "certificate_chain_pem": response.certificate_chain_pem,
                "serial_number": response.serial_number,
                "not_before": response.not_before,
                "not_after": response.not_after,
                "renew_after": response.renew_after,
            }));
        });
        let http = HttpClient::new();
        let mut credential = crate::resolver::resolve(
            &storage::credentials_path(dirs),
            &http,
            Some("test-pat".into()),
        )
        .unwrap();
        let rotation = enroll_and_activate(
            dirs,
            &http,
            &server.base_url(),
            &mut credential,
            SUBJECT,
            CORE_NODE,
        )
        .unwrap();
        enrollment.assert_calls(1);
        (rotation, enrollment)
    }

    #[test]
    fn enrollment_publishes_a_valid_private_generation_and_v1_metadata() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let api_origin = normalize_api_origin(&server.base_url()).unwrap();

        // Stage the pending key first so the static mock can issue a certificate
        // for the exact SPKI enroll_and_activate will retry.
        let response = {
            let store = identity_store(&dirs);
            let lock = store.acquire_lock().unwrap();
            let pending = store
                .prepare_generation(&lock, &api_origin, SUBJECT, CORE_NODE)
                .unwrap();
            issued_response(pending.key(), CORE_NODE)
        };
        let enrollment = server.mock(|when, then| {
            when.method(POST)
                .path("/me/cli/core-node-certificates")
                .header("authorization", "Bearer test-pat")
                .json_body_includes(format!(r#"{{"core_node_name":"{CORE_NODE}"}}"#));
            then.status(200).json_body(json!({
                "core_node_name": response.core_node_name,
                "workspace_id": response.workspace_id,
                "certificate_chain_pem": response.certificate_chain_pem,
                "serial_number": response.serial_number,
                "not_before": response.not_before,
                "not_after": response.not_after,
                "renew_after": response.renew_after,
            }));
        });

        let http = HttpClient::new();
        let mut credential = crate::resolver::resolve(
            &storage::credentials_path(&dirs),
            &http,
            Some("test-pat".into()),
        )
        .unwrap();
        let rotation = enroll_and_activate(
            &dirs,
            &http,
            &server.base_url(),
            &mut credential,
            SUBJECT,
            CORE_NODE,
        )
        .unwrap();
        enrollment.assert_calls(1);

        let (metadata, paths) =
            load_active_identity(&dirs, &server.base_url(), Some(SUBJECT), CORE_NODE).unwrap();
        assert_eq!(metadata.active_generation, metadata.spki_sha256);
        assert_eq!(paths.generation, metadata.active_generation);
        assert!(paths.certificate.is_file());
        assert!(paths.private_key.is_file());
        let credentials_text = std::fs::read_to_string(storage::credentials_path(&dirs)).unwrap();
        assert!(!credentials_text.contains("PRIVATE KEY"));
        assert!(!credentials_text.contains("CERTIFICATE REQUEST"));
        assert_eq!(
            storage::load(&storage::credentials_path(&dirs))
                .unwrap()
                .core_node_identity,
            Some(metadata.clone())
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&identity_root(&dirs)), 0o700);
            assert_eq!(mode(paths.private_key.parent().unwrap()), 0o700);
            assert_eq!(mode(&paths.private_key), 0o600);
            assert_eq!(mode(&paths.certificate), 0o600);
            assert_eq!(mode(&identity_path(&dirs)), 0o600);
        }

        arm_binding_incomplete(&dirs).unwrap();
        assert!(binding_incomplete(&dirs).unwrap());
        rotation.commit_after_probe().unwrap();
        assert!(
            !binding_incomplete(&dirs).unwrap(),
            "a verified durable commit must clear the fail-closed login transition"
        );
    }

    #[test]
    fn final_commit_rechecks_authentication_under_the_credentials_writer_lock() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let (rotation, _) = enroll_test_rotation(&server, &dirs);
        arm_binding_incomplete(&dirs).unwrap();

        storage::update(&storage::credentials_path(&dirs), |credentials| {
            credentials.session = Some(storage::ProfileCreds::with_tokens(
                uuid::Uuid::new_v4(),
                server.base_url(),
                server.base_url(),
                "client".into(),
                SUBJECT.into(),
                "user".into(),
                &crate::device::TokenSet {
                    access_token: "new-access".into(),
                    refresh_token: "new-refresh".into(),
                    expires_at: storage::now_unix() + 300,
                    token_type: "Bearer".into(),
                    scope: "openid".into(),
                },
            ));
            Ok(())
        })
        .unwrap();

        let error = rotation
            .commit_after_probe()
            .expect_err("a PAT rotation cannot commit over a newly published OAuth session");
        assert!(matches!(error, Error::StaleSessionRevision));
        assert!(binding_incomplete(&dirs).unwrap());
        assert!(load_identity_metadata(&dirs).unwrap().is_none());
        assert!(
            storage::load(&storage::credentials_path(&dirs))
                .unwrap()
                .session
                .is_some(),
            "rollback must preserve the replacement login"
        );
    }

    #[test]
    fn post_rename_credentials_failure_restores_both_activation_mirrors() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let api_origin = normalize_api_origin(&server.base_url()).unwrap();
        let response = {
            let store = identity_store(&dirs);
            let lock = store.acquire_lock().unwrap();
            let pending = store
                .prepare_generation(&lock, &api_origin, SUBJECT, CORE_NODE)
                .unwrap();
            issued_response(pending.key(), CORE_NODE)
        };
        server.mock(|when, then| {
            when.method(POST).path("/me/cli/core-node-certificates");
            then.status(200).json_body(json!({
                "core_node_name": response.core_node_name,
                "workspace_id": response.workspace_id,
                "certificate_chain_pem": response.certificate_chain_pem,
                "serial_number": response.serial_number,
                "not_before": response.not_before,
                "not_after": response.not_after,
                "renew_after": response.renew_after,
            }));
        });
        let http = HttpClient::new();
        let mut credential = crate::resolver::resolve(
            &storage::credentials_path(&dirs),
            &http,
            Some("test-pat".into()),
        )
        .unwrap();
        storage::fail_next_credentials_parent_sync_after_rename();

        let error = enroll_and_activate(
            &dirs,
            &http,
            &server.base_url(),
            &mut credential,
            SUBJECT,
            CORE_NODE,
        )
        .expect_err("the injected post-rename durability failure must be reported");

        assert!(error.to_string().contains("injected failure"), "{error}");
        assert_eq!(
            read_optional_json5::<CoreNodeIdentity>(&identity_path(&dirs)).unwrap(),
            None
        );
        assert_eq!(
            storage::load(&storage::credentials_path(&dirs))
                .unwrap()
                .core_node_identity,
            None,
            "a visible activated credentials rename must be restored with the pointer"
        );
        assert!(
            !unverified_path(&dirs).exists(),
            "a proven-consistent restore can remove the recovery receipt"
        );
    }

    #[test]
    fn direct_enrollment_refuses_to_transfer_a_different_owners_core_node_name() {
        let server = MockServer::start();
        let enrollment = server.mock(|when, then| {
            when.method(POST).path("/me/cli/core-node-certificates");
            then.status(500);
        });
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        ensure_private_layout(&dirs).unwrap();
        let mut previous = metadata_for_generation(&"a".repeat(64));
        previous.subject = "different-platform-account".into();
        publish_identity_locked(&dirs, Some(&previous)).unwrap();
        storage::save(
            &storage::credentials_path(&dirs),
            &storage::Credentials {
                core_node_identity: Some(previous),
                ..Default::default()
            },
        )
        .unwrap();
        let http = HttpClient::new();
        let mut credential = crate::resolver::resolve(
            &storage::credentials_path(&dirs),
            &http,
            Some("test-pat".into()),
        )
        .unwrap();

        let error = enroll_and_activate(
            &dirs,
            &http,
            &server.base_url(),
            &mut credential,
            SUBJECT,
            CORE_NODE,
        )
        .expect_err("a direct login enrollment must not transfer another owner's name");

        assert!(
            error.to_string().contains("different platform account"),
            "{error}"
        );
        assert_eq!(
            enrollment.calls(),
            0,
            "ownership must be rejected before staging/sending a CSR"
        );
        assert!(!pending_path(&dirs).exists());
    }

    #[test]
    fn one_rotation_receipt_has_one_live_owner_until_handoff() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let (rotation, enrollment) = enroll_test_rotation(&server, &dirs);

        let error = recover_unverified_rotation(&dirs)
            .expect_err("a second owner must not recover a live receipt");
        assert!(error.to_string().contains("already owned"), "{error}");
        assert_eq!(enrollment.calls(), 1, "ownership denial must not re-enroll");

        rotation.retain_for_restart().unwrap();
        let recovered = recover_unverified_rotation(&dirs)
            .unwrap()
            .expect("handoff releases the lease for exactly one next owner");
        recovered.rollback().unwrap();
    }

    #[test]
    fn in_process_resolver_handoff_preserves_the_binding_transition() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        arm_binding_incomplete(&dirs).unwrap();
        let (rotation, enrollment) = enroll_test_rotation(&server, &dirs);

        rotation.handoff_to_resolver().unwrap();

        assert!(
            binding_incomplete(&dirs).unwrap(),
            "the resolver handoff must remain fail-closed until apply/probe commits"
        );
        let recovered =
            recover_incomplete_binding_rotation(&dirs, &server.base_url(), true, CORE_NODE)
                .unwrap()
                .expect("the resolver must recover the exact enrolled receipt");
        assert_eq!(recovered.activated().core_node_name, CORE_NODE);
        assert_eq!(enrollment.calls(), 1, "handoff recovery must not re-enroll");
        recovered.rollback().unwrap();
    }

    #[test]
    fn namespace_restart_handoff_clears_only_its_matching_transition() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        arm_binding_incomplete(&dirs).unwrap();
        let (rotation, _) = enroll_test_rotation(&server, &dirs);

        rotation.retain_for_restart().unwrap();

        assert!(
            !binding_incomplete(&dirs).unwrap(),
            "the replacement daemon must enter ordinary receipt recovery"
        );
        let recovered = recover_unverified_rotation(&dirs)
            .unwrap()
            .expect("the retained receipt must remain recoverable after restart handoff");
        recovered.rollback().unwrap();
    }

    #[test]
    fn newer_prepare_owner_blocks_recovery_of_an_older_oauth_receipt() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let (rotation, _) = enroll_test_rotation(&server, &dirs);
        rotation.retain_for_restart().unwrap();

        let older_revision = uuid::Uuid::new_v4();
        let newer_revision = uuid::Uuid::new_v4();
        let mut receipt = read_optional_json5::<PersistedRotation>(&unverified_path(&dirs))
            .unwrap()
            .unwrap();
        receipt.activated.session_revision = Some(older_revision);
        write_json5_private_durable(&unverified_path(&dirs), &receipt).unwrap();
        publish_identity_locked(&dirs, Some(&receipt.activated)).unwrap();
        storage::update(&storage::credentials_path(&dirs), |credentials| {
            credentials.core_node_identity = Some(receipt.activated.clone());
            credentials.session = Some(storage::ProfileCreds::with_tokens(
                older_revision,
                server.base_url(),
                server.base_url(),
                "client".into(),
                SUBJECT.into(),
                "user".into(),
                &crate::device::TokenSet {
                    access_token: "access-a".into(),
                    refresh_token: "refresh-a".into(),
                    expires_at: storage::now_unix() + 300,
                    token_type: "Bearer".into(),
                    scope: "openid".into(),
                },
            ));
            Ok(())
        })
        .unwrap();
        arm_binding_incomplete_for_session(&dirs, Some(newer_revision)).unwrap();

        let error =
            recover_incomplete_binding_rotation(&dirs, &server.base_url(), false, CORE_NODE)
                .expect_err("a newer Prepare must supersede an older durable receipt");

        assert!(matches!(error, Error::StaleSessionRevision));
        let store = identity_store(&dirs);
        let lock = store.acquire_lock().unwrap();
        assert_eq!(
            store
                .binding_transition(&lock)
                .unwrap()
                .unwrap()
                .expected_session_revision,
            Some(newer_revision),
            "rejecting the stale receipt must preserve the newer Prepare owner"
        );
        assert!(
            !unverified_path(&dirs).exists(),
            "the stale receipt must be rolled back before router apply"
        );
    }

    #[test]
    fn stale_receipt_cannot_commit_over_newer_durable_ownership() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let (rotation, _enrollment) = enroll_test_rotation(&server, &dirs);
        let active_before = read_optional_json5::<CoreNodeIdentity>(&identity_path(&dirs)).unwrap();

        let mut replacement = read_optional_json5::<PersistedRotation>(&unverified_path(&dirs))
            .unwrap()
            .unwrap();
        replacement.receipt_id = "33333333-3333-4333-8333-333333333333".into();
        write_json5_private_durable(&unverified_path(&dirs), &replacement).unwrap();

        let error = rotation
            .commit_after_probe()
            .expect_err("a stale in-memory receipt must fail CAS");
        assert!(error.to_string().contains("ownership changed"), "{error}");
        assert_eq!(
            read_optional_json5::<PersistedRotation>(&unverified_path(&dirs))
                .unwrap()
                .unwrap()
                .receipt_id,
            replacement.receipt_id,
            "stale commit must not remove or overwrite the newer marker"
        );
        assert_eq!(
            read_optional_json5::<CoreNodeIdentity>(&identity_path(&dirs)).unwrap(),
            active_before,
            "stale commit must not mutate the canonical pointer"
        );
        clear_local_identity(&dirs).unwrap();
    }

    #[test]
    fn rollback_retains_rejected_generation_until_router_restore_confirmation() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let (rotation, _enrollment) = enroll_test_rotation(&server, &dirs);
        let generation = generation_dir(&dirs, &rotation.activated.active_generation);

        let rejected = rotation.rollback_for_router_restore().unwrap();
        assert!(!rejected.restored_previous());
        assert!(
            generation.exists(),
            "metadata rollback must retain files potentially referenced by Zenoh"
        );
        let contention = acquire_identity_maintenance(&dirs)
            .expect_err("router restore token must retain exclusive maintenance ownership");
        assert!(
            contention.to_string().contains("already owned"),
            "{contention}"
        );
        rejected.cleanup_after_router_restore().unwrap();
        assert!(
            !generation.exists(),
            "standalone router confirmation permits deferred deletion"
        );
        drop(
            acquire_identity_maintenance(&dirs)
                .expect("router confirmation releases maintenance ownership"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn active_identity_load_repairs_owned_broadened_modes() {
        use std::os::unix::fs::PermissionsExt;

        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let (rotation, _enrollment) = enroll_test_rotation(&server, &dirs);
        let paths = paths_for(&dirs, rotation.activated());
        let generation = paths.private_key.parent().unwrap();
        std::fs::set_permissions(generation, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&paths.private_key, std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::set_permissions(&paths.certificate, std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::set_permissions(identity_path(&dirs), std::fs::Permissions::from_mode(0o644))
            .unwrap();

        load_active_identity(&dirs, &server.base_url(), Some(SUBJECT), CORE_NODE)
            .expect("owned protected material can be safely re-restricted under the identity lock");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(generation), 0o700);
        assert_eq!(mode(&paths.private_key), 0o600);
        assert_eq!(mode(&paths.certificate), 0o600);
        assert_eq!(mode(&identity_path(&dirs)), 0o600);
        rotation.rollback().unwrap();
    }

    #[test]
    fn returned_leaf_is_rejected_for_the_wrong_core_node_identity_uri() {
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let response = issued_response(&key, "some-other-core-node");
        let pending = PendingEnrollment {
            api_origin: "https://api.peppy.bot".into(),
            subject: SUBJECT.into(),
            core_node_name: CORE_NODE.into(),
            generation: spki_fingerprint(&key),
            spki_sha256: spki_fingerprint(&key),
        };
        let error = validate_enrollment_response(
            "https://api.peppy.bot",
            SUBJECT,
            None,
            CORE_NODE,
            &pending,
            &key,
            &CoreNodeCertificateResponse {
                core_node_name: CORE_NODE.into(),
                ..response
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("identity URI"), "{error}");
    }

    #[test]
    fn returned_leaf_without_its_issuer_is_rejected() {
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let response = issued_response(&key, CORE_NODE);
        let leaf_only = pem::parse_many(&response.certificate_chain_pem)
            .unwrap()
            .remove(0)
            .to_string();
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
    fn returned_p256_ecdsa_chain_profile_is_accepted() {
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let response = issued_response(&key, CORE_NODE);
        inspect_leaf(
            &response.certificate_chain_pem,
            &key,
            &identity_uri(&Namespace::parse(WORKSPACE).unwrap(), CORE_NODE),
            CORE_NODE,
        )
        .unwrap();
    }

    #[test]
    fn returned_rsa_signed_p256_leaf_is_rejected() {
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let rsa_private = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let rsa_pkcs8 = rsa_private.to_pkcs8_pem(LineEnding::LF).unwrap();
        let rsa_ca_key =
            KeyPair::from_pkcs8_pem_and_sign_algo(rsa_pkcs8.as_str(), &rcgen::PKCS_RSA_SHA256)
                .unwrap();
        let response = issued_response_with_ca(&key, CORE_NODE, &rsa_ca_key);

        let error = inspect_leaf(
            &response.certificate_chain_pem,
            &key,
            &identity_uri(&Namespace::parse(WORKSPACE).unwrap(), CORE_NODE),
            CORE_NODE,
        )
        .unwrap_err();
        assert!(error.to_string().contains("ecdsa-with-SHA256"), "{error}");
    }

    #[test]
    fn load_rejects_an_expired_metadata_pointer_before_federation() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        ensure_private_layout(&dirs).unwrap();
        let expired = CoreNodeIdentity {
            api_origin: "https://api.peppy.bot".into(),
            subject: SUBJECT.into(),
            session_revision: None,
            workspace_id: Namespace::parse(WORKSPACE).unwrap(),
            core_node_name: CORE_NODE.into(),
            active_generation: "a".repeat(64),
            serial_number: "01".into(),
            spki_sha256: "a".repeat(64),
            not_before: 0,
            not_after: 2,
            renew_after: 1,
        };
        publish_identity_locked(&dirs, Some(&expired)).unwrap();
        let creds = storage::Credentials {
            core_node_identity: Some(expired),
            ..Default::default()
        };
        storage::save(&storage::credentials_path(&dirs), &creds).unwrap();

        let error = load_active_identity(&dirs, "https://api.peppy.bot", Some(SUBJECT), CORE_NODE)
            .unwrap_err();
        assert!(error.to_string().contains("not currently valid"), "{error}");
    }

    fn metadata_for_generation(generation: &str) -> CoreNodeIdentity {
        let now = storage::now_unix();
        CoreNodeIdentity {
            api_origin: "https://api.peppy.bot".into(),
            subject: SUBJECT.into(),
            session_revision: None,
            workspace_id: Namespace::parse(WORKSPACE).unwrap(),
            core_node_name: CORE_NODE.into(),
            active_generation: generation.into(),
            serial_number: "01".into(),
            spki_sha256: generation.into(),
            not_before: now - 60,
            not_after: now + 3600,
            renew_after: now + 1800,
        }
    }

    #[test]
    fn crash_after_pointer_publication_repairs_the_credentials_mirror() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        ensure_private_layout(&dirs).unwrap();
        let previous = metadata_for_generation(&"a".repeat(64));
        let activated = metadata_for_generation(&"b".repeat(64));
        publish_identity_locked(&dirs, Some(&activated)).unwrap();
        let creds = storage::Credentials {
            core_node_identity: Some(previous.clone()),
            ..Default::default()
        };
        storage::save(&storage::credentials_path(&dirs), &creds).unwrap();
        write_json5_private_durable(
            &unverified_path(&dirs),
            &PersistedRotation {
                receipt_version: ROTATION_RECEIPT_VERSION,
                receipt_id: "11111111-1111-4111-8111-111111111111".into(),
                previous: Some(previous),
                activated: activated.clone(),
            },
        )
        .unwrap();

        let rotation = recover_unverified_rotation(&dirs)
            .unwrap()
            .expect("published rotation is recoverable");
        assert_eq!(
            storage::load(&storage::credentials_path(&dirs))
                .unwrap()
                .core_node_identity,
            Some(activated)
        );
        rotation.retain_for_restart().unwrap();
    }

    #[test]
    fn read_only_metadata_load_never_repairs_the_credentials_mirror() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        ensure_private_layout(&dirs).unwrap();
        let active = metadata_for_generation(&"c".repeat(64));
        publish_identity_locked(&dirs, Some(&active)).unwrap();
        storage::save(
            &storage::credentials_path(&dirs),
            &storage::Credentials::default(),
        )
        .unwrap();

        assert_eq!(
            load_identity_metadata_read_only(&dirs).unwrap(),
            Some(active.clone())
        );
        assert!(
            storage::load(&storage::credentials_path(&dirs))
                .unwrap()
                .core_node_identity
                .is_none(),
            "a status read must not publish the canonical pointer into credentials"
        );

        assert_eq!(load_identity_metadata(&dirs).unwrap(), Some(active.clone()));
        assert_eq!(
            storage::load(&storage::credentials_path(&dirs))
                .unwrap()
                .core_node_identity,
            Some(active),
            "the daemon-owned repairing load retains its recovery behavior"
        );
    }

    #[test]
    fn pointer_at_previous_retains_ambiguous_generation_for_verified_prune() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        ensure_private_layout(&dirs).unwrap();
        let previous = metadata_for_generation(&"a".repeat(64));
        let activated = metadata_for_generation(&"b".repeat(64));
        publish_identity_locked(&dirs, Some(&previous)).unwrap();
        let creds = storage::Credentials {
            core_node_identity: Some(activated.clone()),
            ..Default::default()
        };
        storage::save(&storage::credentials_path(&dirs), &creds).unwrap();
        let rejected = generation_dir(&dirs, &activated.active_generation);
        std::fs::create_dir(&rejected).unwrap();
        write_json5_private_durable(
            &unverified_path(&dirs),
            &PersistedRotation {
                receipt_version: ROTATION_RECEIPT_VERSION,
                receipt_id: "22222222-2222-4222-8222-222222222222".into(),
                previous: Some(previous.clone()),
                activated,
            },
        )
        .unwrap();

        assert!(recover_unverified_rotation(&dirs).unwrap().is_none());
        assert_eq!(
            storage::load(&storage::credentials_path(&dirs))
                .unwrap()
                .core_node_identity,
            Some(previous)
        );
        assert!(
            rejected.exists(),
            "recovery cannot distinguish pre-activation from mid-rollback and must retain files"
        );
        assert!(!unverified_path(&dirs).exists());
    }
}
