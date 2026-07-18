//! Pure rendering of federation TLS endpoint fragments.

use std::path::{Path, PathBuf};

use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::{FederationConfig, ParsedEndpointBuf, validate_locator_path};

use crate::registry::Federations;
use crate::{CA_CERT_FILE, CERT_FILE, Error, KEY_FILE, Result, federation_dir, pki};

/// Certificate, key, and trust-anchor paths for one machine identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub ca: PathBuf,
}

impl IdentityPaths {
    /// The identity files that do not exist yet, in reporting order. Non-empty
    /// means the identity is incomplete; callers supply their own remediation.
    pub fn missing_files(&self) -> Vec<&Path> {
        [self.ca.as_path(), self.cert.as_path(), self.key.as_path()]
            .into_iter()
            .filter(|path| !path.is_file())
            .collect()
    }
}

/// Resolves optional config overrides against the conventional identity
/// directory. Paths are checked here because even a conventional path can
/// inherit fragment delimiters from `PEPPY_HOME`.
pub fn resolve_identity_paths(
    dirs: &PeppyDirs,
    config: &FederationConfig,
) -> Result<IdentityPaths> {
    let directory = federation_dir(dirs);
    let identity = IdentityPaths {
        cert: config
            .cert_path
            .clone()
            .unwrap_or_else(|| directory.join(CERT_FILE)),
        key: config
            .key_path
            .clone()
            .unwrap_or_else(|| directory.join(KEY_FILE)),
        ca: config
            .ca_path
            .clone()
            .unwrap_or_else(|| directory.join(CA_CERT_FILE)),
    };
    let identity = pin_managed_generations(identity)?;
    validate_identity_paths(&identity)?;
    Ok(identity)
}

/// Refreshes a previously resolved identity to the generation that is current
/// now. Callers that retry or reapply federation use this once per operation,
/// then pass the returned snapshot to every locator and probe built by that
/// operation.
pub fn refresh_identity_paths(identity: &IdentityPaths) -> Result<IdentityPaths> {
    pin_managed_generations(identity.clone())
}

/// Resolves each managed directory's generation pointer once and rewrites all
/// conventional paths from that directory to the same immutable generation.
/// Explicit conventional paths receive the same protection as defaults, while
/// arbitrary operator-provided paths remain untouched.
fn pin_managed_generations(mut identity: IdentityPaths) -> Result<IdentityPaths> {
    let mut resolved = Vec::<(PathBuf, Option<PathBuf>)>::new();
    for (path, conventional_name) in [
        (&mut identity.cert, CERT_FILE),
        (&mut identity.key, KEY_FILE),
        (&mut identity.ca, CA_CERT_FILE),
    ] {
        if path.file_name().and_then(|name| name.to_str()) != Some(conventional_name) {
            continue;
        }
        let Some(parent) = managed_root(path) else {
            continue;
        };
        let generation = if let Some((_, generation)) =
            resolved.iter().find(|(candidate, _)| candidate == &parent)
        {
            generation.clone()
        } else {
            let generation = pki::current_generation(&parent)?;
            resolved.push((parent, generation.clone()));
            generation
        };
        if let Some(generation) = generation {
            *path = generation.join(conventional_name);
        }
    }
    Ok(identity)
}

fn managed_root(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let generations = parent.parent();
    if generations.and_then(Path::file_name) == Some(std::ffi::OsStr::new(pki::GENERATIONS_DIR)) {
        return generations?.parent().map(Path::to_path_buf);
    }
    Some(parent.to_path_buf())
}

/// One registry peer paired with its rendered mTLS connect locator. The
/// durable endpoint remains separate from the rendered locator so status and
/// registry joins never key off TLS fragment text.
#[derive(Debug, Clone)]
pub struct PeerLink {
    pub endpoint: ParsedEndpointBuf,
    pub locator: String,
}

/// Renders every registry peer into its connect locator. The single mapping
/// from registry entries to zenohd connect endpoints, shared by daemon startup
/// (the spawn config) and the federation resolver (every poll), so the two can
/// never render the same registry differently.
pub fn peer_links(registry: &Federations, identity: &IdentityPaths) -> Result<Vec<PeerLink>> {
    registry
        .peers()
        .iter()
        .map(|peer| {
            let locator = peer_connect_locator(peer.endpoint(), identity)?;
            Ok(PeerLink {
                endpoint: peer.endpoint().clone(),
                locator,
            })
        })
        .collect()
}

