//! Authenticated calls to the `platform-backend` resource server: `GET /me`,
//! `POST /logout`, and `POST /me/cli/federation` (fetch the
//! shared router's connection config). On a `401` with a refreshable session credential the
//! request is retried once after refreshing (and persisting) the token; a `401`
//! on a PAT is a hard error (a PAT cannot be refreshed). `502`/`503` map to
//! distinct messages so an ops problem isn't mistaken for a bad token.

use secrecy::ExposeSecret;
use serde::{Deserialize, de::DeserializeOwned};

use super::http::{HttpClient, HttpResponse};
use super::resolver::{Credential, CredentialKind, refresh_and_persist};
use super::storage::{self, ProfileCreds};
use crate::error::{Error, Result};

/// The identity the backend reports for the current token. Deserialized
/// tolerantly: only `sub` is required, everything else is optional so a backend
/// that adds fields (or omits an optional one) still parses.
#[derive(Debug, Clone, Deserialize)]
pub struct Principal {
    #[serde(default)]
    pub id: Option<String>,
    pub sub: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub owner_principal_id: Option<String>,
}

impl Principal {
    /// A human label for `whoami` / login confirmation: username, else email,
    /// else the subject.
    pub fn display_name(&self) -> &str {
        self.username
            .as_deref()
            .or(self.email.as_deref())
            .unwrap_or(&self.sub)
    }
}

/// `GET {api_url}/me`, refreshing once on a 401 for session credentials.
pub fn get_me(http: &HttpClient, api_url: &str, cred: &mut Credential) -> Result<Principal> {
    authed_get_json(http, api_url, "/me", cred)
}

/// The connection config the backend hands the CLI for the caller's private
/// per-user zenoh router. Deserialized tolerantly (unknown fields ignored) so a
/// backend that adds fields still parses. The CA the router is validated against
/// is **not** part of this response; it is CLI-side deployment config (the
/// trust root the gateway's routers present a cert chained to).
#[derive(Debug, Clone, Deserialize)]
pub struct ZenohRouterConfig {
    /// The Zenoh locator to dial, `<scheme>/<host>:<port>`, e.g.
    /// `tls/7f3a….zenoh.localhost:7443`. The host is the capability subdomain
    /// (the SNI the gateway routes on); TLS terminates at the user's router.
    pub endpoint: String,
    /// Transport scheme, `"tls"` today.
    pub protocol: String,
    /// How long this config may be reused before re-resolving it. A cache-freshness
    /// hint only: the backend now actively health-checks the daemon, so reusing a
    /// still-fresh config (rather than re-pulling) never risks the router being torn
    /// down.
    pub reconnect_after_secs: u64,
    /// The caller's organization id (the platform's stable per-user `Uuid`, as a
    /// string). Becomes the daemon's session namespace so robots of the same org
    /// interoperate across the federation while different orgs stay routing-isolated.
    /// Required: a backend that predates this field fails to parse, which is the
    /// intended clean break (re-run `peppy platform login`).
    pub organization_id: String,
}

impl ZenohRouterConfig {
    /// Splits this config's `<scheme>/<host>:<port>` locator into the
    /// `(host, port)` a TLS client dials. Thin wrapper over [`split_locator`].
    pub fn host_port(&self) -> Result<(String, u16)> {
        split_locator(&self.endpoint)
    }
}

/// Splits a `<scheme>/<host>:<port>` Zenoh locator into the `(host, port)` a TLS
/// client dials. The host doubles as the SNI the gateway routes on and the name
/// the router certificate is validated against. A scheme prefix is optional; a
/// missing/invalid `host:port` is a hard error. Shared by the live response and
/// the cached endpoint string.
pub fn split_locator(endpoint: &str) -> Result<(String, u16)> {
    let after_scheme = endpoint
        .split_once('/')
        .map(|(_scheme, rest)| rest)
        .unwrap_or(endpoint);
    let (host, port) = after_scheme.rsplit_once(':').ok_or_else(|| {
        Error::Auth(format!(
            "malformed router endpoint {endpoint:?}: expected `<scheme>/<host>:<port>`"
        ))
    })?;
    if host.is_empty() {
        return Err(Error::Auth(format!(
            "malformed router endpoint {endpoint:?}: empty host"
        )));
    }
    let port: u16 = port.parse().map_err(|_| {
        Error::Auth(format!(
            "malformed router endpoint {endpoint:?}: invalid port {port:?}"
        ))
    })?;
    Ok((host.to_string(), port))
}

