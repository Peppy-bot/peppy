//! CLI-owned registry of user federation peers.

use std::fs::File;
use std::path::{Path, PathBuf};

use config::runtime::Name;
use daemon_config::peppy_config::{EndpointPurpose, parse_endpoint};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{Error, Result};

/// On-disk schema version for `federations.json5`.
pub const FEDERATIONS_VERSION: u32 = 1;

/// Process-wide registry mutation lock. Keeping the file handle alive keeps
/// the kernel lock held; dropping it releases the lock, including on error or
/// process exit.
pub struct RegistryLock {
    _file: File,
}

/// Serializes CLI read-modify-write transactions for one registry. The lock
/// file is stable and lives beside the registry, while the registry itself is
/// still atomically replaced on publish.
pub fn lock(path: &Path) -> Result<RegistryLock> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    restrict_dir(parent)?;
    let lock_path = registry_lock_path(path);
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    restrict_file(&lock_path)?;
    file.lock()?;
    Ok(RegistryLock { _file: file })
}

fn registry_lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("federations.json5");
    path.with_file_name(format!(".{name}.lock"))
}

/// One durable federation. The endpoint is the routing key; the core-node name
/// is cached display metadata discovered by the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FederationPeer {
    endpoint: String,
    #[serde(default)]
    core_node: Option<String>,
}

impl FederationPeer {
    /// Parses a peer, enforcing a fragment-free TLS dial locator and, when
    /// present, the shared runtime-name grammar.
    pub fn new(endpoint: impl Into<String>, core_node: Option<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        parse_endpoint(&endpoint, "tls", EndpointPurpose::Dial).map_err(|error| {
            Error::Registry(format!("invalid peer endpoint {endpoint:?}: {error}"))
        })?;
        validate_core_node(core_node.as_deref())?;
        Ok(Self {
            endpoint,
            core_node,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn core_node(&self) -> Option<&str> {
        self.core_node.as_deref()
    }

    fn set_core_node(&mut self, core_node: Option<String>) -> Result<()> {
        validate_core_node(core_node.as_deref())?;
        self.core_node = core_node;
        Ok(())
    }
}

impl<'de> Deserialize<'de> for FederationPeer {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            endpoint: String,
            #[serde(default)]
            core_node: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.endpoint, wire.core_node).map_err(serde::de::Error::custom)
    }
}

fn validate_core_node(core_node: Option<&str>) -> Result<()> {
    let Some(core_node) = core_node else {
        return Ok(());
    };
    Name::new(core_node).map_err(|error| {
        Error::Registry(format!(
            "invalid cached core-node name {core_node:?}: {error}"
        ))
    })?;
    if core_node == crate::RESERVED_BACKEND_NAME {
        return Err(Error::Registry(format!(
            "cached core-node name {core_node:?} is reserved for the platform backend; remove \
             this peer by its exact TLS endpoint"
        )));
    }
    Ok(())
}

/// Versioned `federations.json5` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Federations {
    version: u32,
    federations: Vec<FederationPeer>,
}

impl Default for Federations {
    fn default() -> Self {
        Self {
            version: FEDERATIONS_VERSION,
            federations: Vec::new(),
        }
    }
}

impl Federations {
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn peers(&self) -> &[FederationPeer] {
        &self.federations
    }

    pub fn into_peers(self) -> Vec<FederationPeer> {
        self.federations
    }

    /// Adds a peer. Endpoint identity is exact and duplicates are rejected.
    pub fn insert(&mut self, peer: FederationPeer) -> Result<()> {
        if self
            .federations
            .iter()
            .any(|existing| existing.endpoint == peer.endpoint)
        {
            return Err(Error::Registry(format!(
                "endpoint {:?} is already federated",
                peer.endpoint
            )));
        }
        self.federations.push(peer);
        Ok(())
    }

    /// Removes the exact durable endpoint key.
    pub fn remove(&mut self, endpoint: &str) -> Result<FederationPeer> {
        let index = self
            .federations
            .iter()
            .position(|peer| peer.endpoint == endpoint)
            .ok_or_else(|| {
                Error::Registry(format!("endpoint {endpoint:?} is not in the registry"))
            })?;
        Ok(self.federations.remove(index))
    }

    /// Updates cached display metadata without changing the durable key.
    pub fn set_core_node(&mut self, endpoint: &str, core_node: Option<String>) -> Result<()> {
        let peer = self
            .federations
            .iter_mut()
            .find(|peer| peer.endpoint == endpoint)
            .ok_or_else(|| {
                Error::Registry(format!("endpoint {endpoint:?} is not in the registry"))
            })?;
        peer.set_core_node(core_node)
    }
}

impl<'de> Deserialize<'de> for Federations {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            version: u32,
            #[serde(default)]
            federations: Vec<FederationPeer>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let mut registry = Self {
            version: wire.version,
            federations: Vec::with_capacity(wire.federations.len()),
        };
        for peer in wire.federations {
            registry.insert(peer).map_err(serde::de::Error::custom)?;
        }
        Ok(registry)
    }
}

