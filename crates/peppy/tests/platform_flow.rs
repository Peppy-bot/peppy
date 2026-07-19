//! Command-level platform tests (`peppy platform login` / `logout` / `whoami`)
//! with every HTTP endpoint mocked (`httpmock`): the public `/cli/auth-config`,
//! OIDC discovery, the Zitadel device/token endpoints, and the backend `/me` +
//! `/logout`. All auth state is isolated per test via the `peppy_dirs` seam
//! pointed at a tempdir (no `PEPPY_HOME` mutation, so tests run in parallel);
//! the credentials file and `peppy_config.json5` both land there, and the PAT
//! rides the command's injected `pat` field, never the environment. The engine
//! internals (resolver, router-config cache, federation-target resolution) are
//! covered by the `auth` crate's own tests.

use std::path::PathBuf;
use std::sync::Arc;

use daemon_config::consts::PeppyDirs;
use httpmock::prelude::*;
use secrecy::ExposeSecret;
use serde_json::json;

#[cfg(not(debug_assertions))]
use httpmock::{HttpMockRequest, HttpMockResponse};
#[cfg(not(debug_assertions))]
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType,
};
#[cfg(not(debug_assertions))]
use time::{Duration as TimeDuration, OffsetDateTime, format_description::well_known::Rfc3339};

use auth::storage::{self, Credentials, ProfileCreds};
use peppy::commands::Command;
use peppy::commands::platform::login::LoginCommand;
use peppy::commands::platform::logout::LogoutCommand;
use peppy::commands::platform::whoami::WhoamiCommand;
use peppy::context::AppContext;

const CORE_NODE: &str = "core-node-platform-flow";
const WORKSPACE: &str = "550e8400-e29b-41d4-a716-446655440000";

