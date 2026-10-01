//! Engine-level tests with every HTTP endpoint mocked (`httpmock`): OIDC
//! discovery, the Zitadel token endpoint, and the backend's `/me`, workspace,
//! project and router-peer routes. All state is isolated per test via an
//! explicit credentials path under a tempdir (no `PEPPY_HOME` mutation, so
//! tests run in parallel). The command-level flows (`peppy platform login`,
//! `enroll`, `unenroll`, ...) are covered by the `peppy` crate's own tests.

use std::path::PathBuf;

use httpmock::prelude::*;
use secrecy::ExposeSecret;
use serde_json::json;

use auth::client::{PeerRemoval, PeerStatus, PlatformApi, RouterPhase};
use auth::enrollment::{self, EnrollmentBundle, IssuedMaterial, RouterEndpoint};
use auth::storage::{self, Credentials, ProfileCreds};
use auth::{AuthError, ProblemKind};
use auth::{http::HttpClient, renewal, resolver};
use daemon_config::consts::PeppyDirs;

const WORKSPACE: &str = "ws-1";
const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
const PEERS_PATH: &str =
    "/api/workspace/ws-1/projects/550e8400-e29b-41d4-a716-446655440000/router/peers";

/// `GET /me` returning a `human` principal plus an unknown future field, so the
/// test also exercises tolerant deserialization.
fn mock_me(server: &MockServer) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(GET).path("/me");
        then.status(200).json_body(json!({
            "sub": "user-123",
            "kind": "human",
            "username": "alice",
            "email": "alice@example.com",
            "region": "us-east-1",
            "some_future_field": "ignored by a tolerant client",
        }));
    })
}

fn creds_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("conf").join("credentials.json5")
}

/// A session credential pointing at `server` with the given absolute expiry.
fn seeded_creds(server: &MockServer, expires_at: i64) -> ProfileCreds {
    ProfileCreds {
        api_url: server.base_url(),
        issuer: server.base_url(),
        client_id: "cli-client-id".to_string(),
        access_token: storage::secret("seeded-access".to_string()),
        refresh_token: storage::secret("seeded-refresh".to_string()),
        expires_at,
        token_type: "Bearer".to_string(),
        scope: "openid".to_string(),
        subject: "user-123".to_string(),
        username: "alice".to_string(),
    }
}

/// Seeds a valid session under a tempdir, resolves it into a credential, and
/// returns the API of `server` with it.
fn seeded_api(server: &MockServer) -> (tempfile::TempDir, PlatformApi) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    storage::save(
        &path,
        &Credentials {
            session: Some(seeded_creds(server, storage::now_unix() + 3600)),
            ..Default::default()
        },
    )
    .expect("seed creds");
    let cred = resolver::resolve(&path, &HttpClient::new()).expect("a valid session resolves");
    (
        dir,
        PlatformApi::new(&HttpClient::new(), &server.base_url(), cred),
    )
}

fn mock_discovery_and_refresh(server: &MockServer) -> httpmock::Mock<'_> {
    let base = server.base_url();
    server.mock(|when, then| {
        when.method(GET).path("/.well-known/openid-configuration");
        then.status(200).json_body(json!({
            "issuer": base,
            "device_authorization_endpoint": format!("{base}/oauth/v2/device_authorization"),
            "token_endpoint": format!("{base}/oauth/v2/token"),
        }));
    });
    server.mock(|when, then| {
        when.method(POST).path("/oauth/v2/token");
        then.status(200).json_body(json!({
            "access_token": "refreshed-access",
            "refresh_token": "rotated-refresh",
            "expires_in": 3600,
            "token_type": "Bearer",
            "scope": "openid",
        }));
    })
}

#[test]
fn resolver_needs_a_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let Err(err) = resolver::resolve(&creds_path(&dir), &HttpClient::new()) else {
        panic!("no session on disk must not resolve");
    };
    assert!(matches!(err, auth::AuthError::NotAuthenticated));
}

