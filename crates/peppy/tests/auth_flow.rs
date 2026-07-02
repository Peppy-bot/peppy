//! End-to-end auth tests with every HTTP endpoint mocked (`httpmock`): the
//! public `/cli-config`, OIDC discovery, the Zitadel device/token endpoints, and
//! the backend `/me` + `/logout`. All auth state is isolated per test via the
//! `peppy_dirs` seam pointed at a tempdir (no `PEPPY_HOME` mutation, so tests run
//! in parallel); the credentials file and `peppy_config.json5` both land there.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use daemon_config::consts::PeppyDirs;
use httpmock::prelude::*;
use secrecy::ExposeSecret;
use serde_json::json;

use peppy::auth::resolver::{CredentialKind, SessionContext};
use peppy::auth::storage::{self, Credentials, ProfileCreds, RouterSession};
use peppy::auth::{client, http::HttpClient, resolver, router};
use peppy::commands::Command;
use peppy::commands::auth::login::LoginCommand;
use peppy::commands::auth::logout::LogoutCommand;
use peppy::commands::auth::whoami::WhoamiCommand;
use peppy::context::AppContext;

/// Builds the cli-config + OIDC discovery + device-authorization + token mocks
/// for a server whose issuer is its own base URL, with the device grant
/// succeeding immediately. Returns nothing — the mocks live on the server.
fn mock_login_endpoints(server: &MockServer, access_token: &str) {
    let base = server.base_url();

    server.mock(|when, then| {
        when.method(GET).path("/cli-config");
        then.status(200).json_body(json!({
            "issuer": base,
            "client_id": "cli-client-id",
            "scopes": "openid profile email offline_access urn:zitadel:iam:org:project:id:proj-id:aud",
        }));
    });

    server.mock(|when, then| {
        when.method(GET).path("/.well-known/openid-configuration");
        then.status(200).json_body(json!({
            "device_authorization_endpoint": format!("{base}/oauth/v2/device_authorization"),
            "token_endpoint": format!("{base}/oauth/v2/token"),
        }));
    });

    server.mock(|when, then| {
        when.method(POST).path("/oauth/v2/device_authorization");
        then.status(200).json_body(json!({
            "device_code": "the-device-code",
            "user_code": "WXYZ-1234",
            "verification_uri": format!("{base}/device"),
            "verification_uri_complete": format!("{base}/device?user_code=WXYZ-1234"),
            "expires_in": 300,
            "interval": 0,
        }));
    });

    server.mock(|when, then| {
        when.method(POST).path("/oauth/v2/token");
        then.status(200).json_body(json!({
            "access_token": access_token,
            "refresh_token": "the-refresh-token",
            "expires_in": 3600,
            "token_type": "Bearer",
            "scope": "openid profile email offline_access",
        }));
    });
}

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

fn ctx() -> Arc<AppContext> {
    Arc::new(AppContext::from_current_dir().expect("cwd is readable"))
}

fn creds_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("conf").join("credentials.json5")
}

#[test]
fn login_persists_credentials_and_resolves_identity() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);

    // Strict federation: with no daemon running (no control socket), login fails
    // *after* persisting the credentials. This is the canonical "creds kept on a
    // federation failure" check: the user is authenticated, only the command
    // exits non-zero.
    let err = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
    }
    .execute(&ctx())
    .expect_err("login fails strictly when no daemon is running to federate");
    assert!(
        err.to_string().contains("federation"),
        "the error explains the federation failure: {err}"
    );

    // The single session is still persisted, with identity cached.
    let creds = storage::load(&path).expect("load creds");
    let pc = creds.session.as_ref().expect("session present");
    assert_eq!(pc.access_token.expose_secret(), "access-token-1");
    assert_eq!(pc.refresh_token.expose_secret(), "the-refresh-token");
    assert_eq!(pc.subject, "user-123");
    assert_eq!(pc.username, "alice");
    assert_eq!(pc.issuer, server.base_url());
    assert_eq!(pc.client_id, "cli-client-id");

    // `/me` was consulted (the tolerant parse succeeded).
    assert!(me.calls() >= 1, "GET /me should have been called");
}

