//! Production per-core-node client identity enrollment and protected storage.
//!
//! The daemon identity is exactly its already-resolved `core_node_name`; this
//! module never creates a second device identifier. Each enrollment stages a
//! locally-generated ECDSA P-256 PKCS#8 key in an immutable generation, sends
//! only a proof-of-possession CSR, validates the returned client certificate,
//! and atomically publishes non-secret metadata after the generation is fully
//! durable. A stable advisory lock serializes CLI enrollment, daemon renewal,
//! rollback, and logout cleanup across processes.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use config::namespace::Namespace;
use daemon_config::consts::PeppyDirs;
use rcgen::{CertificateParams, DistinguishedName, KeyPair, PublicKeyData};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use x509_parser::extensions::GeneralName;
use x509_parser::oid_registry::{
    OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY, OID_SIG_ECDSA_WITH_SHA256,
};
use x509_parser::prelude::FromDer;

use crate::client::CoreNodeCertificateResponse;
use crate::error::{Error, Result};
use crate::{client, http::HttpClient, resolver, resolver::Credential, storage};

const IDENTITY_DIR: &str = "platform-core-node";
const IDENTITY_FILE: &str = "identity.json5";
const PENDING_FILE: &str = "pending.json5";
const UNVERIFIED_FILE: &str = "unverified-rotation.json5";
const GENERATIONS_DIR: &str = "generations";
const KEY_FILE: &str = "client-key.pem";
const CHAIN_FILE: &str = "client-chain.pem";
const LOCK_FILE: &str = ".platform-core-node.lock";
const ROTATION_LEASE_FILE: &str = ".platform-core-node-rotation.lock";
const AUTH_OPERATION_LOCK_FILE: &str = ".platform-auth-operation.lock";
const BINDING_INCOMPLETE_FILE: &str = ".platform-binding-incomplete";
const BINDING_INCOMPLETE_CONTENTS: &[u8] = b"peppy-platform-binding-incomplete-v1\n";
const ROTATION_RECEIPT_VERSION: u32 = 1;
const MAX_LEAF_VALIDITY_SECS: i64 = 48 * 60 * 60;
const NOT_BEFORE_CLOCK_SKEW_SECS: i64 = 5 * 60;
const MAX_RENEWAL_JITTER_SECS: i64 = 5 * 60;

/// Non-secret metadata mirrored in `credentials.json5` v3 and the protected
/// identity pointer. The PEM bodies and private key are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreNodeIdentity {
    pub api_origin: String,
    pub subject: String,
    pub workspace_id: Namespace,
    pub core_node_name: String,
    pub active_generation: String,
    pub serial_number: String,
    pub spki_sha256: String,
    pub not_before: i64,
    pub not_after: i64,
    pub renew_after: i64,
}

impl CoreNodeIdentity {
    pub fn is_valid_at(&self, now: i64) -> bool {
        now >= self.not_before && now < self.not_after
    }

    pub fn renewal_due(&self, now: i64) -> bool {
        now >= self.renewal_at()
    }

    /// Stable per-generation early-renewal threshold shared by eligibility and
    /// daemon scheduling. Using one value avoids a one-second busy loop in the
    /// jitter window and actually distributes fleet rotations.
    pub fn renewal_at(&self) -> i64 {
        self.renew_after
            .saturating_sub(stable_renewal_jitter(&self.active_generation))
    }
}

/// Generation-specific files consumed by Zenoh. Paths change on every key
/// rotation so desired-state equality necessarily triggers a router reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPaths {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub generation: String,
    pub workspace_id: Option<Namespace>,
}

/// An activated rotation retained until the caller verifies the real mTLS
/// link. A failed apply/probe can restore the prior still-valid generation.
#[derive(Debug)]
pub struct IdentityRotation {
    dirs: PeppyDirs,
    previous: Option<CoreNodeIdentity>,
    activated: CoreNodeIdentity,
    receipt_id: String,
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

/// Cross-process serialization for complete interactive platform login/logout
/// commands. Commands hold this through their final daemon poke; identity
/// maintenance uses the nested rotation lease, giving the global lock order:
/// auth operation -> rotation lease -> identity lock -> credentials lock.
#[derive(Debug)]
pub struct PlatformAuthOperationGuard {
    _file: File,
}

impl IdentityMaintenanceGuard {
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
        clear_local_identity_with_lease(&self.dirs, false)
    }

    /// Durably clears every renewable/authenticated local field in one first
    /// transaction, then removes key material. A crash may leave orphaned key
    /// files, but can never leave a refresh session capable of re-enrollment.
    pub fn clear_local_logout(&self) -> Result<()> {
        clear_local_identity_with_lease(&self.dirs, true)
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
        let _lock = IdentityLock::acquire(&self.dirs)?;
        if read_optional_json5::<CoreNodeIdentity>(&identity_path(&self.dirs))?
            .is_some_and(|identity| identity.active_generation == generation)
        {
            return Err(Error::Auth(format!(
                "refusing to delete rejected generation {generation} because it became active again"
            )));
        }
        remove_dir_if_exists(&generation_dir(&self.dirs, &generation))
    }
}

impl IdentityRotation {
    pub fn activated(&self) -> &CoreNodeIdentity {
        &self.activated
    }

    /// Keeps the activated generation and removes superseded generations only
    /// after the managed router has passed its mTLS probe.
    pub fn commit_after_probe(mut self) -> Result<()> {
        // A verified generation must remain active even if best-effort cleanup
        // fails, so disarm rollback before touching cleanup state.
        self.armed = false;
        let _lock = IdentityLock::acquire(&self.dirs)?;
        self.verify_receipt_locked()?;
        remove_file_if_exists(&unverified_path(&self.dirs))?;
        prune_generations_locked(&self.dirs, &self.activated.active_generation)
    }

    /// Restores the previous metadata pointer while retaining the rejected
    /// generation. Callers that do not own router restoration can safely leave
    /// the unreferenced files for the next verified commit/prune.
    pub fn rollback(mut self) -> Result<()> {
        self.armed = false;
        self.rollback_inner().map(drop)
    }