#[test]
fn resolver_refreshes_an_expired_session_token() {
    let server = MockServer::start();
    let token = mock_discovery_and_refresh(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    // expires_at in the past → resolver must refresh.
    let creds = Credentials {
        session: Some(seeded_creds(&server, 1)),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    let http = HttpClient::new();
    let cred = resolver::resolve(&path, &http).expect("refresh resolves");

    assert!(
        token.calls() >= 1,
        "token endpoint should be hit for refresh"
    );
    assert_eq!(cred.token.expose_secret(), "refreshed-access");

    // Rotation persisted to disk.
    let after = storage::load(&path).expect("reload");
    let pc = after.session.as_ref().expect("session still present");
    assert_eq!(pc.access_token.expose_secret(), "refreshed-access");
    assert_eq!(pc.refresh_token.expose_secret(), "rotated-refresh");
    assert!(pc.expires_at > storage::now_unix(), "expiry refreshed");
}

#[test]
fn get_me_parses_principal_with_unknown_fields() {
    let server = MockServer::start();
    let _me = mock_me(&server);
    let (_dir, mut api) = seeded_api(&server);

    let principal = api.get_me().expect("get_me");
    assert_eq!(principal.sub, "user-123");
    assert_eq!(principal.kind.as_deref(), Some("human"));
    assert_eq!(principal.region.as_deref(), Some("us-east-1"));
    assert_eq!(principal.display_name(), "alice");
}

/// A `401` on a session credential refreshes once and retries with the new
/// bearer; the rotated tokens are persisted.
#[test]
fn a_401_refreshes_once_and_retries() {
    let server = MockServer::start();
    let _refresh = mock_discovery_and_refresh(&server);
    let stale = server.mock(|when, then| {
        when.method(GET)
            .path("/api/workspaces")
            .header("authorization", "Bearer seeded-access");
        then.status(401);
    });
    let fresh = server.mock(|when, then| {
        when.method(GET)
            .path("/api/workspaces")
            .header("authorization", "Bearer refreshed-access");
        then.status(200).json_body(json!([
            { "id": WORKSPACE, "name": "Alice's workspace", "tier": "free",
              "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z" }
        ]));
    });
    let (dir, mut api) = seeded_api(&server);

    let workspaces = api.list_workspaces().expect("the retry succeeds");
    assert_eq!(stale.calls(), 1);
    assert_eq!(fresh.calls(), 1);
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].id, WORKSPACE);
    assert_eq!(workspaces[0].name, "Alice's workspace");
    let persisted = storage::load(&creds_path(&dir)).unwrap().session.unwrap();
    assert_eq!(persisted.refresh_token.expose_secret(), "rotated-refresh");
}

#[test]
fn projects_list_marks_archived_ones() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/api/workspace/ws-1/projects");
        then.status(200).json_body(json!([
            { "id": PROJECT, "workspace_id": WORKSPACE, "name": "Lab", "brief": null,
              "robot_count": 1, "live_session_count": 0,
              "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z" },
            { "id": "p-old", "workspace_id": WORKSPACE, "name": "Old", "robot_count": 0,
              "live_session_count": 0, "created_at": "2026-10-01T00:00:00Z",
              "updated_at": "2026-10-01T00:00:00Z", "archived_at": "2026-10-02T00:00:00Z" }
        ]));
    });
    let (_dir, mut api) = seeded_api(&server);

    let projects = api.list_projects(WORKSPACE).expect("list");
    assert_eq!(projects.len(), 2);
    assert!(!projects[0].is_archived());
    assert!(projects[1].is_archived());
}

#[test]
fn enrolling_a_peer_posts_the_csr_and_parses_the_material() {
    let server = MockServer::start();
    let enroll = server.mock(|when, then| {
        when.method(POST)
            .path(PEERS_PATH)
            .header("content-type", "application/json")
            .json_body(json!({ "csr": "-----BEGIN CERTIFICATE REQUEST-----\nx\n-----END CERTIFICATE REQUEST-----\n" }));
        then.status(201).json_body(json!({
            "peer": { "id": "peer-1", "name": "robot-7", "certificate_cn": "robot-7",
                      "status": "unknown", "certificate_expires_at": "2027-01-01T00:00:00Z",
                      "created_at": "2026-10-03T00:00:00Z" },
            "certificate": "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n",
            "chain": "-----BEGIN CERTIFICATE-----\nissuer\n-----END CERTIFICATE-----\n",
            "trust_anchor": "-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n",
            "zenoh_id": "2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5",
            "namespace": PROJECT,
            "address": { "host": "rtr-p.example", "port": 7447 },
            "zenoh_config": "{ connect: { endpoints: [\"tls/rtr-p.example:7447\"] } }",
        }));
    });
    let (_dir, mut api) = seeded_api(&server);

    let enrolled = api
        .enroll_peer(
            WORKSPACE,
            PROJECT,
            "-----BEGIN CERTIFICATE REQUEST-----\nx\n-----END CERTIFICATE REQUEST-----\n",
        )
        .expect("enroll");
    assert_eq!(enroll.calls(), 1);
    assert_eq!(enrolled.peer.id, "peer-1");
    assert_eq!(
        enrolled.zenoh_id.as_str(),
        "2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5"
    );
    assert_eq!(enrolled.namespace.as_str(), PROJECT);
    assert_eq!(enrolled.address.locator(), "tls/rtr-p.example:7447");
    assert!(enrolled.certificate.contains("leaf"));
}