/// Builds the cli/auth-config + OIDC discovery + device-authorization + token mocks
/// for a server whose issuer is its own base URL, with the device grant
/// succeeding immediately. Returns nothing; the mocks live on the server.
fn mock_login_endpoints(server: &MockServer, access_token: &str) {
    let base = server.base_url();

    server.mock(|when, then| {
        when.method(GET).path("/cli/auth-config");
        then.status(200).json_body(json!({
            "issuer": base.clone(),
            "client_id": "cli-client-id",
            "scopes": "openid profile email offline_access urn:zitadel:iam:org:project:id:proj-id:aud",
        }));
    });

    server.mock(|when, then| {
        when.method(GET).path("/.well-known/openid-configuration");
        then.status(200).json_body(json!({
            "issuer": base.clone(),
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

/// One-shot managed-daemon stand-in for fail-closed login cleanup. The distinct
/// `defederate` verb proves cleanup does not re-resolve retained auth state.
fn stub_fail_closed_poke(dirs: &PeppyDirs) -> std::thread::JoinHandle<String> {
    stub_federation_poke(
        dirs,
        b"{\"status\":\"ok\",\"endpoint\":null,\"link_state\":\"not_configured\"}\n",
    )
}

#[cfg(not(debug_assertions))]
fn stub_verified_poke(dirs: &PeppyDirs) -> std::thread::JoinHandle<String> {
    stub_federation_poke(
        dirs,
        b"{\"status\":\"ok\",\"endpoint\":\"tls/hub:7447\",\"link_state\":\"verified\"}\n",
    )
}

fn stub_federation_poke(dirs: &PeppyDirs, reply: &'static [u8]) -> std::thread::JoinHandle<String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    let runtime = dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let socket = runtime.join("federation_control.sock");
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove stale control socket: {error}"),
    }
    let listener = UnixListener::bind(socket).expect("bind test control socket");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept federation poke");
        let mut reader = BufReader::new(stream.try_clone().expect("clone control stream"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read poke request");
        stream.write_all(reply).expect("reply to federation poke");
        line.trim().to_string()
    })
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

/// Release login must bind enrollment to the exact name captured by a live
/// daemon generation. The current test process is a live PID and makes a safe
/// state-file stand-in; `federation=None` models an external-router daemon.
fn write_running_daemon_state(dir: &tempfile::TempDir, federation: Option<u64>) {
    let state = daemon::state::DaemonState::new(
        CORE_NODE,
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        "platform-flow-test",
        5,
        config::namespace::Namespace::local(),
        federation,
    )
    .with_service_pat_active(false);
    daemon::state::DaemonState::write_to(
        &daemon::state::DaemonState::state_file_in(dir.path()),
        &state,
    )
    .expect("write live daemon state");
}

/// Signs the request's proof-of-possession CSR with a test CA while replacing
/// every identity/profile field with server-controlled values. This lets the
/// release integration suite exercise the same strict validator as production
/// without weakening or bypassing enrollment under `cfg(test)`.
#[cfg(not(debug_assertions))]
fn mock_certificate_enrollment(server: &MockServer) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(POST).path("/me/cli/core-node-certificates");
        then.respond_with(|request: &HttpMockRequest| enrollment_response(request, WORKSPACE));
    })
}

#[cfg(not(debug_assertions))]
fn enrollment_response(request: &HttpMockRequest, workspace: &str) -> HttpMockResponse {
    let body: serde_json::Value =
        serde_json::from_slice(request.body_ref()).expect("parse enrollment request");
    let core_node_name = body["core_node_name"]
        .as_str()
        .expect("core-node name in enrollment request");
    let csr_pem = body["csr_pem"].as_str().expect("CSR in enrollment request");
    assert_eq!(core_node_name, CORE_NODE);

    let now = OffsetDateTime::now_utc();
    let not_before = now - TimeDuration::minutes(1);
    let not_after = now + TimeDuration::hours(24);
    let renew_after = now + TimeDuration::hours(12);

    let ca_key = KeyPair::generate().expect("generate test CA key");
    let mut ca_params = CertificateParams::default();
    ca_params.distinguished_name = DistinguishedName::new();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
    ];
    ca_params.not_before = not_before;
    ca_params.not_after = not_after + TimeDuration::days(1);
    let ca = ca_params.self_signed(&ca_key).expect("sign test CA");
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let mut csr = CertificateSigningRequestParams::from_pem(csr_pem)
        .expect("parse and verify enrollment CSR");
    csr.params.distinguished_name = DistinguishedName::new();
    csr.params
        .distinguished_name
        .push(DnType::CommonName, core_node_name);
    csr.params.is_ca = IsCa::ExplicitNoCa;
    csr.params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    csr.params.subject_alt_names = vec![SanType::URI(
        format!("peppy://platform/workspaces/{workspace}/core-nodes/{core_node_name}")
            .try_into()
            .expect("valid identity URI"),
    )];
    csr.params.serial_number = Some(vec![0x01, 0x9a, 0xbc, 0xde].into());
    csr.params.not_before = not_before;
    csr.params.not_after = not_after;
    let leaf = csr.signed_by(&issuer).expect("sign requested public key");

    let response = json!({
        "core_node_name": core_node_name,
        "workspace_id": workspace,
        "certificate_chain_pem": format!("{}{}", leaf.pem(), ca.pem()),
        "serial_number": "01:9a:bc:de",
        "not_before": not_before.format(&Rfc3339).expect("format not_before"),
        "not_after": not_after.format(&Rfc3339).expect("format not_after"),
        "renew_after": renew_after.format(&Rfc3339).expect("format renew_after"),
    });
    HttpMockResponse::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(response.to_string())
        .build()
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
        pat: None,
    }
    .execute(&ctx())
    .expect_err("login fails strictly when no daemon is running to federate");
    #[cfg(debug_assertions)]
    assert!(
        err.to_string().contains("federation"),
        "the error explains the federation failure: {err}"
    );
    #[cfg(not(debug_assertions))]
    assert!(
        err.to_string().contains("running daemon state"),
        "release login explains that exact-name enrollment needs a live daemon: {err}"
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
fn oauth_error_after_session_publication_pokes_fail_closed_and_retains_session() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "new-account-access");
    let me = server.mock(|when, then| {
        when.method(GET).path("/me");
        then.status(503);
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let peppy_dirs = PeppyDirs::new(dir.path());
    let stub = stub_fail_closed_poke(&peppy_dirs);

    let error = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(peppy_dirs.clone()),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("identity lookup failure must fail login after saving its session");

    assert!(
        error
            .to_string()
            .contains("authenticated platform identity could not be resolved"),
        "{error}"
    );
    assert_eq!(me.calls(), 1);
    assert_eq!(
        stub.join().expect("fail-closed stub"),
        "defederate",
        "every post-publication OAuth error must poke the daemon fail closed"
    );
    let credentials = storage::load(&creds_path(&dir)).expect("load retained session");
    let session = credentials.session.expect("new OAuth session is retained");
    assert_eq!(session.access_token.expose_secret(), "new-account-access");
    assert!(credentials.router.is_none());
    assert!(
        auth::identity::binding_incomplete(&peppy_dirs).unwrap(),
        "the crash-durable gate remains armed until a retry completes identity handoff"
    );
}

#[cfg(not(debug_assertions))]
#[test]
fn same_subject_enrollment_failure_cannot_reuse_prior_identity() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let server = MockServer::start();
    mock_login_endpoints(&server, "same-subject-access");
    let _me = mock_me(&server);
    let attempt = Arc::new(AtomicUsize::new(0));
    let next_attempt = Arc::clone(&attempt);
    let enrollments = server.mock(|when, then| {
        when.method(POST).path("/me/cli/core-node-certificates");
        then.respond_with(move |request: &HttpMockRequest| {
            if next_attempt.fetch_add(1, Ordering::SeqCst) == 0 {
                enrollment_response(request, WORKSPACE)
            } else {
                HttpMockResponse::builder().status(503).build()
            }
        });
    });

    let dir = tempfile::tempdir().expect("temp dir");
    write_running_daemon_state(&dir, Some(30));
    let dirs = PeppyDirs::new(dir.path());

    let first_poke = stub_verified_poke(&dirs);
    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(dirs.clone()),
        pat: None,
    }
    .execute(&ctx())
    .expect("establish prior same-subject identity");
    assert_eq!(first_poke.join().unwrap(), "refederate");
    assert!(!auth::identity::binding_incomplete(&dirs).unwrap());

    // The test daemon only acknowledges the poke, so explicitly consume and
    // commit the handed-off receipt to model its successful apply/probe.
    let rotation = auth::identity::maintain_identity(
        &dirs,
        &auth::http::HttpClient::new(),
        &server.base_url(),
        None,
        CORE_NODE,
    )
    .expect("recover handed-off rotation")
    .expect("unverified rotation exists");
    rotation
        .commit_after_probe()
        .expect("commit prior identity");
    let prior = auth::identity::load_identity_metadata(&dirs)
        .unwrap()
        .expect("prior identity");

    let cleanup_poke = stub_fail_closed_poke(&dirs);
    let error = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(dirs.clone()),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("second same-subject enrollment is unavailable");

    assert!(error.to_string().contains("enrollment failed"), "{error}");
    assert_eq!(cleanup_poke.join().unwrap(), "defederate");
    assert!(auth::identity::binding_incomplete(&dirs).unwrap());
    assert_eq!(
        auth::identity::load_identity_metadata(&dirs)
            .unwrap()
            .expect("prior identity retained"),
        prior
    );

    let calls_before_resolve = enrollments.calls();
    let resolved = auth::router::resolve_federation_target(
        &dirs,
        &server.base_url(),
        std::time::Duration::from_secs(1),
        CORE_NODE,
    );
    assert!(resolved.upstream.is_none());
    assert!(resolved.resolve_error.is_none());
    assert!(resolved.rotation.is_none());
    assert_eq!(
        enrollments.calls(),
        calls_before_resolve,
        "the daemon must not maintain or reuse the retained same-subject identity while the marker is armed"
    );
}

#[test]
fn external_login_succeeds_without_a_daemon_control_socket() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "external-access-token");
    let _me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    write_external_zenoh_config(&dir);
    #[cfg(not(debug_assertions))]
    write_running_daemon_state(&dir, None);
    #[cfg(not(debug_assertions))]
    let enrollment = mock_certificate_enrollment(&server);
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
        pat: None,
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
    #[cfg(not(debug_assertions))]
    assert_eq!(
        enrollment.calls(),
        1,
        "release external mode still enrolls a local core-node identity"
    );
}

