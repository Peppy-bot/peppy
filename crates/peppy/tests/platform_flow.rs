//! Command-level platform tests (`peppy platform login` / `logout` / `whoami`) with
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
use daemon::state::DaemonState;
use peppy::commands::Command;
use peppy::commands::platform::list::ListCommand;
use peppy::commands::platform::login::LoginCommand;
use peppy::commands::platform::logout::LogoutCommand;
use peppy::commands::platform::whoami::WhoamiCommand;
use peppy::commands::platform::{PlatformCommand, PlatformCommands};
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

/// Writes a config that pins `core_node_name`, so logout can resolve the name to
/// deregister without a daemon ever having run in the tempdir. The daemon state
/// file takes precedence over this in production; the precedence itself is unit
/// tested next to the resolver.
fn write_core_node_name_config(dir: &tempfile::TempDir, core_node_name: &str) {
    let config_dir = dir.path().join("conf");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("peppy_config.json5"),
        format!("{{ core_node_name: \"{core_node_name}\" }}"),
    )
    .expect("write peppy config");
}

/// Seed a logged-in session under `dir` and run logout against `server`, with
/// `core_node_name` pinned in the config so the deregistration has a name.
/// Returns the credentials path so the caller can assert the session was cleared.
fn logout_with_core_node(
    server: &MockServer,
    dir: &tempfile::TempDir,
    core_node_name: &str,
) -> PathBuf {
    write_core_node_name_config(dir, core_node_name);
    let path = creds_path(dir);
    storage::save(
        &path,
        &Credentials {
            session: Some(seeded_creds(server, 9_999_999_999)),
            ..Default::default()
        },
    )
    .expect("seed creds");

    LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
    }
    .execute(&ctx())
    .expect("logout");
    path
}

#[test]
fn logout_deregisters_this_machines_core_node() {
    let server = MockServer::start();
    // Matching on the header as well as the path means an unauthenticated DELETE
    // (or one under some other token) does not match, and `calls()` reads 0.
    // Ordering against the revocation is deliberately NOT asserted here: the
    // `/logout` mock is stateless, so a DELETE sent after it would look
    // identical, and httpmock exposes no ordered request log to tell them apart.
    // What ordering exists for, keeping a live token under the DELETE, is
    // covered from the other side by
    // `deregistration_never_refreshes_the_token_logout_is_about_to_revoke`.
    let deregister = server.mock(|when, then| {
        when.method(DELETE)
            .path("/me/core-nodes/cn-logout-me")
            .header("Authorization", "Bearer seeded-access");
        then.status(204);
    });
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = logout_with_core_node(&server, &dir, "cn-logout-me");

    assert_eq!(
        deregister.calls(),
        1,
        "logout must deregister this machine's core node"
    );
    assert!(logout.calls() >= 1, "POST /logout should have been called");
    assert!(
        storage::load(&path).expect("load creds").session.is_none(),
        "local credentials must be removed after logout"
    );
}

#[test]
fn deregistration_never_refreshes_the_token_logout_is_about_to_revoke() {
    // A 401 is the one trigger the refreshing `authed_*` helpers act on. If
    // `deregister_core_node` ever routes through them, it mints and persists a
    // NEW access token here, and the revocation immediately after would still
    // revoke the old one, leaving the new token valid until its own expiry. That
    // fires whenever the access token has expired but the refresh token has not,
    // which is the ordinary state of an idle CLI.
    //
    // A refresh does OIDC discovery and then posts to the token endpoint, so
    // mounting both and asserting neither is touched catches it. The type
    // signature (`&str`, not `&mut Credential`) is what makes it impossible;
    // this is the behavioral guard that fails if the signature is widened.
    let server = MockServer::start();
    let base = server.base_url();
    let discovery = server.mock(|when, then| {
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
            "refresh_token": "refreshed-refresh",
            "expires_in": 3600,
            "token_type": "Bearer",
            "scope": "openid",
        }));
    });
    let deregister = server.mock(|when, then| {
        when.method(DELETE).path("/me/core-nodes/cn-stale-token");
        then.status(401);
    });
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = logout_with_core_node(&server, &dir, "cn-stale-token");

    // Asserted before the call count, so a refreshing implementation reports the
    // property it broke rather than the retry that broke it.
    assert_eq!(
        discovery.calls(),
        0,
        "deregistration must not begin a token refresh"
    );
    assert_eq!(
        token.calls(),
        0,
        "deregistration must not mint a token the revocation that follows would not cover"
    );
    assert_eq!(
        deregister.calls(),
        1,
        "the 401 is reported, not retried under a new token"
    );
    assert!(logout.calls() >= 1, "a 401 must not stop the revocation");
    assert!(
        storage::load(&path).expect("load creds").session.is_none(),
        "a 401 must not stop the local credentials being cleared"
    );
}

