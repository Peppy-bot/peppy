//! Fleet-CA creation and per-machine identity issuance.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use daemon_config::atomic_write::{publish_atomic, restrict_dir, restrict_file};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use time::{Duration, OffsetDateTime};

use crate::{CA_CERT_FILE, CA_KEY_FILE, CERT_FILE, Error, KEY_FILE, Result};

const CA_VALIDITY_DAYS: i64 = 365 * 10;
const LEAF_VALIDITY_DAYS: i64 = 365 * 2;
const VALIDITY_BACKDATE_MINUTES: i64 = 5;
const PKI_LOCK_FILE: &str = ".pki.lock";
pub(crate) const GENERATIONS_DIR: &str = ".pki-generations";
const CURRENT_GENERATION_LINK: &str = ".pki-current";

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Creates a new ECDSA P-256 fleet CA. Existing CA material is never
/// overwritten, including a half-present certificate/key pair.
pub fn ca_init(directory: &Path) -> Result<()> {
    preflight_mutation_destination(directory)?;
    std::fs::create_dir_all(directory)?;
    restrict_dir(directory)?;
    let _locks = lock_directories(&[directory])?;
    validate_mutation_directory(directory)?;

    let certificate_path = directory.join(CA_CERT_FILE);
    let key_path = directory.join(CA_KEY_FILE);
    if path_entry_exists(&certificate_path)?
        || path_entry_exists(&key_path)?
        || path_entry_exists(&directory.join(CURRENT_GENERATION_LINK))?
        || has_staged_ca_material(directory)?
    {
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

    let key_pem = key.serialize_pem();
    let certificate_pem = certificate.pem();
    let generation = stage_generation(
        directory,
        [
            (CA_KEY_FILE, key_pem.as_bytes()),
            (CA_CERT_FILE, certificate_pem.as_bytes()),
        ],
    )?;
    let links = ensure_conventional_links(directory, &[CA_KEY_FILE, CA_CERT_FILE])?;
    generation.commit()?;
    links.commit();
    Ok(())
}

/// Issues a dual-purpose server/client certificate for all supplied DNS names
/// and IP addresses. Existing machine identity files are replaced so re-issue
/// is the rotation mechanism.
pub fn issue(ca_directory: &Path, hosts: &[String], output_directory: &Path) -> Result<()> {
    validate_hosts(hosts)?;

    preflight_mutation_destination(ca_directory)?;
    preflight_mutation_destination(output_directory)?;

    std::fs::create_dir_all(ca_directory)?;
    std::fs::create_dir_all(output_directory)?;
    restrict_dir(ca_directory)?;
    restrict_dir(output_directory)?;
    let _locks = lock_directories(&[ca_directory, output_directory])?;
    validate_mutation_directory(ca_directory)?;
    validate_mutation_directory(output_directory)?;
    let self_install = same_directory(ca_directory, output_directory)?;

    // Resolve the generation pointer once. The generation directories are
    // immutable and retained, so the certificate and key below cannot straddle
    // a concurrent publication.
    let ca_generation = current_generation(ca_directory)?;
    let ca_base = ca_generation.as_deref().unwrap_or(ca_directory);
    let ca_certificate_path = ca_base.join(CA_CERT_FILE);
    let ca_key_path = ca_base.join(CA_KEY_FILE);
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

    let leaf_key_pem = leaf_key.serialize_pem();
    let leaf_certificate_pem = leaf_certificate.pem();

    let managed_files: &[&str] = if self_install {
        &[CA_KEY_FILE, CA_CERT_FILE, KEY_FILE, CERT_FILE]
    } else {
        &[CA_CERT_FILE, KEY_FILE, CERT_FILE]
    };
    migrate_legacy_bundle(output_directory, managed_files)?;

    let mut files = vec![
        (CA_CERT_FILE, ca_certificate_pem.as_bytes()),
        (KEY_FILE, leaf_key_pem.as_bytes()),
        (CERT_FILE, leaf_certificate_pem.as_bytes()),
    ];
    if self_install {
        files.push((CA_KEY_FILE, ca_key_pem.as_bytes()));
    }
    let generation = stage_generation(output_directory, files)?;
    let links = ensure_conventional_links(output_directory, managed_files)?;
    generation.commit()?;
    links.commit();
    Ok(())
}

/// Returns the immutable generation selected by `directory`, if it uses the
/// managed PKI layout. Reading the link once is the snapshot boundary used by
/// both PKI writers and identity-path resolution.
pub(crate) fn current_generation(directory: &Path) -> Result<Option<PathBuf>> {
    let pointer = directory.join(CURRENT_GENERATION_LINK);
    let target = match std::fs::read_link(&pointer) {
        Ok(target) => target,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::Pki(format!(
                "read PKI generation pointer {}: {error}",
                pointer.display()
            )));
        }
    };

    let mut components = target.components();
    let valid = matches!(
        (components.next(), components.next(), components.next()),
        (Some(Component::Normal(generations)), Some(Component::Normal(_)), None)
            if generations == OsStr::new(GENERATIONS_DIR)
    );
    if !valid {
        return Err(Error::Pki(format!(
            "invalid PKI generation pointer {} -> {}",
            pointer.display(),
            target.display()
        )));
    }

    let generation = directory.join(target);
    if !generation.is_dir() {
        return Err(Error::Pki(format!(
            "PKI generation pointer {} names missing directory {}",
            pointer.display(),
            generation.display()
        )));
    }
    Ok(Some(generation))
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

