//! OIDC discovery against the Zitadel `issuer` to learn the device-authorization
//! and token endpoints (the backend does not expose them). Falls back to
//! Zitadel's conventional `{issuer}/oauth/v2/{device_authorization,token,revoke}`
//! when the discovery document is unavailable or omits the device endpoint.

use serde::Deserialize;

use super::http::HttpClient;
use super::profile;
use crate::error::{Error, Result};

/// The endpoints the device flow needs, plus the (optional) revocation endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcEndpoints {
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
}

/// Security-sensitive subset of an OIDC discovery document. OIDC requires the
/// returned issuer to exactly identify the issuer whose well-known document was
/// requested; accepting endpoints from a mismatched document would let a proxy
/// or configuration error redirect device and refresh grants to another realm.
#[derive(Debug, Deserialize)]
struct OidcDiscoveryDocument {
    issuer: String,
    device_authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    revocation_endpoint: Option<String>,
}

impl OidcDiscoveryDocument {
    fn endpoints(self) -> OidcEndpoints {
        OidcEndpoints {
            device_authorization_endpoint: self.device_authorization_endpoint,
            token_endpoint: self.token_endpoint,
            revocation_endpoint: self.revocation_endpoint,
        }
    }
}

/// Discovers the endpoints for `issuer`. On any non-200 / parse failure / missing
/// device endpoint, returns the Zitadel `oauth/v2` fallback so a flaky discovery
/// document doesn't block login.
pub fn discover(http: &HttpClient, issuer: &str) -> Result<OidcEndpoints> {
    let issuer = issuer.trim_end_matches('/');
    let parsed_issuer = profile::validate_https_or_local(issuer, "OIDC issuer")?;
    if parsed_issuer.query().is_some() || parsed_issuer.fragment().is_some() {
        return Err(Error::Auth(
            "OIDC issuer URL must not contain a query string or fragment".to_string(),
        ));
    }
    let url = format!("{issuer}/.well-known/openid-configuration");

    if let Ok(resp) = http.get(&url, None)
        && resp.status == 200
        && let Ok(document) = serde_json::from_str::<OidcDiscoveryDocument>(&resp.body)
    {
        validate_discovered_issuer(&parsed_issuer, &document.issuer)?;
        let endpoints = document.endpoints();
        if !endpoints.device_authorization_endpoint.is_empty()
            && !endpoints.token_endpoint.is_empty()
        {
            validate_endpoints(&parsed_issuer, &endpoints)?;
            return Ok(endpoints);
        }
    }

    let endpoints = fallback(issuer);
    validate_endpoints(&parsed_issuer, &endpoints)?;
    Ok(endpoints)
}

fn validate_discovered_issuer(configured: &url::Url, discovered: &str) -> Result<()> {
    let discovered = discovered.trim_end_matches('/');
    let parsed = profile::validate_https_or_local(discovered, "discovered OIDC issuer")?;
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::Auth(
            "discovered OIDC issuer URL must not contain a query string or fragment".to_string(),
        ));
    }
    let configured = configured.as_str().trim_end_matches('/');
    let discovered = parsed.as_str().trim_end_matches('/');
    if discovered != configured {
        return Err(Error::Auth(format!(
            "OIDC discovery issuer mismatch: configured {configured}, document returned {discovered}"
        )));
    }
    Ok(())
}

/// Apply the transport policy to every endpoint learned through discovery.
/// An HTTPS issuer may only advertise HTTPS endpoints; this explicitly rejects
/// discovery-driven HTTPS-to-HTTP downgrades even when the target happens to be
/// loopback. A local HTTP issuer may advertise local HTTP endpoints for the
/// hermetic development stack.
fn validate_endpoints(issuer: &url::Url, endpoints: &OidcEndpoints) -> Result<()> {
    let mut values = vec![
        (
            "OIDC device authorization endpoint",
            endpoints.device_authorization_endpoint.as_str(),
        ),
        ("OIDC token endpoint", endpoints.token_endpoint.as_str()),
    ];
    if let Some(endpoint) = endpoints.revocation_endpoint.as_deref() {
        values.push(("OIDC revocation endpoint", endpoint));
    }

    for (what, endpoint) in values {
        let parsed = profile::validate_https_or_local(endpoint, what)?;
        if issuer.scheme() == "https" && parsed.scheme() != "https" {
            return Err(Error::Auth(format!(
                "{what} attempts an HTTPS-to-HTTP downgrade"
            )));
        }
    }
    Ok(())
}

/// Zitadel's conventional endpoint layout, used when discovery is unavailable.
fn fallback(issuer: &str) -> OidcEndpoints {
    OidcEndpoints {
        device_authorization_endpoint: format!("{issuer}/oauth/v2/device_authorization"),
        token_endpoint: format!("{issuer}/oauth/v2/token"),
        revocation_endpoint: Some(format!("{issuer}/oauth/v2/revoke")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    #[test]
    fn fallback_uses_oauth_v2_paths() {
        let ep = fallback("http://auth.peppy.localhost:8080");
        assert_eq!(
            ep.device_authorization_endpoint,
            "http://auth.peppy.localhost:8080/oauth/v2/device_authorization"
        );
        assert_eq!(
            ep.token_endpoint,
            "http://auth.peppy.localhost:8080/oauth/v2/token"
        );
    }

    #[test]
    fn rejects_a_non_local_plain_http_issuer() {
        let error = discover(&HttpClient::new(), "http://auth.example.test")
            .expect_err("remote OIDC must use HTTPS");
        assert!(error.to_string().contains("plain http"), "{error}");
    }

    #[test]
    fn https_issuer_rejects_any_discovered_http_downgrade() {
        let issuer = url::Url::parse("https://auth.example.test").unwrap();
        let endpoints = OidcEndpoints {
            device_authorization_endpoint: "https://auth.example.test/device".into(),
            token_endpoint: "http://127.0.0.1/token".into(),
            revocation_endpoint: Some("https://auth.example.test/revoke".into()),
        };

        let error = validate_endpoints(&issuer, &endpoints)
            .expect_err("an HTTPS issuer cannot downgrade its token endpoint");
        assert!(error.to_string().contains("downgrade"), "{error}");
    }

    #[test]
    fn local_development_issuer_and_endpoints_are_allowed() {
        let issuer = url::Url::parse("http://auth.peppy.localhost:8080").unwrap();
        let endpoints = fallback(issuer.as_str().trim_end_matches('/'));
        validate_endpoints(&issuer, &endpoints).expect("explicit local HTTP development policy");
    }

    #[test]
    fn discovery_rejects_a_mismatched_document_issuer() {
        let server = MockServer::start();
        let base = server.base_url();
        let discovery = server.mock(|when, then| {
            when.method(GET).path("/.well-known/openid-configuration");
            then.status(200).json_body(json!({
                "issuer": "http://different-tenant.localhost:9999",
                "device_authorization_endpoint": format!("{base}/oauth/v2/device_authorization"),
                "token_endpoint": format!("{base}/oauth/v2/token"),
            }));
        });

        let error = discover(&HttpClient::new(), &base)
            .expect_err("endpoints from a different issuer realm must be rejected");
        assert!(error.to_string().contains("issuer mismatch"), "{error}");
        assert_eq!(discovery.calls(), 1);
    }
}