/// Adds the connecting side of the fleet mTLS identity to a peer locator.
pub fn peer_connect_locator(
    endpoint: &ParsedEndpointBuf,
    identity: &IdentityPaths,
) -> Result<String> {
    validate_identity_paths(identity)?;
    append_fragment(
        endpoint,
        [
            path_entry("root_ca_certificate_file", &identity.ca)?,
            path_entry("connect_certificate_file", &identity.cert)?,
            path_entry("connect_private_key_file", &identity.key)?,
            "enable_mtls=true".to_string(),
            "verify_name_on_connect=true".to_string(),
        ],
    )
}

/// Adds the listening side of the fleet mTLS identity to a listener locator.
pub fn listener_locator(endpoint: &ParsedEndpointBuf, identity: &IdentityPaths) -> Result<String> {
    validate_identity_paths(identity)?;
    append_fragment(
        endpoint,
        [
            path_entry("listen_certificate_file", &identity.cert)?,
            path_entry("listen_private_key_file", &identity.key)?,
            path_entry("root_ca_certificate_file", &identity.ca)?,
            "enable_mtls=true".to_string(),
        ],
    )
}

/// Moves the backend's former global TLS settings onto its own connect
/// endpoint. Defaults are omitted so a release build using the system trust
/// store stays an unfragmented locator and inherits Zenoh defaults.
pub fn backend_connect_locator(
    endpoint: &ParsedEndpointBuf,
    tls: &pmi::TlsConfig,
) -> Result<String> {
    let mut entries = Vec::new();
    push_optional_path(
        &mut entries,
        "root_ca_certificate_file",
        tls.root_ca_certificate.as_deref(),
    )?;
    push_optional_path(
        &mut entries,
        "listen_certificate_file",
        tls.listen_certificate.as_deref(),
    )?;
    push_optional_path(
        &mut entries,
        "listen_private_key_file",
        tls.listen_private_key.as_deref(),
    )?;
    push_optional_path(
        &mut entries,
        "connect_certificate_file",
        tls.connect_certificate.as_deref(),
    )?;
    push_optional_path(
        &mut entries,
        "connect_private_key_file",
        tls.connect_private_key.as_deref(),
    )?;
    if tls.enable_mtls {
        entries.push("enable_mtls=true".to_string());
    }
    if !tls.verify_name_on_connect {
        entries.push("verify_name_on_connect=false".to_string());
    }
    append_fragment(endpoint, entries)
}

/// TLS settings for the raw reachability probe of a fleet peer.
pub fn peer_probe_tls(identity: &IdentityPaths) -> pmi::TlsConfig {
    pmi::TlsConfig {
        root_ca_certificate: Some(identity.ca.clone()),
        listen_certificate: None,
        listen_private_key: None,
        connect_certificate: Some(identity.cert.clone()),
        connect_private_key: Some(identity.key.clone()),
        enable_mtls: true,
        verify_name_on_connect: true,
    }
}

fn validate_identity_paths(identity: &IdentityPaths) -> Result<()> {
    for (role, path) in [
        ("certificate", identity.cert.as_path()),
        ("private key", identity.key.as_path()),
        ("CA certificate", identity.ca.as_path()),
    ] {
        validate_fragment_path(role, path)?;
    }
    Ok(())
}

fn validate_fragment_path(role: &str, path: &Path) -> Result<()> {
    validate_locator_path(path)
        .map_err(|error| Error::Tls(format!("{role} path {}: {error}", path.display())))
}

fn path_entry(key: &str, path: &Path) -> Result<String> {
    validate_fragment_path("TLS material", path)?;
    let value = path
        .to_str()
        .expect("validate_fragment_path accepted only UTF-8 paths");
    Ok(format!("{key}={value}"))
}

fn push_optional_path(entries: &mut Vec<String>, key: &str, path: Option<&Path>) -> Result<()> {
    if let Some(path) = path {
        entries.push(path_entry(key, path)?);
    }
    Ok(())
}

