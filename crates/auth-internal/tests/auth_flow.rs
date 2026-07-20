//! Engine-level auth tests with every HTTP endpoint mocked (`httpmock`): OIDC
//! discovery, the Zitadel token endpoint, and the backend `/me` +
//! `/me/cli/federation`. All auth state is isolated per test via an
//! explicit credentials path under a tempdir (no `PEPPY_HOME` mutation, so
//! tests run in parallel). The command-level flows (`peppy platform login` /
//! `logout` / `whoami`) are covered by the `peppy` crate's own auth tests.

use std::path::PathBuf;
use std::time::Duration;

use httpmock::prelude::*;
use secrecy::ExposeSecret;
use serde_json::json;

use auth::resolver::{CredentialKind, SessionContext};
use auth::storage::{self, Credentials, ProfileCreds, RouterSession};
use auth::{client, http::HttpClient, resolver, router};

/// `GET /me` returning a `human` principal plus an unknown future field, so the
/// test also exercises tolerant deserialization.
fn mock_me(server: &MockServer) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(GET).path("/me");
        then.status(200).json_body(json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "sub": "user-123",
            "kind": "human",
            "username": "alice",
            "email": "alice@example.com",
            "role": "user",
            "owner_principal_id": null,
            "some_future_field": "ignored by a tolerant client",
        }));
    })
}

fn creds_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("conf").join("credentials.json5")
}

#[test]
fn resolver_prefers_pat_env_over_files() {
    let http = HttpClient::new();

    // Nonexistent path: a PAT must short-circuit before any file is read.
    let cred = resolver::resolve(
        &PathBuf::from("/nonexistent/credentials.json5"),
        &http,
        Some("pat-secret".to_string()),
    )
    .expect("PAT resolves");

    assert!(matches!(cred.kind, CredentialKind::Pat));
    assert!(!cred.is_refreshable(), "a PAT is not refreshable");
    assert_eq!(cred.session_revision(), None);
    assert_eq!(cred.token.expose_secret(), "pat-secret");
}

