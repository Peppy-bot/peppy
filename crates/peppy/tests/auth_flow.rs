//! End-to-end auth tests with every HTTP endpoint mocked (`httpmock`): the
//! public `/cli-config`, OIDC discovery, the Zitadel device/token endpoints, and
//! the backend `/me` + `/logout`. The credentials file is isolated per test via
//! the `credentials_file` seam (no `PEPPY_HOME` mutation, so tests run in
//! parallel).

use std::path::PathBuf;
use std::sync::Arc;

use httpmock::prelude::*;
use secrecy::ExposeSecret;
use serde_json::json;

use peppy::auth::profile::Profile;
use peppy::auth::resolver::CredentialKind;
use peppy::auth::storage::{self, Credentials, ProfileCreds};
use peppy::auth::{client, http, resolver};
use peppy::commands::Command;
use peppy::commands::login::LoginCommand;
use peppy::commands::logout::LogoutCommand;
use peppy::commands::whoami::WhoamiCommand;
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
            "project_id": "proj-id",
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

    LoginCommand {
        env: Some("dev".to_string()),
        api_url: Some(server.base_url()),
        no_browser: true,
        credentials_file: Some(path.clone()),
    }
    .execute(&ctx())
    .expect("login should succeed against the mock backend");

    // Credentials persisted for the `dev` profile, with identity cached.
    let creds = storage::load(&path).expect("load creds");
    let pc = creds.profiles.get("dev").expect("dev profile present");
    assert_eq!(pc.access_token.expose_secret(), "access-token-1");
    assert_eq!(pc.refresh_token.expose_secret(), "the-refresh-token");
    assert_eq!(pc.subject, "user-123");
    assert_eq!(pc.username, "alice");
    assert_eq!(pc.issuer, server.base_url());
    assert_eq!(pc.client_id, "cli-client-id");

    // `/me` was consulted (the tolerant parse succeeded).
    assert!(me.hits() >= 1, "GET /me should have been called");
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

    LoginCommand {
        env: Some("dev".to_string()),
        api_url: Some(server.base_url()),
        no_browser: true,
        credentials_file: Some(path.clone()),
    }
    .execute(&ctx())
    .expect("login");

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

    // Seed a logged-in profile.
    let mut creds = Credentials::default();
    creds
        .profiles
        .insert("dev".to_string(), seeded_creds(&server, 9_999_999_999));
    storage::save(&path, &creds).expect("seed creds");

    LogoutCommand {
        env: Some("dev".to_string()),
        api_url: Some(server.base_url()),
        credentials_file: Some(path.clone()),
    }
    .execute(&ctx())
    .expect("logout");

    assert!(logout.hits() >= 1, "POST /logout should have been called");
    let after = storage::load(&path).expect("load creds");
    assert!(
        !after.profiles.contains_key("dev"),
        "local credentials must be removed after logout"
    );
}

#[test]
fn resolver_prefers_pat_env_over_files() {
    let server = MockServer::start();
    let profile = Profile {
        name: "dev".to_string(),
        api_url: server.base_url(),
    };
    let agent = http::agent();

    // Nonexistent path: a PAT must short-circuit before any file is read.
    let cred = resolver::resolve(
        &profile,
        &PathBuf::from("/nonexistent/credentials.json5"),
        &agent,
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
    let mut creds = Credentials::default();
    // expires_at in the past → resolver must refresh.
    creds
        .profiles
        .insert("dev".to_string(), seeded_creds(&server, 1));
    storage::save(&path, &creds).expect("seed creds");

    let profile = Profile {
        name: "dev".to_string(),
        api_url: base.clone(),
    };
    let agent = http::agent();
    let cred = resolver::resolve(&profile, &path, &agent, None).expect("refresh resolves");

    assert!(
        token.hits() >= 1,
        "token endpoint should be hit for refresh"
    );
    assert_eq!(cred.token.expose_secret(), "refreshed-access");

    // Rotation persisted to disk.
    let after = storage::load(&path).expect("reload");
    let pc = after.profiles.get("dev").expect("dev still present");
    assert_eq!(pc.access_token.expose_secret(), "refreshed-access");
    assert_eq!(pc.refresh_token.expose_secret(), "rotated-refresh");
    assert!(pc.expires_at > storage::now_unix(), "expiry refreshed");
}

#[test]
fn get_me_parses_principal_with_unknown_fields() {
    let server = MockServer::start();
    let _me = mock_me(&server);
    let agent = http::agent();

    // A PAT-style credential is fine here: `/me` returns 200, no refresh needed.
    let mut cred = peppy::auth::Credential {
        token: storage::secret("any-token".to_string()),
        kind: CredentialKind::Pat,
        profile: "dev".to_string(),
    };
    let principal = client::get_me(&agent, &server.base_url(), &mut cred).expect("get_me");
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
    let mut creds = Credentials::default();
    creds
        .profiles
        .insert("dev".to_string(), seeded_creds(&server, 9_999_999_999));
    storage::save(&path, &creds).expect("seed creds");

    // Both the human and the --json formatter must run without error.
    for json in [false, true] {
        WhoamiCommand {
            env: Some("dev".to_string()),
            api_url: Some(server.base_url()),
            json,
            credentials_file: Some(path.clone()),
        }
        .execute(&ctx())
        .expect("whoami");
    }
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