#[cfg(not(debug_assertions))]
#[test]
fn denied_workspace_discovery_rotates_then_retries_with_the_new_certificate() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CURRENT_WORKSPACE: &str = "7a040224-4dd3-4b73-8d09-f809b176ed2d";

    let server = MockServer::start();
    let _me = mock_me(&server);
    let enrollment_index = Arc::new(AtomicUsize::new(0));
    let next_enrollment = Arc::clone(&enrollment_index);
    let enrollments = server.mock(|when, then| {
        when.method(POST).path("/me/cli/core-node-certificates");
        then.respond_with(move |request: &HttpMockRequest| {
            let workspace = if next_enrollment.fetch_add(1, Ordering::SeqCst) == 0 {
                WORKSPACE
            } else {
                CURRENT_WORKSPACE
            };
            enrollment_response(request, workspace)
        });
    });

    let discovery_index = Arc::new(AtomicUsize::new(0));
    let next_discovery = Arc::clone(&discovery_index);
    let discoveries = server.mock(|when, then| {
        when.method(POST)
            .path("/me/cli/federation")
            .json_body(json!({ "core_node_name": CORE_NODE }));
        then.respond_with(move |_request: &HttpMockRequest| {
            let body = if next_discovery.fetch_add(1, Ordering::SeqCst) == 0 {
                json!({
                    "error": "core_node_workspace_mismatch",
                    "message": "certificate belongs to the former workspace",
                    "workspace_id": CURRENT_WORKSPACE,
                })
            } else {
                json!({
                    "endpoint": "tls/current-workspace.example:7443",
                    "protocol": "tls",
                    "reconnect_after_secs": 3000,
                    "workspace_id": CURRENT_WORKSPACE,
                })
            };
            HttpMockResponse::builder()
                .status(if body.get("error").is_some() {
                    409
                } else {
                    200
                })
                .header("content-type", "application/json")
                .body(body.to_string())
                .build()
        });
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let dirs = PeppyDirs::new(dir.path());
    let path = creds_path(&dir);
    let session = seeded_creds(&server, 9_999_999_999);
    storage::save(
        &path,
        &Credentials {
            session: Some(session.clone()),
            ..Default::default()
        },
    )
    .expect("seed credentials");
    let http = auth::http::HttpClient::new();
    let mut credential = auth::resolver::session_credential(&path, &session);
    let old_rotation = auth::identity::enroll_and_activate(
        &dirs,
        &http,
        &server.base_url(),
        &mut credential,
        "user-123",
        CORE_NODE,
    )
    .expect("enroll the former-workspace certificate");
    let old_generation = old_rotation.activated().active_generation.clone();
    old_rotation
        .commit_after_probe()
        .expect("accept former-workspace fixture");

    let mut resolved = auth::router::resolve_federation_target(
        &dirs,
        &server.base_url(),
        std::time::Duration::from_secs(2),
        CORE_NODE,
    );
    assert!(
        resolved.upstream.is_some(),
        "retry returns a usable upstream"
    );
    assert_eq!(resolved.namespace.as_str(), CURRENT_WORKSPACE);
    let new_rotation = resolved
        .rotation
        .take()
        .expect("workspace denial activates a replacement generation");
    assert_eq!(
        new_rotation.activated().workspace_id.as_str(),
        CURRENT_WORKSPACE
    );
    assert_ne!(
        new_rotation.activated().active_generation,
        old_generation,
        "workspace rebinding must use a fresh key generation"
    );
    new_rotation
        .commit_after_probe()
        .expect("accept replacement fixture");
    assert_eq!(enrollments.calls(), 2, "initial plus forced re-enrollment");
    assert_eq!(
        discoveries.calls(),
        2,
        "the denied discovery is retried only after re-enrollment"
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
    #[cfg(not(debug_assertions))]
    write_running_daemon_state(&dir, Some(30));
    #[cfg(not(debug_assertions))]
    let enrollment = mock_certificate_enrollment(&server);
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
            .write_all(
                b"{\"status\":\"ok\",\"endpoint\":\"tls/hub:7447\",\"link_state\":\"verified\"}\n",
            )
            .expect("reply");
        line.trim().to_string()
    });

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(peppy_dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect("login should succeed");

    let request = stub.join().expect("stub thread");
    assert_eq!(
        request, "refederate",
        "login must poke the daemon to refederate"
    );
    #[cfg(not(debug_assertions))]
    assert_eq!(
        enrollment.calls(),
        1,
        "release login enrolls before its poke"
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
        pat: None,
    }
    .execute(&ctx());

    // A login on a machine that never ran the daemon still seeds
    // peppy_config.json5 with the resource_servers block for this build profile.
    let config = std::fs::read_to_string(dir.path().join("conf").join("peppy_config.json5"))
        .expect("peppy_config.json5 was created by the CLI");
    assert!(
        config.contains("resource_servers:"),
        "resource_servers block missing:\n{config}"
    );
    assert!(
        config.contains(&format!(
            r#"api: "{}""#,
            daemon_config::peppy_config::DEFAULT_API_URL
        )),
        "build-default api URL missing:\n{config}"
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
        pat: None,
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
        pat: None,
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
fn already_logged_out_cleanup_removes_incomplete_binding_marker() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().expect("temp dir");
    let dirs = PeppyDirs::new(dir.path());
    auth::identity::arm_binding_incomplete(&dirs).expect("arm crashed-login marker");

    LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(dirs.clone()),
        pat: None,
    }
    .execute(&ctx())
    .expect("already-logged-out cleanup succeeds");

    assert!(!auth::identity::binding_incomplete(&dirs).unwrap());
}

#[cfg(unix)]
#[test]
fn already_logged_out_managed_replay_refederates_into_local_namespace() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    let dir = tempfile::tempdir().expect("temp dir");
    let dirs = PeppyDirs::new(dir.path());
    write_running_daemon_state(&dir, Some(1));
    let state_path = daemon::state::DaemonState::state_file_in(dir.path());
    let mut state = daemon::state::DaemonState::read_from(&state_path).unwrap();
    state.namespace = config::namespace::Namespace::parse(WORKSPACE).unwrap();
    daemon::state::DaemonState::write_to(&state_path, &state).unwrap();

    let runtime = dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime).unwrap();
    let listener = UnixListener::bind(runtime.join("federation_control.sock")).unwrap();
    let replay_state_path = state_path.clone();
    let stub = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for request_number in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept control request");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request = line.trim().to_string();
            match request_number {
                0 => stream
                    .write_all(
                        b"{\"status\":\"federation_status\",\"endpoint\":\"tls/hub:7447\",\"link_state\":\"verified\",\"pinned\":false}\n",
                    )
                    .unwrap(),
                1 => {
                    // Model the restarted generation writing its new namespace
                    // before binding the path-stable control socket.
                    let mut restarted =
                        daemon::state::DaemonState::read_from(&replay_state_path).unwrap();
                    restarted.namespace = config::namespace::Namespace::local();
                    daemon::state::DaemonState::write_to(&replay_state_path, &restarted).unwrap();
                    stream
                        .write_all(
                            b"{\"status\":\"restarting\",\"target_namespace\":\"local\"}\n",
                        )
                        .unwrap();
                }
                2 => stream
                    .write_all(
                        b"{\"status\":\"ok\",\"endpoint\":null,\"link_state\":\"not_configured\"}\n",
                    )
                    .unwrap(),
                _ => unreachable!(),
            }
            requests.push(request);
        }
        requests
    });

    LogoutCommand {
        api_url: Some("http://127.0.0.1:9".to_string()),
        yes: true,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect("crash replay must finish managed logout");

    assert_eq!(
        stub.join().unwrap(),
        vec!["status", "refederate", "refederate"],
        "ordinary logout must resolve the cleared credentials, request the namespace restart, and confirm the settled local generation"
    );
    assert_eq!(
        daemon::state::DaemonState::read_from(&state_path)
            .unwrap()
            .namespace,
        config::namespace::Namespace::local()
    );
}