#[test]
fn logout_completes_when_deregistration_finds_nothing_to_remove() {
    // A 404 is silent success: a daemon in external mode never registered, and a
    // repeated logout has nothing left to remove.
    let server = MockServer::start();
    let deregister = server.mock(|when, then| {
        when.method(DELETE)
            .path("/me/core-nodes/cn-never-registered");
        then.status(404);
    });
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = logout_with_core_node(&server, &dir, "cn-never-registered");

    assert_eq!(deregister.calls(), 1);
    assert!(logout.calls() >= 1, "a 404 must not stop the revocation");
    assert!(
        storage::load(&path).expect("load creds").session.is_none(),
        "a 404 must not stop the local credentials being cleared"
    );
}

#[test]
fn logout_completes_when_deregistration_fails() {
    // Every deregistration failure is best effort, exactly like the revocation
    // itself: the row is left behind and logout still finishes.
    let server = MockServer::start();
    let deregister = server.mock(|when, then| {
        when.method(DELETE).path("/me/core-nodes/cn-backend-down");
        then.status(503);
    });
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let path = logout_with_core_node(&server, &dir, "cn-backend-down");

    assert_eq!(deregister.calls(), 1);
    assert!(logout.calls() >= 1, "a 503 must not stop the revocation");
    assert!(
        storage::load(&path).expect("load creds").session.is_none(),
        "a 503 must not stop the local credentials being cleared"
    );
}

#[test]
fn logout_without_a_resolvable_core_node_name_sends_no_deregistration() {
    // No daemon state file and no configured name: there is nothing to delete by,
    // and nothing is guessed. The row is left behind and logout still completes.
    let server = MockServer::start();
    let deregister = server.mock(|when, then| {
        // Any DELETE at all, so the assertion covers a guessed name as well as
        // the right one.
        when.method(DELETE);
        then.status(204);
    });
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
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

    LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
    }
    .execute(&ctx())
    .expect("logout");

    assert_eq!(
        deregister.calls(),
        0,
        "an unresolvable name must not produce a guessed DELETE"
    );
    assert!(logout.calls() >= 1);
    assert!(storage::load(&path).expect("load creds").session.is_none());
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
    // A malformed (e.g. pre-`workspace_id`/unversioned) credentials file fails
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

// ─── `peppy platform list` ────────────────────────────────────────────────

const WORKSPACE: &str = "4f1b2e2c-9a71-4d0e-b3c8-0d2b9f6a11c4";

/// A tempdir with a seeded session credential pointing at `server`, ready for a
/// command that needs to be authenticated.
fn authenticated_dir(server: &MockServer) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let creds = Credentials {
        session: Some(seeded_creds(server, 9_999_999_999)),
        ..Default::default()
    };
    storage::save(&creds_path(&dir), &creds).expect("seed creds");
    dir
}

/// Mocks `GET /me/core-nodes` with one registered, online core node.
fn mock_core_nodes<'a>(server: &'a MockServer, core_node_name: &str) -> httpmock::Mock<'a> {
    server.mock(|when, then| {
        when.method(GET).path("/me/core-nodes");
        then.status(200).json_body(json!({
            "workspace_id": WORKSPACE,
            "application_status_available": true,
            "core_nodes": [{
                "core_node_name": core_node_name,
                "registered": true,
                "first_seen_at": "2026-07-01T10:00:00Z",
                "last_config_pull_at": "2026-07-24T09:12:33Z",
                "application": { "status": "online", "live_claimants": 1 },
            }],
        }));
    })
}

fn list_in(server: &MockServer, dir: &tempfile::TempDir, json: bool) -> peppy::error::Result<()> {
    ListCommand {
        api_url: Some(server.base_url()),
        json,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
    }
    .execute(&ctx())
}

#[test]
fn list_renders_the_workspace_roster_in_both_formats() {
    let server = MockServer::start();
    let core_nodes = mock_core_nodes(&server, "cn-a1b2c3d4e5");
    let dir = authenticated_dir(&server);

    for json in [false, true] {
        list_in(&server, &dir, json).expect("list should succeed");
    }

    core_nodes.assert_calls(2);
}

/// Unlike `whoami`, whose output IS the sign-in state, `list` cannot answer at
/// all without a credential, so it fails rather than printing a document
/// saying so. `main` maps the error to exit 1.
#[test]
fn list_without_a_credential_fails_instead_of_emitting_a_document() {
    let server = MockServer::start();
    // No credentials seeded.
    let dir = tempfile::tempdir().expect("temp dir");

    for json in [false, true] {
        let error = list_in(&server, &dir, json).expect_err("list must fail unauthenticated");
        assert!(
            error.to_string().contains("peppy platform login"),
            "the error must say how to fix it: {error}"
        );
    }
}

