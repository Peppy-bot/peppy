//! Authenticated calls to the `platform-backend` resource server, typed at the
//! HTTP boundary. On a `401` the request is retried once after refreshing (and
//! persisting) the session token. Every other refusal is rendered from the
//! backend's `application/problem+json` body when it carries one, so the user
//! sees the platform's own `title` and `detail` (a quota reached, a malformed
//! request) rather than a bare status code.
//!
//! Response types deserialize tolerantly (unknown fields ignored) because the
//! CLI is installed on user machines while the backend deploys independently,
//! so an older CLI routinely meets a newer backend. The platform adds members
//! to its answers in each minor version of the contract, so no type here
//! refuses an unknown member, and no answer is checked against a schema. The
//! values the backend enumerates ([`PeerStatus`], [`RouterPhase`]) are parsed
//! to enums that keep a value this CLI has not heard of as `Other`, so it
//! renders as itself instead of failing the whole listing.

use std::fmt;

use chrono::{DateTime, Utc};
use config::namespace::Namespace;
use pmi::RouterId;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::http::{HttpClient, HttpResponse};
use super::resolver::{Credential, SessionContext, refresh_and_persist};
use super::storage::{self, ProfileCreds};
use crate::enrollment::RouterEndpoint;
use crate::error::{Error, Problem, ProblemKind, Result};

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

/// Whether the router sees a peer connected, as the platform reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(from = "String", into = "String")]
pub enum PeerStatus {
    Connected,
    /// The router has not reported on the peer.
    Unknown,
    /// The peer was removed; the router refuses it from its next restart.
    PendingRestart,
    /// A status this CLI has not heard of, as the platform sent it.
    Other(String),
}

impl PeerStatus {
    /// The status as the platform spells it.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Connected => "connected",
            Self::Unknown => "unknown",
            Self::PendingRestart => "pending_restart",
            Self::Other(status) => status,
        }
    }
}

impl From<String> for PeerStatus {
    fn from(status: String) -> Self {
        match status.as_str() {
            "connected" => Self::Connected,
            "unknown" => Self::Unknown,
            "pending_restart" => Self::PendingRestart,
            _ => Self::Other(status),
        }
    }
}

impl From<PeerStatus> for String {
    fn from(status: PeerStatus) -> Self {
        status.as_str().to_string()
    }
}

impl fmt::Display for PeerStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a project's cloud router is doing, as the platform reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(from = "String", into = "String")]
pub enum RouterPhase {
    Provisioning,
    Running,
    Restarting,
    /// The person stopped the router.
    Stopped,
    Degraded,
    /// A phase this CLI has not heard of, as the platform sent it.
    Other(String),
}

impl RouterPhase {
    /// The phase as the platform spells it.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Stopped => "stopped",
            Self::Degraded => "degraded",
            Self::Other(phase) => phase,
        }
    }
}

impl From<String> for RouterPhase {
    fn from(phase: String) -> Self {
        match phase.as_str() {
            "provisioning" => Self::Provisioning,
            "running" => Self::Running,
            "restarting" => Self::Restarting,
            "stopped" => Self::Stopped,
            "degraded" => Self::Degraded,
            _ => Self::Other(phase),
        }
    }
}

impl From<RouterPhase> for String {
    fn from(phase: RouterPhase) -> Self {
        phase.as_str().to_string()
    }
}

