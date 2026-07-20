//! Authenticated calls to the `platform-backend` resource server: `GET /me`,
//! `POST /logout`, and `POST /me/cli/federation` (fetch the
//! shared router's connection config). On a `401` with a refreshable session credential the
//! request is retried once after refreshing (and persisting) the token; a `401`
//! on a PAT is a hard error (a PAT cannot be refreshed). `502`/`503` map to
//! distinct messages so an ops problem isn't mistaken for a bad token.

use config::namespace::Namespace;
use secrecy::ExposeSecret;
use serde::{Deserialize, Deserializer, de::DeserializeOwned};

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

/// The connection config the backend hands the CLI for the platform router.
/// Deserialized tolerantly (unknown fields ignored) so a
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
    /// The daemon's session namespace, deserialized directly from the backend's
    /// `workspace_id` (the platform's stable per-workspace `Uuid`). Typed at the
    /// HTTP boundary: an invalid workspace id fails the pull instead of leaking
    /// toward a live session, and everything Peppy-side speaks only `namespace`.
    #[serde(
        rename = "workspace_id",
        deserialize_with = "deserialize_workspace_namespace"
    )]
    pub namespace: Namespace,
}

/// Server-controlled result of enrolling or renewing one exact
/// `core_node_name`. All times are RFC 3339; the identity engine validates them
/// against the actual returned leaf before anything is activated.
#[derive(Debug, Clone, Deserialize)]
pub struct CoreNodeCertificateResponse {
    pub core_node_name: String,
    pub workspace_id: String,
    pub certificate_chain_pem: String,
    pub serial_number: String,
    pub not_before: String,
    pub not_after: String,
    pub renew_after: String,
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
    let path = "/me/cli/federation";
    let url = format!("{}{}", api_url.trim_end_matches('/'), path);
    let response = authed_post(http, &url, &body, cred)?;
    if response.status == 409 {
        #[derive(Deserialize)]
        struct ConflictCode {
            error: String,
        }
        #[derive(Deserialize)]
        struct ConflictBody {
            #[serde(default, deserialize_with = "deserialize_optional_workspace_namespace")]
            workspace_id: Option<Namespace>,
        }

        if serde_json::from_str::<ConflictCode>(&response.body)
            .is_ok_and(|conflict| conflict.error == "core_node_workspace_mismatch")
        {
            let conflict =
                serde_json::from_str::<ConflictBody>(&response.body).map_err(|error| {
                    Error::Http(format!(
                        "POST {url} returned a malformed workspace-mismatch conflict: {error}"
                    ))
                })?;
            let current = conflict.workspace_id.ok_or_else(|| {
                Error::Http(format!(
                    "POST {url} returned a workspace-mismatch conflict without a valid workspace_id"
                ))
            })?;
            return Err(Error::DiscoveryWorkspaceMismatch { current });
        }
    }
    interpret_authed_json(response, cred, "POST", &url, path)
}

/// Parses the backend's workspace UUID into the namespace type used by the
/// daemon. Namespace syntax alone is intentionally broader than a UUID, so
/// enforce the canonical lower-case hyphenated representation at this trust
/// boundary to prevent two textual aliases for one workspace.
pub(crate) fn parse_workspace_id(raw: &str) -> Result<Namespace> {
    let parsed = uuid::Uuid::parse_str(raw)
        .map_err(|error| Error::Auth(format!("invalid workspace_id {raw:?}: {error}")))?;
    if parsed.hyphenated().to_string() != raw {
        return Err(Error::Auth(format!(
            "workspace_id {raw:?} is not a canonical lower-case hyphenated UUID"
        )));
    }
    Namespace::parse(raw)
        .map_err(|error| Error::Auth(format!("invalid workspace_id {raw:?}: {error}")))
}

fn deserialize_workspace_namespace<'de, D>(
    deserializer: D,
) -> std::result::Result<Namespace, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse_workspace_id(&raw).map_err(serde::de::Error::custom)
}

fn deserialize_optional_workspace_namespace<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Namespace>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|raw| parse_workspace_id(&raw).map_err(serde::de::Error::custom))
        .transpose()
}