/// The platform's refusal reaches the user as its own title and detail, which
/// is the only remedy text a quota or a malformed request comes with.
#[test]
fn a_refused_enrollment_carries_the_platform_problem_detail() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path(PEERS_PATH);
        then.status(422)
            .header("content-type", "application/problem+json")
            .json_body(json!({
                "type": "https://peppy.bot/problems/peer-limit-reached",
                "title": "Peer limit reached",
                "status": 422,
                "detail": "this router admits 5 peers; remove one first",
            }));
    });
    let (_dir, mut api) = seeded_api(&server);

    let err = api
        .enroll_peer(WORKSPACE, PROJECT, "csr")
        .expect_err("422 is a refusal");
    let AuthError::Problem(problem) = &err else {
        panic!("expected a problem, got {err:?}");
    };
    assert_eq!(problem.kind, ProblemKind::PeerLimitReached);
    assert_eq!(problem.status, 422);
    assert_eq!(
        err.to_string(),
        "Peer limit reached: this router admits 5 peers; remove one first"
    );
}

/// The delay the platform asks for reaches the caller as data.
#[test]
fn a_refusal_carries_the_retry_after_delay() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path(PEERS_PATH);
        then.status(503)
            .header("content-type", "application/problem+json")
            .header("retry-after", "5")
            .json_body(json!({
                "type": "https://peppy.bot/problems/provisioner-unavailable",
                "title": "Provisioner unavailable",
                "status": 503,
            }));
    });
    let (_dir, mut api) = seeded_api(&server);

    let err = api
        .enroll_peer(WORKSPACE, PROJECT, "csr")
        .expect_err("503 is a refusal");
    let AuthError::Problem(problem) = err else {
        panic!("expected a problem, got {err:?}");
    };
    assert_eq!(problem.kind, ProblemKind::ProvisionerUnavailable);
    assert_eq!(problem.retry_after_secs, Some(5));
}

fn router_status_body(phase: &str) -> serde_json::Value {
    json!({
        "phase": phase, "desired_state": "running", "can_manage_infra": true,
        "size": "micro", "entitled_sizes": ["micro"],
        "pending_changes": true,
        "pending_change_entries": [ { "kind": "peer",
            "description": "peer robot-7 removed", "staged_at": "2026-10-03T00:00:00Z" } ],
        "peers": []
    })
}

#[test]
fn restarting_and_starting_the_router_post_with_no_body() {
    let server = MockServer::start();
    let restart = server.mock(|when, then| {
        when.method(POST)
            .path(
                "/api/workspace/ws-1/projects/550e8400-e29b-41d4-a716-446655440000/router/restart",
            )
            .body("");
        then.status(202).json_body(router_status_body("restarting"));
    });
    let start = server.mock(|when, then| {
        when.method(POST)
            .path("/api/workspace/ws-1/projects/550e8400-e29b-41d4-a716-446655440000/router/start")
            .body("");
        then.status(202)
            .json_body(router_status_body("provisioning"));
    });
    let (_dir, mut api) = seeded_api(&server);

    let restarted = api.restart_router(WORKSPACE, PROJECT).expect("restart");
    assert_eq!(restart.calls(), 1);
    assert_eq!(restarted.phase, RouterPhase::Restarting);
    assert_eq!(restarted.pending_change_entries.len(), 1);
    assert_eq!(
        restarted.pending_change_entries[0].description,
        "peer robot-7 removed"
    );

    let started = api.start_router(WORKSPACE, PROJECT).expect("start");
    assert_eq!(start.calls(), 1);
    assert_eq!(started.phase, RouterPhase::Provisioning);
}

