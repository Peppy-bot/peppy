use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use rcgen::KeyPair;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::{Host, Url};

use crate::{
    CoreNodeIdentity, IdentityPaths, LogoutIntent, MAX_LEAF_VALIDITY_SECS, PendingEnrollment,
    generate_private_key, identity_uri, inspect_leaf, normalize_serial, parse_private_key_pem,
    spki_fingerprint,
};

pub(crate) const IDENTITY_DIR: &str = "platform-core-node";
pub(crate) const IDENTITY_FILE: &str = "identity.json5";
pub(crate) const PENDING_FILE: &str = "pending.json5";
pub(crate) const UNVERIFIED_FILE: &str = "unverified-rotation.json5";
pub(crate) const GENERATIONS_DIR: &str = "generations";
pub(crate) const KEY_FILE: &str = "client-key.pem";
pub(crate) const CHAIN_FILE: &str = "client-chain.pem";
const LOCK_FILE: &str = ".platform-core-node.lock";
const ROTATION_LEASE_FILE: &str = ".platform-core-node-rotation.lock";
const IDENTITY_OWNER_LOCK_FILE: &str = ".platform-auth-operation.lock";
const BINDING_INCOMPLETE_FILE: &str = ".platform-binding-incomplete";
const LOGOUT_INTENT_FILE: &str = ".platform-logout-pending.json5";
const MAX_CORE_NODE_NAME_LEN: usize = 63;

#[derive(Debug)]
pub enum IdentityError {
    Io(std::io::Error),
    Invalid(String),
}

impl IdentityError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for IdentityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<std::io::Error> for IdentityError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub type IdentityResult<T> = std::result::Result<T, IdentityError>;

/// Filesystem owner for all protected core-node identity state. The only input
/// is Peppy's data root; this crate deliberately has no daemon-config coupling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityStore {
    data_root: PathBuf,
}

/// Short critical-section lock for pointer, receipt, and generation mutations.
#[derive(Debug)]
pub struct IdentityLock {
    _file: File,
}

/// Exclusive lease held across enrollment and the router apply/probe window.
#[derive(Debug)]
pub struct RotationLease {
    _file: File,
}

/// Stable process-lifetime identity owner lock. It deliberately reuses the
/// historical auth-operation inode so upgraded processes cannot acquire two
/// logically equivalent locks under different names.
#[derive(Debug)]
pub struct IdentityOwnerGuard {
    _file: File,
}

/// Durable owner of one in-progress OAuth/PAT binding transition. OAuth uses
/// its not-yet-published session revision; `None` is reserved for daemon-local
/// PAT or emergency standalone transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingTransition {
    pub version: u32,
    pub expected_session_revision: Option<uuid::Uuid>,
}

/// A durable pending key plus its transport-neutral enrollment description.
pub struct PendingGeneration {
    enrollment: PendingEnrollment,
    key: KeyPair,
}

impl PendingGeneration {
    pub fn enrollment(&self) -> &PendingEnrollment {
        &self.enrollment
    }

    pub fn key(&self) -> &KeyPair {
        &self.key
    }
}

impl IdentityStore {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn conf_dir(&self) -> PathBuf {
        self.data_root.join("conf")
    }

    pub fn identity_root(&self) -> PathBuf {
        self.conf_dir().join(IDENTITY_DIR)
    }

    pub fn identity_path(&self) -> PathBuf {
        self.identity_root().join(IDENTITY_FILE)
    }

    pub(crate) fn pending_path(&self) -> PathBuf {
        self.identity_root().join(PENDING_FILE)
    }

    pub(crate) fn unverified_path(&self) -> PathBuf {
        self.identity_root().join(UNVERIFIED_FILE)
    }

    pub(crate) fn generation_dir(&self, generation: &str) -> PathBuf {
        self.identity_root().join(GENERATIONS_DIR).join(generation)
    }