/// `POST {api_url}/me/cli/federation`: fetch the shared router's
/// connection config (the daemon's discovery point), refreshing the access token
/// once on a 401 for session credentials (the same reactive-refresh contract as
/// [`get_me`]). The
/// body always carries the daemon's core-node name — the backend requires it and
/// upserts the name into its per-principal core-node registry (its `last_seen_at`
/// tracks config pulls, not liveness). The daemon dials the returned endpoint
/// over mTLS, presenting its client certificate.
pub fn establish_federation(
    http: &HttpClient,
    api_url: &str,
    cred: &mut Credential,
    core_node_name: &str,
) -> Result<ZenohRouterConfig> {
    let body = serde_json::json!({ "core_node_name": core_node_name }).to_string();
    authed_post_json(http, api_url, "/me/cli/federation", &body, cred)
}

/// `POST {api_url}/logout` with the current access token. Returns the status code
/// so the caller can decide what to print; never refreshes (the token is being
/// thrown away regardless).
pub fn logout(http: &HttpClient, api_url: &str, access_token: &str) -> Result<u16> {
    let url = format!("{}/logout", api_url.trim_end_matches('/'));
    let resp = http.post_empty(&url, Some(access_token))?;
    Ok(resp.status)
}

/// A GET that, on 401 with a session credential, refreshes (and persists) the
/// token and retries exactly once.
fn authed_get(http: &HttpClient, url: &str, cred: &mut Credential) -> Result<HttpResponse> {
    let resp = http.get(url, Some(cred.token.expose_secret()))?;
    if resp.status == 401 && cred.is_refreshable() {
        refresh_in_place(http, cred)?;
        return http.get(url, Some(cred.token.expose_secret()));
    }
    Ok(resp)
}

/// A JSON POST that, on 401 with a session credential, refreshes (and persists)
/// the token and retries exactly once with the same body.
fn authed_post(
    http: &HttpClient,
    url: &str,
    body: &str,
    cred: &mut Credential,
) -> Result<HttpResponse> {
    let resp = http.post_json(url, body, Some(cred.token.expose_secret()))?;
    if resp.status == 401 && cred.is_refreshable() {
        refresh_in_place(http, cred)?;
        return http.post_json(url, body, Some(cred.token.expose_secret()));
    }
    Ok(resp)
}

/// An authenticated `GET {api_url}{path}` whose `200` body deserializes to `T`.
/// See [`interpret_authed_json`] for the shared status contract.
fn authed_get_json<T: DeserializeOwned>(
    http: &HttpClient,
    api_url: &str,
    path: &str,
    cred: &mut Credential,
) -> Result<T> {
    let url = format!("{}{}", api_url.trim_end_matches('/'), path);
    let resp = authed_get(http, &url, cred)?;
    interpret_authed_json(resp, cred, "GET", &url, path)
}

/// An authenticated `POST {api_url}{path}` with a JSON `body` whose `200` response
/// deserializes to `T`. See [`interpret_authed_json`] for the shared status
/// contract.
fn authed_post_json<T: DeserializeOwned>(
    http: &HttpClient,
    api_url: &str,
    path: &str,
    body: &str,
    cred: &mut Credential,
) -> Result<T> {
    let url = format!("{}{}", api_url.trim_end_matches('/'), path);
    let resp = authed_post(http, &url, body, cred)?;
    interpret_authed_json(resp, cred, "POST", &url, path)
}

/// The status contract every authenticated `/me*` JSON endpoint shares: a `200`
/// body deserializes to `T`, a `401` becomes the credential-specific auth error
/// (after the single reactive refresh the `authed_*` callers already attempt),
/// `502`/`503` map to distinct ops-vs-token messages, and any other status to a
/// generic HTTP error. `path` doubles as the deserialization context label, so a
/// new authed endpoint is one line, not a copied block.
fn interpret_authed_json<T: DeserializeOwned>(
    resp: HttpResponse,
    cred: &Credential,
    method: &str,
    url: &str,
    path: &str,
) -> Result<T> {
    match resp.status {
        200 => resp.json(path),
        401 => Err(unauthorized_error(cred)),
        502 => Err(Error::Auth(
            "the backend's introspection credentials were rejected (server-side problem)"
                .to_string(),
        )),
        503 => Err(Error::Http(
            "backend temporarily unavailable, try again shortly".to_string(),
        )),
        s => Err(Error::Http(format!("{method} {url} returned {s}"))),
    }
}