fn validate_mutation_directory(path: &Path) -> Result<()> {
    let canonical = std::fs::canonicalize(path)?;
    if has_internal_pki_component(path) || managed_storage_root(&canonical).is_some() {
        return Err(internal_destination_error(path));
    }
    Ok(())
}

fn preflight_mutation_destination(path: &Path) -> Result<()> {
    if has_internal_pki_component(path) || managed_storage_root_for_path(path)?.is_some() {
        return Err(internal_destination_error(path));
    }
    Ok(())
}

fn managed_storage_root_for_path(path: &Path) -> Result<Option<PathBuf>> {
    let canonical = canonical_existing_ancestor(path)?;
    Ok(managed_storage_root(&canonical))
}

fn canonical_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut candidate = path;
    loop {
        match std::fs::canonicalize(candidate) {
            Ok(canonical) => return Ok(canonical),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                candidate = candidate.parent().unwrap_or_else(|| Path::new("."));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn managed_storage_root(path: &Path) -> Option<PathBuf> {
    let mut root = PathBuf::new();
    for component in path.components() {
        if component.as_os_str() == OsStr::new(GENERATIONS_DIR) {
            return Some(root);
        }
        root.push(component.as_os_str());
    }
    None
}

fn has_internal_pki_component(path: &Path) -> bool {
    path.components().any(|component| {
        let component = component.as_os_str();
        component == OsStr::new(PKI_LOCK_FILE)
            || component == OsStr::new(CURRENT_GENERATION_LINK)
            || component == OsStr::new(GENERATIONS_DIR)
            || component
                .to_str()
                .is_some_and(|component| component.starts_with(".pki-link-"))
    })
}

fn internal_destination_error(path: &Path) -> Error {
    Error::Pki(format!(
        "refusing to write into managed PKI internal storage {}; choose a separate identity directory",
        path.display()
    ))
}

struct PkiLocks {
    _files: Vec<File>,
}

/// Takes every lock in canonical path order so an issuance whose CA and output
/// directories differ cannot deadlock with another issuance using the reverse
/// pairing. The files are deliberately stable and never unlinked.
fn lock_directories(directories: &[&Path]) -> Result<PkiLocks> {
    let mut directories = directories
        .iter()
        .map(std::fs::canonicalize)
        .collect::<std::io::Result<Vec<_>>>()?;
    directories.sort();
    directories.dedup();

    let mut files = Vec::with_capacity(directories.len());
    for directory in directories {
        let path = directory.join(PKI_LOCK_FILE);
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        restrict_file(&path)?;
        file.lock()?;
        files.push(file);
    }
    Ok(PkiLocks { _files: files })
}

fn path_entry_exists(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn has_staged_ca_material(directory: &Path) -> std::io::Result<bool> {
    let generations = directory.join(GENERATIONS_DIR);
    let mut entries = match std::fs::read_dir(generations) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(entries.next().transpose()?.is_some())
}

/// Converts an old flat bundle to the generation layout before rotating it.
/// The pointer first selects an immutable byte-for-byte snapshot, after which
/// each conventional file can be changed to an indirection without changing
/// what a reader observes at that path.
fn migrate_legacy_bundle(directory: &Path, files: &[&str]) -> Result<()> {
    if current_generation(directory)?.is_some() {
        return Ok(());
    }

    // Capture every entry before staging or publishing anything. A malformed
    // later entry must not leave an earlier conventional path converted.
    let originals = files
        .iter()
        .copied()
        .map(|name| {
            let path = directory.join(name);
            capture_legacy_entry(&path).map(|entry| (name, entry))
        })
        .collect::<Result<Vec<_>>>()?;
    if originals
        .iter()
        .all(|(_, entry)| matches!(entry, LegacyEntry::Missing))
    {
        return Ok(());
    }

    let mut generation = stage_generation(
        directory,
        originals
            .iter()
            .filter_map(|(name, entry)| entry.content().map(|content| (*name, content))),
    )?;

    // Recheck after staging so an uncoordinated mutation cannot be silently
    // overwritten by the conversion transaction.
    for (name, original) in &originals {
        let path = directory.join(name);
        let current = capture_legacy_entry(&path)?;
        if !original.equivalent(&current) {
            return Err(Error::Pki(format!(
                "legacy PKI path {} changed while preparing rotation",
                path.display()
            )));
        }
    }

    generation.publish()?;
    for (installed, (name, _)) in originals.iter().enumerate() {
        let path = directory.join(name);
        let target = Path::new(CURRENT_GENERATION_LINK).join(name);
        if let Err(error) = replace_symlink(&target, &path) {
            let rollback =
                rollback_legacy_migration(directory, &originals[..installed], &mut generation);
            return match rollback {
                Ok(()) => Err(error.into()),
                Err(rollback) => Err(Error::Pki(format!(
                    "install PKI indirection {}: {error}; rollback failed: {rollback}",
                    path.display()
                ))),
            };
        }
    }
    Ok(())
}

enum LegacyEntry {
    Missing,
    File {
        content: Vec<u8>,
        permissions: std::fs::Permissions,
    },
    Symlink {
        target: PathBuf,
        content: Vec<u8>,
    },
}

impl LegacyEntry {
    fn content(&self) -> Option<&[u8]> {
        match self {
            Self::Missing => None,
            Self::File { content, .. } | Self::Symlink { content, .. } => Some(content),
        }
    }

    fn equivalent(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Missing, Self::Missing) => true,
            (
                Self::File {
                    content: left,
                    permissions: left_permissions,
                },
                Self::File {
                    content: right,
                    permissions: right_permissions,
                },
            ) => left == right && same_permissions(left_permissions, right_permissions),
            (
                Self::Symlink {
                    target: left_target,
                    content: left_content,
                },
                Self::Symlink {
                    target: right_target,
                    content: right_content,
                },
            ) => left_target == right_target && left_content == right_content,
            _ => false,
        }
    }
}

fn capture_legacy_entry(path: &Path) -> Result<LegacyEntry> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(LegacyEntry::Missing),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        let content = std::fs::read(path).map_err(|error| {
            Error::Pki(format!(
                "read legacy PKI symlink {} -> {} while preparing rotation: {error}",
                path.display(),
                target.display()
            ))
        })?;
        return Ok(LegacyEntry::Symlink { target, content });
    }
    if metadata.is_file() {
        return Ok(LegacyEntry::File {
            content: std::fs::read(path)?,
            permissions: metadata.permissions(),
        });
    }
    Err(Error::Pki(format!(
        "legacy PKI path {} is neither a regular file nor a symlink",
        path.display()
    )))
}