/// Enrolls or renews a per-core-node mTLS certificate. The private key never
/// crosses this boundary: the request contains only the fixed daemon name and
/// a proof-of-possession PKCS#10 CSR.
pub fn enroll_core_node_certificate(
    http: &HttpClient,
    api_url: &str,
    cred: &mut Credential,
    core_node_name: &str,
    csr_pem: &str,
) -> Result<CoreNodeCertificateResponse> {
    let url = format!(
        "{}/me/cli/core-node-certificates",
        api_url.trim_end_matches('/')
    );
    let body = serde_json::json!({
        "core_node_name": core_node_name,
        "csr_pem": csr_pem,
    })
    .to_string();
    let response = authed_post(http, &url, &body, cred)?;
    match response.status {
        200 => response.json("/me/cli/core-node-certificates"),
        401 => Err(unauthorized_error(cred)),
        409 => {
            #[derive(Deserialize)]
            struct ConflictBody {
                error: String,
            }
            let code = serde_json::from_str::<ConflictBody>(&response.body)
                .map(|body| body.error)
                .unwrap_or_default();
            match code.as_str() {
                "core_node_name_taken" => {
                    Err(Error::CoreNodeNameTaken(core_node_name.to_string()))
                }
                "core_node_revoked" => Err(Error::CoreNodeRevoked(core_node_name.to_string())),
                "core_node_key_already_used" => {
                    Err(Error::CoreNodeKeyAlreadyUsed(core_node_name.to_string()))
                }
                _ => Err(Error::Auth(format!(
                    "core-node certificate enrollment for `{core_node_name}` was rejected with an unknown conflict; update Peppy or contact platform support"
                ))),
            }
        }
        422 => Err(Error::Auth(
            "the platform rejected the generated core-node certificate request".into(),
        )),
        429 => Err(Error::Auth(
            "core-node certificate enrollment quota or rate limit reached; try again later".into(),
        )),
        503 => Err(Error::Http(
            "core-node certificate issuer temporarily unavailable; the existing valid identity was left in place"
                .into(),
        )),
        status => Err(Error::Http(format!(
            "POST {url} returned {status} while enrolling the core-node certificate"
        ))),
    }
}

/// Revokes the active enrollment for an owned core-node name. The caller invokes
/// this before `/logout`, while the bearer can still authorize the deletion.
pub fn delete_core_node_certificate(
    http: &HttpClient,
    api_url: &str,
    cred: &mut Credential,
    core_node_name: &str,
) -> Result<u16> {
    let encoded_name: String =
        url::form_urlencoded::byte_serialize(core_node_name.as_bytes()).collect();
    let url = format!(
        "{}/me/cli/core-node-certificates/{encoded_name}",
        api_url.trim_end_matches('/')
    );
    let response = authed_delete(http, &url, cred)?;
    Ok(response.status)
}

/// `POST {api_url}/logout` with the exact current credential. Returns the status
/// code so the caller can decide what to print; never refreshes (the token is
/// being thrown away regardless). Origin policy and the persisted-session CAS
/// are still enforced immediately before I/O, just like every other bearer
/// request.
pub fn logout(http: &HttpClient, api_url: &str, credential: &Credential) -> Result<u16> {
    let url = format!("{}/logout", api_url.trim_end_matches('/'));
    ensure_credential_origin(&url, credential)?;
    let resp = http.post_empty(&url, Some(credential.token.expose_secret()))?;
    Ok(resp.status)
}