#[test]
fn external_logout_uses_authoritative_pat_absence_without_a_control_socket() {
    let server = MockServer::start();
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });

    let dir = tempfile::tempdir().expect("temp dir");
    write_external_zenoh_config(&dir);
    write_running_daemon_state(&dir, None);
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
        pat: None,
    }
    .execute(&ctx())
    .expect("external logout succeeds after daemon state proves the service PAT is absent");
    assert!(logout.calls() >= 1, "POST /logout should have been called");
    assert!(
        storage::load(&path).expect("load creds").session.is_none(),
        "external logout clears local credentials"
    );
}

#[test]
fn external_logout_refuses_when_service_pat_state_cannot_be_proven() {
    let server = MockServer::start();
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let dir = tempfile::tempdir().expect("temp dir");
    write_external_zenoh_config(&dir);
    write_running_daemon_state(&dir, None);
    let state_path = daemon::state::DaemonState::state_file_in(dir.path());
    let mut legacy_state = daemon::state::DaemonState::read_from(&state_path).unwrap();
    legacy_state.service_pat_active = None;
    daemon::state::DaemonState::write_to(&state_path, &legacy_state).unwrap();
    let path = creds_path(&dir);
    storage::save(
        &path,
        &Credentials {
            session: Some(seeded_creds(&server, 9_999_999_999)),
            ..Default::default()
        },
    )
    .expect("seed creds");

    let error = LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("external router ownership cannot prove the service PAT is absent");

    assert!(error.to_string().contains("cannot verify"), "{error}");
    assert_eq!(logout.calls(), 0, "backend logout must not be called");
    assert!(
        storage::load(&path)
            .expect("reload credentials")
            .session
            .is_some(),
        "credentials remain untouched until daemon PAT state is knowable"
    );
}