/// Refreshes a session credential in place via the shared refresh-and-persist
/// pipeline, then rebuilds the [`Credential`] from the rotated tokens.
fn refresh_in_place(http: &HttpClient, cred: &mut Credential) -> Result<()> {
    let CredentialKind::Session(ctx) = &cred.kind else {
        return Ok(());
    };

    // Load the stored session to refresh from (it may have changed since the
    // credential was built, e.g. another command refreshed in parallel).
    let creds = storage::load(&ctx.creds_path)?;
    let Some(pc) = creds.session.as_ref() else {
        return Ok(());
    };

    let updated = refresh_and_persist(http, &ctx.creds_path, pc)?;

    cred.token = storage::secret(updated.access_token.expose_secret().to_string());
    cred.kind = CredentialKind::Session(super::resolver::SessionContext {
        issuer: updated.issuer.clone(),
        client_id: updated.client_id.clone(),
        refresh_token: storage::secret(updated.refresh_token.expose_secret().to_string()),
        creds_path: ctx.creds_path.clone(),
    });
    Ok(())
}

/// The 401 message, specialized by credential kind.
fn unauthorized_error(cred: &Credential) -> Error {
    match cred.kind {
        CredentialKind::Pat => {
            Error::Auth("API key rejected (revoked or expired?), cannot refresh".to_string())
        }
        CredentialKind::Session(_) => Error::NotAuthenticated,
    }
}

/// Builds the [`ProfileCreds`] to persist after a fresh login.
pub fn creds_from_login(
    cfg: &super::cli_config::CliConfig,
    api_url: &str,
    tokens: &super::device::TokenSet,
) -> ProfileCreds {
    ProfileCreds::with_tokens(
        api_url.trim_end_matches('/').to_string(),
        cfg.issuer.clone(),
        cfg.client_id.clone(),
        String::new(),
        String::new(),
        tokens,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(endpoint: &str) -> ZenohRouterConfig {
        ZenohRouterConfig {
            endpoint: endpoint.to_string(),
            protocol: "tls".to_string(),
            reconnect_after_secs: 3000,
            organization_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        }
    }

    #[test]
    fn host_port_splits_a_tls_locator() {
        let (host, port) = cfg("tls/7f3a.zenoh.localhost:7443")
            .host_port()
            .expect("valid locator");
        assert_eq!(host, "7f3a.zenoh.localhost");
        assert_eq!(port, 7443);
    }

    #[test]
    fn host_port_tolerates_a_missing_scheme() {
        let (host, port) = cfg("cap.zenoh.localhost:7443")
            .host_port()
            .expect("scheme is optional");
        assert_eq!(host, "cap.zenoh.localhost");
        assert_eq!(port, 7443);
    }

    #[test]
    fn host_port_rejects_a_missing_port() {
        assert!(cfg("tls/cap.zenoh.localhost").host_port().is_err());
    }

    #[test]
    fn host_port_rejects_a_non_numeric_port() {
        assert!(cfg("tls/cap.zenoh.localhost:https").host_port().is_err());
    }

    #[test]
    fn host_port_rejects_an_empty_host() {
        assert!(cfg("tls/:7443").host_port().is_err());
    }

    #[test]
    fn router_config_parses_tolerantly() {
        // A backend that adds an unknown field still deserializes.
        let json = r#"{
            "endpoint": "tls/abc.zenoh.localhost:7443",
            "protocol": "tls",
            "mode": "client",
            "reconnect_after_secs": 3000,
            "organization_id": "550e8400-e29b-41d4-a716-446655440000",
            "some_future_field": "ignored"
        }"#;
        let cfg: ZenohRouterConfig = serde_json::from_str(json).expect("tolerant parse");
        assert_eq!(cfg.protocol, "tls");
        assert_eq!(cfg.reconnect_after_secs, 3000);
        assert_eq!(cfg.organization_id, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(
            cfg.host_port().unwrap(),
            ("abc.zenoh.localhost".to_string(), 7443)
        );
    }
}