impl fmt::Display for RouterPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One enrolled router peer of a project.
#[derive(Debug, Clone, Deserialize)]
pub struct RouterPeer {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub certificate_cn: String,
    pub status: PeerStatus,
    pub certificate_expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// The answer to an enrollment and to each renewal: the signed material and
/// the identity the daemon's router must run under. `zenoh_id`, `namespace`
/// and `address` are parsed here, at the boundary, so a value zenoh would
/// refuse fails the call before anything is written.
#[derive(Debug, Clone, Deserialize)]
pub struct RouterPeerEnrolled {
    pub peer: RouterPeer,
    /// Where the project's cloud router answers when it runs.
    pub address: RouterEndpoint,
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

/// The platform's view of one peer's connection.
#[derive(Debug, Clone, Deserialize)]
pub struct RouterPeerStatus {
    pub id: String,
    pub status: PeerStatus,
}

/// One change that waits for a router restart to apply.
#[derive(Debug, Clone, Deserialize)]
pub struct PendingChange {
    /// The platform's sentence for the person.
    pub description: String,
    pub staged_at: DateTime<Utc>,
    /// When the platform restarts the router itself, if it set a date.
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,
}

/// A project's cloud router (`GET .../router`, and the body of a staged peer
/// removal, a restart and a start).
#[derive(Debug, Clone, Deserialize)]
pub struct RouterStatus {
    pub phase: RouterPhase,
    #[serde(default)]
    pub desired_state: String,
    /// Where the router answers, present only while it runs.
    #[serde(default)]
    pub address: Option<RouterEndpoint>,
    /// Whether a change (a removed peer, a new size) waits for a restart.
    #[serde(default)]
    pub pending_changes: bool,
    /// Every change that waits for a restart, oldest first.
    #[serde(default)]
    pub pending_change_entries: Vec<PendingChange>,
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

/// The platform API as the signed-in person calls it: the HTTP client, the
/// base URL of the backend, and the bearer of the session. On a `401` the
/// request is retried once after the bearer is refreshed (and persisted) in
/// place.
pub struct PlatformApi {
    http: HttpClient,
    api_url: String,
    cred: Credential,
}

impl PlatformApi {
    pub fn new(http: &HttpClient, api_url: &str, cred: Credential) -> Self {
        Self {
            http: http.clone(),
            api_url: api_url.to_string(),
            cred,
        }
    }

    /// `GET {api_url}/me`.
    pub fn get_me(&mut self) -> Result<Principal> {
        self.get_json(&["me"])
    }

    /// `GET {api_url}/api/workspaces`.
    pub fn list_workspaces(&mut self) -> Result<Vec<Workspace>> {
        self.get_json(&["api", "workspaces"])
    }

    /// `GET {api_url}/api/workspace/{workspace_id}/projects`.
    pub fn list_projects(&mut self, workspace_id: &str) -> Result<Vec<Project>> {
        self.get_json(&["api", "workspace", workspace_id, "projects"])
    }

    /// `POST {api_url}/api/workspace/{ws}/projects/{p}/router/peers` with the
    /// PEM signing request. The platform answers `201` with the signed
    /// material.
    pub fn enroll_peer(
        &mut self,
        workspace_id: &str,
        project_id: &str,
        csr_pem: &str,
    ) -> Result<RouterPeerEnrolled> {
        let url = router_path(&self.api_url, workspace_id, project_id, &["peers"])?;
        let body = serde_json::json!({ "csr": csr_pem }).to_string();
        let resp = self.authed(|http, bearer| http.post_json(&url, &body, Some(bearer)))?;
        match resp.status {
            201 => resp.json("router peer enrollment"),
            _ => Err(interpret_refusal(&resp, "POST", &url)),
        }
    }

    /// `POST {api_url}/api/workspace/{ws}/projects/{p}/router/peers/{peer_id}/renew`
    /// with no body. The platform signs again the request the peer enrolled
    /// with, so the answer carries a new leaf for the enrolled key, and the
    /// identity of the peer stays as it is.
    pub fn renew_peer(
        &mut self,
        workspace_id: &str,
        project_id: &str,
        peer_id: &str,
    ) -> Result<RouterPeerEnrolled> {
        let url = router_path(
            &self.api_url,
            workspace_id,
            project_id,
            &["peers", peer_id, "renew"],
        )?;
        let resp = self.authed(|http, bearer| http.post_empty(&url, Some(bearer)))?;
        match resp.status {
            200 => resp.json("router peer renewal"),
            _ => Err(interpret_refusal(&resp, "POST", &url)),
        }
    }

    /// `GET {api_url}/api/workspace/{ws}/projects/{p}/router/peers`.
    pub fn list_peers(&mut self, workspace_id: &str, project_id: &str) -> Result<Vec<RouterPeer>> {
        let url = router_path(&self.api_url, workspace_id, project_id, &["peers"])?;
        self.get_json_at(&url)
    }

    /// `DELETE {api_url}/api/workspace/{ws}/projects/{p}/router/peers/{peer_id}`.
    /// A `404` is a definite answer (the peer is already gone), not an error.
    pub fn remove_peer(
        &mut self,
        workspace_id: &str,
        project_id: &str,
        peer_id: &str,
    ) -> Result<PeerRemoval> {
        let url = router_path(&self.api_url, workspace_id, project_id, &["peers", peer_id])?;
        let resp = self.authed(|http, bearer| http.delete(&url, Some(bearer)))?;
        match resp.status {
            200 | 202 => Ok(PeerRemoval::Staged(resp.json("router peer removal")?)),
            404 => Ok(PeerRemoval::AlreadyRemoved),
            _ => Err(interpret_refusal(&resp, "DELETE", &url)),
        }
    }