#[test]
fn external_logout_refuses_service_pat_before_any_mutation() {
    let server = MockServer::start();
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let dir = tempfile::tempdir().expect("temp dir");
    write_external_zenoh_config(&dir);
    write_running_daemon_state(&dir, None);
    let state_path = daemon::state::DaemonState::state_file_in(dir.path());
    let mut state = daemon::state::DaemonState::read_from(&state_path).unwrap();
    state.service_pat_active = Some(true);
    daemon::state::DaemonState::write_to(&state_path, &state).unwrap();
    let path = creds_path(&dir);
    storage::save(
        &path,
        &Credentials {
            session: Some(seeded_creds(&server, 9_999_999_999)),
            ..Default::default()
        },
    )
    .expect("seed creds");

    let error = LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("service-environment PAT must block external logout");

    assert!(error.to_string().contains("service environment"), "{error}");
    assert_eq!(logout.calls(), 0, "backend logout must not be called");
    assert!(
        storage::load(&path)
            .expect("reload credentials")
            .session
            .is_some(),
        "credentials remain untouched while the service PAT is active"
    );
}

#[test]
fn logout_heals_a_malformed_credentials_file() {
    // A malformed credentials file fails to parse with `AuthError::Auth`.
    // Logout treats that as "already logged out",
    // but it must still rewrite the file to a clean default so the bad file does
    // not linger on disk (the early "Not logged in" return used to skip the save).
    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir conf");
    std::fs::write(&path, "{ this is not valid JSON5").expect("write malformed creds");

    LogoutCommand {
        // Never contacted: the malformed path returns "Not logged in" before any
        // backend call. A dummy keeps the test independent of build-default URLs.
        api_url: Some("http://127.0.0.1:9".to_string()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
        pat: None,
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
            pat: None,
        }
        .execute(&ctx())
        .expect("whoami");
    }
}