/// A GET that, on 401 with a session credential, refreshes (and persists) the
/// token and retries exactly once.
fn authed_get(http: &HttpClient, url: &str, cred: &mut Credential) -> Result<HttpResponse> {
    ensure_credential_origin(url, cred)?;
    let resp = http.get(url, Some(cred.token.expose_secret()))?;
    if resp.status == 401 && cred.is_refreshable() {
        refresh_in_place(http, cred)?;
        // Refresh persistence is a concurrency boundary: a fresh login may
        // replace this session after the CAS succeeds but before the retry.
        ensure_credential_origin(url, cred)?;
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
    ensure_credential_origin(url, cred)?;
    let resp = http.post_json(url, body, Some(cred.token.expose_secret()))?;
    if resp.status == 401 && cred.is_refreshable() {
        refresh_in_place(http, cred)?;
        ensure_credential_origin(url, cred)?;
        return http.post_json(url, body, Some(cred.token.expose_secret()));
    }
    Ok(resp)
}

/// A bodyless DELETE that follows the same one-refresh-on-401 contract as the
/// authenticated GET/POST helpers.
fn authed_delete(http: &HttpClient, url: &str, cred: &mut Credential) -> Result<HttpResponse> {
    ensure_credential_origin(url, cred)?;
    let response = http.delete_empty(url, Some(cred.token.expose_secret()))?;
    if response.status == 401 && cred.is_refreshable() {
        refresh_in_place(http, cred)?;
        ensure_credential_origin(url, cred)?;
        return http.delete_empty(url, Some(cred.token.expose_secret()));
    }
    Ok(response)
}

/// OAuth access tokens are resource-server credentials, not ambient secrets.
/// Refuse before I/O when a caller combines a cached session with a different
/// `--api-url`/configuration origin. PATs remain explicitly ambient and are
/// bound by the caller-selected API for that operation.
fn ensure_credential_origin(url: &str, cred: &Credential) -> Result<()> {
    let CredentialKind::Session(ctx) = &cred.kind else {
        return Ok(());
    };
    let expected = crate::profile::normalize_api_origin(&ctx.api_url).map_err(|error| {
        Error::Auth(format!(
            "stored platform session has an invalid API origin: {error}"
        ))
    })?;
    let actual = crate::profile::normalize_api_origin(url)?;
    if expected != actual {
        return Err(Error::Auth(format!(
            "refusing to send a cached OAuth bearer for {expected} to {actual}; log in to the requested platform API first"
        )));
    }
    // Origin equality alone is insufficient: another account can log in to
    // the same API/issuer while this request is in flight. Require the exact
    // persisted session/token context immediately before bearer transmission.
    super::resolver::ensure_session_credential_current(cred)?;
    Ok(())
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

    // Compare-and-swap against the exact session this request started with.
    // A concurrent login/logout/refresh must never donate its bearer to this
    // stale request, even when both sessions happen to use the same issuer.
    let pc =
        super::resolver::ensure_session_credential_current(cred)?.ok_or(Error::NotAuthenticated)?;
    let updated = refresh_and_persist(http, &ctx.creds_path, &pc)?;

    cred.token = storage::secret(updated.access_token.expose_secret().to_string());
    cred.kind = CredentialKind::Session(super::resolver::SessionContext {
        session_revision: updated.session_revision,
        api_url: updated.api_url.clone(),
        issuer: updated.issuer.clone(),
        client_id: updated.client_id.clone(),
        subject: updated.subject.clone(),
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
        uuid::Uuid::new_v4(),
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

    #[test]
    fn every_fresh_login_gets_a_distinct_opaque_session_revision() {
        let cfg = super::super::cli_config::CliConfig {
            issuer: "https://issuer.example".into(),
            client_id: "cli-client".into(),
            scopes: "openid offline_access".into(),
        };
        let tokens = super::super::device::TokenSet {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 1_000,
            token_type: "Bearer".into(),
            scope: "openid".into(),
        };

        let first = creds_from_login(&cfg, "https://api.example/", &tokens);
        let second = creds_from_login(&cfg, "https://api.example/", &tokens);

        assert_ne!(first.session_revision, second.session_revision);
        assert!(!first.session_revision.is_nil());
        assert_eq!(first.api_url, "https://api.example");
    }

    #[test]
    fn router_config_parses_tolerantly() {
        // A backend that adds an unknown field still deserializes, and the wire
        // `workspace_id` lands in the Peppy-side `namespace` field.
        let json = r#"{
            "endpoint": "tls/abc.zenoh.localhost:7443",
            "protocol": "tls",
            "mode": "client",
            "reconnect_after_secs": 3000,
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            "some_future_field": "ignored"
        }"#;
        let cfg: ZenohRouterConfig = serde_json::from_str(json).expect("tolerant parse");
        assert_eq!(cfg.endpoint, "tls/abc.zenoh.localhost:7443");
        assert_eq!(cfg.protocol, "tls");
        assert_eq!(cfg.reconnect_after_secs, 3000);
        assert_eq!(
            cfg.namespace.as_str(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn router_config_rejects_an_invalid_workspace_id() {
        // Fail-closed one layer early: a workspace id that is not a valid zenoh
        // namespace fails the HTTP parse and can never reach the federation gate.
        let json = r#"{
            "endpoint": "tls/abc.zenoh.localhost:7443",
            "protocol": "tls",
            "reconnect_after_secs": 3000,
            "workspace_id": "**"
        }"#;
        assert!(serde_json::from_str::<ZenohRouterConfig>(json).is_err());
    }
}