    /// `GET {api_url}/api/workspace/{ws}/projects/{p}/router`.
    pub fn router_status(&mut self, workspace_id: &str, project_id: &str) -> Result<RouterStatus> {
        let url = router_path(&self.api_url, workspace_id, project_id, &[])?;
        self.get_json_at(&url)
    }

    /// `POST {api_url}/api/workspace/{ws}/projects/{p}/router/restart`. The
    /// platform answers `202` with the router, now restarting. A restart
    /// applies every pending change and drops each peer link for a moment.
    pub fn restart_router(&mut self, workspace_id: &str, project_id: &str) -> Result<RouterStatus> {
        self.router_action(workspace_id, project_id, "restart")
    }

    /// `POST {api_url}/api/workspace/{ws}/projects/{p}/router/start`. The
    /// platform answers `202` with the router and the phase it moves to.
    pub fn start_router(&mut self, workspace_id: &str, project_id: &str) -> Result<RouterStatus> {
        self.router_action(workspace_id, project_id, "start")
    }

    /// A bodyless `POST` on the router that the platform accepts with `202`
    /// and the router's status.
    fn router_action(
        &mut self,
        workspace_id: &str,
        project_id: &str,
        action: &str,
    ) -> Result<RouterStatus> {
        let url = router_path(&self.api_url, workspace_id, project_id, &[action])?;
        let resp = self.authed(|http, bearer| http.post_empty(&url, Some(bearer)))?;
        match resp.status {
            200 | 202 => resp.json("router status"),
            _ => Err(interpret_refusal(&resp, "POST", &url)),
        }
    }

    /// An authenticated `GET` of `segments` under the API whose `200` body
    /// deserializes to `T`.
    fn get_json<T: DeserializeOwned>(&mut self, segments: &[&str]) -> Result<T> {
        let url = api_path(&self.api_url, segments)?;
        self.get_json_at(&url)
    }

    /// An authenticated `GET url` whose `200` body deserializes to `T`.
    fn get_json_at<T: DeserializeOwned>(&mut self, url: &str) -> Result<T> {
        let resp = self.authed(|http, bearer| http.get(url, Some(bearer)))?;
        match resp.status {
            200 => resp.json(url),
            _ => Err(interpret_refusal(&resp, "GET", url)),
        }
    }

