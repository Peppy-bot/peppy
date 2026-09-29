//! `GET {api_url}/cli/auth-config`: the public bootstrap endpoint that hands the CLI
//! the Zitadel `issuer`, the Native app `client_id`, the exact `scopes` string to
//! request (already including `offline_access` and the project-audience scope,
//! sent to Zitadel **verbatim**, never reassembled), and the address of the
//! platform's own device page, `device_verification_uri`.

use serde::Deserialize;
use url::Url;

use super::http::HttpClient;
use super::profile::{self, TransportPolicy};
use crate::error::{Error, Result};

/// What the backend serves to the CLI (no OIDC endpoint URLs; those come from
/// OIDC discovery against `issuer`). Every URL in it has passed the transport
/// policy.
#[derive(Debug, Clone)]
pub struct CliConfig {
    pub issuer: String,
    pub client_id: String,
    pub scopes: String,
    /// The platform's own device page, where a person approves a device login.
    /// `None` when the platform does not publish it (API contract older than
    /// 3.3.0); the login then falls back to the identity provider's address.
    pub device_verification_uri: Option<Url>,
}

/// The answer as it arrives on the wire. Unknown members are ignored, because
/// every contract minor may add one.
#[derive(Deserialize)]
struct CliConfigResponse {
    issuer: String,
    client_id: String,
    scopes: String,
    #[serde(default)]
    device_verification_uri: Option<String>,
}

/// Fetches `/cli/auth-config`. A `503` means the deployment hasn't provisioned the
/// CLI client yet (`PEPPY_CLI_CLIENT_ID` / `PEPPY_INTROSPECT_AUDIENCE` unset), or
/// does not know the address of its own device page.
/// Callers pass [`profile::build_transport_policy`]; the parameter exists so the
/// strict policy stays exercisable from tests in any build profile.
pub fn fetch(http: &HttpClient, api_url: &str, policy: TransportPolicy) -> Result<CliConfig> {
    let url = format!("{}/cli/auth-config", api_url.trim_end_matches('/'));
    let resp = http.get(&url, None)?;
    match resp.status {
        200 => {
            let response: CliConfigResponse = resp.json("/cli/auth-config")?;
            // The issuer is server supplied and every later step of the device
            // flow, up to and including the token exchange, is aimed at
            // whatever it names. Apply the transport policy here, at the point
            // it enters the process, rather than at each of those steps.
            profile::validate_https_or_local_with(&response.issuer, "OIDC issuer", policy)?;
            let device_verification_uri = response
                .device_verification_uri
                .as_deref()
                .map(|raw| parse_device_verification_uri(raw, api_url, policy))
                .transpose()?;
            Ok(CliConfig {
                issuer: response.issuer,
                client_id: response.client_id,
                scopes: response.scopes,
                device_verification_uri,
            })
        }
        503 => Err(Error::Auth(
            "CLI login isn't configured on this backend yet (the deployment hasn't provisioned the CLI client).".to_string(),
        )),
        s => Err(Error::Http(format!("GET {url} returned {s}"))),
    }
}

/// Parses the platform's device page address. A person types their login code
/// into whatever page this names, so it gets the same transport policy as the
/// issuer (https, or plain http to a loopback host only), and a platform served
/// over https cannot hand back a plain http page, not even a loopback one. The
/// CLI appends the `user_code` query itself, so an address that already
/// carries a query or a fragment is refused rather than merged.
fn parse_device_verification_uri(raw: &str, api_url: &str, policy: TransportPolicy) -> Result<Url> {
    const WHAT: &str = "platform device verification address";
    let address = profile::validate_https_or_local_with(raw, WHAT, policy)?;
    if address.query().is_some() || address.fragment().is_some() {
        return Err(Error::Auth(format!(
            "invalid {WHAT}: a query string or fragment is not allowed"
        )));
    }
    let api = Url::parse(api_url)
        .map_err(|e| Error::Auth(format!("invalid platform API `{api_url}`: {e}")))?;
    if api.scheme() == "https" && address.scheme() != "https" {
        return Err(Error::Auth(format!(
            "{WHAT} attempts an HTTPS to HTTP downgrade"
        )));
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;
    use serde_json::{Value, json};

    /// Serves `body` as the `/cli/auth-config` answer and fetches it under the
    /// strict policy, the one every release build uses.
    fn fetch_strict(body: Value) -> Result<CliConfig> {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/cli/auth-config");
            then.status(200).json_body(body);
        });
        fetch(
            &HttpClient::new(),
            &server.base_url(),
            TransportPolicy::Strict,
        )
    }

    /// A 3.3.0 answer from a loopback platform, with the device page address
    /// set to `address`.
    fn answer_with_address(address: &str) -> Value {
        json!({
            "issuer": "https://auth.example.test",
            "client_id": "client",
            "project_id": "project",
            "scopes": "openid offline_access",
            "device_verification_uri": address,
        })
    }

    #[test]
    fn rejects_an_insecure_server_supplied_issuer() {
        let error = fetch_strict(json!({
            "issuer": "http://auth.example.test",
            "client_id": "client",
            "scopes": "openid offline_access"
        }))
        .expect_err("a remote plain http issuer must not reach discovery");
        assert!(error.to_string().contains("plain http"), "{error}");
    }

    #[test]
    fn reads_an_https_device_verification_address() {
        let config = fetch_strict(answer_with_address("https://app.example.test/device"))
            .expect("an https device page is accepted");
        assert_eq!(
            config.device_verification_uri.map(String::from).as_deref(),
            Some("https://app.example.test/device")
        );
    }

    #[test]
    fn an_absent_device_verification_address_is_an_older_platform() {
        let config = fetch_strict(json!({
            "issuer": "https://auth.example.test",
            "client_id": "client",
            "scopes": "openid offline_access"
        }))
        .expect("a platform older than 3.3.0 still logs in");
        assert!(config.device_verification_uri.is_none());
    }

    #[test]
    fn refuses_a_plain_http_device_verification_address_on_a_remote_host() {
        let error = fetch_strict(answer_with_address("http://app.example.test/device"))
            .expect_err("a person must not type a code into a cleartext remote page");
        assert!(error.to_string().contains("plain http"), "{error}");
        assert!(
            error.to_string().contains("device verification address"),
            "{error}"
        );
    }

    #[test]
    fn admits_a_plain_http_device_verification_address_on_a_loopback_host() {
        for address in [
            "http://127.0.0.1:5173/device",
            "http://localhost:5173/device",
            "http://[::1]:5173/device",
        ] {
            let config = fetch_strict(answer_with_address(address))
                .unwrap_or_else(|e| panic!("{address} is a loopback page: {e}"));
            assert!(config.device_verification_uri.is_some(), "{address}");
        }
    }

    #[test]
    fn refuses_a_device_verification_address_that_carries_a_query_or_fragment() {
        for address in [
            "https://app.example.test/device?code=ABCD-EFGH",
            "https://app.example.test/device#fragment",
        ] {
            let error = fetch_strict(answer_with_address(address))
                .expect_err("the CLI appends the query itself");
            assert!(
                error.to_string().contains("query string or fragment"),
                "{address}: {error}"
            );
        }
    }

    #[test]
    fn an_https_platform_cannot_hand_back_a_plain_http_device_address() {
        let error = parse_device_verification_uri(
            "http://127.0.0.1:5173/device",
            "https://api.example.test",
            TransportPolicy::Strict,
        )
        .expect_err("an https platform cannot downgrade even to loopback http");
        assert!(error.to_string().contains("downgrade"), "{error}");
    }
}