/// A caller without the permission to manage the infrastructure gets a
/// problem with status 403, which the command words on its own.
#[test]
fn a_restart_without_the_permission_is_a_403_problem() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path(
            "/api/workspace/ws-1/projects/550e8400-e29b-41d4-a716-446655440000/router/restart",
        );
        then.status(403)
            .header("content-type", "application/problem+json")
            .json_body(json!({ "type": "about:blank", "title": "Forbidden", "status": 403 }));
    });
    let (_dir, mut api) = seeded_api(&server);

    let err = api
        .restart_router(WORKSPACE, PROJECT)
        .expect_err("403 is a refusal");
    let AuthError::Problem(problem) = err else {
        panic!("expected a problem, got {err:?}");
    };
    assert_eq!(problem.status, 403);
}

#[test]
fn peers_and_router_status_parse_the_contract() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path(PEERS_PATH);
        then.status(200).json_body(json!([
            { "id": "peer-1", "name": "robot-7", "certificate_cn": "robot-7",
              "status": "connected", "certificate_expires_at": "2027-01-01T00:00:00Z",
              "created_at": "2026-10-03T00:00:00Z" },
            { "id": "peer-2", "name": "bench", "certificate_cn": "bench",
              "status": "a_status_this_cli_has_not_heard_of",
              "certificate_expires_at": "2027-01-01T00:00:00Z",
              "created_at": "2026-10-03T00:00:00Z" }
        ]));
    });
    server.mock(|when, then| {
        when.method(GET)
            .path("/api/workspace/ws-1/projects/550e8400-e29b-41d4-a716-446655440000/router");
        then.status(200).json_body(json!({
            "phase": "running", "desired_state": "running", "can_manage_infra": true,
            "address": { "host": "rtr-p.example", "port": 7447 },
            "size": "micro", "entitled_sizes": ["micro"],
            "pending_changes": false, "pending_change_entries": [],
            "peers": [ { "id": "peer-1", "status": "connected",
                         "last_seen_at": "2026-10-03T01:00:00Z" } ]
        }));
    });
    let (_dir, mut api) = seeded_api(&server);

    let peers = api.list_peers(WORKSPACE, PROJECT).expect("peers");
    assert_eq!(peers.len(), 2);
    assert_eq!(peers[0].status, PeerStatus::Connected);
    assert_eq!(
        peers[1].status,
        PeerStatus::Other("a_status_this_cli_has_not_heard_of".into())
    );

    let router = api.router_status(WORKSPACE, PROJECT).expect("router");
    assert_eq!(router.phase, RouterPhase::Running);
    assert_eq!(
        router.address,
        Some(RouterEndpoint::parse("rtr-p.example", 7447).unwrap())
    );
    assert_eq!(router.peers[0].id, "peer-1");
    assert!(!router.pending_changes);
}

#[test]
fn removing_a_peer_is_staged_and_a_missing_peer_is_already_removed() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(DELETE).path(format!("{PEERS_PATH}/peer-1"));
        then.status(202).json_body(json!({
            "phase": "running", "desired_state": "running", "can_manage_infra": true,
            "size": "micro", "entitled_sizes": ["micro"],
            "pending_changes": true,
            "pending_change_entries": [ { "kind": "peer_removed",
                "description": "peer robot-7 removed", "staged_at": "2026-10-03T00:00:00Z" } ],
            "peers": []
        }));
    });
    server.mock(|when, then| {
        when.method(DELETE).path(format!("{PEERS_PATH}/peer-gone"));
        then.status(404)
            .header("content-type", "application/problem+json")
            .json_body(json!({ "type": "about:blank", "title": "Not Found", "status": 404 }));
    });
    let (_dir, mut api) = seeded_api(&server);

    match api
        .remove_peer(WORKSPACE, PROJECT, "peer-1")
        .expect("remove")
    {
        PeerRemoval::Staged(status) => assert!(status.pending_changes),
        PeerRemoval::AlreadyRemoved => panic!("a 202 is a staged removal"),
    }
    assert!(matches!(
        api.remove_peer(WORKSPACE, PROJECT, "peer-gone")
            .expect("a 404 is a definite answer"),
        PeerRemoval::AlreadyRemoved
    ));
}