#[test]
fn login_pokes_the_running_daemon_to_refederate() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    let peppy_dirs = PeppyDirs::new(dir.path());
    let runtime = peppy_dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    // Matches `daemon_control::FEDERATION_CONTROL_SOCK` (the wire contract).
    let socket = runtime.join("federation_control.sock");

    // A stub daemon: accept one poke, capture the request line, reply ok.
    let listener = UnixListener::bind(&socket).expect("bind stub control socket");
    let stub = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept poke");
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).expect("read poke request");
        stream
            .write_all(b"{\"status\":\"ok\",\"applied\":\"tls/cap:7443\"}\n")
            .expect("reply");
        line.trim().to_string()
    });

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(peppy_dirs),
    }
    .execute(&ctx())
    .expect("login should succeed");

    let request = stub.join().expect("stub thread");
    assert_eq!(
        request, "refederate",
        "login must poke the daemon to refederate"
    );
}

#[test]
fn login_seeds_peppy_config_with_resource_servers_block() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-3");
    let _me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");

    // The federation poke fails (no daemon) so login exits non-zero, but the
    // config seeding happens earlier in the flow; ignore the federation error.
    let _ = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
    }
    .execute(&ctx());

    // A login on a machine that never ran the daemon still seeds
    // peppy_config.json5 with the resource_servers block (build's default URL,
    // which is the dev backend in this debug test build).
    let config = std::fs::read_to_string(dir.path().join("conf").join("peppy_config.json5"))
        .expect("peppy_config.json5 was created by the CLI");
    assert!(
        config.contains("resource_servers:"),
        "resource_servers block missing:\n{config}"
    );
    assert!(
        config.contains(r#"api: "http://127.0.0.1:3000""#),
        "default api URL missing:\n{config}"
    );
}

#[cfg(unix)]
#[test]
fn login_writes_credentials_file_0600() {
    use std::os::unix::fs::PermissionsExt;

    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-2");
    let _me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);

    // No daemon ⇒ login exits non-zero on the federation step, but the
    // credentials file is written (0600) earlier; ignore the federation error.
    let _ = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
    }
    .execute(&ctx());

    let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "credentials must be owner-only");
}

#[test]
fn logout_calls_backend_and_clears_local_credentials() {
    let server = MockServer::start();
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);

    // Seed a logged-in session.
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
    }
    .execute(&ctx())
    .expect("logout");

    assert!(logout.calls() >= 1, "POST /logout should have been called");
    let after = storage::load(&path).expect("load creds");
    assert!(
        after.session.is_none(),
        "local credentials must be removed after logout"
    );
}

#[test]
fn logout_heals_a_malformed_credentials_file() {
    // A malformed (e.g. pre-`organization_id`/unversioned) credentials file fails
    // to parse with `Error::Auth`. Logout treats that as "already logged out", but
    // it must still rewrite the file to a clean default so the bad file does not
    // linger on disk (the early "Not logged in" return used to skip the save).
    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir conf");
    // Unversioned old shape ⇒ rejected by `storage::load` with `Error::Auth`.
    std::fs::write(
        &path,
        r#"{ session: { api_url: "http://x", issuer: "http://y", client_id: "c",
            access_token: "a", refresh_token: "r", expires_at: 1, token_type: "Bearer",
            scope: "openid" } }"#,
    )
    .expect("write malformed creds");

    LogoutCommand {
        // Never contacted: the malformed path returns "Not logged in" before any
        // backend call. A dummy keeps the test independent of build-default URLs.
        api_url: Some("http://127.0.0.1:9".to_string()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
    }
    .execute(&ctx())
    .expect("logout tolerates a malformed file");

    // The file now parses cleanly (healed to a current-version default) and is
    // logged out.
    let after = storage::load(&path).expect("malformed file must be healed, not left on disk");
    assert!(after.session.is_none(), "healed file is logged out");
    assert!(after.router.is_none(), "healed file has no router cache");
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
    assert_eq!(cred.token.expose_secret(), "pat-secret");
}

