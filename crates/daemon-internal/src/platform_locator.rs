//! Pure rendering of the platform upstream's connect locator: the daemon's
//! single federation target, with its mTLS material carried as per-endpoint
//! `#key=val;...` fragments on the locator string (never a global zenohd TLS
//! block, which would also apply to the plaintext local listener).

use std::path::Path;

use daemon_config::peppy_config::{ParsedEndpointBuf, validate_locator_path};

/// Moves the platform link's TLS settings onto its own connect endpoint.
/// Defaults are omitted so a release build using the system trust store stays
/// an unfragmented locator and inherits Zenoh defaults.
pub(crate) fn platform_connect_locator(
    endpoint: &ParsedEndpointBuf,
    tls: &pmi::TlsConfig,
) -> Result<String, String> {
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

    // The endpoint grammar rejects `#`, so the parsed endpoint can never
    // already carry a fragment.
    let fragment = entries.join(";");
    if fragment.is_empty() {
        Ok(endpoint.as_str().to_string())
    } else {
        Ok(format!("{endpoint}#{fragment}"))
    }
}

fn push_optional_path(
    entries: &mut Vec<String>,
    key: &str,
    path: Option<&Path>,
) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    validate_locator_path(path)
        .map_err(|error| format!("TLS material path {}: {error}", path.display()))?;
    let value = path
        .to_str()
        .expect("validate_locator_path accepted only UTF-8 paths");
    entries.push(format!("{key}={value}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::dial;

    #[test]
    fn platform_defaults_remain_unfragmented() {
        assert_eq!(
            platform_connect_locator(&dial("tls/api.example:7448"), &pmi::TlsConfig::default())
                .unwrap(),
            "tls/api.example:7448"
        );
    }

    #[test]
    fn platform_material_is_rendered_per_endpoint() {
        let tls = pmi::TlsConfig {
            root_ca_certificate: Some("/backend/ca.pem".into()),
            connect_certificate: Some("/backend/cert.pem".into()),
            connect_private_key: Some("/backend/key.pem".into()),
            enable_mtls: true,
            verify_name_on_connect: false,
            ..pmi::TlsConfig::default()
        };

        assert_eq!(
            platform_connect_locator(&dial("tls/api.example:7448"), &tls).unwrap(),
            "tls/api.example:7448#root_ca_certificate_file=/backend/ca.pem;connect_certificate_file=/backend/cert.pem;connect_private_key_file=/backend/key.pem;enable_mtls=true;verify_name_on_connect=false"
        );
    }

    #[test]
    fn fragment_delimiters_in_paths_are_rejected() {
        let tls = pmi::TlsConfig {
            root_ca_certificate: Some("/backend/bad#ca.pem".into()),
            ..pmi::TlsConfig::default()
        };
        let error = platform_connect_locator(&dial("tls/api.example:7448"), &tls)
            .expect_err("a fragment delimiter in a path must be rejected");
        assert!(error.contains("reserved locator delimiter"), "{error}");
    }
}