#[test]
fn login_with_a_pat_skips_the_device_flow_and_pokes_federation() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    let server = MockServer::start();
    let me = mock_me(&server);
    // The device flow must never start: a named mock pins zero calls.
    let device = server.mock(|when, then| {
        when.method(POST).path("/oauth/v2/device_authorization");
        then.status(500);
    });

    let dir = tempfile::tempdir().expect("temp dir");
    #[cfg(not(debug_assertions))]
    write_running_daemon_state(&dir, Some(30));
    #[cfg(not(debug_assertions))]
    let enrollment = mock_certificate_enrollment(&server);
    let peppy_dirs = PeppyDirs::new(dir.path());
    let runtime = peppy_dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let socket = runtime.join("federation_control.sock");
    let listener = UnixListener::bind(&socket).expect("bind stub control socket");
    let stub = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept poke");
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).expect("read poke request");
        stream
            .write_all(
                b"{\"status\":\"ok\",\"endpoint\":\"tls/hub:7447\",\"link_state\":\"verified\"}\n",
            )
            .expect("reply");
        line.trim().to_string()
    });

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(peppy_dirs),
        pat: Some("the-personal-access-token".to_string()),
    }
    .execute(&ctx())
    .expect("a PAT login with a verified link succeeds");

    let request = stub.join().expect("stub thread");
    assert_eq!(request, "refederate", "a PAT login pokes federation");
    assert!(me.calls() >= 1, "the PAT is verified against /me");
    assert_eq!(
        device.calls(),
        0,
        "a PAT login never starts the device flow"
    );
    #[cfg(debug_assertions)]
    assert!(
        !creds_path(&dir).exists(),
        "debug PAT login retains the shared-certificate no-file behavior"
    );
    #[cfg(not(debug_assertions))]
    {
        assert_eq!(
            enrollment.calls(),
            1,
            "release PAT login enrolls exactly once"
        );
        let persisted = std::fs::read_to_string(creds_path(&dir)).expect("v3 identity metadata");
        assert!(
            !persisted.contains("the-personal-access-token"),
            "the PAT itself must never be persisted"
        );
        let credentials = storage::load(&creds_path(&dir)).expect("load PAT identity metadata");
        assert!(
            credentials.session.is_none(),
            "PAT login stores no OAuth session"
        );
        assert!(
            credentials.core_node_identity.is_some(),
            "release PAT login persists only non-secret certificate metadata"
        );
    }
}

