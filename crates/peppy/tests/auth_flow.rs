//! Command-level auth tests (`peppy auth login` / `logout` / `whoami`) with
//! every HTTP endpoint mocked (`httpmock`): the public `/cli/auth-config`, OIDC
//! discovery, the Zitadel device/token endpoints, and the backend `/me` +
//! `/logout`. All auth state is isolated per test via the `peppy_dirs` seam
//! pointed at a tempdir (no `PEPPY_HOME` mutation, so tests run in parallel);
//! the credentials file and `peppy_config.json5` both land there. The engine
//! internals (resolver, router-config cache, federation-target resolution) are
//! covered by the `auth` crate's own tests.

use std::path::PathBuf;
use std::sync::Arc;

use daemon_config::consts::PeppyDirs;
use httpmock::prelude::*;
use secrecy::ExposeSecret;
use serde_json::json;

use auth::storage::{self, Credentials, ProfileCreds};
use peppy::commands::Command;
use peppy::commands::auth::login::LoginCommand;
use peppy::commands::auth::logout::LogoutCommand;
use peppy::commands::auth::whoami::WhoamiCommand;
use peppy::context::AppContext;

/// Builds the cli/auth-config + OIDC discovery + device-authorization + token mocks
/// for a server whose issuer is its own base URL, with the device grant
/// succeeding immediately. Returns nothing; the mocks live on the server.
fn mock_login_endpoints(server: &MockServer, access_token: &str) {
    let base = server.base_url();

    server.mock(|when, then| {
        when.method(GET).path("/cli/auth-config");
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

/// Writes the minimal explicit external-router variant. Config completion fills
/// unrelated defaulted sections while leaving `zenoh.external` untouched.
fn write_external_zenoh_config(dir: &tempfile::TempDir) {
    let config_dir = dir.path().join("conf");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("peppy_config.json5"),
        r#"{ zenoh: { external: { endpoint: "tcp/127.0.0.1:7447" } } }"#,
    )
    .expect("write external peppy config");
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
fn external_login_succeeds_without_a_daemon_control_socket() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "external-access-token");
    let _me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    write_external_zenoh_config(&dir);
    let peppy_dirs = PeppyDirs::new(dir.path());
    let control_socket = peppy_dirs
        .runtime_config_dir()
        .join("federation_control.sock");
    assert!(
        !control_socket.exists(),
        "the test must start without a daemon control socket"
    );

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(peppy_dirs),
    }
    .execute(&ctx())
    .expect("external login must not require a running daemon");

    assert!(
        !control_socket.exists(),
        "external login must not create or require federation control"
    );
    let creds = storage::load(&creds_path(&dir)).expect("load external login credentials");
    assert_eq!(
        creds
            .session
            .expect("external login session")
            .access_token
            .expose_secret(),
        "external-access-token"
    );
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

#[cfg(unix)]
#[test]
fn external_logout_does_not_poke_federation_control() {
    use std::io::{ErrorKind, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let server = MockServer::start();
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });

    let dir = tempfile::tempdir().expect("temp dir");
    write_external_zenoh_config(&dir);
    let path = creds_path(&dir);
    storage::save(
        &path,
        &Credentials {
            session: Some(seeded_creds(&server, 9_999_999_999)),
            ..Default::default()
        },
    )
    .expect("seed creds");

    let peppy_dirs = PeppyDirs::new(dir.path());
    let runtime = peppy_dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let listener = UnixListener::bind(runtime.join("federation_control.sock"))
        .expect("bind federation-control trap");
    listener
        .set_nonblocking(true)
        .expect("make federation-control trap nonblocking");
    let command_finished = Arc::new(AtomicBool::new(false));
    let monitor_finished = Arc::clone(&command_finished);
    let monitor = std::thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .write_all(b"{\"status\":\"ok\",\"applied\":null}\n")
                        .expect("reply to unexpected poke");
                    return true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if monitor_finished.load(Ordering::SeqCst) {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("federation-control trap failed: {error}"),
            }
        }
    });

    let result = LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(peppy_dirs),
    }
    .execute(&ctx());
    command_finished.store(true, Ordering::SeqCst);
    let poked = monitor.join().expect("join federation-control trap");

    result.expect("external logout succeeds");
    assert!(!poked, "external logout must not poke federation control");
    assert!(logout.calls() >= 1, "POST /logout should have been called");
    assert!(
        storage::load(&path).expect("load creds").session.is_none(),
        "external logout clears local credentials"
    );
}

#[test]
fn logout_heals_a_malformed_credentials_file() {
    // A malformed (e.g. pre-`organization_id`/unversioned) credentials file fails
    // to parse with `AuthError::Auth`. Logout treats that as "already logged out",
    // but it must still rewrite the file to a clean default so the bad file does
    // not linger on disk (the early "Not logged in" return used to skip the save).
    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir conf");
    // Unversioned old shape ⇒ rejected by `storage::load` with `AuthError::Auth`.
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