const RENEW_PATH: &str =
    "/api/workspace/ws-1/projects/550e8400-e29b-41d4-a716-446655440000/router/peers/peer-1/renew";
const ZENOH_ID: &str = "2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5";
const ENROLLED_AT: i64 = 1_700_000_000;

/// The platform's answer to the enrollment and to the renewal of `peer-1`.
fn peer_material(leaf: &str, expires_at: &str) -> serde_json::Value {
    json!({
        "peer": { "id": "peer-1", "name": "robot-7", "certificate_cn": "robot-7",
                  "status": "connected", "certificate_expires_at": expires_at,
                  "created_at": "2026-10-03T00:00:00Z" },
        "address": { "host": "rtr-p.example", "port": 7447 },
        "certificate": leaf,
        "chain": "chain",
        "trust_anchor": "ca",
        "zenoh_id": ZENOH_ID,
        "namespace": PROJECT,
        "zenoh_config": "{}",
    })
}

/// A machine enrolled at `api_url` as `peer-1`, with a session of
/// `session_api_url` when one is given.
fn enrolled_machine(
    api_url: &str,
    session: Option<ProfileCreds>,
) -> (tempfile::TempDir, PeppyDirs) {
    let dir = tempfile::tempdir().expect("temp dir");
    let dirs = PeppyDirs::new(dir.path());
    let answer = serde_json::from_value(peer_material("first leaf", "2027-01-01T00:00:00Z"))
        .expect("the enrollment answer parses");
    enrollment::save(
        &dirs,
        &EnrollmentBundle {
            peer_key_pem: storage::secret("key".to_string()),
            issued: IssuedMaterial::for_enrollment(
                api_url,
                WORKSPACE,
                PROJECT,
                answer,
                ENROLLED_AT,
            ),
        },
    )
    .expect("write the enrollment");
    if let Some(session) = session {
        storage::save(
            &storage::credentials_path(&dirs),
            &Credentials {
                session: Some(session),
                ..Default::default()
            },
        )
        .expect("seed the session");
    }
    (dir, dirs)
}

fn leaf_on_disk(dirs: &PeppyDirs) -> String {
    std::fs::read_to_string(dirs.peer_dir().join(enrollment::PEER_CERTIFICATE_FILE))
        .expect("the leaf is on disk")
}

#[test]
fn a_renewal_posts_no_body_and_writes_the_new_leaf() {
    let server = MockServer::start();
    let renew = server.mock(|when, then| {
        when.method(POST)
            .path(RENEW_PATH)
            .header("authorization", "Bearer seeded-access")
            .body("");
        then.status(200)
            .json_body(peer_material("second leaf", "2027-04-01T00:00:00Z"));
    });
    let session = seeded_creds(&server, storage::now_unix() + 3600);
    let (_dir, dirs) = enrolled_machine(&server.base_url(), Some(session));
    let renewed_at = ENROLLED_AT + 60 * 24 * 60 * 60;

    let document = renewal::renew(&dirs, &HttpClient::new(), renewed_at).expect("renewed");

    assert_eq!(renew.calls(), 1);
    assert_eq!(document.certificate_issued_at, renewed_at);
    assert_eq!(document.certificate_expires_at, 1_806_537_600);
    assert_eq!(document.enrolled_at, ENROLLED_AT);
    assert_eq!(document.zenoh_id.as_str(), ZENOH_ID);
    assert_eq!(
        document.router,
        RouterEndpoint::parse("rtr-p.example", 7447).unwrap()
    );
    assert_eq!(leaf_on_disk(&dirs), "second leaf");
    let on_disk = enrollment::load(&dirs).unwrap().expect("enrolled");
    assert_eq!(on_disk.document, document);
    assert_eq!(std::fs::read_to_string(&on_disk.peer_key).unwrap(), "key");
}

#[test]
fn a_renewal_with_an_expired_token_refreshes_the_session_first() {
    let server = MockServer::start();
    let token = mock_discovery_and_refresh(&server);
    let renew = server.mock(|when, then| {
        when.method(POST)
            .path(RENEW_PATH)
            .header("authorization", "Bearer refreshed-access");
        then.status(200)
            .json_body(peer_material("second leaf", "2027-04-01T00:00:00Z"));
    });
    let (_dir, dirs) = enrolled_machine(&server.base_url(), Some(seeded_creds(&server, 1)));

    renewal::renew(&dirs, &HttpClient::new(), ENROLLED_AT + 100).expect("renewed");

    assert_eq!(token.calls(), 1);
    assert_eq!(renew.calls(), 1);
    let session = storage::load(&storage::credentials_path(&dirs))
        .unwrap()
        .session
        .expect("the session stays");
    assert_eq!(session.refresh_token.expose_secret(), "rotated-refresh");
}