fn rollback_legacy_migration(
    directory: &Path,
    installed: &[(&str, LegacyEntry)],
    generation: &mut StagedGeneration,
) -> Result<()> {
    for (name, entry) in installed.iter().rev() {
        restore_legacy_entry(&directory.join(name), entry)?;
    }
    generation.rollback_publication()?;
    Ok(())
}

fn restore_legacy_entry(path: &Path, entry: &LegacyEntry) -> Result<()> {
    match entry {
        LegacyEntry::Missing => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        },
        LegacyEntry::File {
            content,
            permissions,
        } => {
            publish_atomic(path, |temporary| {
                std::fs::write(temporary, content)?;
                std::fs::set_permissions(temporary, permissions.clone())
            })?;
            Ok(())
        }
        LegacyEntry::Symlink { target, .. } => {
            replace_symlink(target, path)?;
            Ok(())
        }
    }
}

#[cfg(unix)]
fn same_permissions(left: &std::fs::Permissions, right: &std::fs::Permissions) -> bool {
    use std::os::unix::fs::PermissionsExt;
    left.mode() == right.mode()
}

#[cfg(not(unix))]
fn same_permissions(left: &std::fs::Permissions, right: &std::fs::Permissions) -> bool {
    left.readonly() == right.readonly()
}