    /// Restores prior metadata and returns a cleanup token for the rejected
    /// generation. The daemon consumes it only after prior-link reapply/probe
    /// or an intentional standalone apply has succeeded.
    pub fn rollback_for_router_restore(mut self) -> Result<RejectedIdentityGeneration> {
        self.armed = false;
        self.rollback_inner()
    }

    /// Keeps the activated pointer across a namespace-generation restart while
    /// retaining the durable unverified marker. The next daemon generation
    /// recovers a fresh receipt, forces a real probe, and only then prunes.
    pub fn retain_for_restart(mut self) -> Result<()> {
        let _lock = IdentityLock::acquire(&self.dirs)?;
        self.verify_receipt_locked()?;
        self.armed = false;
        Ok(())
    }

    fn verify_receipt_locked(&self) -> Result<()> {
        let persisted = read_optional_json5::<PersistedRotation>(&unverified_path(&self.dirs))?
            .ok_or_else(|| {
                Error::Auth(
                    "core-node rotation receipt disappeared before its terminal operation".into(),
                )
            })?;
        validate_persisted_receipt(&persisted)?;
        if persisted.receipt_id != self.receipt_id
            || persisted.previous != self.previous
            || persisted.activated != self.activated
        {
            return Err(Error::Auth(
                "core-node rotation receipt ownership changed; refusing a stale commit/rollback"
                    .into(),
            ));
        }
        if read_optional_json5::<CoreNodeIdentity>(&identity_path(&self.dirs))?.as_ref()
            != Some(&self.activated)
        {
            return Err(Error::Auth(
                "core-node rotation pointer changed; refusing a stale commit/rollback".into(),
            ));
        }
        Ok(())
    }