#[test]
fn login_with_a_pat_fails_strictly_without_a_daemon() {
    let server = MockServer::start();
    let _me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    let err = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
        pat: Some("the-personal-access-token".to_string()),
    }
    .execute(&ctx())
    .expect_err("a PAT login fails strictly when no daemon is running to federate");
    #[cfg(debug_assertions)]
    assert!(
        err.to_string().contains("federation"),
        "the error explains the federation failure: {err}"
    );
    #[cfg(not(debug_assertions))]
    assert!(
        err.to_string().contains("running daemon state"),
        "release PAT login explains that exact-name enrollment needs a live daemon: {err}"
    );
    #[cfg(debug_assertions)]
    assert!(
        !creds_path(&dir).exists(),
        "the failed debug PAT login persists nothing"
    );
    #[cfg(not(debug_assertions))]
    {
        let persisted = std::fs::read_to_string(creds_path(&dir))
            .expect("release PAT preparation may persist an empty v3 document");
        assert!(
            !persisted.contains("the-personal-access-token"),
            "a failed release PAT login must not persist its PAT"
        );
        let credentials = storage::load(&creds_path(&dir)).expect("load empty v3 credentials");
        assert!(credentials.session.is_none());
        assert!(credentials.core_node_identity.is_none());
    }
}

#[cfg(not(debug_assertions))]
#[test]
fn pat_error_after_auth_mode_change_pokes_fail_closed() {
    let server = MockServer::start();
    let _me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    storage::save(
        &path,
        &Credentials {
            session: Some(seeded_creds(&server, 9_999_999_999)),
            ..Credentials::default()
        },
    )
    .expect("seed prior OAuth mode");
    let peppy_dirs = PeppyDirs::new(dir.path());
    let stub = stub_fail_closed_poke(&peppy_dirs);

    let error = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(peppy_dirs.clone()),
        pat: Some("new-account-pat".to_string()),
    }
    .execute(&ctx())
    .expect_err("missing daemon state fails after PAT mode is durably prepared");

    assert!(
        error.to_string().contains("running daemon state"),
        "{error}"
    );
    assert_eq!(
        stub.join().expect("fail-closed stub"),
        "defederate",
        "every post-preparation PAT error must poke the daemon fail closed"
    );
    let credentials = storage::load(&path).expect("load prepared PAT credentials");
    assert!(
        credentials.session.is_none(),
        "PAT preparation must not resurrect the prior OAuth session"
    );
    let persisted = std::fs::read_to_string(path).expect("read prepared credentials");
    assert!(!persisted.contains("new-account-pat"));
    assert!(auth::identity::binding_incomplete(&peppy_dirs).unwrap());
}

#[test]
fn logout_with_a_pat_refuses_before_any_cleanup() {
    let server = MockServer::start();
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

    // The PAT cannot be cleared from here, so logout refuses before revoking the
    // OAuth token or deleting any local state.
    let error = LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
        pat: Some("the-personal-access-token".to_string()),
    }
    .execute(&ctx())
    .expect_err("logout must refuse while a PAT remains active");

    assert!(error.to_string().contains("Remove it"), "{error}");
    assert_eq!(logout.calls(), 0, "backend logout must not be called");
    let after = storage::load(&path).expect("load creds");
    assert!(
        after.session.is_some(),
        "the OAuth session must remain untouched"
    );
}

#[test]
fn logout_refuses_when_live_managed_state_has_no_control_socket() {
    let server = MockServer::start();
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
    write_running_daemon_state(&dir, Some(30));

    let error = LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("a contradictory live state and absent control socket must fail safely");

    assert!(error.to_string().contains("cannot verify"), "{error}");
    assert_eq!(logout.calls(), 0, "logout must not contact the backend");
    assert!(
        storage::load(&path)
            .expect("reload creds")
            .session
            .is_some(),
        "credentials remain untouched until daemon PAT state is knowable"
    );
}