struct StagedGeneration {
    directory: PathBuf,
    root: PathBuf,
    committed: bool,
}

impl StagedGeneration {
    fn publish(&mut self) -> Result<()> {
        let name = self
            .directory
            .file_name()
            .expect("a staged generation always has a name");
        let target = Path::new(GENERATIONS_DIR).join(name);
        replace_symlink(&target, &self.root.join(CURRENT_GENERATION_LINK))?;
        // Old generations are intentionally retained: IdentityPaths resolved
        // before this commit may remain in use for the life of a daemon.
        self.committed = true;
        Ok(())
    }

    fn commit(mut self) -> Result<()> {
        self.publish()
    }

    fn rollback_publication(&mut self) -> Result<()> {
        let pointer = self.root.join(CURRENT_GENERATION_LINK);
        let name = self
            .directory
            .file_name()
            .expect("a staged generation always has a name");
        let expected = Path::new(GENERATIONS_DIR).join(name);
        let current = std::fs::read_link(&pointer)?;
        if current != expected {
            return Err(Error::Pki(format!(
                "PKI generation pointer {} changed during rollback",
                pointer.display()
            )));
        }
        std::fs::remove_file(pointer)?;
        self.committed = false;
        Ok(())
    }
}

impl Drop for StagedGeneration {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
}

