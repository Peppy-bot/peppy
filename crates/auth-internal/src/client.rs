//! Authenticated calls to the `platform-backend` resource server, typed at the
//! HTTP boundary. On a `401` the request is retried once after refreshing (and
//! persisting) the session token. Every other refusal is rendered from the
//! backend's `application/problem+json` body when it carries one, so the user
//! sees the platform's own `title` and `detail` (a quota reached, a malformed
//! request) rather than a bare status code.
//!
//! Response types deserialize tolerantly (unknown fields ignored) because the
//! CLI is installed on user machines while the backend deploys independently,
//! so an older CLI routinely meets a newer backend. Status-like fields the
//! backend enumerates (`RouterPeer::status`, `RouterStatus::phase`) are kept
//! as strings for the same reason: a value this CLI has not heard of renders
//! as itself instead of failing the whole listing.

use chrono::{DateTime, Utc};
use config::namespace::Namespace;
use pmi::RouterId;
use secrecy::ExposeSecret;
use serde::{Deserialize, de::DeserializeOwned};

use super::http::{HttpClient, HttpResponse};
use super::resolver::{Credential, SessionContext, refresh_and_persist};
use super::storage::{self, ProfileCreds};
use crate::error::{Error, Result};

/// The identity the backend reports for the current token (`GET /me`).
#[derive(Debug, Clone, Deserialize)]
pub struct Principal {
    pub sub: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
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

/// One workspace the caller belongs to (`GET /api/workspaces`).
#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tier: String,
}

/// One project in a workspace (`GET /api/workspace/{ws}/projects`). An
/// archived project carries `archived_at`; it owns no running router.
#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    #[serde(default)]
    pub archived_at: Option<DateTime<Utc>>,
}