fn append_fragment<I>(endpoint: &ParsedEndpointBuf, entries: I) -> Result<String>
where
    I: IntoIterator<Item = String>,
{
    // The endpoint grammar rejects `#`, so the parsed endpoint can never
    // already carry a fragment.
    let fragment = entries.into_iter().collect::<Vec<_>>().join(";");
    if fragment.is_empty() {
        Ok(endpoint.as_str().to_string())
    } else {
        Ok(format!("{endpoint}#{fragment}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::peppy_config::EndpointPurpose;

    fn identity() -> IdentityPaths {
        IdentityPaths {
            cert: "/identity/cert.pem".into(),
            key: "/identity/key.pem".into(),
            ca: "/identity/ca.pem".into(),
        }
    }

    fn dial(endpoint: &str) -> ParsedEndpointBuf {
        ParsedEndpointBuf::parse(endpoint, "tls", EndpointPurpose::Dial).unwrap()
    }

    fn listen(endpoint: &str) -> ParsedEndpointBuf {
        ParsedEndpointBuf::parse(endpoint, "tls", EndpointPurpose::Listen).unwrap()
    }

    #[test]
    fn peer_and_listener_fragments_are_stable() {
        assert_eq!(
            peer_connect_locator(&dial("tls/router.example:7449"), &identity()).unwrap(),
            "tls/router.example:7449#root_ca_certificate_file=/identity/ca.pem;connect_certificate_file=/identity/cert.pem;connect_private_key_file=/identity/key.pem;enable_mtls=true;verify_name_on_connect=true"
        );
        assert_eq!(
            listener_locator(&listen("tls/0.0.0.0:7449"), &identity()).unwrap(),
            "tls/0.0.0.0:7449#listen_certificate_file=/identity/cert.pem;listen_private_key_file=/identity/key.pem;root_ca_certificate_file=/identity/ca.pem;enable_mtls=true"
        );
    }

    #[test]
    fn peer_links_pair_every_registry_entry_with_its_locator() {
        let mut registry = Federations::default();
        registry
            .insert(crate::FederationPeer::new("tls/router.example:7449", None).unwrap())
            .unwrap();

        let links = peer_links(&registry, &identity()).unwrap();

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].endpoint.as_str(), "tls/router.example:7449");
        assert_eq!(
            links[0].locator,
            peer_connect_locator(&dial("tls/router.example:7449"), &identity()).unwrap()
        );
    }

    #[test]
    fn missing_files_reports_only_absent_identity_material() {
        let temporary = tempfile::tempdir().unwrap();
        let present = temporary.path().join("cert.pem");
        std::fs::write(&present, b"cert").unwrap();
        let identity = IdentityPaths {
            cert: present,
            key: temporary.path().join("key.pem"),
            ca: temporary.path().join("ca.pem"),
        };

        let missing = identity.missing_files();

        assert_eq!(missing, vec![identity.ca.as_path(), identity.key.as_path()]);
    }

    #[test]
    fn backend_defaults_remain_unfragmented() {
        assert_eq!(
            backend_connect_locator(&dial("tls/api.example:7448"), &pmi::TlsConfig::default())
                .unwrap(),
            "tls/api.example:7448"
        );
    }

    #[test]
    fn backend_material_is_rendered_per_endpoint() {
        let tls = pmi::TlsConfig {
            root_ca_certificate: Some("/backend/ca.pem".into()),
            connect_certificate: Some("/backend/cert.pem".into()),
            connect_private_key: Some("/backend/key.pem".into()),
            enable_mtls: true,
            verify_name_on_connect: false,
            ..pmi::TlsConfig::default()
        };

        assert_eq!(
            backend_connect_locator(&dial("tls/api.example:7448"), &tls).unwrap(),
            "tls/api.example:7448#root_ca_certificate_file=/backend/ca.pem;connect_certificate_file=/backend/cert.pem;connect_private_key_file=/backend/key.pem;enable_mtls=true;verify_name_on_connect=false"
        );
    }

    #[test]
    fn peer_probe_uses_the_connecting_identity() {
        let tls = peer_probe_tls(&identity());
        assert_eq!(
            tls.root_ca_certificate.as_deref(),
            Some(Path::new("/identity/ca.pem"))
        );
        assert_eq!(
            tls.connect_certificate.as_deref(),
            Some(Path::new("/identity/cert.pem"))
        );
        assert_eq!(
            tls.connect_private_key.as_deref(),
            Some(Path::new("/identity/key.pem"))
        );
        assert!(tls.enable_mtls);
        assert!(tls.verify_name_on_connect);
        assert!(tls.listen_certificate.is_none());
    }

    #[test]
    fn fragment_delimiters_in_any_path_are_rejected() {
        for path in [
            "/identity/bad#cert.pem",
            "/identity/bad;cert.pem",
            "/identity/bad=cert.pem",
        ] {
            let identity = IdentityPaths {
                cert: path.into(),
                ..identity()
            };
            let error = peer_connect_locator(&dial("tls/router.example:7449"), &identity)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("reserved locator delimiter"),
                "{path}: {error}"
            );
        }
    }
}