    pub fn acquire_lock(&self) -> IdentityResult<IdentityLock> {
        let conf = self.ensure_conf_private_durable()?;
        let path = conf.join(LOCK_FILE);
        let file = open_private_lock(&path)?;
        file.lock()?;
        Ok(IdentityLock { _file: file })
    }

    pub fn try_acquire_rotation_lease(&self) -> IdentityResult<RotationLease> {
        let conf = self.ensure_conf_private_durable()?;
        let path = conf.join(ROTATION_LEASE_FILE);
        let file = open_private_lock(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(RotationLease { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(IdentityError::invalid(
                "a core-node certificate rotation is already owned by another login/daemon operation; wait for its apply/probe to finish and retry",
            )),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub fn try_acquire_owner(&self) -> IdentityResult<IdentityOwnerGuard> {
        let conf = self.ensure_conf_private_durable()?;
        let path = conf.join(IDENTITY_OWNER_LOCK_FILE);
        let file = open_private_lock(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(IdentityOwnerGuard { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(IdentityError::invalid(
                "another Peppy process already owns core-node identity maintenance; stop it or wait for it to exit, then retry",
            )),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub fn arm_binding_incomplete(
        &self,
        _lock: &IdentityLock,
        expected_session_revision: Option<uuid::Uuid>,
    ) -> IdentityResult<()> {
        self.ensure_conf_private_durable()?;
        write_json5_private_durable(
            &self.conf_dir().join(BINDING_INCOMPLETE_FILE),
            &BindingTransition {
                version: 1,
                expected_session_revision,
            },
        )
    }

    pub fn clear_binding_incomplete(&self, _lock: &IdentityLock) -> IdentityResult<()> {
        remove_file_if_exists(&self.conf_dir().join(BINDING_INCOMPLETE_FILE))
    }

    pub fn binding_transition(
        &self,
        _lock: &IdentityLock,
    ) -> IdentityResult<Option<BindingTransition>> {
        let conf = self.conf_dir();
        if conf.exists() {
            restrict_dir(&conf)?;
        }
        let marker = conf.join(BINDING_INCOMPLETE_FILE);
        let transition = read_optional_json5::<BindingTransition>(&marker)?;
        if let Some(transition) = transition.as_ref()
            && transition.version != 1
        {
            return Err(IdentityError::invalid(format!(
                "unsupported binding-transition version {}",
                transition.version
            )));
        }
        Ok(transition)
    }

    pub fn clear_binding_incomplete_if_matches(
        &self,
        lock: &IdentityLock,
        expected_session_revision: Option<uuid::Uuid>,
    ) -> IdentityResult<()> {
        match self.binding_transition(lock)? {
            None => Ok(()),
            Some(transition)
                if transition.expected_session_revision == expected_session_revision =>
            {
                self.clear_binding_incomplete(lock)
            }
            Some(_) => Err(IdentityError::invalid(
                "a newer platform login owns the fail-closed binding transition",
            )),
        }
    }

    /// Inspects the fail-closed transition marker without creating directories,
    /// locks, or repairing permissions. Intended for CLI status only.
    pub fn binding_incomplete_read_only(&self) -> IdentityResult<bool> {
        let conf = self.conf_dir();
        if !conf.exists() {
            return Ok(false);
        }
        validate_private_dir_read_only(&conf)?;
        let marker = conf.join(BINDING_INCOMPLETE_FILE);
        let transition = read_optional_json5_read_only::<BindingTransition>(&marker)?;
        if let Some(transition) = transition.as_ref()
            && transition.version != 1
        {
            return Err(IdentityError::invalid(format!(
                "unsupported binding-transition version {}",
                transition.version
            )));
        }
        Ok(transition.is_some())
    }

    pub fn write_logout_intent(&self, intent: &LogoutIntent) -> IdentityResult<()> {
        if intent.version != 1 {
            return Err(IdentityError::invalid(format!(
                "unsupported logout intent version {}",
                intent.version
            )));
        }
        self.ensure_conf_private_durable()?;
        write_json5_private_durable(&self.conf_dir().join(LOGOUT_INTENT_FILE), intent)
    }

    pub fn read_logout_intent(&self) -> IdentityResult<Option<LogoutIntent>> {
        let conf = self.conf_dir();
        if conf.exists() {
            restrict_dir(&conf)?;
        }
        let intent = read_optional_json5::<LogoutIntent>(&conf.join(LOGOUT_INTENT_FILE))?;
        if let Some(intent) = intent.as_ref()
            && intent.version != 1
        {
            return Err(IdentityError::invalid(format!(
                "unsupported logout intent version {}",
                intent.version
            )));
        }
        Ok(intent)
    }

    pub fn clear_logout_intent(&self) -> IdentityResult<()> {
        remove_file_if_exists(&self.conf_dir().join(LOGOUT_INTENT_FILE))
    }

    pub fn ensure_private_layout(&self, _lock: &IdentityLock) -> IdentityResult<()> {
        let conf = self.ensure_conf_private_durable()?;
        let root = self.identity_root();
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

    pub fn read_pointer(&self, _lock: &IdentityLock) -> IdentityResult<Option<CoreNodeIdentity>> {
        let root = self.identity_root();
        if root.exists() {
            restrict_dir(&root)?;
        }
        let generations = root.join(GENERATIONS_DIR);
        if generations.exists() {
            restrict_dir(&generations)?;
        }
        let pointer = read_optional_json5(&self.identity_path())?;
        if let Some(identity) = pointer.as_ref() {
            validate_identity_metadata_shape(identity)?;
        }
        Ok(pointer)
    }

    /// Reads the canonical metadata pointer without taking/creating a writer
    /// lock and without chmod-based repair. Atomic publication makes the file
    /// independently readable; unsafe ownership, type, or mode is rejected.
    pub fn read_pointer_read_only(&self) -> IdentityResult<Option<CoreNodeIdentity>> {
        let conf = self.conf_dir();
        if !conf.exists() {
            return Ok(None);
        }
        validate_private_dir_read_only(&conf)?;
        let root = self.identity_root();
        if !root.exists() {
            return Ok(None);
        }
        validate_private_dir_read_only(&root)?;
        let generations = root.join(GENERATIONS_DIR);
        if generations.exists() {
            validate_private_dir_read_only(&generations)?;
        }
        let pointer = read_optional_json5_read_only(&self.identity_path())?;
        if let Some(identity) = pointer.as_ref() {
            validate_identity_metadata_shape(identity)?;
        }
        Ok(pointer)
    }

    pub fn publish_pointer(
        &self,
        _lock: &IdentityLock,
        identity: Option<&CoreNodeIdentity>,
    ) -> IdentityResult<()> {
        match identity {
            Some(identity) => {
                validate_identity_metadata_shape(identity)?;
                write_json5_private_durable(&self.identity_path(), identity)
            }
            None => remove_file_if_exists(&self.identity_path()),
        }
    }

    pub fn paths_for(&self, metadata: &CoreNodeIdentity) -> IdentityResult<IdentityPaths> {
        validate_identity_metadata_shape(metadata)?;
        let generation = self.generation_dir(&metadata.active_generation);
        Ok(IdentityPaths {
            certificate: generation.join(CHAIN_FILE),
            private_key: generation.join(KEY_FILE),
            generation: metadata.active_generation.clone(),
            workspace_id: Some(metadata.workspace_id.clone()),
        })
    }

    pub fn validate_stored_material(
        &self,
        metadata: &CoreNodeIdentity,
        now: i64,
    ) -> IdentityResult<IdentityPaths> {
        let paths = self.paths_for(metadata)?;
        if !metadata.is_valid_at(now) {
            return Err(IdentityError::invalid(format!(
                "core-node certificate for `{}` is not currently valid (expires at unix {}); run `peppy platform login`",
                metadata.core_node_name, metadata.not_after
            )));
        }
        let generation = paths.private_key.parent().ok_or_else(|| {
            IdentityError::invalid("stored core-node private-key path has no generation directory")
        })?;
        restrict_dir(generation)?;
        restrict_file(&paths.private_key)?;
        restrict_file(&paths.certificate)?;
        let key_pem = std::fs::read_to_string(&paths.private_key).map_err(|error| {
            IdentityError::invalid(format!(
                "cannot read core-node private key {}: {error}",
                paths.private_key.display()
            ))
        })?;
        let key = parse_private_key_pem(&key_pem).map_err(|error| {
            IdentityError::invalid(format!("invalid stored core-node private key: {error}"))
        })?;
        let chain = std::fs::read_to_string(&paths.certificate).map_err(|error| {
            IdentityError::invalid(format!(
                "cannot read core-node certificate chain {}: {error}",
                paths.certificate.display()
            ))
        })?;
        let inspected = inspect_leaf(
            &chain,
            &key,
            &identity_uri(&metadata.workspace_id, &metadata.core_node_name),
            &metadata.core_node_name,
        )
        .map_err(|error| IdentityError::invalid(error.to_string()))?;
        if inspected.spki_sha256 != metadata.spki_sha256
            || inspected.spki_sha256 != metadata.active_generation
            || inspected.not_before != metadata.not_before
            || inspected.not_after != metadata.not_after
            || normalize_serial(&inspected.serial_number)
                .map_err(|error| IdentityError::invalid(error.to_string()))?
                != normalize_serial(&metadata.serial_number)
                    .map_err(|error| IdentityError::invalid(error.to_string()))?
        {
            return Err(IdentityError::invalid(
                "stored core-node certificate metadata does not match its immutable generation",
            ));
        }
        Ok(paths)
    }

    /// Validates the active private key and certificate without creating locks
    /// or repairing any filesystem mode. Status callers can therefore report a
    /// compromised/broadened layout without becoming an identity-state writer.
    pub fn validate_stored_material_read_only(
        &self,
        metadata: &CoreNodeIdentity,
        now: i64,
    ) -> IdentityResult<IdentityPaths> {
        let paths = self.paths_for(metadata)?;
        if !metadata.is_valid_at(now) {
            return Err(IdentityError::invalid(format!(
                "core-node certificate for `{}` is not currently valid (expires at unix {}); run `peppy platform login`",
                metadata.core_node_name, metadata.not_after
            )));
        }
        validate_private_dir_read_only(&self.conf_dir())?;
        validate_private_dir_read_only(&self.identity_root())?;
        validate_private_dir_read_only(&self.identity_root().join(GENERATIONS_DIR))?;
        let generation = paths.private_key.parent().ok_or_else(|| {
            IdentityError::invalid("stored core-node private-key path has no generation directory")
        })?;
        validate_private_dir_read_only(generation)?;
        validate_private_file_read_only(&paths.private_key)?;
        validate_private_file_read_only(&paths.certificate)?;
        let key_pem = std::fs::read_to_string(&paths.private_key).map_err(|error| {
            IdentityError::invalid(format!(
                "cannot read core-node private key {}: {error}",
                paths.private_key.display()
            ))
        })?;
        let key = parse_private_key_pem(&key_pem).map_err(|error| {
            IdentityError::invalid(format!("invalid stored core-node private key: {error}"))
        })?;
        let chain = std::fs::read_to_string(&paths.certificate).map_err(|error| {
            IdentityError::invalid(format!(
                "cannot read core-node certificate chain {}: {error}",
                paths.certificate.display()
            ))
        })?;
        let inspected = inspect_leaf(
            &chain,
            &key,
            &identity_uri(&metadata.workspace_id, &metadata.core_node_name),
            &metadata.core_node_name,
        )
        .map_err(|error| IdentityError::invalid(error.to_string()))?;
        if inspected.spki_sha256 != metadata.spki_sha256
            || inspected.spki_sha256 != metadata.active_generation
            || inspected.not_before != metadata.not_before
            || inspected.not_after != metadata.not_after
            || normalize_serial(&inspected.serial_number)
                .map_err(|error| IdentityError::invalid(error.to_string()))?
                != normalize_serial(&metadata.serial_number)
                    .map_err(|error| IdentityError::invalid(error.to_string()))?
        {
            return Err(IdentityError::invalid(
                "stored core-node certificate metadata does not match its immutable generation",
            ));
        }
        Ok(paths)
    }

    pub fn prepare_generation(
        &self,
        lock: &IdentityLock,
        api_origin: &str,
        subject: &str,
        core_node_name: &str,
    ) -> IdentityResult<PendingGeneration> {
        self.ensure_private_layout(lock)?;
        restrict_file_if_exists(&self.pending_path())?;
        if let Some(pending) = read_optional_json5::<PendingEnrollment>(&self.pending_path())?
            && pending.api_origin == api_origin
            && pending.subject == subject
            && pending.core_node_name == core_node_name
            && validate_generation_name(&pending.generation).is_ok()
        {
            let generation = self.generation_dir(&pending.generation);
            restrict_dir(&generation)?;
            restrict_file(&generation.join(KEY_FILE))?;
            let key_pem = std::fs::read_to_string(generation.join(KEY_FILE))?;
            let key = parse_private_key_pem(&key_pem).map_err(|error| {
                IdentityError::invalid(format!("invalid pending core-node private key: {error}"))
            })?;
            let fingerprint = spki_fingerprint(&key);
            if fingerprint == pending.spki_sha256 && fingerprint == pending.generation {
                return Ok(PendingGeneration {
                    enrollment: pending,
                    key,
                });
            }
        }

        if let Some(stale) = read_optional_json5::<PendingEnrollment>(&self.pending_path())? {
            validate_generation_name(&stale.generation)?;
            let active = self.read_pointer(lock)?;
            if active
                .as_ref()
                .is_none_or(|identity| identity.active_generation != stale.generation)
            {
                remove_dir_if_exists(&self.generation_dir(&stale.generation))?;
            }
            remove_file_if_exists(&self.pending_path())?;
        }

        let key = generate_private_key().map_err(|error| {
            IdentityError::invalid(format!("failed to generate core-node private key: {error}"))
        })?;
        let spki_sha256 = spki_fingerprint(&key);
        let pending = PendingEnrollment {
            api_origin: api_origin.to_string(),
            subject: subject.to_string(),
            core_node_name: core_node_name.to_string(),
            generation: spki_sha256.clone(),
            spki_sha256,
        };
        let generation = self.generation_dir(&pending.generation);
        std::fs::create_dir(&generation)?;
        restrict_dir(&generation)?;
        File::open(
            generation
                .parent()
                .expect("generation path always has a parent"),
        )?
        .sync_all()?;
        write_private_durable(&generation.join(KEY_FILE), key.serialize_pem().as_bytes())?;
        File::open(&generation)?.sync_all()?;
        write_json5_private_durable(&self.pending_path(), &pending)?;
        Ok(PendingGeneration {
            enrollment: pending,
            key,
        })
    }

    pub fn discard_pending_generation(
        &self,
        _lock: &IdentityLock,
        generation: &str,
        active: Option<&CoreNodeIdentity>,
    ) -> IdentityResult<()> {
        validate_generation_name(generation)?;
        remove_file_if_exists(&self.pending_path())?;
        if active.is_none_or(|identity| identity.active_generation != generation) {
            remove_dir_if_exists(&self.generation_dir(generation))?;
        }
        Ok(())
    }

    pub fn write_certificate_chain(
        &self,
        _lock: &IdentityLock,
        generation: &str,
        chain_pem: &str,
    ) -> IdentityResult<()> {
        validate_generation_name(generation)?;
        let generation = self.generation_dir(generation);
        restrict_dir(&generation)?;
        write_private_durable(&generation.join(CHAIN_FILE), chain_pem.as_bytes())?;
        File::open(&generation)?.sync_all()?;
        Ok(())
    }

    pub(crate) fn finish_pending(&self, _lock: &IdentityLock) -> IdentityResult<()> {
        remove_file_if_exists(&self.pending_path())
    }

    pub fn prune_generations(
        &self,
        _lock: &IdentityLock,
        active_generation: &str,
    ) -> IdentityResult<()> {
        validate_generation_name(active_generation)?;
        let pending = read_optional_json5::<PendingEnrollment>(&self.pending_path())?;
        let generations = self.identity_root().join(GENERATIONS_DIR);
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

    pub fn remove_generation_if_inactive(
        &self,
        lock: &IdentityLock,
        generation: &str,
    ) -> IdentityResult<()> {
        validate_generation_name(generation)?;
        if self
            .read_pointer(lock)?
            .is_some_and(|identity| identity.active_generation == generation)
        {
            return Err(IdentityError::invalid(format!(
                "refusing to delete rejected generation {generation} because it became active again"
            )));
        }
        remove_dir_if_exists(&self.generation_dir(generation))
    }

    pub fn clear_identity(&self, _lock: &IdentityLock) -> IdentityResult<()> {
        remove_dir_if_exists(&self.identity_root())
    }

    pub(crate) fn read_receipt<T: DeserializeOwned>(
        &self,
        _lock: &IdentityLock,
    ) -> IdentityResult<Option<T>> {
        read_optional_json5(&self.unverified_path())
    }

    pub(crate) fn write_receipt<T: Serialize>(
        &self,
        _lock: &IdentityLock,
        receipt: &T,
    ) -> IdentityResult<()> {
        write_json5_private_durable(&self.unverified_path(), receipt)
    }

    pub(crate) fn remove_receipt(&self, _lock: &IdentityLock) -> IdentityResult<()> {
        remove_file_if_exists(&self.unverified_path())
    }

    fn ensure_conf_private_durable(&self) -> IdentityResult<PathBuf> {
        let conf = self.conf_dir();
        let data_root_existed = self.data_root.exists();
        let conf_existed = conf.exists();
        std::fs::create_dir_all(&conf)?;
        restrict_dir(&conf)?;
        if !data_root_existed && let Some(parent) = self.data_root.parent() {
            File::open(parent)?.sync_all()?;
        }
        if !conf_existed {
            File::open(&self.data_root)?.sync_all()?;
        }
        Ok(conf)
    }
}

pub fn acquire_identity_owner(data_root: impl Into<PathBuf>) -> IdentityResult<IdentityOwnerGuard> {
    IdentityStore::new(data_root).try_acquire_owner()
}

pub fn normalize_api_origin(api_url: &str) -> IdentityResult<String> {
    Ok(validate_https_or_local(api_url, "platform API")?
        .origin()
        .ascii_serialization())
}

fn validate_https_or_local(raw: &str, what: &str) -> IdentityResult<Url> {
    if raw.trim() != raw || raw.is_empty() {
        return Err(IdentityError::invalid(format!(
            "invalid {what} URL: it must be non-empty and contain no surrounding whitespace"
        )));
    }
    let parsed = Url::parse(raw)
        .map_err(|error| IdentityError::invalid(format!("invalid {what} URL: {error}")))?;
    if parsed.host().is_none() || parsed.cannot_be_a_base() {
        return Err(IdentityError::invalid(format!(
            "invalid {what} URL: an absolute URL with a host is required"
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(IdentityError::invalid(format!(
            "invalid {what} URL: embedded credentials are not allowed"
        )));
    }
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if is_local(&parsed) => Ok(parsed),
        "http" => Err(IdentityError::invalid(format!(
            "refusing plain http for non-local {what} (use https)"
        ))),
        other => Err(IdentityError::invalid(format!(
            "unsupported URL scheme `{other}` for {what}"
        ))),
    }
}

fn is_local(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(host)) => host == "localhost" || host.ends_with(".localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

pub fn validate_core_node_name(name: &str) -> IdentityResult<()> {
    if config::runtime::Name::new(name).is_err() || name.len() > MAX_CORE_NODE_NAME_LEN {
        return Err(IdentityError::invalid(format!(
            "invalid running daemon core_node_name {name:?}; fix the daemon configuration and restart"
        )));
    }
    Ok(())
}

pub fn validate_generation_name(generation: &str) -> IdentityResult<()> {
    if generation.len() == 64
        && generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(IdentityError::invalid(
            "invalid core-node certificate generation identifier",
        ))
    }
}

pub fn validate_identity_metadata_shape(identity: &CoreNodeIdentity) -> IdentityResult<()> {
    validate_generation_name(&identity.active_generation)?;
    if identity.spki_sha256 != identity.active_generation {
        return Err(IdentityError::invalid(
            "core-node identity generation does not match its SPKI fingerprint",
        ));
    }
    if identity.subject.is_empty() {
        return Err(IdentityError::invalid(
            "core-node identity has an empty authenticated subject",
        ));
    }
    validate_core_node_name(&identity.core_node_name)?;
    if normalize_api_origin(&identity.api_origin)? != identity.api_origin {
        return Err(IdentityError::invalid(
            "core-node identity API origin is not canonical",
        ));
    }
    if identity.not_before >= identity.renew_after
        || identity.renew_after >= identity.not_after
        || identity.not_after.saturating_sub(identity.not_before) > MAX_LEAF_VALIDITY_SECS
    {
        return Err(IdentityError::invalid(
            "core-node identity has invalid validity/renewal metadata",
        ));
    }
    Ok(())
}

fn open_private_lock(path: &Path) -> IdentityResult<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    restrict_file(path)?;
    Ok(file)
}

fn write_json5_private_durable<T: Serialize>(path: &Path, value: &T) -> IdentityResult<()> {
    let bytes = json5_pretty::to_string_pretty(value).map_err(|error| {
        IdentityError::invalid(format!("failed to serialize {}: {error}", path.display()))
    })?;
    write_private_durable(path, bytes.as_bytes())
}

fn write_private_durable(path: &Path, bytes: &[u8]) -> IdentityResult<()> {
    let parent = path.parent().ok_or_else(|| {
        IdentityError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("identity path has no parent: {}", path.display()),
        ))
    })?;
    std::fs::create_dir_all(parent)?;
    restrict_dir(parent)?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    let temporary_path = temporary.path().to_path_buf();
    {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&temporary_path)?;
        file.write_all(bytes)?;
        restrict_file(&temporary_path)?;
        file.sync_all()?;
    }
    temporary.persist(path).map_err(|error| error.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_optional_json5<T: DeserializeOwned>(path: &Path) -> IdentityResult<Option<T>> {
    restrict_file_if_exists(path)?;
    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json5::from_str(&contents).map(Some).map_err(|error| {
            IdentityError::invalid(format!("failed to parse {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_optional_json5_read_only<T: DeserializeOwned>(path: &Path) -> IdentityResult<Option<T>> {
    match validate_private_file_read_only(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let contents = std::fs::read_to_string(path)?;
    serde_json5::from_str(&contents).map(Some).map_err(|error| {
        IdentityError::invalid(format!("failed to parse {}: {error}", path.display()))
    })
}

fn restrict_file_if_exists(path: &Path) -> IdentityResult<()> {
    match restrict_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn validate_private_file_read_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

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
    if metadata.permissions().mode() & 0o7777 != 0o600 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "protected identity file {} is not mode 0600",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_file_read_only(path: &Path) -> std::io::Result<()> {
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
fn validate_private_dir_read_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

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
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "protected identity directory {} is not mode 0700",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_dir_read_only(path: &Path) -> std::io::Result<()> {
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

fn remove_file_if_exists(path: &Path) -> IdentityResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_parent_after_unlink(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_dir_if_exists(path: &Path) -> IdentityResult<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => sync_parent_after_unlink(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn sync_parent_after_unlink(path: &Path) -> IdentityResult<()> {
    let parent = path.parent().ok_or_else(|| {
        IdentityError::Io(std::io::Error::new(
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

    const API: &str = "https://api.peppy.bot";
    const SUBJECT: &str = "user-test-subject";
    const CORE_NODE: &str = "core-node-test-0001";

    #[test]
    fn pending_generation_is_reused_for_the_same_binding() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let lock = store.acquire_lock().unwrap();

        let first = store
            .prepare_generation(&lock, API, SUBJECT, CORE_NODE)
            .unwrap();
        let first_generation = first.enrollment().generation.clone();
        drop(first);
        let second = store
            .prepare_generation(&lock, API, SUBJECT, CORE_NODE)
            .unwrap();

        assert_eq!(second.enrollment().generation, first_generation);
        assert_eq!(spki_fingerprint(second.key()), first_generation);
    }

    #[test]
    fn a_different_binding_replaces_the_pending_generation() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let lock = store.acquire_lock().unwrap();
        let first = store
            .prepare_generation(&lock, API, SUBJECT, CORE_NODE)
            .unwrap();
        let old = first.enrollment().generation.clone();
        drop(first);

        let replacement = store
            .prepare_generation(&lock, API, SUBJECT, "core-node-test-0002")
            .unwrap();

        assert_ne!(replacement.enrollment().generation, old);
        assert!(!store.generation_dir(&old).exists());
    }

    #[cfg(unix)]
    #[test]
    fn protected_layout_is_owner_only_and_rejects_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let lock = store.acquire_lock().unwrap();
        let pending = store
            .prepare_generation(&lock, API, SUBJECT, CORE_NODE)
            .unwrap();
        let generation = store.generation_dir(&pending.enrollment().generation);
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&store.conf_dir()), 0o700);
        assert_eq!(mode(&store.identity_root()), 0o700);
        assert_eq!(mode(&generation), 0o700);
        assert_eq!(mode(&generation.join(KEY_FILE)), 0o600);

        let marker = store.conf_dir().join(BINDING_INCOMPLETE_FILE);
        symlink(generation.join(KEY_FILE), &marker).unwrap();
        assert!(matches!(
            store.binding_transition(&lock),
            Err(IdentityError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn owner_lock_is_fail_fast_and_uses_the_stable_inode() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let owner = store.try_acquire_owner().unwrap();
        let error = store.try_acquire_owner().unwrap_err();
        assert!(error.to_string().contains("already owns"));
        assert!(store.conf_dir().join(IDENTITY_OWNER_LOCK_FILE).exists());
        drop(owner);
        store.try_acquire_owner().unwrap();
    }

    #[test]
    fn logout_intent_requires_exact_v1_shape() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        store.ensure_conf_private_durable().unwrap();
        let path = store.conf_dir().join(LOGOUT_INTENT_FILE);

        std::fs::write(
            &path,
            r#"{ "version": 2, "expected_session_revision": null }"#,
        )
        .unwrap();
        assert!(
            store
                .read_logout_intent()
                .unwrap_err()
                .to_string()
                .contains("unsupported logout intent version")
        );

        std::fs::write(
            &path,
            r#"{ "version": 1, "expected_session_revision": null, "legacy": true }"#,
        )
        .unwrap();
        assert!(store.read_logout_intent().is_err());
    }

    #[test]
    fn origin_and_generation_validation_are_canonical() {
        assert_eq!(
            normalize_api_origin("HTTPS://API.PEPPY.BOT:443/v1").unwrap(),
            "https://api.peppy.bot"
        );
        assert_eq!(
            normalize_api_origin("http://LOCALHOST:3000/api").unwrap(),
            "http://localhost:3000"
        );
        assert!(normalize_api_origin("http://api.peppy.bot").is_err());
        assert!(validate_generation_name(&"a".repeat(64)).is_ok());
        assert!(validate_generation_name("../escape").is_err());
    }
}
