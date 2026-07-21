//! `GET {api_url}/cli/auth-config`: the public bootstrap endpoint that hands the CLI
//! the Zitadel `issuer`, the Native app `client_id`, and the exact `scopes`
//! string to request (already including `offline_access` and the project-audience
//! scope, sent to Zitadel **verbatim**, never reassembled).

use serde::Deserialize;

use super::http::HttpClient;
use super::profile::{self, TransportPolicy};
use crate::error::{Error, Result};

/// The three fields the backend serves to the CLI (no endpoint URLs; those come
/// from OIDC discovery against `issuer`).
#[derive(Debug, Clone, Deserialize)]
pub struct CliConfig {
    pub issuer: String,
    pub client_id: String,
    pub scopes: String,
}

/// Fetches `/cli/auth-config`. A `503` means the deployment hasn't provisioned the
/// CLI client yet (`PEPPY_CLI_CLIENT_ID` / `PEPPY_INTROSPECT_AUDIENCE` unset).
/// Callers pass [`profile::build_transport_policy`]; the parameter exists so the
/// strict policy stays exercisable from tests in any build profile.
pub fn fetch(http: &HttpClient, api_url: &str, policy: TransportPolicy) -> Result<CliConfig> {
    let url = format!("{}/cli/auth-config", api_url.trim_end_matches('/'));
    let resp = http.get(&url, None)?;
    match resp.status {
        200 => {
            let config: CliConfig = resp.json("/cli/auth-config")?;
            // The issuer is server supplied and every later step of the device
            // flow, up to and including the token exchange, is aimed at
            // whatever it names. Apply the transport policy here, at the point
            // it enters the process, rather than at each of those steps.
            profile::validate_https_or_local_with(&config.issuer, "OIDC issuer", policy)?;
            Ok(config)
        }
        503 => Err(Error::Auth(
            "CLI login isn't configured on this backend yet (the deployment hasn't provisioned the CLI client).".to_string(),
        )),
        s => Err(Error::Http(format!("GET {url} returned {s}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;

    #[test]
    fn rejects_an_insecure_server_supplied_issuer() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/cli/auth-config");
            then.status(200).json_body(serde_json::json!({
                "issuer": "http://auth.example.test",
                "client_id": "client",
                "scopes": "openid offline_access"
            }));
        });

        let error = fetch(
            &HttpClient::new(),
            &server.base_url(),
            TransportPolicy::Strict,
        )
        .expect_err("a remote plain http issuer must not reach discovery");
        assert!(error.to_string().contains("plain http"), "{error}");
    }
}
