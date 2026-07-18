//! Pure rendering of federation TLS endpoint fragments.

use std::path::{Path, PathBuf};

use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::{EndpointPurpose, FederationConfig, parse_endpoint};

use crate::{CA_CERT_FILE, CERT_FILE, Error, KEY_FILE, Result, federation_dir};

/// Certificate, key, and trust-anchor paths for one machine identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub ca: PathBuf,
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
    validate_identity_paths(&identity)?;
    Ok(identity)
}

/// Adds the connecting side of the fleet mTLS identity to a peer locator.
pub fn peer_connect_locator(endpoint: &str, identity: &IdentityPaths) -> Result<String> {
    parse_endpoint(endpoint, "tls", EndpointPurpose::Dial)
        .map_err(|error| Error::Tls(format!("invalid peer endpoint {endpoint:?}: {error}")))?;
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
pub fn listener_locator(endpoint: &str, identity: &IdentityPaths) -> Result<String> {
    parse_endpoint(endpoint, "tls", EndpointPurpose::Listen)
        .map_err(|error| Error::Tls(format!("invalid listener endpoint {endpoint:?}: {error}")))?;
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
pub fn backend_connect_locator(endpoint: &str, tls: &pmi::TlsConfig) -> Result<String> {
    parse_endpoint(endpoint, "tls", EndpointPurpose::Dial)
        .map_err(|error| Error::Tls(format!("invalid backend endpoint {endpoint:?}: {error}")))?;

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
    let path_text = path.to_str().ok_or_else(|| {
        Error::Tls(format!(
            "{role} path {} is not valid UTF-8 and cannot be placed in a Zenoh locator",
            path.display()
        ))
    })?;
    if let Some(delimiter) = path_text
        .chars()
        .find(|character| ['#', ';', '='].contains(character))
    {
        return Err(Error::Tls(format!(
            "{role} path {} contains reserved locator delimiter {delimiter:?}; move the identity to a path without `#`, `;`, or `=`",
            path.display()
        )));
    }
    Ok(())
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

fn append_fragment<I>(endpoint: &str, entries: I) -> Result<String>
where
    I: IntoIterator<Item = String>,
{
    if endpoint.contains('#') {
        return Err(Error::Tls(format!(
            "endpoint {endpoint:?} already contains a configuration fragment"
        )));
    }
    let fragment = entries.into_iter().collect::<Vec<_>>().join(";");
    if fragment.is_empty() {
        Ok(endpoint.to_string())
    } else {
        Ok(format!("{endpoint}#{fragment}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> IdentityPaths {
        IdentityPaths {
            cert: "/identity/cert.pem".into(),
            key: "/identity/key.pem".into(),
            ca: "/identity/ca.pem".into(),
        }
    }

    #[test]
    fn peer_and_listener_fragments_are_stable() {
        assert_eq!(
            peer_connect_locator("tls/router.example:7449", &identity()).unwrap(),
            "tls/router.example:7449#root_ca_certificate_file=/identity/ca.pem;connect_certificate_file=/identity/cert.pem;connect_private_key_file=/identity/key.pem;enable_mtls=true;verify_name_on_connect=true"
        );
        assert_eq!(
            listener_locator("tls/0.0.0.0:7449", &identity()).unwrap(),
            "tls/0.0.0.0:7449#listen_certificate_file=/identity/cert.pem;listen_private_key_file=/identity/key.pem;root_ca_certificate_file=/identity/ca.pem;enable_mtls=true"
        );
    }

    #[test]
    fn backend_defaults_remain_unfragmented() {
        assert_eq!(
            backend_connect_locator("tls/api.example:7448", &pmi::TlsConfig::default()).unwrap(),
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
            backend_connect_locator("tls/api.example:7448", &tls).unwrap(),
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
            let error = peer_connect_locator("tls/router.example:7449", &identity)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("reserved locator delimiter"),
                "{path}: {error}"
            );
        }
    }
}