#[test]
fn list_fails_when_the_backend_is_unreachable() {
    let server = MockServer::start();
    let dir = authenticated_dir(&server);
    // A port nothing listens on, so the request cannot complete. There is
    // deliberately no local fallback: a local query would answer a different
    // question than the one the command claims to answer.
    let unreachable = "http://127.0.0.1:1";

    let error = ListCommand {
        api_url: Some(unreachable.to_string()),
        json: false,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
    }
    .execute(&ctx())
    .expect_err("an unreachable backend must fail the command");

    assert!(
        !error.to_string().is_empty(),
        "the failure must carry a message"
    );
}

#[test]
fn list_surfaces_a_backend_outage_as_a_retryable_error() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/me/core-nodes");
        then.status(503);
    });
    let dir = authenticated_dir(&server);

    let error = list_in(&server, &dir, false).expect_err("a 503 must fail the command");

    assert!(
        error.to_string().contains("try again"),
        "a 503 must read as transient: {error}"
    );
}

/// A newer CLI against a backend that predates the endpoint. Without the
/// explicit mapping this reads as an unexplained `returned 404`.
#[test]
fn list_explains_a_backend_that_does_not_have_the_endpoint() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/me/core-nodes");
        then.status(404);
    });
    let dir = authenticated_dir(&server);

    let error = list_in(&server, &dir, false).expect_err("a 404 must fail the command");

    assert!(
        error.to_string().contains("upgrade the platform"),
        "a 404 must name the cause and the fix: {error}"
    );
}

/// `--core-node` redirects a command at another machine's daemon, which no
/// `platform` command does. The whole group refuses it, rather than each
/// command silently answering a different question.
#[test]
fn every_platform_command_refuses_a_core_node_override() {
    let server = MockServer::start();
    let dir = authenticated_dir(&server);
    let redirected = Arc::new(
        AppContext::from_current_dir()
            .expect("cwd is readable")
            .with_core_node_override(Some("robot-7".to_string())),
    );

    let commands: Vec<(&str, PlatformCommands)> = vec![
        (
            "list",
            PlatformCommands::List {
                api_url: Some(server.base_url()),
                json: false,
            },
        ),
        (
            "whoami",
            PlatformCommands::Whoami {
                api_url: Some(server.base_url()),
                json: false,
            },
        ),
        (
            "logout",
            PlatformCommands::Logout {
                api_url: Some(server.base_url()),
                yes: true,
            },
        ),
        (
            "login",
            PlatformCommands::Login {
                api_url: Some(server.base_url()),
                no_browser: true,
                yes: true,
            },
        ),
    ];

    for (name, command) in commands {
        let error = PlatformCommand { command }
            .execute(&redirected)
            .expect_err(&format!("`platform {name}` must refuse --core-node"));
        assert!(
            error.to_string().contains("--core-node"),
            "`platform {name}` must name the flag it refused: {error}"
        );
        assert!(
            error.to_string().contains("peppy stack list"),
            "`platform {name}` must point at the command that does show other core nodes: {error}"
        );
    }

    // The refusal happens before any work: the backend was never called.
    server.mock(|when, then| {
        when.method(GET).path("/me/core-nodes");
        then.status(500);
    });
    let _ = dir;
}

/// A stale daemon state file must not fail the command, whatever it records.
///
/// The marker RULES are pinned as a pure unit in `platform::list`
/// (`this_machine_name`), where each case is asserted directly rather than
/// inferred from a command that succeeded. What this covers is the wiring: a
/// state file on disk is read, and a daemon running in another workspace is a
/// normal case rather than an error.
#[test]
fn a_daemon_in_another_workspace_does_not_fail_the_listing() {
    let server = MockServer::start();
    let core_nodes = mock_core_nodes(&server, "cn-local-daemon");
    let dir = authenticated_dir(&server);
    // Same core-node name as the listed row, different workspace: the
    // mid-login case.
    write_daemon_state(&dir, "cn-local-daemon", "local");

    list_in(&server, &dir, false).expect("a mismatched namespace must not fail the command");

    core_nodes.assert();
}

/// Writes a daemon state file recording `core_node_name` under `namespace`.
fn write_daemon_state(dir: &tempfile::TempDir, core_node_name: &str, namespace: &str) {
    let state = DaemonState::new(
        core_node_name,
        "127.0.0.1",
        7447,
        "test-git-hash",
        30,
        config::namespace::Namespace::parse(namespace).expect("valid namespace"),
        Some(30),
    );
    let path = DaemonState::state_file_in(dir.path());
    std::fs::create_dir_all(path.parent().expect("state file has a parent"))
        .expect("state file dir");
    DaemonState::write_to(&path, &state).expect("write daemon state");
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