fn stage_generation<'a>(
    root: &Path,
    files: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Result<StagedGeneration> {
    let generations = root.join(GENERATIONS_DIR);
    std::fs::create_dir_all(&generations)?;
    restrict_dir(&generations)?;

    let directory = loop {
        let candidate = generations.join(format!("generation-{}", unique_token()));
        match std::fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let staged = StagedGeneration {
        directory,
        root: root.to_path_buf(),
        committed: false,
    };
    restrict_dir(&staged.directory)?;

    for (name, content) in files {
        let path = staged.directory.join(name);
        let mut file = File::options().write(true).create_new(true).open(&path)?;
        file.write_all(content)?;
        restrict_file(&path)?;
        file.sync_all()?;
    }
    File::open(&staged.directory)?.sync_all()?;
    Ok(staged)
}

struct LinkInstallGuard {
    created: Vec<PathBuf>,
    committed: bool,
}

impl LinkInstallGuard {
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for LinkInstallGuard {
    fn drop(&mut self) {
        if !self.committed {
            for path in &self.created {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Installs the documented flat filenames as stable indirections through the
/// current-generation link. When a pointer already exists, an unexpected flat
/// entry is replaced only after confirming it exposes the exact same bytes as
/// the selected generation.
fn ensure_conventional_links(directory: &Path, files: &[&str]) -> Result<LinkInstallGuard> {
    let current = current_generation(directory)?;
    let mut guard = LinkInstallGuard {
        created: Vec::new(),
        committed: false,
    };

    for name in files {
        let path = directory.join(name);
        let target = Path::new(CURRENT_GENERATION_LINK).join(name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        if metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
            && std::fs::read_link(&path).is_ok_and(|existing| existing == target)
        {
            continue;
        }

        if metadata.is_some() {
            let Some(current) = current.as_deref() else {
                return Err(Error::Pki(format!(
                    "cannot replace unmanaged PKI path {} without first snapshotting it",
                    path.display()
                )));
            };
            let selected = current.join(name);
            let existing_bytes = std::fs::read(&path)?;
            let selected_bytes = std::fs::read(&selected).map_err(|error| {
                Error::Pki(format!(
                    "managed PKI path {} has no matching material in {}: {error}",
                    path.display(),
                    selected.display()
                ))
            })?;
            if existing_bytes != selected_bytes {
                return Err(Error::Pki(format!(
                    "managed PKI path {} differs from selected generation {}",
                    path.display(),
                    selected.display()
                )));
            }
        } else {
            guard.created.push(path.clone());
        }
        replace_symlink(&target, &path)?;
    }
    Ok(guard)
}

#[cfg(unix)]
fn replace_symlink(target: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::symlink;

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("symlink path has no parent: {}", path.display()),
        )
    })?;
    loop {
        let temporary = parent.join(format!(".pki-link-{}.tmp", unique_token()));
        match symlink(target, &temporary) {
            Ok(()) => match std::fs::rename(&temporary, path) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let _ = std::fs::remove_file(&temporary);
                    return Err(error);
                }
            },
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(not(unix))]
fn replace_symlink(_target: &Path, path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        format!(
            "atomic PKI generation publication is unsupported on this platform: {}",
            path.display()
        ),
    ))
}

fn unique_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{counter}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::consts::PeppyDirs;
    use daemon_config::peppy_config::FederationConfig;
    use rcgen::PublicKeyData;
    use x509_parser::extensions::GeneralName;

    fn assert_leaf_matches_key(directory: &Path) {
        let certificate_bytes = std::fs::read(directory.join(CERT_FILE)).unwrap();
        let (_, certificate_pem) = x509_parser::pem::parse_x509_pem(&certificate_bytes).unwrap();
        let certificate = certificate_pem.parse_x509().unwrap();
        let key_pem = std::fs::read_to_string(directory.join(KEY_FILE)).unwrap();
        let key = KeyPair::from_pem(&key_pem).unwrap();
        assert_eq!(certificate.public_key().raw, key.subject_public_key_info());
    }

    fn assert_certificate_has_dns_name(path: &Path, expected: &str) {
        let bytes = std::fs::read(path).unwrap();
        let (_, pem) = x509_parser::pem::parse_x509_pem(&bytes).unwrap();
        let certificate = pem.parse_x509().unwrap();
        let names = &certificate
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names;
        assert!(names.contains(&GeneralName::DNSName(expected)));
    }

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
        assert_leaf_matches_key(&directory);
    }

    #[test]
    fn issue_rejects_missing_or_whitespace_hosts() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("fleet");
        ca_init(&directory).unwrap();

        assert!(issue(&directory, &[], temporary.path()).is_err());
        assert!(issue(&directory, &[" bad.example".into()], temporary.path()).is_err());
    }

    #[test]
    fn resolved_identity_pins_one_generation_and_refreshes_after_rotation() {
        let temporary = tempfile::tempdir().unwrap();
        let ca_directory = temporary.path().join("fleet");
        let output_directory = temporary.path().join("machine");
        ca_init(&ca_directory).unwrap();
        issue(&ca_directory, &["first.example".into()], &output_directory).unwrap();

        let config = FederationConfig {
            cert_path: Some(output_directory.join(CERT_FILE)),
            key_path: Some(output_directory.join(KEY_FILE)),
            ca_path: Some(output_directory.join(CA_CERT_FILE)),
            ..FederationConfig::default()
        };
        let pinned = crate::resolve_identity_paths(
            &PeppyDirs::new(temporary.path().join("unused-root")),
            &config,
        )
        .unwrap();
        assert_eq!(pinned.cert.parent(), pinned.key.parent());
        assert_eq!(pinned.cert.parent(), pinned.ca.parent());
        assert_certificate_has_dns_name(&pinned.cert, "first.example");

        issue(&ca_directory, &["second.example".into()], &output_directory).unwrap();

        // A consumer already holding a snapshot keeps a coherent immutable
        // generation, while the next poll can explicitly refresh to the newly
        // committed one.
        assert_certificate_has_dns_name(&pinned.cert, "first.example");
        let refreshed = crate::refresh_identity_paths(&pinned).unwrap();
        assert_ne!(refreshed.cert, pinned.cert);
        assert_eq!(refreshed.cert.parent(), refreshed.key.parent());
        assert_eq!(refreshed.cert.parent(), refreshed.ca.parent());
        assert_certificate_has_dns_name(&refreshed.cert, "second.example");
        assert_leaf_matches_key(&output_directory);
    }

    #[test]
    fn issue_migrates_a_legacy_flat_bundle_before_rotating() {
        let temporary = tempfile::tempdir().unwrap();
        let ca_directory = temporary.path().join("fleet");
        let source_directory = temporary.path().join("source");
        let legacy_directory = temporary.path().join("legacy");
        ca_init(&ca_directory).unwrap();
        issue(&ca_directory, &["old.example".into()], &source_directory).unwrap();
        std::fs::create_dir(&legacy_directory).unwrap();
        for name in [CA_CERT_FILE, CERT_FILE, KEY_FILE] {
            std::fs::copy(source_directory.join(name), legacy_directory.join(name)).unwrap();
        }

        issue(&ca_directory, &["new.example".into()], &legacy_directory).unwrap();

        for name in [CA_CERT_FILE, CERT_FILE, KEY_FILE] {
            assert!(
                std::fs::symlink_metadata(legacy_directory.join(name))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        assert_certificate_has_dns_name(&legacy_directory.join(CERT_FILE), "new.example");
        assert_leaf_matches_key(&legacy_directory);
    }

    #[cfg(unix)]
    #[test]
    fn invalid_later_legacy_symlink_leaves_every_entry_unchanged() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let ca_directory = temporary.path().join("fleet");
        let source_directory = temporary.path().join("source");
        let legacy_directory = temporary.path().join("legacy");
        ca_init(&ca_directory).unwrap();
        issue(&ca_directory, &["old.example".into()], &source_directory).unwrap();
        std::fs::create_dir(&legacy_directory).unwrap();
        for name in [CA_CERT_FILE, KEY_FILE] {
            std::fs::copy(source_directory.join(name), legacy_directory.join(name)).unwrap();
        }
        let bad_target = Path::new("mismatched-cert.pem");
        symlink(bad_target, legacy_directory.join(CERT_FILE)).unwrap();
        let before = [CA_CERT_FILE, KEY_FILE].map(|name| {
            let path = legacy_directory.join(name);
            (
                name,
                std::fs::read(&path).unwrap(),
                std::fs::metadata(path).unwrap().permissions().mode(),
            )
        });

        let error = issue(&ca_directory, &["new.example".into()], &legacy_directory)
            .unwrap_err()
            .to_string();

        assert!(error.contains("read legacy PKI symlink"), "{error}");
        assert!(!path_entry_exists(&legacy_directory.join(CURRENT_GENERATION_LINK)).unwrap());
        assert!(!legacy_directory.join(GENERATIONS_DIR).exists());
        for (name, content, mode) in before {
            let path = legacy_directory.join(name);
            assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
            assert_eq!(std::fs::read(&path).unwrap(), content);
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode(), mode);
        }
        let cert = legacy_directory.join(CERT_FILE);
        assert!(
            std::fs::symlink_metadata(&cert)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_link(cert).unwrap(), bad_target);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_install_rollback_restores_regular_symlink_and_missing_entries() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("legacy");
        std::fs::create_dir(&directory).unwrap();
        let ca = directory.join(CA_CERT_FILE);
        std::fs::write(&ca, b"legacy CA").unwrap();
        std::fs::set_permissions(&ca, std::fs::Permissions::from_mode(0o640)).unwrap();
        let external_certificate = temporary.path().join("external-cert.pem");
        std::fs::write(&external_certificate, b"legacy certificate").unwrap();
        let certificate = directory.join(CERT_FILE);
        symlink(&external_certificate, &certificate).unwrap();

        let files = [CA_CERT_FILE, CERT_FILE, KEY_FILE];
        let originals = files
            .iter()
            .map(|name| capture_legacy_entry(&directory.join(name)).map(|entry| (*name, entry)))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let mut generation = stage_generation(
            &directory,
            originals
                .iter()
                .filter_map(|(name, entry)| entry.content().map(|content| (*name, content))),
        )
        .unwrap();
        generation.publish().unwrap();
        for name in files {
            replace_symlink(
                &Path::new(CURRENT_GENERATION_LINK).join(name),
                &directory.join(name),
            )
            .unwrap();
        }

        rollback_legacy_migration(&directory, &originals, &mut generation).unwrap();
        drop(generation);

        assert!(!path_entry_exists(&directory.join(CURRENT_GENERATION_LINK)).unwrap());
        assert_eq!(std::fs::read(&ca).unwrap(), b"legacy CA");
        assert!(std::fs::symlink_metadata(&ca).unwrap().is_file());
        assert_eq!(
            std::fs::metadata(&ca).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(
            std::fs::symlink_metadata(&certificate)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(&certificate).unwrap(),
            external_certificate
        );
        assert!(!path_entry_exists(&directory.join(KEY_FILE)).unwrap());
        assert_eq!(
            std::fs::read_dir(directory.join(GENERATIONS_DIR))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn failed_self_install_rolls_back_new_links_and_staged_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("fleet");
        ca_init(&directory).unwrap();
        let pointer_before = std::fs::read_link(directory.join(CURRENT_GENERATION_LINK)).unwrap();
        let generations_before = std::fs::read_dir(directory.join(GENERATIONS_DIR))
            .unwrap()
            .count();
        std::fs::write(directory.join(CERT_FILE), b"conflicting legacy certificate").unwrap();

        let error = issue(&directory, &["machine.example".into()], &directory)
            .unwrap_err()
            .to_string();

        assert!(error.contains("has no matching material"), "{error}");
        assert_eq!(
            std::fs::read_link(directory.join(CURRENT_GENERATION_LINK)).unwrap(),
            pointer_before
        );
        assert!(!path_entry_exists(&directory.join(KEY_FILE)).unwrap());
        assert_eq!(
            std::fs::read(directory.join(CERT_FILE)).unwrap(),
            b"conflicting legacy certificate"
        );
        assert_eq!(
            std::fs::read_dir(directory.join(GENERATIONS_DIR))
                .unwrap()
                .count(),
            generations_before
        );
    }

    #[test]
    fn managed_generation_destinations_are_rejected_without_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("fleet");
        ca_init(&directory).unwrap();
        let pointer_before = std::fs::read_link(directory.join(CURRENT_GENERATION_LINK)).unwrap();
        let generation = current_generation(&directory).unwrap().unwrap();
        let ca_before = std::fs::read(generation.join(CA_CERT_FILE)).unwrap();
        let key_before = std::fs::read(generation.join(CA_KEY_FILE)).unwrap();
        let mut entries_before = std::fs::read_dir(&generation)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries_before.sort();

        let alias = directory.join(CURRENT_GENERATION_LINK);
        let error = issue(&directory, &["machine.example".into()], &alias)
            .unwrap_err()
            .to_string();
        assert!(error.contains("managed PKI internal storage"), "{error}");
        let nested = generation.join("nested-output");
        assert!(
            issue(&directory, &["machine.example".into()], &nested).is_err(),
            "lexically nested generation output must be rejected"
        );
        assert!(!nested.exists());
        assert!(ca_init(&alias).is_err());
        let unrelated_output = temporary.path().join("machine");
        assert!(
            issue(&alias, &["machine.example".into()], &unrelated_output).is_err(),
            "a CA alias into a managed generation is also a mutation destination"
        );
        assert!(!unrelated_output.exists());

        assert_eq!(
            std::fs::read_link(directory.join(CURRENT_GENERATION_LINK)).unwrap(),
            pointer_before
        );
        assert_eq!(
            std::fs::read(generation.join(CA_CERT_FILE)).unwrap(),
            ca_before
        );
        assert_eq!(
            std::fs::read(generation.join(CA_KEY_FILE)).unwrap(),
            key_before
        );
        let mut entries_after = std::fs::read_dir(&generation)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries_after.sort();
        assert_eq!(entries_after, entries_before);
    }

    #[test]
    fn concurrent_issuance_is_serialized_and_publishes_a_matching_pair() {
        let temporary = tempfile::tempdir().unwrap();
        let ca_directory = temporary.path().join("fleet");
        let output_directory = temporary.path().join("machine");
        ca_init(&ca_directory).unwrap();

        std::thread::scope(|scope| {
            for index in 0..8 {
                let ca_directory = &ca_directory;
                let output_directory = &output_directory;
                scope.spawn(move || {
                    issue(
                        ca_directory,
                        &[format!("machine-{index}.example")],
                        output_directory,
                    )
                    .unwrap();
                });
            }
        });

        assert_leaf_matches_key(&output_directory);
        let generation = current_generation(&output_directory).unwrap().unwrap();
        assert_eq!(
            std::fs::read(output_directory.join(CERT_FILE)).unwrap(),
            std::fs::read(generation.join(CERT_FILE)).unwrap()
        );
        assert_eq!(
            std::fs::read(output_directory.join(KEY_FILE)).unwrap(),
            std::fs::read(generation.join(KEY_FILE)).unwrap()
        );
    }
}