impl Project {
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

/// One enrolled router peer of a project.
#[derive(Debug, Clone, Deserialize)]
pub struct RouterPeer {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub certificate_cn: String,
    /// `connected`, `unknown` or `pending_restart` as the platform reports it.
    pub status: String,
    pub certificate_expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// The one-time answer to an enrollment: the signed material and the identity
/// the daemon's router must run under. `zenoh_id` and `namespace` are parsed
/// here, at the boundary, so a value zenoh would refuse fails the enrollment
/// before anything is written.
#[derive(Debug, Clone, Deserialize)]
pub struct RouterPeerEnrolled {
    pub peer: RouterPeer,
    /// The leaf certificate, PEM.
    pub certificate: String,
    /// The issuing chain (peer issuer, then project CA), PEM.
    pub chain: String,
    /// The project CA the cloud router's certificate chains to, PEM.
    pub trust_anchor: String,
    pub zenoh_id: RouterId,
    pub namespace: Namespace,
    /// The peer config the platform rendered for this machine.
    pub zenoh_config: String,
}

/// Where a project's cloud router listens.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RouterAddress {
    pub host: String,
    pub port: u16,
}

/// The platform's view of one peer's connection.
#[derive(Debug, Clone, Deserialize)]
pub struct RouterPeerStatus {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// A project's cloud router (`GET .../router`, and the body of a staged peer
/// removal).
#[derive(Debug, Clone, Deserialize)]
pub struct RouterStatus {
    /// `provisioning`, `running`, `restarting`, `stopped` or `degraded`.
    pub phase: String,
    #[serde(default)]
    pub desired_state: String,
    #[serde(default)]
    pub address: Option<RouterAddress>,
    /// Whether a change (a removed peer, a new size) waits for a restart.
    #[serde(default)]
    pub pending_changes: bool,
    #[serde(default)]
    pub peers: Vec<RouterPeerStatus>,
}

/// What `DELETE .../router/peers/{id}` answered.
#[derive(Debug, Clone)]
pub enum PeerRemoval {
    /// The platform accepted the removal; it takes effect when the router
    /// restarts, and the returned status shows the pending change.
    Staged(RouterStatus),
    /// The platform holds no such peer any more.
    AlreadyRemoved,
}

/// `GET {api_url}/me`, refreshing once on a 401.
pub fn get_me(http: &HttpClient, api_url: &str, cred: &mut Credential) -> Result<Principal> {
    authed_get_json(http, &api_path(api_url, &["me"])?, cred)
}

/// `GET {api_url}/api/workspaces`.
pub fn list_workspaces(
    http: &HttpClient,
    api_url: &str,
    cred: &mut Credential,
) -> Result<Vec<Workspace>> {
    authed_get_json(http, &api_path(api_url, &["api", "workspaces"])?, cred)
}

/// `GET {api_url}/api/workspace/{workspace_id}/projects`.
pub fn list_projects(
    http: &HttpClient,
    api_url: &str,
    cred: &mut Credential,
    workspace_id: &str,
) -> Result<Vec<Project>> {
    authed_get_json(
        http,
        &api_path(api_url, &["api", "workspace", workspace_id, "projects"])?,
        cred,
    )
}

/// `POST {api_url}/api/workspace/{ws}/projects/{p}/router/peers` with the PEM
/// signing request. The platform answers `201` with the signed material.
pub fn enroll_peer(
    http: &HttpClient,
    api_url: &str,
    cred: &mut Credential,
    workspace_id: &str,
    project_id: &str,
    csr_pem: &str,
) -> Result<RouterPeerEnrolled> {
    let url = api_path(
        api_url,
        &[
            "api",
            "workspace",
            workspace_id,
            "projects",
            project_id,
            "router",
            "peers",
        ],
    )?;
    let body = serde_json::json!({ "csr": csr_pem }).to_string();
    let resp = authed(http, cred, |http, bearer| {
        http.post_json(&url, &body, Some(bearer))
    })?;
    match resp.status {
        201 => resp.json("router peer enrollment"),
        _ => Err(interpret_refusal(&resp, "POST", &url)),
    }
}

/// `GET {api_url}/api/workspace/{ws}/projects/{p}/router/peers`.
pub fn list_peers(
    http: &HttpClient,
    api_url: &str,
    cred: &mut Credential,
    workspace_id: &str,
    project_id: &str,
) -> Result<Vec<RouterPeer>> {
    authed_get_json(
        http,
        &api_path(
            api_url,
            &[
                "api",
                "workspace",
                workspace_id,
                "projects",
                project_id,
                "router",
                "peers",
            ],
        )?,
        cred,
    )
}

/// `DELETE {api_url}/api/workspace/{ws}/projects/{p}/router/peers/{peer_id}`.
/// A `404` is a definite answer (the peer is already gone), not an error.
pub fn remove_peer(
    http: &HttpClient,
    api_url: &str,
    cred: &mut Credential,
    workspace_id: &str,
    project_id: &str,
    peer_id: &str,
) -> Result<PeerRemoval> {
    let url = api_path(
        api_url,
        &[
            "api",
            "workspace",
            workspace_id,
            "projects",
            project_id,
            "router",
            "peers",
            peer_id,
        ],
    )?;
    let resp = authed(http, cred, |http, bearer| http.delete(&url, Some(bearer)))?;
    match resp.status {
        200 | 202 => Ok(PeerRemoval::Staged(resp.json("router peer removal")?)),
        404 => Ok(PeerRemoval::AlreadyRemoved),
        _ => Err(interpret_refusal(&resp, "DELETE", &url)),
    }
}

/// `GET {api_url}/api/workspace/{ws}/projects/{p}/router`.
pub fn router_status(
    http: &HttpClient,
    api_url: &str,
    cred: &mut Credential,
    workspace_id: &str,
    project_id: &str,
) -> Result<RouterStatus> {
    authed_get_json(
        http,
        &api_path(
            api_url,
            &[
                "api",
                "workspace",
                workspace_id,
                "projects",
                project_id,
                "router",
            ],
        )?,
        cred,
    )
}

/// Joins `segments` onto `api_url` as percent-encoded path segments, so an id
/// taken from a flag can never smuggle a slash or a query into the path.
fn api_path(api_url: &str, segments: &[&str]) -> Result<String> {
    let mut url = url::Url::parse(api_url)
        .map_err(|e| Error::Auth(format!("invalid platform API `{api_url}`: {e}")))?;
    url.path_segments_mut()
        .map_err(|_| {
            Error::Auth(format!(
                "invalid platform API `{api_url}`: cannot be a base"
            ))
        })?
        .pop_if_empty()
        .extend(segments);
    Ok(url.to_string())
}

/// Runs `request` with the current bearer and, on a `401`, refreshes (and
/// persists) the session token and retries exactly once.
fn authed(
    http: &HttpClient,
    cred: &mut Credential,
    request: impl Fn(&HttpClient, &str) -> Result<HttpResponse>,
) -> Result<HttpResponse> {
    let resp = request(http, cred.token.expose_secret())?;
    if resp.status != 401 {
        return Ok(resp);
    }
    refresh_in_place(http, cred)?;
    request(http, cred.token.expose_secret())
}

/// An authenticated `GET url` whose `200` body deserializes to `T`.
fn authed_get_json<T: DeserializeOwned>(
    http: &HttpClient,
    url: &str,
    cred: &mut Credential,
) -> Result<T> {
    let resp = authed(http, cred, |http, bearer| http.get(url, Some(bearer)))?;
    match resp.status {
        200 => resp.json(url),
        _ => Err(interpret_refusal(&resp, "GET", url)),
    }
}

/// The subset of an RFC 9457 problem document the CLI renders.
#[derive(Deserialize)]
struct Problem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    detail: Option<String>,
}

/// The error for a refused request, after the single reactive refresh the
/// `authed` callers already attempted: a `401` means the session is gone, and
/// anything else is rendered from the platform's problem body when it has one.
/// `502`/`503` without a body map to distinct ops-vs-token messages so an
/// outage is not mistaken for a bad token.
fn interpret_refusal(resp: &HttpResponse, method: &str, url: &str) -> Error {
    if resp.status == 401 {
        return Error::NotAuthenticated;
    }
    if let Ok(problem) = serde_json::from_str::<Problem>(&resp.body)
        && !problem.title.is_empty()
    {
        return Error::Http(match problem.detail.filter(|d| !d.is_empty()) {
            Some(detail) => format!("{}: {detail}", problem.title),
            None => problem.title,
        });
    }
    match resp.status {
        502 => Error::Auth(
            "the backend's introspection credentials were rejected (server-side problem)"
                .to_string(),
        ),
        503 => Error::Http("backend temporarily unavailable, try again shortly".to_string()),
        s => Error::Http(format!("{method} {url} returned {s}")),
    }
}

/// Refreshes a session credential in place via the shared refresh-and-persist
/// pipeline, then rebuilds the [`Credential`] from the rotated tokens.
fn refresh_in_place(http: &HttpClient, cred: &mut Credential) -> Result<()> {
    // Load the stored session to refresh from (it may have changed since the
    // credential was built, e.g. another command refreshed in parallel).
    let creds = storage::load(&cred.session.creds_path)?;
    let Some(pc) = creds.session.as_ref() else {
        return Ok(());
    };

    let updated = refresh_and_persist(http, &cred.session.creds_path, pc)?;

    cred.token = storage::secret(updated.access_token.expose_secret().to_string());
    cred.session = SessionContext {
        issuer: updated.issuer.clone(),
        client_id: updated.client_id.clone(),
        refresh_token: storage::secret(updated.refresh_token.expose_secret().to_string()),
        creds_path: cred.session.creds_path.clone(),
    };
    Ok(())
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

    #[test]
    fn api_paths_are_joined_as_encoded_segments() {
        assert_eq!(
            api_path("https://api.example.test/", &["api", "workspaces"]).unwrap(),
            "https://api.example.test/api/workspaces"
        );
        assert_eq!(
            api_path(
                "http://127.0.0.1:3000",
                &["api", "workspace", "ws/../x?y", "projects"]
            )
            .unwrap(),
            "http://127.0.0.1:3000/api/workspace/ws%2F..%2Fx%3Fy/projects"
        );
    }

    #[test]
    fn a_problem_body_is_rendered_as_title_and_detail() {
        let resp = HttpResponse {
            status: 422,
            body: r#"{"type":"https://peppy.bot/problems/peer-limit-reached",
                      "title":"Peer limit reached","status":422,
                      "detail":"the plan allows 5 peers per router"}"#
                .into(),
        };
        assert_eq!(
            interpret_refusal(&resp, "POST", "u").to_string(),
            "Peer limit reached: the plan allows 5 peers per router"
        );

        let bare = HttpResponse {
            status: 503,
            body: String::new(),
        };
        assert!(
            interpret_refusal(&bare, "GET", "u")
                .to_string()
                .contains("temporarily")
        );
        let gone = HttpResponse {
            status: 401,
            body: String::new(),
        };
        assert!(matches!(
            interpret_refusal(&gone, "GET", "u"),
            Error::NotAuthenticated
        ));
    }

    #[test]
    fn an_enrollment_response_rejects_an_id_zenoh_would_refuse() {
        let json = |zid: &str| {
            format!(
                r#"{{"peer":{{"id":"p","name":"n","certificate_cn":"n","status":"unknown",
                    "certificate_expires_at":"2027-01-01T00:00:00Z",
                    "created_at":"2026-10-03T00:00:00Z"}},
                    "certificate":"c","chain":"","trust_anchor":"t","zenoh_id":"{zid}",
                    "namespace":"550e8400-e29b-41d4-a716-446655440000","zenoh_config":"{{}}",
                    "some_future_field":1}}"#
            )
        };
        let ok: RouterPeerEnrolled = serde_json::from_str(&json("7f3a9c1e")).expect("parses");
        assert_eq!(ok.zenoh_id.as_str(), "7f3a9c1e");
        assert!(serde_json::from_str::<RouterPeerEnrolled>(&json("0abc")).is_err());
    }
}
