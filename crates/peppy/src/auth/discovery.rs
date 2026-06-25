//! OIDC discovery against the Zitadel `issuer` to learn the device-authorization
//! and token endpoints (the backend does not expose them). Falls back to
//! Zitadel's conventional `{issuer}/oauth/v2/{device_authorization,token,revoke}`
//! when the discovery document is unavailable or omits the device endpoint.

use serde::Deserialize;

use super::http::HttpClient;
use crate::error::Result;

/// The endpoints the device flow needs, plus the (optional) revocation endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcEndpoints {
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
}

/// Discovers the endpoints for `issuer`. On any non-200 / parse failure / missing
/// device endpoint, returns the Zitadel `oauth/v2` fallback so a flaky discovery
/// document doesn't block login.
pub fn discover(http: &HttpClient, issuer: &str) -> Result<OidcEndpoints> {
    let issuer = issuer.trim_end_matches('/');
    let url = format!("{issuer}/.well-known/openid-configuration");

    if let Ok(resp) = http.get(&url, None)
        && resp.status == 200
        && let Ok(ep) = serde_json::from_str::<OidcEndpoints>(&resp.body)
        && !ep.device_authorization_endpoint.is_empty()
        && !ep.token_endpoint.is_empty()
    {
        return Ok(ep);
    }

    Ok(fallback(issuer))
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
}