#[test]
fn logout_allows_a_definitively_stopped_daemon_with_stale_state() {
    let server = MockServer::start();
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
    .unwrap();
    let mut stale = daemon::state::DaemonState::new(
        CORE_NODE,
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        "platform-flow-test",
        5,
        config::namespace::Namespace::local(),
        Some(30),
    );
    stale.daemon_pid = None;
    daemon::state::DaemonState::write_to(
        &daemon::state::DaemonState::state_file_in(dir.path()),
        &stale,
    )
    .unwrap();

    LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
        pat: None,
    }
    .execute(&ctx())
    .expect("a definitively stopped daemon cannot immediately re-enrol");

    assert_eq!(logout.calls(), 1);
    assert!(storage::load(&path).unwrap().session.is_none());
}

#[test]
fn logout_refuses_ambiguous_malformed_daemon_state_before_backend_calls() {
    let server = MockServer::start();
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
    .unwrap();
    std::fs::write(
        daemon::state::DaemonState::state_file_in(dir.path()),
        "{ malformed daemon state",
    )
    .unwrap();

    let error = LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(PeppyDirs::new(dir.path())),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("malformed state plus absent status must fail closed");
    assert!(error.to_string().contains("cannot verify"), "{error}");
    assert_eq!(logout.calls(), 0);
    assert!(storage::load(&path).unwrap().session.is_some());
}

#[test]
fn logout_refuses_before_remote_revocation_when_rotation_is_owned() {
    let server = MockServer::start();
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let dir = tempfile::tempdir().expect("temp dir");
    let dirs = PeppyDirs::new(dir.path());
    let path = creds_path(&dir);
    storage::save(
        &path,
        &Credentials {
            session: Some(seeded_creds(&server, 9_999_999_999)),
            ..Default::default()
        },
    )
    .unwrap();
    let _rotation_owner = auth::identity::acquire_identity_maintenance(&dirs).unwrap();

    let error = LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("logout must fail before side effects while rotation is owned");
    assert!(
        error.to_string().contains("maintenance is active"),
        "{error}"
    );
    assert_eq!(logout.calls(), 0);
    assert!(storage::load(&path).unwrap().session.is_some());
}

#[cfg(unix)]
#[test]
fn logout_pokes_standalone_even_when_local_identity_cleanup_fails() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    let server = MockServer::start();
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let dir = tempfile::tempdir().expect("temp dir");
    let dirs = PeppyDirs::new(dir.path());
    let path = creds_path(&dir);
    storage::save(
        &path,
        &Credentials {
            session: Some(seeded_creds(&server, 9_999_999_999)),
            ..Default::default()
        },
    )
    .unwrap();
    write_running_daemon_state(&dir, Some(1));

    // A regular file at the protected identity-root path makes the final
    // remove_dir_all fail deterministically, after the credentials transaction
    // has already cleared the bearer/cache fields.
    std::fs::write(auth::identity::identity_root(&dirs), "not a directory").unwrap();

    let runtime = dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime).unwrap();
    let listener = UnixListener::bind(runtime.join("federation_control.sock")).unwrap();
    let stub = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept control request");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request = line.trim().to_string();
            if request == "status" {
                stream
                    .write_all(
                        b"{\"status\":\"federation_status\",\"endpoint\":\"tls/hub:7447\",\"link_state\":\"verified\",\"pinned\":false}\n",
                    )
                    .unwrap();
            } else {
                stream
                    .write_all(
                        b"{\"status\":\"ok\",\"endpoint\":null,\"link_state\":\"not_configured\"}\n",
                    )
                    .unwrap();
            }
            requests.push(request);
        }
        requests
    });

    let error = LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("the invalid identity root must still report cleanup failure");

    assert!(error.to_string().contains("Not a directory"), "{error}");
    assert_eq!(logout.calls(), 1, "remote logout happens before cleanup");
    assert!(storage::load(&path).unwrap().session.is_none());
    assert_eq!(
        stub.join().unwrap(),
        vec!["status", "defederate"],
        "cleanup failure must still issue the fail-closed daemon poke"
    );
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