    fn rollback_inner(&mut self) -> Result<RejectedIdentityGeneration> {
        let _lock = IdentityLock::acquire(&self.dirs)?;
        self.verify_receipt_locked()?;
        // Never restore an expired or corrupted prior generation. In that case
        // rollback deliberately clears the active pointer so the release daemon
        // can only de-federate; it must not limp along with unusable mTLS files.
        let previous = self.previous.clone().filter(|previous| {
            previous.is_valid_at(storage::now_unix())
                && validate_stored_material(previous, &paths_for(&self.dirs, previous)).is_ok()
        });
        publish_identity_locked(&self.dirs, previous.as_ref())?;
        storage::update(&storage::credentials_path(&self.dirs), |creds| {
            creds.core_node_identity = previous.clone();
            creds.router = None;
            Ok(())
        })?;
        remove_file_if_exists(&unverified_path(&self.dirs))?;
        let generation = previous
            .as_ref()
            .is_none_or(|previous| previous.active_generation != self.activated.active_generation)
            .then(|| self.activated.active_generation.clone());
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingEnrollment {
    api_origin: String,
    subject: String,
    core_node_name: String,
    generation: String,
    spki_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedRotation {
    receipt_version: u32,
    receipt_id: String,
    previous: Option<CoreNodeIdentity>,
    activated: CoreNodeIdentity,
}

fn validate_persisted_receipt(receipt: &PersistedRotation) -> Result<()> {
    if receipt.receipt_version != ROTATION_RECEIPT_VERSION {
        return Err(Error::Auth(format!(
            "unsupported core-node rotation receipt version {}",
            receipt.receipt_version
        )));
    }
    let id = uuid::Uuid::parse_str(&receipt.receipt_id)
        .map_err(|error| Error::Auth(format!("invalid core-node rotation receipt id: {error}")))?;
    if id.hyphenated().to_string() != receipt.receipt_id {
        return Err(Error::Auth(
            "core-node rotation receipt id is not a canonical UUID".into(),
        ));
    }
    validate_identity_metadata_shape(&receipt.activated)?;
    if let Some(previous) = receipt.previous.as_ref() {
        validate_identity_metadata_shape(previous)?;
    }
    Ok(())
}

#[derive(Debug)]
struct IdentityLock {
    _file: File,
}

impl IdentityLock {
    fn acquire(dirs: &PeppyDirs) -> Result<Self> {
        let conf = ensure_conf_private_durable(dirs)?;
        let path = conf.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        restrict_file(&path)?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

/// Separate from [`IdentityLock`]: this advisory lock is held across the whole
/// async router apply/probe window, while short identity reads still take the
/// ordinary lock without deadlocking. OS lock release makes a crashed owner's
/// durable receipt recoverable by the next process.
#[derive(Debug)]
struct RotationLease {
    _file: File,
}

impl RotationLease {
    fn try_acquire(dirs: &PeppyDirs) -> Result<Self> {
        let conf = ensure_conf_private_durable(dirs)?;
        let path = conf.join(ROTATION_LEASE_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        restrict_file(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(Error::Auth(
                "a core-node certificate rotation is already owned by another login/daemon operation; wait for its apply/probe to finish and retry"
                    .into(),
            )),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

/// Acquires the stable CLI platform-auth operation lock. It is fail-fast so a
/// second login/logout changes nothing and can be retried after the active
/// command (including its daemon poke) completes.
pub fn acquire_platform_auth_operation(dirs: &PeppyDirs) -> Result<PlatformAuthOperationGuard> {
    let conf = ensure_conf_private_durable(dirs)?;
    let path = conf.join(AUTH_OPERATION_LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    restrict_file(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(PlatformAuthOperationGuard { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::Auth(
            "another peppy platform login/logout operation is already in progress; wait for it to finish and retry"
                .into(),
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
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
    ensure_conf_private_durable(dirs)?;
    write_private_durable(
        &dirs.conf_dir().join(BINDING_INCOMPLETE_FILE),
        BINDING_INCOMPLETE_CONTENTS,
    )
}

/// Removes the binding transition only after login has handed off a usable
/// identity, or as part of durable logout cleanup. Parent-directory fsync makes
/// the removal crash durable.
pub fn clear_binding_incomplete(dirs: &PeppyDirs) -> Result<()> {
    remove_file_if_exists(&dirs.conf_dir().join(BINDING_INCOMPLETE_FILE))
}

/// Whether a login binding transition is still incomplete. Protected-file
/// ownership, type, symlink, and mode checks run before reporting presence.
pub fn binding_incomplete(dirs: &PeppyDirs) -> Result<bool> {
    let conf = dirs.conf_dir();
    if conf.exists() {
        restrict_dir(&conf)?;
    }
    let marker = conf.join(BINDING_INCOMPLETE_FILE);
    match restrict_file(&marker) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub fn identity_root(dirs: &PeppyDirs) -> PathBuf {
    dirs.conf_dir().join(IDENTITY_DIR)
}

fn identity_path(dirs: &PeppyDirs) -> PathBuf {
    identity_root(dirs).join(IDENTITY_FILE)
}

fn pending_path(dirs: &PeppyDirs) -> PathBuf {
    identity_root(dirs).join(PENDING_FILE)
}

fn unverified_path(dirs: &PeppyDirs) -> PathBuf {
    identity_root(dirs).join(UNVERIFIED_FILE)
}

fn generation_dir(dirs: &PeppyDirs, generation: &str) -> PathBuf {
    identity_root(dirs).join(GENERATIONS_DIR).join(generation)
}

/// Normalizes and validates a platform API origin for identity binding.
pub fn normalize_api_origin(api_url: &str) -> Result<String> {
    crate::profile::normalize_api_origin(api_url)
}

/// Makes a PAT login the sole active authentication mode without ever writing
/// the PAT itself. This also heals a legacy/corrupt credentials document before
/// enrollment, preserving a separately valid v3 identity pointer when possible
/// and otherwise clearing unusable identity state for a clean enrollment.
pub fn prepare_pat_login(dirs: &PeppyDirs) -> Result<()> {
    let _lease = RotationLease::try_acquire(dirs)?;
    prepare_pat_login_with_lease(dirs)
}

fn prepare_pat_login_with_lease(dirs: &PeppyDirs) -> Result<()> {
    let _lock = IdentityLock::acquire(dirs)?;
    let pointer = match read_optional_json5::<CoreNodeIdentity>(&identity_path(dirs)) {
        Ok(pointer) => pointer,
        Err(_) => {
            remove_dir_if_exists(&identity_root(dirs))?;
            None
        }
    };
    storage::update_or_default(&storage::credentials_path(dirs), |creds| {
        creds.session = None;
        creds.router = None;
        creds.core_node_identity = pointer;
        Ok(())
    })
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
    maintain_identity_inner(dirs, http, api_url, pat, core_node_name, false)
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
    maintain_identity_inner(dirs, http, api_url, pat, core_node_name, true)
}

fn maintain_identity_inner(
    dirs: &PeppyDirs,
    http: &HttpClient,
    api_url: &str,
    pat: Option<String>,
    core_node_name: &str,
    force_rotation: bool,
) -> Result<Option<IdentityRotation>> {
    validate_core_node_name(core_node_name)?;
    let api_origin = normalize_api_origin(api_url)?;
    if let Some(rotation) = recover_unverified_rotation(dirs)? {
        if rotation.activated.api_origin == api_origin
            && rotation.activated.core_node_name == core_node_name
        {
            return Ok(Some(rotation));
        }
        // This process cannot apply an abandoned rotation for another binding.
        // Its armed drop restores the prior valid generation before continuing.
        rotation.rollback()?;
    }
    let metadata = load_identity_metadata(dirs)?;
    let creds_path = storage::credentials_path(dirs);
    let stored_subject = storage::load(&creds_path)?
        .session
        .map(|session| session.subject)
        .filter(|subject| !subject.is_empty());

    let exact_binding = metadata.as_ref().is_some_and(|identity| {
        identity.api_origin == api_origin && identity.core_node_name == core_node_name
    });
    if !force_rotation
        && exact_binding
        && let Some(identity) = metadata.as_ref()
        && !identity.renewal_due(storage::now_unix())
        && load_active_identity(dirs, api_url, stored_subject.as_deref(), core_node_name).is_ok()
    {
        return Ok(None);
    }

    let mut credential = resolver::resolve(&creds_path, http, pat)?;
    let principal = client::get_me(http, api_url, &mut credential)?;
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
    enroll_and_activate_with_lease(
        dirs,
        http,
        api_url,
        credential,
        subject,
        core_node_name,
        RotationLease::try_acquire(dirs)?,
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
    let lease = match recover_unverified_rotation_with_lease(dirs, initial_lease)? {
        RecoveredRotation::Active(rotation)
            if rotation.activated.api_origin == api_origin
                && rotation.activated.subject == subject
                && rotation.activated.core_node_name == core_node_name =>
        {
            return Ok(*rotation);
        }
        RecoveredRotation::Active(rotation) => {
            // Resolve the old receipt under its unique lease before creating a
            // new binding. Its rejected files stay until a later verified prune.
            rotation.rollback()?;
            RotationLease::try_acquire(dirs)?
        }
        RecoveredRotation::Clean(lease) => lease,
    };
    // Logout may have won while a daemon resolved `/me` and waited for the
    // rotation lease. Revalidate the exact OAuth session before recreating the
    // identity root or staging any pending private key. PAT credentials are a
    // no-op here and remain environment-only.
    crate::resolver::ensure_session_credential_current(credential)?;
    let _lock = IdentityLock::acquire(dirs)?;
    ensure_private_layout(dirs)?;

    let previous = load_metadata_locked(dirs, true)?;
    // Login uses this direct enrollment path after resolving its principal.
    // Enforce the same local ownership invariant as daemon maintenance before
    // staging a key or sending a CSR: a backend conflict response is defense in
    // depth, not authorization to transfer a globally reserved core-node name.
    ensure_same_identity_owner(previous.as_ref(), subject)?;
    let (pending, key) = prepare_pending_locked(dirs, &api_origin, subject, core_node_name)?;
    let csr_pem = build_csr(&key)?;
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
            remove_file_if_exists(&pending_path(dirs))?;
            if previous
                .as_ref()
                .is_none_or(|identity| identity.active_generation != pending.generation)
            {
                remove_dir_if_exists(&generation_dir(dirs, &pending.generation))?;
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let activated = validate_enrollment_response(
        &api_origin,
        subject,
        core_node_name,
        &pending,
        &key,
        &response,
    )?;

    let generation = generation_dir(dirs, &pending.generation);
    write_private_durable(
        &generation.join(CHAIN_FILE),
        response.certificate_chain_pem.as_bytes(),
    )?;
    File::open(&generation)?.sync_all()?;

    // Persist rollback intent before publishing the new pointer. If the
    // process is cancelled, crashes, or restarts for a namespace change, a
    // later daemon generation can either probe+commit or restore `previous`.
    let receipt_id = uuid::Uuid::new_v4().hyphenated().to_string();
    write_json5_private_durable(
        &unverified_path(dirs),
        &PersistedRotation {
            receipt_version: ROTATION_RECEIPT_VERSION,
            receipt_id: receipt_id.clone(),
            previous: previous.clone(),
            activated: activated.clone(),
        },
    )?;

    // Publish the canonical pointer first, then mirror it into credentials v3
    // while the cross-process lock excludes every reader/writer in this module.
    publish_identity_locked(dirs, Some(&activated))?;
    let creds_path = storage::credentials_path(dirs);
    if let Err(error) = storage::update(&creds_path, |creds| {
        creds.core_node_identity = Some(activated.clone());
        creds.router = None;
        Ok(())
    }) {
        // `storage::update` can report an error after its atomic rename became
        // visible (for example, parent-directory fsync failure). Restore both
        // mirrors in fresh durable writes and remove the receipt only after
        // their operations and exact read-back all agree. Otherwise retain the
        // receipt: crash recovery can reconcile it, while deleting it would
        // strand pointer=previous / credentials=activated permanently.
        let pointer_restore = publish_identity_locked(dirs, previous.as_ref());
        let mirror_restore = storage::update(&creds_path, |creds| {
            creds.core_node_identity = previous.clone();
            creds.router = None;
            Ok(())
        });
        let read_back_consistent = pointer_restore.is_ok()
            && mirror_restore.is_ok()
            && read_optional_json5::<CoreNodeIdentity>(&identity_path(dirs))? == previous
            && storage::load(&creds_path)?.core_node_identity == previous;
        if read_back_consistent {
            remove_file_if_exists(&unverified_path(dirs))?;
            return Err(error);
        }
        return Err(Error::Auth(format!(
            "{error}; core-node activation mirrors could not be durably restored, so the recovery receipt was retained"
        )));
    }
    remove_file_if_exists(&pending_path(dirs))?;

    Ok(IdentityRotation {
        dirs: dirs.clone(),
        previous,
        activated,
        receipt_id,
        lease: Some(lease),
        armed: true,
    })
}

fn recover_unverified_rotation(dirs: &PeppyDirs) -> Result<Option<IdentityRotation>> {
    match recover_unverified_rotation_with_lease(dirs, RotationLease::try_acquire(dirs)?)? {
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
    let _lock = IdentityLock::acquire(dirs)?;
    restrict_file_if_exists(&unverified_path(dirs))?;
    restrict_file_if_exists(&pending_path(dirs))?;
    let Some(persisted) = read_optional_json5::<PersistedRotation>(&unverified_path(dirs))? else {
        return Ok(RecoveredRotation::Clean(lease));
    };
    validate_persisted_receipt(&persisted)?;
    let pointer = read_optional_json5::<CoreNodeIdentity>(&identity_path(dirs))?;
    let creds_path = storage::credentials_path(dirs);
    if pointer.as_ref() == Some(&persisted.activated) {
        // Canonical pointer publication precedes the credentials mirror. A
        // crash between those writes is recovered from the durable receipt,
        // not rejected as a two-Some inconsistency.
        storage::update(&creds_path, |creds| {
            creds.core_node_identity = Some(persisted.activated.clone());
            creds.router = None;
            Ok(())
        })?;
        remove_file_if_exists(&pending_path(dirs))?;
    } else if pointer == persisted.previous {
        // This can be either a crash before activation or a crash midway
        // through rollback after Zenoh saw the activated paths. Reconcile the
        // old pointer/receipt, but retain the ambiguous generation; a later
        // verified commit/prune can safely remove it after router state is known.
        storage::update(&creds_path, |creds| {
            creds.core_node_identity = persisted.previous.clone();
            creds.router = None;
            Ok(())
        })?;
        remove_file_if_exists(&unverified_path(dirs))?;
        remove_file_if_exists(&pending_path(dirs))?;
        return Ok(RecoveredRotation::Clean(lease));
    } else {
        return Err(Error::Auth(
            "unverified core-node rotation metadata does not match the active identity pointer"
                .into(),
        ));
    }
    Ok(RecoveredRotation::Active(Box::new(IdentityRotation {
        dirs: dirs.clone(),
        previous: persisted.previous,
        activated: persisted.activated,
        receipt_id: persisted.receipt_id,
        lease: Some(lease),
        armed: true,
    })))
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
    let _lock = IdentityLock::acquire(dirs)?;
    let metadata = load_metadata_locked(dirs, true)?.ok_or_else(|| {
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
    validate_generation_name(&metadata.active_generation)?;
    let paths = paths_for(dirs, &metadata);
    validate_stored_material(&metadata, &paths)?;
    Ok((metadata, paths))
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
    Ok(IdentityMaintenanceGuard {
        dirs: dirs.clone(),
        _lease: RotationLease::try_acquire(dirs)?,
    })
}

fn clear_local_identity_with_lease(dirs: &PeppyDirs, clear_session: bool) -> Result<()> {
    let _lock = IdentityLock::acquire(dirs)?;
    let creds_path = storage::credentials_path(dirs);
    storage::update_or_default(&creds_path, |creds| {
        creds.core_node_identity = None;
        creds.router = None;
        if clear_session {
            creds.session = None;
        }
        Ok(())
    })?;
    if clear_session {
        clear_binding_incomplete(dirs)?;
    }
    remove_dir_if_exists(&identity_root(dirs))
}

/// Reads the non-secret identity metadata for logout/status without exposing
/// the private layout. An interrupted credentials mirror is repaired while the
/// cross-process identity lock is held.
pub fn load_identity_metadata(dirs: &PeppyDirs) -> Result<Option<CoreNodeIdentity>> {
    let _lock = IdentityLock::acquire(dirs)?;
    load_metadata_locked(dirs, true)
}

/// Removes superseded generations after an mTLS probe has verified the active
/// identity. A pending retry generation is retained.
pub fn prune_generations(dirs: &PeppyDirs) -> Result<()> {
    let _lease = RotationLease::try_acquire(dirs)?;
    let _lock = IdentityLock::acquire(dirs)?;
    let Some(active) = load_metadata_locked(dirs, true)? else {
        return Ok(());
    };
    prune_generations_locked(dirs, &active.active_generation)
}

fn prepare_pending_locked(
    dirs: &PeppyDirs,
    api_origin: &str,
    subject: &str,
    core_node_name: &str,
) -> Result<(PendingEnrollment, KeyPair)> {
    restrict_file_if_exists(&pending_path(dirs))?;
    if let Some(pending) = read_optional_json5::<PendingEnrollment>(&pending_path(dirs))?
        && pending.api_origin == api_origin
        && pending.subject == subject
        && pending.core_node_name == core_node_name
        && validate_generation_name(&pending.generation).is_ok()
    {
        let generation = generation_dir(dirs, &pending.generation);
        restrict_dir(&generation)?;
        restrict_file(&generation.join(KEY_FILE))?;
        let key_pem = std::fs::read_to_string(generation.join(KEY_FILE))?;
        let key = KeyPair::from_pem(&key_pem)
            .map_err(|e| Error::Auth(format!("invalid pending core-node private key: {e}")))?;
        let fingerprint = spki_fingerprint(&key);
        if fingerprint == pending.spki_sha256 && fingerprint == pending.generation {
            return Ok((pending, key));
        }
    }

    // A pending entry for another binding must never be reused. It is safe to
    // remove unless it names the currently active immutable generation.
    if let Some(stale) = read_optional_json5::<PendingEnrollment>(&pending_path(dirs))? {
        // Never feed untrusted metadata into a path join/remove. A malformed
        // generation wedges safely for operator cleanup instead of traversing
        // outside the immutable generations directory.
        validate_generation_name(&stale.generation)?;
        let active = read_optional_json5::<CoreNodeIdentity>(&identity_path(dirs))?;
        if active
            .as_ref()
            .is_none_or(|identity| identity.active_generation != stale.generation)
        {
            remove_dir_if_exists(&generation_dir(dirs, &stale.generation))?;
        }
        remove_file_if_exists(&pending_path(dirs))?;
    }

    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| Error::Auth(format!("failed to generate core-node private key: {e}")))?;
    let spki_sha256 = spki_fingerprint(&key);
    let pending = PendingEnrollment {
        api_origin: api_origin.to_string(),
        subject: subject.to_string(),
        core_node_name: core_node_name.to_string(),
        generation: spki_sha256.clone(),
        spki_sha256,
    };
    let generation = generation_dir(dirs, &pending.generation);
    std::fs::create_dir(&generation)?;
    restrict_dir(&generation)?;
    // Persist the new generation directory entry before any pointer/receipt
    // can name it. Syncing only the child does not make its name durable in the
    // parent `generations/` directory.
    File::open(
        generation
            .parent()
            .expect("generation path always has a parent"),
    )?
    .sync_all()?;
    write_private_durable(&generation.join(KEY_FILE), key.serialize_pem().as_bytes())?;
    File::open(&generation)?.sync_all()?;
    write_json5_private_durable(&pending_path(dirs), &pending)?;
    Ok((pending, key))
}

fn build_csr(key: &KeyPair) -> Result<String> {
    // Identity/profile extensions are server-controlled. The CSR carries only
    // the P-256 SPKI and proof-of-possession signature.
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    let csr = params
        .serialize_request(key)
        .map_err(|e| Error::Auth(format!("failed to build core-node CSR: {e}")))?;
    csr.pem()
        .map_err(|e| Error::Auth(format!("failed to encode core-node CSR: {e}")))
}

fn validate_enrollment_response(
    api_origin: &str,
    subject: &str,
    core_node_name: &str,
    pending: &PendingEnrollment,
    key: &KeyPair,
    response: &CoreNodeCertificateResponse,
) -> Result<CoreNodeIdentity> {
    if response.core_node_name != core_node_name {
        return Err(Error::Auth(format!(
            "certificate enrollment returned core-node name {:?}, expected {:?}",
            response.core_node_name, core_node_name
        )));
    }
    let workspace_id = client::parse_workspace_id(&response.workspace_id).map_err(|error| {
        Error::Auth(format!(
            "certificate enrollment returned an invalid workspace_id: {error}"
        ))
    })?;
    let not_before = parse_rfc3339("not_before", &response.not_before)?;
    let not_after = parse_rfc3339("not_after", &response.not_after)?;
    let renew_after = parse_rfc3339("renew_after", &response.renew_after)?;
    let now = storage::now_unix();
    if not_before > now.saturating_add(NOT_BEFORE_CLOCK_SKEW_SECS)
        || not_after <= now
        || not_after <= not_before
        || not_after.saturating_sub(not_before) > MAX_LEAF_VALIDITY_SECS
        || renew_after <= not_before
        || renew_after >= not_after
    {
        return Err(Error::Auth(
            "certificate enrollment returned unacceptable validity/renewal timestamps".into(),
        ));
    }

    let expected_uri = identity_uri(&workspace_id, core_node_name);
    let inspected = inspect_leaf(
        &response.certificate_chain_pem,
        key,
        &expected_uri,
        core_node_name,
    )?;
    if inspected.spki_sha256 != pending.spki_sha256
        || inspected.not_before != not_before
        || inspected.not_after != not_after
    {
        return Err(Error::Auth(
            "certificate enrollment response metadata does not match the returned leaf".into(),
        ));
    }
    if normalize_serial(&response.serial_number)? != normalize_serial(&inspected.serial_number)? {
        return Err(Error::Auth(
            "certificate enrollment serial_number does not match the returned leaf".into(),
        ));
    }

    Ok(CoreNodeIdentity {
        api_origin: api_origin.to_string(),
        subject: subject.to_string(),
        workspace_id,
        core_node_name: core_node_name.to_string(),
        active_generation: pending.generation.clone(),
        serial_number: response.serial_number.clone(),
        spki_sha256: pending.spki_sha256.clone(),
        not_before,
        not_after,
        renew_after,
    })
}

#[derive(Debug)]
struct InspectedLeaf {
    spki_sha256: String,
    serial_number: String,
    not_before: i64,
    not_after: i64,
}

fn inspect_leaf(
    chain_pem: &str,
    key: &KeyPair,
    expected_uri: &str,
    expected_common_name: &str,
) -> Result<InspectedLeaf> {
    let blocks = pem::parse_many(chain_pem)
        .map_err(|e| Error::Auth(format!("invalid certificate chain PEM: {e}")))?;
    if blocks.len() < 2 {
        return Err(Error::Auth(
            "certificate chain must contain a leaf and at least one issuing CA certificate".into(),
        ));
    }
    if blocks.iter().any(|block| block.tag() != "CERTIFICATE") {
        return Err(Error::Auth(
            "certificate chain contains a non-certificate PEM block".into(),
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
                .map_err(|e| {
                    Error::Auth(format!(
                        "invalid certificate at position {} in returned chain: {e}",
                        index + 1
                    ))
                })
        })
        .collect::<Result<Vec<_>>>()?;
    for (index, pair) in certificates.windows(2).enumerate() {
        let certificate = &pair[0];
        let issuer = &pair[1];
        if certificate.issuer() != issuer.subject() {
            return Err(Error::Auth(format!(
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
            return Err(Error::Auth(format!(
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
            return Err(Error::Auth(format!(
                "certificate at chain position {} must use an EC prime256v1 signing key",
                index + 2
            )));
        }
        let issuer_basic = issuer
            .basic_constraints()
            .map_err(|e| Error::Auth(format!("invalid issuer Basic Constraints: {e}")))?
            .ok_or_else(|| {
                Error::Auth(format!(
                    "certificate at chain position {} is missing CA Basic Constraints",
                    index + 2
                ))
            })?;
        if !issuer_basic.critical || !issuer_basic.value.ca {
            return Err(Error::Auth(format!(
                "certificate at chain position {} must have critical CA Basic Constraints",
                index + 2
            )));
        }
        let issuer_usage = issuer
            .key_usage()
            .map_err(|e| Error::Auth(format!("invalid issuer Key Usage: {e}")))?
            .ok_or_else(|| {
                Error::Auth(format!(
                    "certificate at chain position {} is missing Key Usage",
                    index + 2
                ))
            })?;
        if !issuer_usage.critical || !issuer_usage.value.key_cert_sign() {
            return Err(Error::Auth(format!(
                "certificate at chain position {} must have critical keyCertSign usage",
                index + 2
            )));
        }
        if issuer.validity().not_before.timestamp() > certificate.validity().not_before.timestamp()
            || issuer.validity().not_after.timestamp()
                < certificate.validity().not_after.timestamp()
        {
            return Err(Error::Auth(format!(
                "certificate at chain position {} does not contain its child's validity interval",
                index + 2
            )));
        }
        certificate
            .verify_signature(Some(issuer.public_key()))
            .map_err(|e| {
                Error::Auth(format!(
                    "certificate chain signature verification failed between positions {} and {}: {e}",
                    index + 1,
                    index + 2
                ))
            })?;
    }
    let leaf = &certificates[0];

    if !is_valid_positive_der_serial(leaf.raw_serial()) {
        return Err(Error::Auth(
            "returned leaf serial number is not a canonical positive RFC 5280 serial".into(),
        ));
    }

    if leaf.public_key().raw != key.subject_public_key_info() {
        return Err(Error::Auth(
            "returned leaf certificate does not match the locally generated private key".into(),
        ));
    }
    let basic = leaf
        .basic_constraints()
        .map_err(|e| Error::Auth(format!("invalid Basic Constraints: {e}")))?
        .ok_or_else(|| Error::Auth("returned leaf is missing Basic Constraints".into()))?;
    if !basic.critical || basic.value.ca {
        return Err(Error::Auth(
            "returned leaf Basic Constraints must be critical with CA=false".into(),
        ));
    }
    let usage = leaf
        .key_usage()
        .map_err(|e| Error::Auth(format!("invalid Key Usage: {e}")))?
        .ok_or_else(|| Error::Auth("returned leaf is missing Key Usage".into()))?;
    if !usage.critical || usage.value.flags != 1 || !usage.value.digital_signature() {
        return Err(Error::Auth(
            "returned leaf Key Usage must be critical and restricted to digitalSignature".into(),
        ));
    }
    let eku = leaf
        .extended_key_usage()
        .map_err(|e| Error::Auth(format!("invalid Extended Key Usage: {e}")))?
        .ok_or_else(|| Error::Auth("returned leaf is missing Extended Key Usage".into()))?;
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
        return Err(Error::Auth(
            "returned leaf Extended Key Usage must be restricted to clientAuth".into(),
        ));
    }
    let san = leaf
        .subject_alternative_name()
        .map_err(|e| Error::Auth(format!("invalid Subject Alternative Name: {e}")))?
        .ok_or_else(|| Error::Auth("returned leaf is missing Subject Alternative Name".into()))?;
    if !matches!(
        san.value.general_names.as_slice(),
        [GeneralName::URI(uri)] if *uri == expected_uri
    ) {
        return Err(Error::Auth(format!(
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
        return Err(Error::Auth(format!(
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
fn is_valid_positive_der_serial(raw_serial: &[u8]) -> bool {
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

fn validate_stored_material(metadata: &CoreNodeIdentity, paths: &IdentityPaths) -> Result<()> {
    validate_identity_metadata_shape(metadata)?;
    let now = storage::now_unix();
    if !metadata.is_valid_at(now) {
        return Err(Error::Auth(format!(
            "core-node certificate for `{}` is not currently valid (expires at unix {}); run `peppy platform login`",
            metadata.core_node_name, metadata.not_after
        )));
    }
    let generation = paths.private_key.parent().ok_or_else(|| {
        Error::Auth("stored core-node private-key path has no generation directory".into())
    })?;
    restrict_dir(generation)?;
    restrict_file(&paths.private_key)?;
    restrict_file(&paths.certificate)?;
    let key_pem = std::fs::read_to_string(&paths.private_key).map_err(|e| {
        Error::Auth(format!(
            "cannot read core-node private key {}: {e}",
            paths.private_key.display()
        ))
    })?;
    let key = KeyPair::from_pem(&key_pem)
        .map_err(|e| Error::Auth(format!("invalid stored core-node private key: {e}")))?;
    let chain = std::fs::read_to_string(&paths.certificate).map_err(|e| {
        Error::Auth(format!(
            "cannot read core-node certificate chain {}: {e}",
            paths.certificate.display()
        ))
    })?;
    let inspected = inspect_leaf(
        &chain,
        &key,
        &identity_uri(&metadata.workspace_id, &metadata.core_node_name),
        &metadata.core_node_name,
    )?;
    if inspected.spki_sha256 != metadata.spki_sha256
        || inspected.spki_sha256 != metadata.active_generation
        || inspected.not_before != metadata.not_before
        || inspected.not_after != metadata.not_after
        || normalize_serial(&inspected.serial_number)? != normalize_serial(&metadata.serial_number)?
    {
        return Err(Error::Auth(
            "stored core-node certificate metadata does not match its immutable generation".into(),
        ));
    }
    Ok(())
}

fn paths_for(dirs: &PeppyDirs, metadata: &CoreNodeIdentity) -> IdentityPaths {
    let generation = generation_dir(dirs, &metadata.active_generation);
    IdentityPaths {
        certificate: generation.join(CHAIN_FILE),
        private_key: generation.join(KEY_FILE),
        generation: metadata.active_generation.clone(),
        workspace_id: Some(metadata.workspace_id.clone()),
    }
}

fn load_metadata_locked(dirs: &PeppyDirs, repair_mirror: bool) -> Result<Option<CoreNodeIdentity>> {
    if identity_root(dirs).exists() {
        restrict_dir(&identity_root(dirs))?;
    }
    if identity_root(dirs).join(GENERATIONS_DIR).exists() {
        restrict_dir(&identity_root(dirs).join(GENERATIONS_DIR))?;
    }
    restrict_file_if_exists(&identity_path(dirs))?;
    let pointer = read_optional_json5::<CoreNodeIdentity>(&identity_path(dirs))?;
    let creds_path = storage::credentials_path(dirs);
    let creds = storage::load(&creds_path)?;
    if let Some(identity) = pointer.as_ref() {
        validate_identity_metadata_shape(identity)?;
    }
    if let Some(identity) = creds.core_node_identity.as_ref() {
        validate_identity_metadata_shape(identity)?;
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
            publish_identity_locked(dirs, Some(identity))?;
            Ok(Some(identity.clone()))
        }
        (Some(identity), _) | (_, Some(identity)) => Ok(Some(identity.clone())),
        (None, None) => Ok(None),
    }
}

fn validate_identity_metadata_shape(identity: &CoreNodeIdentity) -> Result<()> {
    validate_generation_name(&identity.active_generation)?;
    if identity.spki_sha256 != identity.active_generation {
        return Err(Error::Auth(
            "core-node identity generation does not match its SPKI fingerprint".into(),
        ));
    }
    if identity.subject.is_empty() {
        return Err(Error::Auth(
            "core-node identity has an empty authenticated subject".into(),
        ));
    }
    validate_core_node_name(&identity.core_node_name)?;
    if normalize_api_origin(&identity.api_origin)? != identity.api_origin {
        return Err(Error::Auth(
            "core-node identity API origin is not canonical".into(),
        ));
    }
    if identity.not_before >= identity.renew_after
        || identity.renew_after >= identity.not_after
        || identity.not_after.saturating_sub(identity.not_before) > MAX_LEAF_VALIDITY_SECS
    {
        return Err(Error::Auth(
            "core-node identity has invalid validity/renewal metadata".into(),
        ));
    }
    Ok(())
}

fn publish_identity_locked(dirs: &PeppyDirs, identity: Option<&CoreNodeIdentity>) -> Result<()> {
    match identity {
        Some(identity) => write_json5_private_durable(&identity_path(dirs), identity),
        None => remove_file_if_exists(&identity_path(dirs)),
    }
}

fn prune_generations_locked(dirs: &PeppyDirs, active_generation: &str) -> Result<()> {
    let pending = read_optional_json5::<PendingEnrollment>(&pending_path(dirs))?;
    let generations = identity_root(dirs).join(GENERATIONS_DIR);
    let entries = match std::fs::read_dir(&generations) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let keep = name == active_generation
            || pending
                .as_ref()
                .is_some_and(|pending| name == pending.generation.as_str());
        if !keep && entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    File::open(&generations)?.sync_all()?;
    Ok(())
}

fn ensure_private_layout(dirs: &PeppyDirs) -> Result<()> {
    let conf = ensure_conf_private_durable(dirs)?;
    let root = identity_root(dirs);
    let generations = root.join(GENERATIONS_DIR);
    let root_existed = root.exists();
    let generations_existed = generations.exists();
    std::fs::create_dir_all(&generations)?;
    restrict_dir(&root)?;
    restrict_dir(&generations)?;
    if !root_existed {
        File::open(&conf)?.sync_all()?;
    }
    if !generations_existed {
        File::open(&root)?.sync_all()?;
    }
    File::open(&generations)?.sync_all()?;
    Ok(())
}

fn ensure_conf_private_durable(dirs: &PeppyDirs) -> Result<PathBuf> {
    let conf = dirs.conf_dir();
    let data_root = dirs.root();
    let data_root_existed = data_root.exists();
    let existed = conf.exists();
    std::fs::create_dir_all(&conf)?;
    restrict_dir(&conf)?;
    if !data_root_existed && let Some(parent) = data_root.parent() {
        File::open(parent)?.sync_all()?;
    }
    if !existed {
        File::open(data_root)?.sync_all()?;
    }
    Ok(conf)
}

fn write_json5_private_durable<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = json5_pretty::to_string_pretty(value)
        .map_err(|e| Error::Auth(format!("failed to serialize {}: {e}", path.display())))?;
    write_private_durable(path, bytes.as_bytes())
}

fn write_private_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("identity path has no parent: {}", path.display()),
        ))
    })?;
    std::fs::create_dir_all(parent)?;
    restrict_dir(parent)?;
    daemon_config::atomic_write::publish_atomic(path, |temporary| {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(temporary)?;
        file.write_all(bytes)?;
        restrict_file(temporary)?;
        file.sync_all()
    })?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_optional_json5<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    // Every JSON document in this module is protected identity metadata. Keep
    // owner/symlink/mode enforcement centralized so new receipt/pointer reads
    // cannot accidentally bypass the private-layout invariant.
    restrict_file_if_exists(path)?;
    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json5::from_str(&contents)
            .map(Some)
            .map_err(|e| Error::Auth(format!("failed to parse {}: {e}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn restrict_file_if_exists(path: &Path) -> Result<()> {
    match restrict_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn parse_rfc3339(field: &str, value: &str) -> Result<i64> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|time| time.unix_timestamp())
        .map_err(|e| {
            Error::Auth(format!(
                "invalid certificate {field} timestamp {value:?}: {e}"
            ))
        })
}

fn spki_fingerprint(key: &KeyPair) -> String {
    hex_sha256(&key.subject_public_key_info())
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn stable_renewal_jitter(generation: &str) -> i64 {
    let digest = Sha256::digest(generation.as_bytes());
    let mut prefix = [0_u8; 8];
    prefix.copy_from_slice(&digest[..8]);
    (u64::from_be_bytes(prefix) % (MAX_RENEWAL_JITTER_SECS as u64 + 1)) as i64
}

fn normalize_serial(serial: &str) -> Result<String> {
    if serial.is_empty() || serial.trim() != serial {
        return Err(Error::Auth(
            "certificate serial number must be non-empty hexadecimal".into(),
        ));
    }
    let compact = if serial.contains(':') {
        let parts = serial.split(':').collect::<Vec<_>>();
        if parts
            .iter()
            .any(|part| part.len() != 2 || !part.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(Error::Auth(
                "certificate serial number must use two-digit hexadecimal bytes separated by colons"
                    .into(),
            ));
        }
        parts.concat()
    } else {
        if !serial.len().is_multiple_of(2) || !serial.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Auth(
                "certificate serial number must be an even-length hexadecimal string".into(),
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

fn identity_uri(workspace: &Namespace, core_node_name: &str) -> String {
    format!(
        "peppy://platform/workspaces/{}/core-nodes/{core_node_name}",
        workspace.as_str()
    )
}

fn validate_core_node_name(name: &str) -> Result<()> {
    if config::runtime::Name::new(name).is_err()
        || name.len() > daemon_config::peppy_config::MAX_CORE_NODE_NAME_LEN
    {
        return Err(Error::Auth(format!(
            "invalid running daemon core_node_name {name:?}; fix the daemon configuration and restart"
        )));
    }
    Ok(())
}

fn validate_generation_name(generation: &str) -> Result<()> {
    if generation.len() == 64
        && generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(Error::Auth(
            "invalid core-node certificate generation identifier".into(),
        ))
    }
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_parent_after_unlink(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => sync_parent_after_unlink(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn sync_parent_after_unlink(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("identity path has no parent: {}", path.display()),
        ))
    })?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    let metadata = validate_owned_non_symlink(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-regular file in protected identity layout: {}",
                path.display()
            ),
        ));
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-regular file in protected identity layout: {}",
                path.display()
            ),
        ))
    }
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> std::io::Result<()> {
    let metadata = validate_owned_non_symlink(path)?;
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-directory in protected identity layout: {}",
                path.display()
            ),
        ));
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn validate_owned_non_symlink(path: &Path) -> std::io::Result<std::fs::Metadata> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing symlink in protected identity layout: {}",
                path.display()
            ),
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "protected identity path {} is not owned by the current user",
                path.display()
            ),
        ));
    }
    Ok(metadata)
}

#[cfg(not(unix))]
fn restrict_dir(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-directory in protected identity layout: {}",
                path.display()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use rcgen::{
        BasicConstraints, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyUsagePurpose, SanType,
    };
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use serde_json::json;
    use time::Duration;

    const CORE_NODE: &str = "core-node-test-0001";
    const SUBJECT: &str = "user-test-subject";
    const WORKSPACE: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn binding_incomplete_marker_round_trips_as_a_private_durable_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(tmp.path());
        assert!(!binding_incomplete(&dirs).unwrap());

        arm_binding_incomplete(&dirs).unwrap();
        assert!(binding_incomplete(&dirs).unwrap());
        assert_eq!(
            std::fs::read(dirs.conf_dir().join(BINDING_INCOMPLETE_FILE)).unwrap(),
            BINDING_INCOMPLETE_CONTENTS
        );
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
    fn renewal_eligibility_uses_the_same_stable_jittered_threshold() {
        let identity = metadata_for_generation(&"a".repeat(64));
        let threshold = identity.renewal_at();
        assert!(threshold <= identity.renew_after);
        assert!(!identity.renewal_due(threshold - 1));
        assert!(identity.renewal_due(threshold));
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
        let key = {
            let _lock = IdentityLock::acquire(dirs).unwrap();
            ensure_private_layout(dirs).unwrap();
            prepare_pending_locked(dirs, &api_origin, SUBJECT, CORE_NODE)
                .unwrap()
                .1
        };
        let response = issued_response(&key, CORE_NODE);
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
    fn enrollment_publishes_a_valid_private_generation_and_v3_metadata() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let api_origin = normalize_api_origin(&server.base_url()).unwrap();

        // Stage the pending key first so the static mock can issue a certificate
        // for the exact SPKI enroll_and_activate will retry.
        let key = {
            let _lock = IdentityLock::acquire(&dirs).unwrap();
            ensure_private_layout(&dirs).unwrap();
            prepare_pending_locked(&dirs, &api_origin, SUBJECT, CORE_NODE)
                .unwrap()
                .1
        };
        let response = issued_response(&key, CORE_NODE);
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

        rotation.commit_after_probe().unwrap();
    }

    #[test]
    fn post_rename_credentials_failure_restores_both_activation_mirrors() {
        let server = MockServer::start();
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let api_origin = normalize_api_origin(&server.base_url()).unwrap();
        let key = {
            let _lock = IdentityLock::acquire(&dirs).unwrap();
            ensure_private_layout(&dirs).unwrap();
            prepare_pending_locked(&dirs, &api_origin, SUBJECT, CORE_NODE)
                .unwrap()
                .1
        };
        let response = issued_response(&key, CORE_NODE);
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