/// Loads a registry, returning an empty v1 document when it does not exist.
pub fn load(path: &Path) -> Result<Federations> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Federations::default());
        }
        Err(error) => return Err(Error::Io(error)),
    };

    #[derive(Deserialize)]
    struct VersionOnly {
        #[serde(default)]
        version: u32,
    }

    let header: VersionOnly = serde_json5::from_str(&content)
        .map_err(|error| Error::Registry(format!("failed to parse {}: {error}", path.display())))?;
    if header.version != FEDERATIONS_VERSION {
        return Err(Error::Registry(format!(
            "{} uses unsupported format v{} (expected v{}); move it aside and recreate its entries with `peppy federation federate`",
            path.display(),
            header.version,
            FEDERATIONS_VERSION
        )));
    }

    serde_json5::from_str(&content)
        .map_err(|error| Error::Registry(format!("failed to parse {}: {error}", path.display())))
}

/// Publishes a registry atomically with owner-only file permissions.
pub fn save(path: &Path, registry: &Federations) -> Result<()> {
    let content = json5_pretty::to_string_pretty(registry)
        .map_err(|error| Error::Registry(format!("failed to serialize registry: {error}")))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        restrict_dir(parent)?;
    }
    daemon_config::atomic_write::publish_atomic(path, |temporary| {
        std::fs::write(temporary, &content)?;
        restrict_file(temporary)
    })?;
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(endpoint: &str, core_node: Option<&str>) -> FederationPeer {
        FederationPeer::new(endpoint, core_node.map(str::to_string)).unwrap()
    }

    #[test]
    fn registry_round_trips_and_updates_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("conf/federations.json5");
        let mut registry = Federations::default();
        registry
            .insert(peer("tls/router-a.example:7449", None))
            .unwrap();
        registry
            .set_core_node("tls/router-a.example:7449", Some("daemon-a".into()))
            .unwrap();

        save(&path, &registry).unwrap();
        assert_eq!(load(&path).unwrap(), registry);
        assert_eq!(registry.version(), FEDERATIONS_VERSION);
        assert_eq!(registry.peers()[0].core_node(), Some("daemon-a"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn missing_registry_loads_as_current_empty_document() {
        let temporary = tempfile::tempdir().unwrap();
        let registry = load(&temporary.path().join("missing.json5")).unwrap();
        assert_eq!(registry, Federations::default());
    }

    #[test]
    fn registry_lock_is_exclusive_and_released_on_drop() {
        use std::fs::TryLockError;

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("conf/federations.json5");
        let first = lock(&path).unwrap();
        let second_file = File::options()
            .read(true)
            .write(true)
            .open(registry_lock_path(&path))
            .unwrap();
        assert!(matches!(
            second_file.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        drop(first);
        second_file.try_lock().unwrap();
    }

    #[test]
    fn unsupported_version_is_actionable_even_when_peer_shape_is_bad() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("federations.json5");
        std::fs::write(&path, r#"{ version: 2, federations: [{ endpoint: 12 }] }"#).unwrap();

        let error = load(&path).unwrap_err().to_string();
        assert!(error.contains("unsupported format v2"));
        assert!(error.contains("peppy federation federate"));
    }

    #[test]
    fn duplicate_endpoints_are_rejected_at_insert_and_load() {
        let mut registry = Federations::default();
        registry
            .insert(peer("tls/router.example:7449", None))
            .unwrap();
        let error = registry
            .insert(peer("tls/router.example:7449", Some("daemon-b")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("already federated"));

        let duplicate = r#"{
            version: 1,
            federations: [
                { endpoint: "tls/router.example:7449" },
                { endpoint: "tls/router.example:7449", core_node: "daemon-b" },
            ],
        }"#;
        let error = serde_json5::from_str::<Federations>(duplicate)
            .unwrap_err()
            .to_string();
        assert!(error.contains("already federated"));
    }

    #[test]
    fn remove_uses_the_exact_endpoint_key() {
        let mut registry = Federations::default();
        registry
            .insert(peer("tls/router.example:7449", Some("daemon-a")))
            .unwrap();

        let removed = registry.remove("tls/router.example:7449").unwrap();
        assert_eq!(removed.core_node(), Some("daemon-a"));
        assert!(registry.peers().is_empty());

        let error = registry
            .remove("tls/router.example:7449")
            .unwrap_err()
            .to_string();
        assert!(error.contains("is not in the registry"));
    }

    #[test]
    fn malformed_or_non_tls_endpoints_are_rejected() {
        for endpoint in [
            "tcp/router.example:7449",
            "tls/router.example",
            "tls/0.0.0.0:7449",
            "tls/router.example:7449#enable_mtls=true",
        ] {
            let error = FederationPeer::new(endpoint, None).unwrap_err().to_string();
            assert!(
                error.contains("invalid peer endpoint"),
                "{endpoint}: {error}"
            );
        }
    }

    #[test]
    fn invalid_cached_core_node_is_rejected_at_every_boundary() {
        let error = FederationPeer::new("tls/router.example:7449", Some("bad name".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid cached core-node name"));

        let document = r#"{
            version: 1,
            federations: [{ endpoint: "tls/router.example:7449", core_node: "bad/name" }],
        }"#;
        let error = serde_json5::from_str::<Federations>(document)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid cached core-node name"));

        let reserved = FederationPeer::new(
            "tls/router.example:7449",
            Some(crate::RESERVED_BACKEND_NAME.into()),
        )
        .unwrap_err()
        .to_string();
        assert!(reserved.contains("reserved for the platform backend"));
    }
}