#[test]
fn a_renewal_with_no_session_asks_nothing_and_writes_nothing() {
    let server = MockServer::start();
    let renew = server.mock(|when, then| {
        when.method(POST).path(RENEW_PATH);
        then.status(200)
            .json_body(peer_material("second leaf", "2027-04-01T00:00:00Z"));
    });
    let (_dir, dirs) = enrolled_machine(&server.base_url(), None);

    let err = renewal::renew(&dirs, &HttpClient::new(), ENROLLED_AT + 100).expect_err("no session");

    assert!(matches!(err, AuthError::NotAuthenticated), "{err}");
    assert_eq!(renew.calls(), 0);
    assert_eq!(leaf_on_disk(&dirs), "first leaf");
}

/// A token of one platform is not sent to a different platform.
#[test]
fn a_session_of_a_different_platform_does_not_renew() {
    let enrolled_at = MockServer::start();
    let signed_in_at = MockServer::start();
    let renew = enrolled_at.mock(|when, then| {
        when.method(POST).path(RENEW_PATH);
        then.status(200)
            .json_body(peer_material("second leaf", "2027-04-01T00:00:00Z"));
    });
    let session = seeded_creds(&signed_in_at, storage::now_unix() + 3600);
    let (_dir, dirs) = enrolled_machine(&enrolled_at.base_url(), Some(session));

    let err = renewal::renew(&dirs, &HttpClient::new(), ENROLLED_AT + 100)
        .expect_err("a different platform")
        .to_string();

    assert!(err.contains(&enrolled_at.base_url()), "{err}");
    assert!(err.contains(&signed_in_at.base_url()), "{err}");
    assert!(err.contains("peppy platform login --api-url"), "{err}");
    assert_eq!(renew.calls(), 0);
    assert_eq!(leaf_on_disk(&dirs), "first leaf");
}

#[test]
fn a_peer_the_platform_cannot_renew_is_a_typed_refusal() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path(RENEW_PATH);
        then.status(409)
            .header("content-type", "application/problem+json")
            .json_body(json!({
                "type": "https://peppy.bot/problems/peer-not-renewable",
                "title": "Peer not renewable",
                "status": 409,
                "detail": "the peer was removed and waits for a router restart",
            }));
    });
    let session = seeded_creds(&server, storage::now_unix() + 3600);
    let (_dir, dirs) = enrolled_machine(&server.base_url(), Some(session));

    let err = renewal::renew(&dirs, &HttpClient::new(), ENROLLED_AT + 100).expect_err("refused");

    let AuthError::Problem(problem) = err else {
        panic!("expected a problem, got {err:?}");
    };
    assert_eq!(problem.kind, ProblemKind::PeerNotRenewable);
    assert_eq!(problem.status, 409);
    assert_eq!(leaf_on_disk(&dirs), "first leaf");
}

#[test]
fn a_renewal_inside_the_minimum_interval_carries_the_delay() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path(RENEW_PATH);
        then.status(429)
            .header("content-type", "application/problem+json")
            .header("retry-after", "1800")
            .json_body(json!({
                "type": "https://peppy.bot/problems/rate-limited",
                "title": "Too many requests",
                "status": 429,
            }));
    });
    let session = seeded_creds(&server, storage::now_unix() + 3600);
    let (_dir, dirs) = enrolled_machine(&server.base_url(), Some(session));

    let err = renewal::renew(&dirs, &HttpClient::new(), ENROLLED_AT + 100).expect_err("refused");

    let AuthError::Problem(problem) = err else {
        panic!("expected a problem, got {err:?}");
    };
    assert_eq!(
        problem.kind,
        ProblemKind::Other("https://peppy.bot/problems/rate-limited".into()),
        "a delay is read from `Retry-After`, whatever the type of the refusal"
    );
    assert_eq!(problem.status, 429);
    assert_eq!(problem.retry_after_secs, Some(1800));
    assert_eq!(leaf_on_disk(&dirs), "first leaf");
}