    /// Runs `request` with the current bearer and, on a `401`, refreshes (and
    /// persists) the session token and retries exactly once.
    fn authed(
        &mut self,
        request: impl Fn(&HttpClient, &str) -> Result<HttpResponse>,
    ) -> Result<HttpResponse> {
        let resp = request(&self.http, self.cred.token.expose_secret())?;
        if resp.status != 401 {
            return Ok(resp);
        }
        refresh_in_place(&self.http, &mut self.cred)?;
        request(&self.http, self.cred.token.expose_secret())
    }
}

/// The URL of a project's router, or of `tail` under it.
fn router_path(
    api_url: &str,
    workspace_id: &str,
    project_id: &str,
    tail: &[&str],
) -> Result<String> {
    let mut segments = vec![
        "api",
        "workspace",
        workspace_id,
        "projects",
        project_id,
        "router",
    ];
    segments.extend_from_slice(tail);
    api_path(api_url, &segments)
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

/// The members of an RFC 9457 problem document the CLI reads.
#[derive(Deserialize)]
struct ProblemDocument {
    #[serde(rename = "type", default)]
    problem_type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    pending_removals: Option<u32>,
}

/// The error for a refused request, after the single reactive refresh the
/// `authed` callers already attempted: a `401` means the session is gone, and
/// anything else is the platform's problem document when the body carries one,
/// typed so a command can act on its kind. `502`/`503` without a body map to
/// distinct ops-vs-token messages so an outage is not mistaken for a bad token.
fn interpret_refusal(resp: &HttpResponse, method: &str, url: &str) -> Error {
    if resp.status == 401 {
        return Error::NotAuthenticated;
    }
    if let Ok(document) = serde_json::from_str::<ProblemDocument>(&resp.body)
        && !document.title.is_empty()
    {
        return Error::Problem(Problem {
            kind: ProblemKind::parse(&document.problem_type),
            status: resp.status,
            title: document.title,
            detail: document.detail,
            retry_after_secs: resp.retry_after_secs,
            pending_removals: document.pending_removals,
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

    fn response(status: u16, body: &str, retry_after_secs: Option<u64>) -> HttpResponse {
        HttpResponse {
            status,
            body: body.into(),
            retry_after_secs,
        }
    }

    /// A refusal that carries a problem document is typed: the kind from the
    /// `type` member, the status, the text for the person, and the delay the
    /// platform asked for.
    #[test]
    fn a_problem_body_becomes_a_typed_problem() {
        let refusal = interpret_refusal(
            &response(
                503,
                r#"{"type":"https://peppy.bot/problems/provisioner-unavailable",
                    "title":"Provisioner unavailable","status":503,
                    "detail":"the certificate was not signed in time"}"#,
                Some(5),
            ),
            "POST",
            "u",
        );
        let Error::Problem(problem) = refusal else {
            panic!("expected a problem, got {refusal:?}");
        };
        assert_eq!(problem.kind, ProblemKind::ProvisionerUnavailable);
        assert_eq!(problem.status, 503);
        assert_eq!(problem.retry_after_secs, Some(5));
        assert_eq!(
            problem.to_string(),
            "Provisioner unavailable: the certificate was not signed in time"
        );
    }

    #[test]
    fn a_refusal_with_no_problem_body_keeps_its_status_message() {
        assert!(
            interpret_refusal(&response(503, "", None), "GET", "u")
                .to_string()
                .contains("temporarily")
        );
        assert!(
            interpret_refusal(&response(404, "not json", None), "GET", "u")
                .to_string()
                .contains("returned 404")
        );
        assert!(matches!(
            interpret_refusal(&response(401, "", None), "GET", "u"),
            Error::NotAuthenticated
        ));
    }

    /// What a refusal says about the slots a restart frees reaches the caller
    /// as it was sent: a number, zero, or nothing.
    #[test]
    fn a_full_router_refusal_carries_the_slots_a_restart_frees() {
        let refusal = |body: &str| match interpret_refusal(&response(422, body, None), "POST", "u")
        {
            Error::Problem(problem) => problem,
            other => panic!("expected a problem, got {other:?}"),
        };
        let base = r#""type":"https://peppy.bot/problems/peer-limit-reached","title":"Peer limit reached","status":422"#;
        assert_eq!(refusal(&format!("{{{base}}}")).pending_removals, None);
        assert_eq!(
            refusal(&format!(r#"{{{base},"pending_removals":0}}"#)).pending_removals,
            Some(0)
        );
        assert_eq!(
            refusal(&format!(r#"{{{base},"pending_removals":2}}"#)).pending_removals,
            Some(2)
        );
    }

    /// The platform adds members to its answers in each minor version. Every
    /// answer type, and the problem document, must parse an answer that
    /// carries a member this CLI does not know, at each level of the answer.
    #[test]
    fn every_answer_type_accepts_a_member_it_does_not_know() {
        const NEW: &str = r#""member_of_a_later_version":{"a":[1,2]}"#;
        let peer = format!(
            r#"{{"id":"p","name":"n","certificate_cn":"n","status":"unknown",
                "certificate_expires_at":"2027-01-01T00:00:00Z",
                "created_at":"2026-10-03T00:00:00Z",{NEW}}}"#
        );
        let router = format!(
            r#"{{"phase":"running","desired_state":"running","can_manage_infra":true,
                "address":{{"host":"h","port":7447,{NEW}}},
                "pending_changes":true,
                "pending_change_entries":[{{"kind":"peer","description":"d",
                    "staged_at":"2026-10-03T00:00:00Z",{NEW}}}],
                "peers":[{{"id":"p","status":"connected",{NEW}}}],{NEW}}}"#
        );

        serde_json::from_str::<Principal>(&format!(r#"{{"sub":"s",{NEW}}}"#)).expect("Principal");
        serde_json::from_str::<Workspace>(&format!(r#"{{"id":"w","name":"n",{NEW}}}"#))
            .expect("Workspace");
        serde_json::from_str::<Project>(&format!(
            r#"{{"id":"p","workspace_id":"w","name":"n",{NEW}}}"#
        ))
        .expect("Project");
        serde_json::from_str::<RouterPeer>(&peer).expect("RouterPeer");
        serde_json::from_str::<RouterStatus>(&router).expect("RouterStatus");
        serde_json::from_str::<RouterPeerEnrolled>(&format!(
            r#"{{"peer":{peer},"address":{{"host":"h","port":7447,{NEW}}},
                "certificate":"c","chain":"","trust_anchor":"t",
                "zenoh_id":"7f3a9c1e","namespace":"550e8400-e29b-41d4-a716-446655440000",
                "zenoh_config":"{{}}",{NEW}}}"#
        ))
        .expect("RouterPeerEnrolled");
        serde_json::from_str::<ProblemDocument>(&format!(
            r#"{{"type":"about:blank","title":"t","status":422,{NEW}}}"#
        ))
        .expect("ProblemDocument");
    }

    /// A value the platform enumerates parses to its variant, and a value this
    /// CLI has not heard of is kept and renders as itself.
    #[test]
    fn enumerated_values_parse_to_their_variant_and_keep_an_unknown_one() {
        let status = |json: &str| serde_json::from_str::<PeerStatus>(json).unwrap();
        assert_eq!(status(r#""connected""#), PeerStatus::Connected);
        assert_eq!(status(r#""unknown""#), PeerStatus::Unknown);
        assert_eq!(status(r#""pending_restart""#), PeerStatus::PendingRestart);
        assert_eq!(
            status(r#""some_future_state""#),
            PeerStatus::Other("some_future_state".into())
        );
        assert_eq!(
            status(r#""some_future_state""#).to_string(),
            "some_future_state"
        );
        assert_eq!(
            serde_json::to_string(&PeerStatus::PendingRestart).unwrap(),
            r#""pending_restart""#
        );

        let phase = |json: &str| serde_json::from_str::<RouterPhase>(json).unwrap();
        for (json, expected) in [
            (r#""provisioning""#, RouterPhase::Provisioning),
            (r#""running""#, RouterPhase::Running),
            (r#""restarting""#, RouterPhase::Restarting),
            (r#""stopped""#, RouterPhase::Stopped),
            (r#""degraded""#, RouterPhase::Degraded),
            (r#""hibernating""#, RouterPhase::Other("hibernating".into())),
        ] {
            assert_eq!(phase(json), expected);
            assert_eq!(
                serde_json::to_string(&phase(json)).unwrap(),
                json,
                "renders as the platform spells it"
            );
        }
    }

    #[test]
    fn router_paths_end_in_the_given_tail() {
        assert_eq!(
            router_path("https://api.example.test", "ws", "p", &[]).unwrap(),
            "https://api.example.test/api/workspace/ws/projects/p/router"
        );
        assert_eq!(
            router_path("https://api.example.test", "ws", "p", &["peers", "a/b"]).unwrap(),
            "https://api.example.test/api/workspace/ws/projects/p/router/peers/a%2Fb"
        );
        assert_eq!(
            router_path(
                "https://api.example.test",
                "ws",
                "p",
                &["peers", "peer-1", "renew"]
            )
            .unwrap(),
            "https://api.example.test/api/workspace/ws/projects/p/router/peers/peer-1/renew"
        );
    }

    fn enrolled_json(zid: &str, host: &str) -> String {
        format!(
            r#"{{"peer":{{"id":"p","name":"n","certificate_cn":"n","status":"unknown",
                "certificate_expires_at":"2027-01-01T00:00:00Z",
                "created_at":"2026-10-03T00:00:00Z"}},
                "address":{{"host":"{host}","port":7447}},
                "certificate":"c","chain":"","trust_anchor":"t","zenoh_id":"{zid}",
                "namespace":"550e8400-e29b-41d4-a716-446655440000","zenoh_config":"{{}}",
                "some_future_field":1}}"#
        )
    }

    #[test]
    fn an_enrollment_response_rejects_an_id_zenoh_would_refuse() {
        let ok: RouterPeerEnrolled =
            serde_json::from_str(&enrolled_json("7f3a9c1e", "rtr.example")).expect("parses");
        assert_eq!(ok.zenoh_id.as_str(), "7f3a9c1e");
        assert!(
            serde_json::from_str::<RouterPeerEnrolled>(&enrolled_json("0abc", "rtr.example"))
                .is_err()
        );
    }

    #[test]
    fn an_enrollment_response_carries_the_router_address_as_an_endpoint() {
        let ok: RouterPeerEnrolled =
            serde_json::from_str(&enrolled_json("7f3a9c1e", "rtr.example")).expect("parses");
        assert_eq!(ok.address.locator(), "tls/rtr.example:7447");
        assert!(
            serde_json::from_str::<RouterPeerEnrolled>(&enrolled_json("7f3a9c1e", "")).is_err(),
            "an address with no host is refused"
        );
    }
}