#[test]
fn resolver_refreshes_an_expired_session_token() {
    let server = MockServer::start();
    let base = server.base_url();

    server.mock(|when, then| {
        when.method(GET).path("/.well-known/openid-configuration");
        then.status(200).json_body(json!({
            "issuer": base.clone(),
            "device_authorization_endpoint": format!("{base}/oauth/v2/device_authorization"),
            "token_endpoint": format!("{base}/oauth/v2/token"),
        }));
    });
    let token = server.mock(|when, then| {
        when.method(POST).path("/oauth/v2/token");
        then.status(200).json_body(json!({
            "access_token": "refreshed-access",
            "refresh_token": "rotated-refresh",
            "expires_in": 3600,
            "token_type": "Bearer",
            "scope": "openid",
        }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    // expires_at in the past → resolver must refresh.
    let creds = Credentials {
        session: Some(seeded_creds(&server, 1)),
        ..Default::default()
    };
    let original_revision = creds.session.as_ref().unwrap().session_revision;
    storage::save(&path, &creds).expect("seed creds");

    let http = HttpClient::new();
    let cred = resolver::resolve(&path, &http, None).expect("refresh resolves");

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
    assert_eq!(
        pc.session_revision, original_revision,
        "a proactive refresh must preserve the login revision"
    );
}

#[test]
fn get_me_parses_principal_with_unknown_fields() {
    let server = MockServer::start();
    let _me = mock_me(&server);
    let http = HttpClient::new();

    // A PAT-style credential is fine here: `/me` returns 200, no refresh needed.
    let mut cred = auth::Credential {
        token: storage::secret("any-token".to_string()),
        kind: CredentialKind::Pat,
    };
    let principal = client::get_me(&http, &server.base_url(), &mut cred).expect("get_me");
    assert_eq!(principal.sub, "user-123");
    assert_eq!(principal.kind.as_deref(), Some("human"));
    assert_eq!(principal.display_name(), "alice");
}

#[test]
fn session_bearer_is_never_sent_to_a_different_api_origin() {
    let origin = MockServer::start();
    let attacker = MockServer::start();
    let received = attacker.mock(|when, then| {
        when.method(GET).path("/me");
        then.status(200).json_body(json!({ "sub": "unexpected" }));
    });
    let dir = tempfile::tempdir().unwrap();
    let path = creds_path(&dir);
    let credentials = Credentials {
        session: Some(seeded_creds(&origin, 9_999_999_999)),
        ..Default::default()
    };
    storage::save(&path, &credentials).unwrap();
    let mut credential = resolver::resolve(&path, &HttpClient::new(), None).unwrap();

    let error = client::get_me(&HttpClient::new(), &attacker.base_url(), &mut credential)
        .expect_err("cross-origin OAuth bearer use must fail before I/O");
    assert!(error.to_string().contains("refusing to send"), "{error}");
    assert_eq!(
        received.calls(),
        0,
        "the foreign origin must see no request"
    );
}

#[test]
fn stale_request_cannot_adopt_a_concurrent_same_origin_login() {
    let server = MockServer::start();
    let received = server.mock(|when, then| {
        when.method(GET).path("/me");
        then.status(401);
    });
    let dir = tempfile::tempdir().unwrap();
    let path = creds_path(&dir);
    let old = seeded_creds(&server, 9_999_999_999);
    storage::save(
        &path,
        &Credentials {
            session: Some(old.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut stale = resolver::session_credential(&path, &old);
    storage::update(&path, |credentials| {
        let mut replacement = seeded_creds(&server, 9_999_999_999);
        replacement.subject = "other-account".into();
        replacement.access_token = storage::secret("other-access".into());
        replacement.refresh_token = storage::secret("other-refresh".into());
        credentials.session = Some(replacement);
        Ok(())
    })
    .unwrap();

    let error = client::get_me(&HttpClient::new(), &server.base_url(), &mut stale)
        .expect_err("stale request must fail CAS");
    assert!(matches!(error, auth::AuthError::NotAuthenticated));
    assert_eq!(
        received.calls(),
        0,
        "neither the stale nor replacement bearer may be transmitted"
    );
}

#[test]
fn same_subject_relogin_invalidates_the_previous_session_revision() {
    let server = MockServer::start();
    let received = server.mock(|when, then| {
        when.method(GET).path("/me");
        then.status(200).json_body(json!({ "sub": "user-123" }));
    });
    let dir = tempfile::tempdir().unwrap();
    let path = creds_path(&dir);
    let old = seeded_creds(&server, 9_999_999_999);
    storage::save(
        &path,
        &Credentials {
            session: Some(old.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut stale = resolver::session_credential(&path, &old);

    storage::update(&path, |credentials| {
        let mut replacement = old.clone();
        replacement.session_revision = "22222222-2222-4222-8222-222222222222"
            .parse()
            .expect("valid replacement revision");
        credentials.session = Some(replacement);
        Ok(())
    })
    .unwrap();

    let error = client::get_me(&HttpClient::new(), &server.base_url(), &mut stale)
        .expect_err("the old revision must fail before authenticated I/O");
    assert!(matches!(error, auth::AuthError::StaleSessionRevision));
    assert_eq!(received.calls(), 0);
}

#[test]
fn logout_never_sends_a_session_bearer_cross_origin() {
    let origin = MockServer::start();
    let attacker = MockServer::start();
    let received = attacker.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let dir = tempfile::tempdir().unwrap();
    let path = creds_path(&dir);
    let session = seeded_creds(&origin, 9_999_999_999);
    storage::save(
        &path,
        &Credentials {
            session: Some(session.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let credential = resolver::session_credential(&path, &session);

    let error = client::logout(&HttpClient::new(), &attacker.base_url(), &credential)
        .expect_err("logout must reject cross-origin bearer use before I/O");
    assert!(error.to_string().contains("refusing to send"), "{error}");
    assert_eq!(
        received.calls(),
        0,
        "the foreign origin must see no request"
    );
}

#[test]
fn logout_never_sends_a_stale_same_origin_session() {
    let server = MockServer::start();
    let received = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let dir = tempfile::tempdir().unwrap();
    let path = creds_path(&dir);
    let old = seeded_creds(&server, 9_999_999_999);
    storage::save(
        &path,
        &Credentials {
            session: Some(old.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let stale = resolver::session_credential(&path, &old);
    storage::update(&path, |credentials| {
        let mut replacement = seeded_creds(&server, 9_999_999_999);
        replacement.subject = "other-account".into();
        replacement.access_token = storage::secret("other-access".into());
        replacement.refresh_token = storage::secret("other-refresh".into());
        credentials.session = Some(replacement);
        Ok(())
    })
    .unwrap();

    let error = client::logout(&HttpClient::new(), &server.base_url(), &stale)
        .expect_err("logout must reject a stale same-origin session before I/O");
    assert!(matches!(error, auth::AuthError::NotAuthenticated));
    assert_eq!(
        received.calls(),
        0,
        "neither the stale nor replacement bearer may be transmitted"
    );
}

/// The core-node name the federation-pull tests identify the daemon with. The
/// pull mocks *require* it as the POST body (`json_body`), so a pull that
/// drops or malforms the body gets no mock response and fails its test.
const CORE_NODE: &str = "core-node-test-1";

#[test]
fn establish_federation_parses_the_contract() {
    let server = MockServer::start();
    // The shared router is static, so the POST provisions nothing — but its body
    // must always identify the daemon by core-node name (the backend registry
    // requires it). The matcher rejects any other body.
    let cfg_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/me/cli/federation")
            .json_body(json!({ "core_node_name": CORE_NODE }));
        then.status(200).json_body(json!({
            "endpoint": "tls/localhost:7447",
            "protocol": "tls",
            "mode": "client",
            "reconnect_after_secs": 3000,
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            "some_future_field": "ignored by a tolerant client",
        }));
    });
    let http = HttpClient::new();

    // A PAT credential is fine here: 200, no refresh needed.
    let mut cred = auth::Credential {
        token: storage::secret("any-token".to_string()),
        kind: CredentialKind::Pat,
    };
    let cfg = client::establish_federation(&http, &server.base_url(), &mut cred, CORE_NODE)
        .expect("fetch shared router config");
    assert_eq!(cfg.protocol, "tls");
    assert_eq!(cfg.reconnect_after_secs, 3000);
    assert_eq!(
        cfg.namespace.as_str(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
    let endpoint = router::parse_router_endpoint(&cfg.endpoint).expect("parse endpoint");
    assert_eq!((endpoint.host(), endpoint.port()), ("localhost", 7447));
    assert!(cfg_mock.calls() >= 1, "the config endpoint should be hit");
}

#[test]
fn establish_federation_returns_a_typed_current_workspace_conflict() {
    let server = MockServer::start();
    let mismatch = server.mock(|when, then| {
        when.method(POST)
            .path("/me/cli/federation")
            .json_body(json!({ "core_node_name": CORE_NODE }));
        then.status(409).json_body(json!({
            "error": "core_node_workspace_mismatch",
            "message": "certificate belongs to the former workspace",
            "workspace_id": "7a040224-4dd3-4b73-8d09-f809b176ed2d",
        }));
    });
    let mut credential = auth::Credential {
        token: storage::secret("any-token".to_string()),
        kind: CredentialKind::Pat,
    };

    let error = client::establish_federation(
        &HttpClient::new(),
        &server.base_url(),
        &mut credential,
        CORE_NODE,
    )
    .expect_err("workspace drift must deny discovery with a typed error");
    match error {
        auth::AuthError::DiscoveryWorkspaceMismatch { current } => {
            assert_eq!(current.as_str(), "7a040224-4dd3-4b73-8d09-f809b176ed2d")
        }
        other => panic!("expected typed workspace mismatch, got {other}"),
    }
    assert_eq!(mismatch.calls(), 1);
}

#[test]
fn establish_federation_rejects_an_invalid_workspace_conflict_uuid() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/me/cli/federation");
        then.status(409).json_body(json!({
            "error": "core_node_workspace_mismatch",
            "message": "certificate belongs to the former workspace",
            "workspace_id": "not-a-workspace-uuid",
        }));
    });
    let mut credential = auth::Credential {
        token: storage::secret("any-token".to_string()),
        kind: CredentialKind::Pat,
    };

    let error = client::establish_federation(
        &HttpClient::new(),
        &server.base_url(),
        &mut credential,
        CORE_NODE,
    )
    .expect_err("an invalid server workspace must not trigger re-enrollment");
    assert!(
        matches!(error, auth::AuthError::Http(_)),
        "invalid UUID must remain a fail-closed transport contract error: {error}"
    );
}

#[test]
fn establish_federation_does_not_reinterpret_other_409_conflicts() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/me/cli/federation");
        then.status(409).json_body(json!({
            "error": "core_node_name_taken",
            "message": "the name belongs to another principal",
        }));
    });
    let mut credential = auth::Credential {
        token: storage::secret("any-token".to_string()),
        kind: CredentialKind::Pat,
    };

    let error = client::establish_federation(
        &HttpClient::new(),
        &server.base_url(),
        &mut credential,
        CORE_NODE,
    )
    .expect_err("the normal discovery conflict must retain generic status handling");
    assert!(
        matches!(error, auth::AuthError::Http(ref message) if message.contains("returned 409")),
        "an unrelated 409 must not be reported as a malformed workspace conflict: {error}"
    );
}

#[test]
fn router_resolve_compares_denied_workspace_with_the_local_certificate() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/me/cli/federation");
        then.status(409).json_body(json!({
            "error": "core_node_workspace_mismatch",
            "message": "certificate belongs to the former workspace",
            "workspace_id": "7a040224-4dd3-4b73-8d09-f809b176ed2d",
        }));
    });
    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    storage::save(
        &path,
        &Credentials {
            session: Some(seeded_creds(&server, 9_999_999_999)),
            ..Default::default()
        },
    )
    .expect("seed creds");
    let mut identity = test_client_identity();
    identity.workspace_id = Some(test_namespace());
    identity.subject = Some("user-123".into());

    let error = match router::resolve_router_endpoint(
        &path,
        &HttpClient::new(),
        &server.base_url(),
        None,
        None,
        identity,
        CORE_NODE,
    ) {
        Err(error) => error,
        Ok(_) => panic!("denied discovery must request re-enrollment, not use old workspace data"),
    };
    match error {
        auth::AuthError::WorkspaceMismatch {
            discovered,
            certificate,
        } => {
            assert_eq!(discovered.as_str(), "7a040224-4dd3-4b73-8d09-f809b176ed2d");
            assert_eq!(certificate, test_namespace());
        }
        other => panic!("expected compared workspace mismatch, got {other}"),
    }
}

#[test]
fn router_config_pull_refreshes_on_401_then_re_pulls() {
    // Mid-session 401 ⇒ refresh the access token ⇒ retry the pull with the
    // rotated token. The single most important Phase F acceptance check.
    let server = MockServer::start();
    let base = server.base_url();

    // OIDC discovery + token endpoint for the reactive refresh.
    server.mock(|when, then| {
        when.method(GET).path("/.well-known/openid-configuration");
        then.status(200).json_body(json!({
            "issuer": base.clone(),
            "device_authorization_endpoint": format!("{base}/oauth/v2/device_authorization"),
            "token_endpoint": format!("{base}/oauth/v2/token"),
        }));
    });
    let token = server.mock(|when, then| {
        when.method(POST).path("/oauth/v2/token");
        then.status(200).json_body(json!({
            "access_token": "refreshed-access",
            "refresh_token": "rotated-refresh",
            "expires_in": 3600,
            "token_type": "Bearer",
            "scope": "openid",
        }));
    });
    // First pull (seeded token) is rejected; the retry (rotated token) succeeds.
    // Both matchers require the core-node-name body, so the retry provably
    // re-sends it.
    let pull_rejected = server.mock(|when, then| {
        when.method(POST)
            .path("/me/cli/federation")
            .header("Authorization", "Bearer seeded-access")
            .json_body(json!({ "core_node_name": CORE_NODE }));
        then.status(401);
    });
    let pull_ok = server.mock(|when, then| {
        when.method(POST)
            .path("/me/cli/federation")
            .header("Authorization", "Bearer refreshed-access")
            .json_body(json!({ "core_node_name": CORE_NODE }));
        then.status(200).json_body(json!({
            "endpoint": "tls/cap.zenoh.localhost:7443",
            "protocol": "tls",
            "mode": "client",
            "reconnect_after_secs": 3000,
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
        }));
    });

    // A stored, non-expired session so `refresh_in_place` can reload + rotate it.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    let http = HttpClient::new();
    let mut cred = auth::Credential {
        token: storage::secret("seeded-access".to_string()),
        kind: CredentialKind::Session(SessionContext {
            session_revision: test_session_revision(),
            api_url: server.base_url(),
            issuer: server.base_url(),
            client_id: "cli-client-id".to_string(),
            subject: "user-123".to_string(),
            refresh_token: storage::secret("seeded-refresh".to_string()),
            creds_path: path.clone(),
        }),
    };

    let cfg = client::establish_federation(&http, &server.base_url(), &mut cred, CORE_NODE)
        .expect("pull after refresh");
    let endpoint = router::parse_router_endpoint(&cfg.endpoint).expect("parse endpoint");
    assert_eq!(
        (endpoint.host(), endpoint.port()),
        ("cap.zenoh.localhost", 7443)
    );
    assert!(pull_rejected.calls() >= 1, "the seeded token must be tried");
    assert!(token.calls() >= 1, "a refresh must occur on the 401");
    assert!(pull_ok.calls() >= 1, "the retry uses the rotated token");
    assert_eq!(cred.session_revision(), Some(test_session_revision()));

    // The rotation was persisted (so a later command starts from the new token).
    let after = storage::load(&path).expect("reload");
    assert_eq!(
        after
            .session
            .as_ref()
            .unwrap()
            .refresh_token
            .expose_secret(),
        "rotated-refresh"
    );
    assert_eq!(
        after.session.as_ref().unwrap().session_revision,
        test_session_revision(),
        "a reactive refresh must preserve the login revision"
    );
}

#[test]
fn resolve_router_endpoint_reuses_a_fresh_cache_without_pulling() {
    let server = MockServer::start();
    // A mock serving a bogus endpoint *if* it is ever hit. The proof that the
    // fresh cache was reused is `pull.calls() == 0` (asserted below) plus the
    // returned host matching the cached value, not this mock's response; the
    // bogus body only makes an accidental pull obvious.
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/cli/federation");
        then.status(200)
            .json_body(json!({ "endpoint": "tls/should-not-be-used:1" }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)),
        router: Some(RouterSession {
            endpoint: "tls/cached.zenoh.localhost:7443".into(),
            protocol: "tls".into(),
            // Far in the future ⇒ fresh ⇒ reuse.
            repull_after: storage::now_unix() + 100_000,
            namespace: test_namespace(),
            // Matches `seeded_creds`'s subject so the identity tag agrees and the
            // fresh cache is reused (a mismatch would force a re-pull).
            subject: "user-123".into(),
            // Matches the resolve's core-node name for the same reason.
            core_node_name: CORE_NODE.into(),
            certificate_generation: "test-generation".into(),
        }),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    let http = HttpClient::new();
    let endpoint = router::resolve_router_endpoint(
        &path,
        &http,
        &server.base_url(),
        None,
        None,
        test_client_identity(),
        CORE_NODE,
    )
    .expect("resolve from cache");
    assert_eq!(endpoint.endpoint.host(), "cached.zenoh.localhost");
    assert_eq!(endpoint.endpoint.port(), 7443);
    assert!(endpoint.tls.verify_name_on_connect);
    assert_eq!(pull.calls(), 0, "a fresh cache must not trigger a pull");
}

#[test]
fn resolve_federation_target_derives_the_upstream_tls_locator() {
    // The daemon's federation target: pull the per-user router config and turn it
    // into the `tls/<host>:<port>` connect endpoint the local zenohd federates to,
    // plus the connect-side trust. Proves the derivation the `serve` builder and
    // the daemon identity controller both rely on.
    let server = MockServer::start();
    // The body matcher doubles as the C1 wire-contract check: the daemon's
    // federation pull must always identify itself by core-node name.
    let pull = server.mock(|when, then| {
        when.method(POST)
            .path("/me/cli/federation")
            .json_body(json!({ "core_node_name": CORE_NODE }));
        then.status(200).json_body(json!({
            "endpoint": "tls/cap.zenoh.localhost:7443",
            "protocol": "tls",
            "reconnect_after_secs": 3000,
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
        }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    let ca = std::path::PathBuf::from("/etc/peppy/ca.pem");
    let resolved = router::resolve_federation_target_at(
        &path,
        &server.base_url(),
        None,
        Some(ca),
        Some(test_client_identity()),
        SECS_30,
        CORE_NODE,
    );
    let (endpoint, tls) = resolved.upstream.expect("logged in ⇒ a federation target");
    assert_eq!(endpoint.as_str(), "tls/cap.zenoh.localhost:7443");
    assert_eq!(
        (endpoint.host(), endpoint.port()),
        ("cap.zenoh.localhost", 7443),
        "the endpoint is handed over already parsed for dialing"
    );
    assert!(
        tls.verify_name_on_connect,
        "the upstream link verifies the router's cert name"
    );
    assert!(
        tls.enable_mtls,
        "the daemon presents its client cert for mTLS to the shared router"
    );
    assert!(
        tls.connect_certificate.is_some(),
        "the mTLS client certificate is set"
    );
    assert_eq!(
        resolved.namespace.as_str(),
        "550e8400-e29b-41d4-a716-446655440000",
        "the namespace rides out of the same resolve"
    );
    assert!(pull.calls() >= 1, "a logged-in resolve pulls the config");
}

#[test]
fn resolve_federation_target_is_none_when_not_logged_in() {
    // No session and no PAT ⇒ the local router stays standalone, and crucially the
    // backend is never contacted (the daemon must start cleanly on an
    // un-provisioned dev box).
    let server = MockServer::start();
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/cli/federation");
        then.status(200)
            .json_body(json!({ "endpoint": "tls/should-not-be-pulled:1" }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir); // no creds file written ⇒ no session
    let resolved = router::resolve_federation_target_at(
        &path,
        &server.base_url(),
        None,
        None,
        Some(test_client_identity()),
        SECS_30,
        CORE_NODE,
    );
    assert!(
        resolved.upstream.is_none(),
        "not logged in ⇒ no federation target"
    );
    assert!(
        resolved.namespace.is_local(),
        "not logged in ⇒ the standalone local namespace"
    );
    assert_eq!(pull.calls(), 0, "not logged in ⇒ the backend is never hit");
}

#[test]
fn resolve_federation_target_fails_closed_on_an_invalid_workspace_namespace() {
    // Fail closed: a logged-in pull whose `workspace_id` cannot be a zenoh
    // namespace (here a wildcard) must NOT federate. The typed HTTP boundary
    // rejects the response outright, so the local router stays standalone
    // rather than dialing the shared router under a bogus namespace.
    let server = MockServer::start();
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/cli/federation");
        then.status(200).json_body(json!({
            "endpoint": "tls/cap.zenoh.localhost:7443",
            "protocol": "tls",
            "reconnect_after_secs": 3000,
            "workspace_id": "**",
        }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    let resolved = router::resolve_federation_target_at(
        &path,
        &server.base_url(),
        None,
        None,
        Some(test_client_identity()),
        SECS_30,
        CORE_NODE,
    );
    assert!(
        resolved.upstream.is_none(),
        "a workspace id that is not a valid namespace must fail closed (no federation)"
    );
    assert!(
        pull.calls() >= 1,
        "the rejection happens at the pull's parse, not before the pull"
    );
}

/// A generous federation timeout for tests that don't exercise the bound itself.
const SECS_30: Duration = Duration::from_secs(30);

#[test]
fn resolve_federation_target_honors_a_short_connect_timeout() {
    // The federation pull is bounded by `connect_timeout`: a backend slower than
    // the bound resolves to `None` (local router stays standalone) rather than
    // hanging, while a generous bound against the same delay succeeds, proving
    // it's the timeout, not the mock, that fails the short case.
    let server = MockServer::start();
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/cli/federation");
        then.status(200)
            .delay(Duration::from_millis(500))
            .json_body(json!({
                "endpoint": "tls/cap.zenoh.localhost:7443",
                "protocol": "tls",
                "reconnect_after_secs": 3000,
                "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    // A 100ms bound against a 500ms backend aborts the pull ⇒ no target.
    let too_slow = router::resolve_federation_target_at(
        &path,
        &server.base_url(),
        None,
        None,
        Some(test_client_identity()),
        Duration::from_millis(100),
        CORE_NODE,
    );
    assert!(
        too_slow.upstream.is_none(),
        "a backend slower than the timeout ⇒ no federation target"
    );

    // A generous bound against the same delay succeeds.
    let in_time = router::resolve_federation_target_at(
        &path,
        &server.base_url(),
        None,
        None,
        Some(test_client_identity()),
        SECS_30,
        CORE_NODE,
    );
    assert!(
        in_time.upstream.is_some(),
        "a backend within the timeout ⇒ a federation target"
    );
    assert!(
        pull.calls() >= 1,
        "the bounded resolve still hit the backend"
    );
}

#[test]
fn resolve_router_endpoint_re_pulls_and_caches_when_stale() {
    let server = MockServer::start();
    // The stale re-pull must also carry the core-node-name body (the matcher
    // rejects anything else).
    let pull = server.mock(|when, then| {
        when.method(POST)
            .path("/me/cli/federation")
            .json_body(json!({ "core_node_name": CORE_NODE }));
        then.status(200).json_body(json!({
            "endpoint": "tls/fresh.zenoh.localhost:7443",
            "protocol": "tls",
            "mode": "client",
            "reconnect_after_secs": 3000,
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
        }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)),
        router: Some(RouterSession {
            endpoint: "tls/stale.zenoh.localhost:7443".into(),
            protocol: "tls".into(),
            repull_after: 1, // long past ⇒ stale ⇒ re-pull
            namespace: test_namespace(),
            subject: "user-123".into(),
            core_node_name: CORE_NODE.into(),
            certificate_generation: "test-generation".into(),
        }),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    let http = HttpClient::new();
    let ca = std::path::PathBuf::from("/etc/peppy/ca.pem");
    let endpoint = router::resolve_router_endpoint(
        &path,
        &http,
        &server.base_url(),
        None,
        Some(ca.clone()),
        test_client_identity(),
        CORE_NODE,
    )
    .expect("re-pull");
    assert_eq!(endpoint.endpoint.host(), "fresh.zenoh.localhost");
    assert_eq!(endpoint.endpoint.port(), 7443);
    assert_eq!(
        endpoint.tls.root_ca_certificate.as_deref(),
        Some(ca.as_path())
    );
    assert!(pull.calls() >= 1, "a stale cache must trigger a pull");

    // The fresh config was cached (endpoint replaced, deadline pushed out) so the
    // next connect reuses it. The trust anchor is resolved fresh at connect time,
    // so it is deliberately not part of the cached session.
    let after = storage::load(&path).expect("reload");
    let rs = after.router.as_ref().expect("router cached");
    assert_eq!(rs.endpoint, "tls/fresh.zenoh.localhost:7443");
    assert!(rs.repull_after > storage::now_unix(), "deadline pushed out");
    assert_eq!(
        rs.core_node_name, CORE_NODE,
        "the cache is tagged with the name the config was pulled under"
    );
}

#[test]
fn resolve_router_endpoint_re_pulls_when_the_core_node_name_changed() {
    // The collision-fix workflow: the daemon registered under one name, the user
    // renames it (`core_node_name` in peppy_config.json5) and restarts within the
    // cache-freshness window. The still-fresh cache was pulled under the OLD name,
    // so it must NOT be reused: the resolve re-pulls, registering the new name in
    // the backend registry, and re-tags the cache with it.
    let server = MockServer::start();
    let pull = server.mock(|when, then| {
        when.method(POST)
            .path("/me/cli/federation")
            .json_body(json!({ "core_node_name": CORE_NODE }));
        then.status(200).json_body(json!({
            "endpoint": "tls/cap.zenoh.localhost:7443",
            "protocol": "tls",
            "reconnect_after_secs": 3000,
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
        }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)),
        router: Some(RouterSession {
            endpoint: "tls/cap.zenoh.localhost:7443".into(),
            protocol: "tls".into(),
            // Far in the future ⇒ fresh; only the name tag differs.
            repull_after: storage::now_unix() + 100_000,
            namespace: test_namespace(),
            subject: "user-123".into(),
            core_node_name: "the-old-name".into(),
            certificate_generation: "test-generation".into(),
        }),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    let http = HttpClient::new();
    let endpoint = router::resolve_router_endpoint(
        &path,
        &http,
        &server.base_url(),
        None,
        None,
        test_client_identity(),
        CORE_NODE,
    )
    .expect("resolve re-pulls under the new name");
    assert_eq!(endpoint.endpoint.host(), "cap.zenoh.localhost");
    assert_eq!(
        pull.calls(),
        1,
        "a fresh cache pulled under a different core-node name must re-pull \
         (registering the new name), not be reused"
    );

    // The cache is re-tagged with the new name, so the next resolve reuses it.
    let cached = storage::load(&path)
        .expect("reload")
        .router
        .expect("router cached");
    assert_eq!(cached.core_node_name, CORE_NODE);
}

#[test]
fn router_cache_is_bound_to_the_pull_identity_not_the_on_disk_session() {
    // A PAT-authenticated pull must tag the cache with the PAT owner's stable
    // backend subject (`/me`), NOT the on-disk session subject. Otherwise, once the
    // PAT is gone, a session resolve would reuse the PAT's workspace, a cross-identity
    // (cross-tenant) leak.
    let server = MockServer::start();
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/cli/federation");
        then.status(200).json_body(json!({
            "endpoint": "tls/pat-workspace.zenoh.localhost:7443",
            "protocol": "tls",
            "reconnect_after_secs": 3000,
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
        }));
    });
    // Only a PAT pull resolves `/me` (to learn the PAT owner's stable subject).
    let me = server.mock(|when, then| {
        when.method(GET).path("/me");
        then.status(200)
            .json_body(json!({ "sub": "pat-owner-xyz" }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    // A session for a *different* identity is on disk at the same time as the PAT.
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)), // subject "user-123"
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    let http = HttpClient::new();

    // Pull as the PAT.
    let ep = router::resolve_router_endpoint(
        &path,
        &http,
        &server.base_url(),
        Some("the-pat".to_string()),
        None,
        test_client_identity(),
        CORE_NODE,
    )
    .expect("PAT pull resolves");
    assert_eq!(ep.endpoint.host(), "pat-workspace.zenoh.localhost");
    assert_eq!(pull.calls(), 1, "the PAT pull hit the backend once");
    assert_eq!(
        me.calls(),
        1,
        "a PAT pull resolves /me to bind the cache to the PAT identity"
    );

    // The cache is tagged with the PAT owner's subject, not the session subject.
    let cached = storage::load(&path)
        .expect("reload")
        .router
        .expect("router cached");
    assert_eq!(
        cached.subject, "pat-owner-xyz",
        "a PAT pull must bind the cache to the PAT identity, not the on-disk session"
    );

    // With the PAT gone, a session resolve must NOT reuse the PAT's cache: the
    // subjects differ, so it re-pulls rather than leaking the PAT's workspace.
    let _ = router::resolve_router_endpoint(
        &path,
        &http,
        &server.base_url(),
        None,
        None,
        test_client_identity(),
        CORE_NODE,
    )
    .expect("session resolve");
    assert_eq!(
        pull.calls(),
        2,
        "the session must re-pull, not reuse the PAT-identity cache"
    );
    assert_eq!(me.calls(), 1, "a session pull does not need /me");
}

/// The workspace namespace every seeded router cache in this file was pulled for.
fn test_namespace() -> config::namespace::Namespace {
    config::namespace::Namespace::parse("550e8400-e29b-41d4-a716-446655440000")
        .expect("valid test namespace")
}

/// A non-secret dummy identity for HTTP/cache tests. No network TLS dial occurs;
/// the paths only prove that production target construction is never one-way.
fn test_client_identity() -> router::RouterClientIdentity {
    router::RouterClientIdentity {
        certificate: std::path::PathBuf::from("/etc/peppy/client.pem"),
        private_key: std::path::PathBuf::from("/etc/peppy/client-key.pem"),
        generation: "test-generation".into(),
        workspace_id: None,
        subject: None,
        certificate_not_after: None,
    }
}

/// A session credential pointing at `server` with the given absolute expiry.
fn seeded_creds(server: &MockServer, expires_at: i64) -> ProfileCreds {
    ProfileCreds {
        session_revision: test_session_revision(),
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

fn test_session_revision() -> uuid::Uuid {
    uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111")
        .expect("valid test session revision")
}