#[test]
fn resolver_refreshes_an_expired_session_token() {
    let server = MockServer::start();
    let base = server.base_url();

    server.mock(|when, then| {
        when.method(GET).path("/.well-known/openid-configuration");
        then.status(200).json_body(json!({
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
}

#[test]
fn get_me_parses_principal_with_unknown_fields() {
    let server = MockServer::start();
    let _me = mock_me(&server);
    let http = HttpClient::new();

    // A PAT-style credential is fine here: `/me` returns 200, no refresh needed.
    let mut cred = peppy::auth::Credential {
        token: storage::secret("any-token".to_string()),
        kind: CredentialKind::Pat,
    };
    let principal = client::get_me(&http, &server.base_url(), &mut cred).expect("get_me");
    assert_eq!(principal.sub, "user-123");
    assert_eq!(principal.kind.as_deref(), Some("human"));
    assert_eq!(principal.display_name(), "alice");
}

#[test]
fn whoami_runs_against_a_seeded_session() {
    let server = MockServer::start();
    let _me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    // Both the human and the --json formatter must run without error.
    for json in [false, true] {
        WhoamiCommand {
            api_url: Some(server.base_url()),
            json,
            peppy_dirs: Some(PeppyDirs::new(dir.path())),
        }
        .execute(&ctx())
        .expect("whoami");
    }
}

#[test]
fn establish_messaging_federation_parses_the_contract() {
    let server = MockServer::start();
    // The shared router is static: the daemon just POSTs to discover it (no body —
    // it no longer identifies itself with a core-node name).
    let cfg_mock = server.mock(|when, then| {
        when.method(POST).path("/me/messaging-federation");
        then.status(200).json_body(json!({
            "endpoint": "tls/localhost:7447",
            "protocol": "tls",
            "mode": "client",
            "reconnect_after_secs": 3000,
            "organization_id": "550e8400-e29b-41d4-a716-446655440000",
            "some_future_field": "ignored by a tolerant client",
        }));
    });
    let http = HttpClient::new();

    // A PAT credential is fine here: 200, no refresh needed.
    let mut cred = peppy::auth::Credential {
        token: storage::secret("any-token".to_string()),
        kind: CredentialKind::Pat,
    };
    let cfg = client::establish_messaging_federation(&http, &server.base_url(), &mut cred)
        .expect("fetch shared router config");
    assert_eq!(cfg.protocol, "tls");
    assert_eq!(cfg.reconnect_after_secs, 3000);
    assert_eq!(cfg.organization_id, "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(
        cfg.host_port().expect("parse endpoint"),
        ("localhost".to_string(), 7447)
    );
    assert!(cfg_mock.calls() >= 1, "the config endpoint should be hit");
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
    let pull_rejected = server.mock(|when, then| {
        when.method(POST)
            .path("/me/messaging-federation")
            .header("Authorization", "Bearer seeded-access");
        then.status(401);
    });
    let pull_ok = server.mock(|when, then| {
        when.method(POST)
            .path("/me/messaging-federation")
            .header("Authorization", "Bearer refreshed-access");
        then.status(200).json_body(json!({
            "endpoint": "tls/cap.zenoh.localhost:7443",
            "protocol": "tls",
            "mode": "client",
            "reconnect_after_secs": 3000,
            "organization_id": "550e8400-e29b-41d4-a716-446655440000",
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
    let mut cred = peppy::auth::Credential {
        token: storage::secret("seeded-access".to_string()),
        kind: CredentialKind::Session(SessionContext {
            issuer: server.base_url(),
            client_id: "cli-client-id".to_string(),
            refresh_token: storage::secret("seeded-refresh".to_string()),
            creds_path: path.clone(),
        }),
    };

    let cfg = client::establish_messaging_federation(&http, &server.base_url(), &mut cred)
        .expect("pull after refresh");
    assert_eq!(
        cfg.host_port().unwrap(),
        ("cap.zenoh.localhost".to_string(), 7443)
    );
    assert!(pull_rejected.calls() >= 1, "the seeded token must be tried");
    assert!(token.calls() >= 1, "a refresh must occur on the 401");
    assert!(pull_ok.calls() >= 1, "the retry uses the rotated token");

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
}

#[test]
fn resolve_router_endpoint_reuses_a_fresh_cache_without_pulling() {
    let server = MockServer::start();
    // A mock serving a bogus endpoint *if* it is ever hit. The proof that the
    // fresh cache was reused is `pull.calls() == 0` (asserted below) plus the
    // returned host matching the cached value — not this mock's response; the
    // bogus body only makes an accidental pull obvious.
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/messaging-federation");
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
            organization_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            // Matches `seeded_creds`'s subject so the identity tag agrees and the
            // fresh cache is reused (a mismatch would force a re-pull).
            subject: "user-123".into(),
        }),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    let http = HttpClient::new();
    let endpoint =
        router::resolve_router_endpoint(&path, &http, &server.base_url(), None, None, None)
            .expect("resolve from cache");
    assert_eq!(endpoint.host, "cached.zenoh.localhost");
    assert_eq!(endpoint.port, 7443);
    assert!(endpoint.tls.verify_name_on_connect);
    assert_eq!(pull.calls(), 0, "a fresh cache must not trigger a pull");
}

#[test]
fn resolve_federation_target_derives_the_upstream_tls_locator() {
    // The daemon's federation target: pull the per-user router config and turn it
    // into the `tls/<host>:<port>` connect endpoint the local zenohd federates to,
    // plus the connect-side trust. Proves the derivation the `serve` builder and
    // the `RouterFederation` task both rely on.
    let server = MockServer::start();
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/messaging-federation");
        then.status(200).json_body(json!({
            "endpoint": "tls/cap.zenoh.localhost:7443",
            "protocol": "tls",
            "reconnect_after_secs": 3000,
            "organization_id": "550e8400-e29b-41d4-a716-446655440000",
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
    let client_identity = (
        std::path::PathBuf::from("/etc/peppy/client.pem"),
        std::path::PathBuf::from("/etc/peppy/client-key.pem"),
    );
    let target = router::resolve_federation_target_at(
        &path,
        &server.base_url(),
        None,
        Some(ca),
        Some(client_identity),
        SECS_30,
    )
    .expect("logged in ⇒ a federation target");
    assert_eq!(target.0, "tls/cap.zenoh.localhost:7443");
    assert!(
        target.1.verify_name_on_connect,
        "the upstream link verifies the router's cert name"
    );
    assert!(
        target.1.enable_mtls,
        "the daemon presents its client cert for mTLS to the shared router"
    );
    assert!(
        target.1.connect_certificate.is_some(),
        "the mTLS client certificate is set"
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
        when.method(POST).path("/me/messaging-federation");
        then.status(200)
            .json_body(json!({ "endpoint": "tls/should-not-be-pulled:1" }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir); // no creds file written ⇒ no session
    let target =
        router::resolve_federation_target_at(&path, &server.base_url(), None, None, None, SECS_30);
    assert!(target.is_none(), "not logged in ⇒ no federation target");
    assert_eq!(pull.calls(), 0, "not logged in ⇒ the backend is never hit");
}

#[test]
fn resolve_federation_target_fails_closed_on_an_invalid_org_namespace() {
    // Fail closed: a logged-in pull whose `organization_id` cannot be a zenoh
    // namespace (here a wildcard) must NOT federate. The local router stays
    // standalone rather than dialing the shared router under a bogus namespace.
    // The daemon resolves its session namespace from the same org id, so a value
    // that cannot federate also cannot carry a federating namespace.
    let server = MockServer::start();
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/messaging-federation");
        then.status(200).json_body(json!({
            "endpoint": "tls/cap.zenoh.localhost:7443",
            "protocol": "tls",
            "reconnect_after_secs": 3000,
            "organization_id": "**",
        }));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    let creds = Credentials {
        session: Some(seeded_creds(&server, 9_999_999_999)),
        ..Default::default()
    };
    storage::save(&path, &creds).expect("seed creds");

    let target =
        router::resolve_federation_target_at(&path, &server.base_url(), None, None, None, SECS_30);
    assert!(
        target.is_none(),
        "an org id that is not a valid namespace must fail closed (no federation)"
    );
    assert!(
        pull.calls() >= 1,
        "the gate is applied after the pull, not before"
    );
}

/// A generous federation timeout for tests that don't exercise the bound itself.
const SECS_30: Duration = Duration::from_secs(30);

#[test]
fn resolve_federation_target_honors_a_short_connect_timeout() {
    // The federation pull is bounded by `connect_timeout`: a backend slower than
    // the bound resolves to `None` (local router stays standalone) rather than
    // hanging, while a generous bound against the same delay succeeds — proving
    // it's the timeout, not the mock, that fails the short case.
    let server = MockServer::start();
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/messaging-federation");
        then.status(200)
            .delay(Duration::from_millis(500))
            .json_body(json!({
                "endpoint": "tls/cap.zenoh.localhost:7443",
                "protocol": "tls",
                "reconnect_after_secs": 3000,
                "organization_id": "550e8400-e29b-41d4-a716-446655440000",
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
        None,
        Duration::from_millis(100),
    );
    assert!(
        too_slow.is_none(),
        "a backend slower than the timeout ⇒ no federation target"
    );

    // A generous bound against the same delay succeeds.
    let in_time =
        router::resolve_federation_target_at(&path, &server.base_url(), None, None, None, SECS_30);
    assert!(
        in_time.is_some(),
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
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/messaging-federation");
        then.status(200).json_body(json!({
            "endpoint": "tls/fresh.zenoh.localhost:7443",
            "protocol": "tls",
            "mode": "client",
            "reconnect_after_secs": 3000,
            "organization_id": "550e8400-e29b-41d4-a716-446655440000",
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
            organization_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            subject: "user-123".into(),
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
        None,
    )
    .expect("re-pull");
    assert_eq!(endpoint.host, "fresh.zenoh.localhost");
    assert_eq!(endpoint.port, 7443);
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
}

#[test]
fn router_cache_is_bound_to_the_pull_identity_not_the_on_disk_session() {
    // A PAT-authenticated pull must tag the cache with the PAT owner's stable
    // backend subject (`/me`), NOT the on-disk session subject. Otherwise, once the
    // PAT is gone, a session resolve would reuse the PAT's org — a cross-identity
    // (cross-tenant) leak.
    let server = MockServer::start();
    let pull = server.mock(|when, then| {
        when.method(POST).path("/me/messaging-federation");
        then.status(200).json_body(json!({
            "endpoint": "tls/pat-org.zenoh.localhost:7443",
            "protocol": "tls",
            "reconnect_after_secs": 3000,
            "organization_id": "550e8400-e29b-41d4-a716-446655440000",
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
        None,
    )
    .expect("PAT pull resolves");
    assert_eq!(ep.host, "pat-org.zenoh.localhost");
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
    // subjects differ, so it re-pulls rather than leaking the PAT's org.
    let _ = router::resolve_router_endpoint(&path, &http, &server.base_url(), None, None, None)
        .expect("session resolve");
    assert_eq!(
        pull.calls(),
        2,
        "the session must re-pull, not reuse the PAT-identity cache"
    );
    assert_eq!(me.calls(), 1, "a session pull does not need /me");
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
