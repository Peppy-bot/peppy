//! Command-level tests for daemon-owned platform login/logout. The control
//! stand-in speaks only strict protocol-v1 JSON; no raw federation commands or
//! CLI-side certificate enrollment are exercised here.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;

use daemon_config::consts::PeppyDirs;
use httpmock::prelude::*;
use secrecy::ExposeSecret;
use serde_json::{Value, json};

use auth::storage::{self, Credentials, ProfileCreds};
use peppy::commands::Command;
use peppy::commands::platform::login::LoginCommand;
use peppy::commands::platform::logout::LogoutCommand;
use peppy::commands::platform::whoami::WhoamiCommand;
use peppy::context::AppContext;

const REVISION: &str = "11111111-1111-4111-8111-111111111111";

fn ctx() -> Arc<AppContext> {
    Arc::new(AppContext::from_current_dir().expect("cwd is readable"))
}

fn dirs(temp: &tempfile::TempDir) -> PeppyDirs {
    PeppyDirs::new(temp.path())
}

fn creds_path(temp: &tempfile::TempDir) -> PathBuf {
    temp.path().join("conf").join("credentials.json5")
}

fn response(result: Value) -> Value {
    json!({ "protocol_version": 1, "response": result })
}

fn hello_response() -> Value {
    response(json!({ "result": "hello" }))
}

fn applied_response(endpoint: Option<&str>, link_state: &str) -> Value {
    response(json!({
        "result": "applied",
        "link": {
            "endpoint": endpoint,
            "link_state": link_state,
        }
    }))
}

fn standalone_response() -> Value {
    applied_response(None, "not_configured")
}

fn operator_managed_response() -> Value {
    response(json!({ "result": "operator_managed" }))
}

fn status_response(pat_active: bool, operator_managed: bool) -> Value {
    let mut status = json!({
        "controller_settled": true,
        "authentication": if pat_active { "pat" } else { "missing" },
        "certificate": "missing",
        "bound_core_node_name": null,
        "certificate_expiry_unix": null,
        "generation": null,
        "next_retry_after_secs": null,
        "router_apply_state": if operator_managed {
            "operator_managed"
        } else {
            "standalone"
        },
        "operator_managed": operator_managed,
        "offline_recovery_required": false,
        "link": { "endpoint": null, "link_state": "not_configured" },
        "pinned": false,
    });
    if pat_active {
        status["pat_active"] = json!(true);
    }
    response(json!({ "result": "status", "status": status }))
}

fn logout_response(
    router_apply: &str,
    local_cleanup: &str,
    operator_action_required: bool,
) -> Value {
    response(json!({
        "result": "logged_out",
        "outcome": {
            "certificate_revocation": "succeeded",
            "oauth_revocation": "succeeded",
            "router_apply": router_apply,
            "local_cleanup": local_cleanup,
            "operator_action_required": operator_action_required,
            "target_namespace": null,
        }
    }))
}

fn error_response(code: &str, message: &str) -> Value {
    response(json!({ "result": "error", "code": code, "message": message }))
}

/// One connection per request, matching the real control client. The returned
/// values are exact parsed request envelopes in arrival order.
fn stub_control(dirs: &PeppyDirs, replies: Vec<Value>) -> std::thread::JoinHandle<Vec<Value>> {
    let runtime = dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let socket = runtime.join("federation_control.sock");
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove stale socket: {error}"),
    }
    let listener = UnixListener::bind(socket).expect("bind control stand-in");
    let identity_dirs = dirs.clone();
    std::thread::spawn(move || {
        let mut requests = Vec::with_capacity(replies.len());
        for reply in replies {
            let (mut stream, _) = listener.accept().expect("accept control request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            let request: Value =
                serde_json::from_str(line.trim()).expect("protocol-v1 request JSON");
            let operation = request["request"]["operation"].as_str();
            if operation == Some("prepare_oauth_login") {
                let revision = request["request"]["expected_session_revision"]
                    .as_str()
                    .expect("Prepare carries a revision")
                    .parse()
                    .expect("Prepare revision is a UUID");
                auth::identity::arm_binding_incomplete_for_session(&identity_dirs, Some(revision))
                    .expect("control stand-in arms the daemon-owned transition");
            }
            if operation == Some("enroll_current_credential")
                && matches!(
                    reply["response"]["result"].as_str(),
                    Some("applied" | "operator_managed" | "restarting")
                )
            {
                auth::identity::clear_binding_incomplete(&identity_dirs)
                    .expect("control stand-in commits the daemon-owned transition");
            }
            requests.push(request);
            serde_json::to_writer(&mut stream, &reply).expect("write response");
            stream.write_all(b"\n").expect("terminate response");
        }
        requests
    })
}

fn assert_hello(request: &Value) {
    assert_eq!(request["protocol_version"], 1);
    assert_eq!(request["request"]["operation"], "hello");
}

fn mock_login_endpoints(server: &MockServer, access_token: &str) {
    let base = server.base_url();
    server.mock(|when, then| {
        when.method(GET).path("/cli/auth-config");
        then.status(200).json_body(json!({
            "issuer": base.clone(),
            "client_id": "cli-client-id",
            "scopes": "openid profile email offline_access",
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
            "device_code": "device-code",
            "user_code": "WXYZ-1234",
            "verification_uri": format!("{base}/device"),
            "expires_in": 300,
            "interval": 0,
        }));
    });
    server.mock(|when, then| {
        when.method(POST).path("/oauth/v2/token");
        then.status(200).json_body(json!({
            "access_token": access_token,
            "refresh_token": "refresh-token",
            "expires_in": 3600,
            "token_type": "Bearer",
            "scope": "openid profile email",
        }));
    });
}

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
        }));
    })
}

fn write_external_config(temp: &tempfile::TempDir) {
    let conf = temp.path().join("conf");
    std::fs::create_dir_all(&conf).unwrap();
    std::fs::write(
        conf.join("peppy_config.json5"),
        r#"{ zenoh: { external: { endpoint: "tcp/127.0.0.1:7447" } } }"#,
    )
    .unwrap();
}

fn write_live_state(
    temp: &tempfile::TempDir,
    router_ownership: daemon::state::RouterOwnership,
    control_timeout: Option<u64>,
) {
    let state = daemon::state::DaemonState::new(
        "core-node-test",
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        "platform-flow",
        5,
        config::namespace::Namespace::local(),
        router_ownership,
        control_timeout,
    )
    .with_service_pat_active(false);
    daemon::state::DaemonState::write_to(
        &daemon::state::DaemonState::state_file_in(temp.path()),
        &state,
    )
    .unwrap();
}

fn seeded_creds(server: &MockServer, expires_at: i64) -> ProfileCreds {
    ProfileCreds {
        session_revision: REVISION.parse().unwrap(),
        api_url: server.base_url(),
        issuer: server.base_url(),
        client_id: "cli-client-id".into(),
        access_token: storage::secret("seeded-access".into()),
        refresh_token: storage::secret("seeded-refresh".into()),
        expires_at,
        token_type: "Bearer".into(),
        scope: "openid".into(),
        subject: "user-123".into(),
        username: "alice".into(),
    }
}

fn seed_session(temp: &tempfile::TempDir, server: &MockServer) {
    storage::save(
        &creds_path(temp),
        &Credentials {
            session: Some(seeded_creds(server, 9_999_999_999)),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn login_requires_hello_before_oauth_or_config_storage() {
    let server = MockServer::start();
    let bootstrap = server.mock(|when, then| {
        when.method(GET).path("/cli/auth-config");
        then.status(500);
    });
    let temp = tempfile::tempdir().unwrap();

    let error = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(dirs(&temp)),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("no daemon must stop login before OAuth");

    assert!(
        error.to_string().contains("running Peppy daemon"),
        "{error}"
    );
    assert_eq!(bootstrap.calls(), 0);
    assert!(!creds_path(&temp).exists());
    assert!(!temp.path().join("conf/peppy_config.json5").exists());
}

#[test]
fn oauth_login_persists_v1_and_sends_only_its_revision_to_daemon() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "fresh-access");
    let me = mock_me(&server);
    let direct_enrollment = server.mock(|when, then| {
        when.method(POST).path("/me/cli/core-node-certificates");
        then.status(500);
    });
    let temp = tempfile::tempdir().unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(
        &dirs,
        vec![
            hello_response(),
            status_response(false, false),
            hello_response(),
            status_response(false, false),
            standalone_response(),
            hello_response(),
            status_response(false, false),
            applied_response(Some("tls/hub:7447"), "verified"),
        ],
    );

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(dirs.clone()),
        pat: None,
    }
    .execute(&ctx())
    .expect("daemon-owned OAuth login");

    let credentials = storage::load(&creds_path(&temp)).unwrap();
    let session = credentials.session.unwrap();
    assert_eq!(credentials.version, 1);
    assert_eq!(session.access_token.expose_secret(), "fresh-access");
    assert_eq!(session.subject, "user-123");
    assert!(!session.session_revision.is_nil());
    let requests = control.join().unwrap();
    assert_hello(&requests[0]);
    assert_eq!(
        requests[4],
        json!({
            "protocol_version": 1,
            "request": {
                "operation": "prepare_oauth_login",
                "expected_session_revision": session.session_revision,
            }
        })
    );
    assert_eq!(
        requests[7],
        json!({
            "protocol_version": 1,
            "request": {
                "operation": "enroll_current_credential",
                "expected_session_revision": session.session_revision,
            }
        })
    );
    assert_eq!(me.calls(), 1);
    assert_eq!(direct_enrollment.calls(), 0);
    assert!(!auth::identity::identity_root(&dirs).exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(creds_path(&temp))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
        );
    }
}

#[test]
fn oauth_login_aborts_before_session_publication_if_fail_closed_handoff_is_unsafe() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "must-not-persist");
    let me = mock_me(&server);
    let temp = tempfile::tempdir().unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(
        &dirs,
        vec![
            hello_response(),
            status_response(false, false),
            hello_response(),
            status_response(false, false),
            applied_response(Some("tls/still-applied:7447"), "verified"),
        ],
    );

    let error = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("unsafe begin-login acknowledgement must stop publication");

    assert!(error.to_string().contains("fail-closed"), "{error}");
    let requests = control.join().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[4]["request"]["operation"], "prepare_oauth_login");
    assert!(!creds_path(&temp).exists());
    assert_eq!(me.calls(), 0, "identity lookup follows durable publication");
}

#[test]
fn post_publication_identity_error_retains_session_after_fail_closed_handoff() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "retained-access");
    let me = server.mock(|when, then| {
        when.method(GET).path("/me");
        then.status(503);
    });
    let temp = tempfile::tempdir().unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(
        &dirs,
        vec![
            hello_response(),
            status_response(false, false),
            hello_response(),
            status_response(false, false),
            standalone_response(),
        ],
    );

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(dirs.clone()),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("/me failure must stop before daemon enrollment");

    let requests = control.join().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[4]["request"]["operation"], "prepare_oauth_login");
    assert_eq!(me.calls(), 1);
    let session = storage::load(&creds_path(&temp))
        .unwrap()
        .session
        .expect("OAuth session retained");
    assert_eq!(session.access_token.expose_secret(), "retained-access");
    assert!(!auth::identity::identity_root(&dirs).exists());
    assert!(
        auth::identity::binding_incomplete(&dirs).unwrap(),
        "the daemon-owned transition remains armed after publication fails"
    );
}

#[test]
fn pat_login_validates_cli_pat_without_storage_then_sends_no_pat_or_revision() {
    let server = MockServer::start();
    let me = mock_me(&server);
    let direct_enrollment = server.mock(|when, then| {
        when.method(POST).path("/me/cli/core-node-certificates");
        then.status(500);
    });
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);
    let before = std::fs::read_to_string(creds_path(&temp)).unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(
        &dirs,
        vec![
            hello_response(),
            status_response(true, false),
            hello_response(),
            status_response(true, false),
            applied_response(Some("tls/hub:7447"), "verified"),
        ],
    );

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(dirs),
        pat: Some("must-never-cross-control".into()),
    }
    .execute(&ctx())
    .expect("daemon validates its own PAT");

    let requests = control.join().unwrap();
    assert_hello(&requests[0]);
    assert_eq!(
        requests[4],
        json!({
            "protocol_version": 1,
            "request": {
                "operation": "enroll_current_credential",
                "expected_pat_subject": "user-123",
                "expected_api_origin": server.base_url(),
            }
        })
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.to_string().contains("must-never-cross-control"))
    );
    assert_eq!(me.calls(), 1, "the CLI validates its own ambient PAT");
    assert_eq!(
        direct_enrollment.calls(),
        0,
        "certificate enrollment remains daemon-owned"
    );
    assert_eq!(
        std::fs::read_to_string(creds_path(&temp)).unwrap(),
        before,
        "PAT precedence must not rewrite the retained OAuth session"
    );
}

#[test]
fn pat_login_surfaces_daemon_environment_error() {
    let server = MockServer::start();
    let me = mock_me(&server);
    let temp = tempfile::tempdir().unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(
        &dirs,
        vec![
            hello_response(),
            status_response(true, false),
            hello_response(),
            status_response(true, false),
            error_response(
                "unavailable",
                "PEPPY_API_KEY is not configured in the daemon service environment",
            ),
        ],
    );

    let error = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(dirs),
        pat: Some("cli-pat".into()),
    }
    .execute(&ctx())
    .expect_err("daemon PAT configuration must be authoritative");

    assert!(error.to_string().contains("daemon service environment"));
    assert_eq!(control.join().unwrap().len(), 5);
    assert_eq!(me.calls(), 1, "the CLI PAT is independently valid");
    assert!(!creds_path(&temp).exists());
}

#[test]
fn pat_login_rejection_stops_after_hello_before_daemon_enrollment_or_storage() {
    let server = MockServer::start();
    let me = server.mock(|when, then| {
        when.method(GET).path("/me");
        then.status(401);
    });
    let temp = tempfile::tempdir().unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(&dirs, vec![hello_response(), status_response(true, false)]);

    let error = LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(dirs),
        pat: Some("rejected-cli-pat".into()),
    }
    .execute(&ctx())
    .expect_err("invalid CLI PAT must not reach enrollment");

    assert!(
        error.to_string().contains("could not be validated"),
        "{error}"
    );
    let requests = control.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert_hello(&requests[0]);
    assert_eq!(me.calls(), 1);
    assert!(!creds_path(&temp).exists());
}

#[test]
fn external_router_login_still_requires_and_uses_daemon_control() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "external-access");
    let _me = mock_me(&server);
    let temp = tempfile::tempdir().unwrap();
    write_external_config(&temp);
    write_live_state(
        &temp,
        daemon::state::RouterOwnership::OperatorManaged,
        Some(daemon_config::peppy_config::DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS),
    );
    let dirs = dirs(&temp);
    let control = stub_control(
        &dirs,
        vec![
            hello_response(),
            status_response(false, true),
            hello_response(),
            status_response(false, true),
            operator_managed_response(),
            hello_response(),
            status_response(false, true),
            operator_managed_response(),
        ],
    );

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        yes: true,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect("external router leaves routing operator-managed after daemon identity apply");

    let requests = control.join().unwrap();
    assert_hello(&requests[0]);
    assert_eq!(
        requests[7]["request"]["operation"],
        "enroll_current_credential"
    );
}

#[test]
fn normal_logout_sends_expected_revision_and_never_cleans_up_directly() {
    let server = MockServer::start();
    let backend_logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let certificate_delete = server.mock(|when, then| {
        when.method(DELETE)
            .path("/me/cli/core-node-certificates/core-node-test");
        then.status(204);
    });
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);
    let before = std::fs::read_to_string(creds_path(&temp)).unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(
        &dirs,
        vec![
            hello_response(),
            status_response(false, false),
            hello_response(),
            status_response(false, false),
            logout_response("standalone", "succeeded", false),
        ],
    );

    LogoutCommand {
        api_url: Some(server.base_url()),
        yes: true,
        offline: false,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect("daemon-owned logout");

    let requests = control.join().unwrap();
    assert_hello(&requests[0]);
    assert_eq!(
        requests[4],
        json!({
            "protocol_version": 1,
            "request": {
                "operation": "logout",
                "expected_session_revision": REVISION,
            }
        })
    );
    assert_eq!(std::fs::read_to_string(creds_path(&temp)).unwrap(), before);
    assert_eq!(backend_logout.calls(), 0);
    assert_eq!(certificate_delete.calls(), 0);
}

#[test]
fn normal_logout_refuses_cli_pat_after_hello_without_mutation() {
    let server = MockServer::start();
    let backend_logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);
    let before = std::fs::read_to_string(creds_path(&temp)).unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(&dirs, vec![hello_response()]);

    let error = LogoutCommand {
        api_url: None,
        yes: true,
        offline: false,
        peppy_dirs: Some(dirs),
        pat: Some("cli-pat-must-stay-local".into()),
    }
    .execute(&ctx())
    .expect_err("ambient CLI PAT prevents logout");

    assert!(error.to_string().contains("PEPPY_API_KEY"), "{error}");
    let requests = control.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert_hello(&requests[0]);
    assert!(!requests[0].to_string().contains("cli-pat-must-stay-local"));
    assert_eq!(std::fs::read_to_string(creds_path(&temp)).unwrap(), before);
    assert_eq!(backend_logout.calls(), 0);
}

#[test]
fn normal_logout_surfaces_daemon_pat_refusal_without_mutation() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);
    let before = std::fs::read_to_string(creds_path(&temp)).unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(&dirs, vec![hello_response(), status_response(true, false)]);

    let error = LogoutCommand {
        api_url: None,
        yes: true,
        offline: false,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("daemon PAT state is authoritative");

    assert!(error.to_string().contains("active in the daemon service"));
    let requests = control.join().unwrap();
    assert_eq!(requests[1]["request"]["operation"], "status");
    assert_eq!(std::fs::read_to_string(creds_path(&temp)).unwrap(), before);
}

#[test]
fn normal_logout_does_not_claim_success_when_daemon_local_cleanup_failed() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);
    let before = std::fs::read_to_string(creds_path(&temp)).unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(
        &dirs,
        vec![
            hello_response(),
            status_response(false, false),
            hello_response(),
            status_response(false, false),
            logout_response("standalone", "failed", false),
        ],
    );

    let error = LogoutCommand {
        api_url: None,
        yes: true,
        offline: false,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("failed daemon cleanup must remain nonzero");

    assert!(error.to_string().contains("local credential"), "{error}");
    assert_eq!(control.join().unwrap().len(), 5);
    assert_eq!(
        std::fs::read_to_string(creds_path(&temp)).unwrap(),
        before,
        "the CLI never compensates with its own cleanup"
    );
}

#[test]
fn normal_logout_without_daemon_instructs_offline_and_preserves_session() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);

    let error = LogoutCommand {
        api_url: None,
        yes: true,
        offline: false,
        peppy_dirs: Some(dirs(&temp)),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("normal logout requires daemon ownership");

    assert!(error.to_string().contains("logout --offline"), "{error}");
    assert!(storage::load(&creds_path(&temp)).unwrap().session.is_some());
}

#[test]
fn malformed_credentials_are_preserved_after_successful_hello() {
    let temp = tempfile::tempdir().unwrap();
    let path = creds_path(&temp);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = "{ not valid JSON5";
    std::fs::write(&path, original).unwrap();
    let dirs = dirs(&temp);
    let control = stub_control(
        &dirs,
        vec![
            hello_response(),
            status_response(false, false),
            hello_response(),
            status_response(false, false),
        ],
    );

    LogoutCommand {
        api_url: None,
        yes: true,
        offline: false,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("malformed credentials fail closed");

    assert_eq!(control.join().unwrap().len(), 4);
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn external_router_logout_still_delegates_identity_to_daemon() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();
    write_external_config(&temp);
    write_live_state(
        &temp,
        daemon::state::RouterOwnership::OperatorManaged,
        Some(daemon_config::peppy_config::DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS),
    );
    seed_session(&temp, &server);
    let dirs = dirs(&temp);
    let control = stub_control(
        &dirs,
        vec![
            hello_response(),
            status_response(false, true),
            hello_response(),
            status_response(false, true),
            logout_response("operator_managed", "succeeded", true),
        ],
    );

    LogoutCommand {
        api_url: None,
        yes: true,
        offline: false,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect("external router still uses daemon identity control");

    let requests = control.join().unwrap();
    assert_eq!(requests[4]["request"]["operation"], "logout");
}

#[test]
fn offline_logout_proves_absence_then_uses_local_recovery_only() {
    let server = MockServer::start();
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);

    LogoutCommand {
        api_url: None,
        yes: true,
        offline: true,
        peppy_dirs: Some(dirs(&temp)),
        pat: None,
    }
    .execute(&ctx())
    .expect("offline recovery with no daemon");

    assert!(storage::load(&creds_path(&temp)).unwrap().session.is_none());
    assert_eq!(logout.calls(), 1);
}

#[test]
fn offline_external_logout_clears_local_state_through_operator_managed_path() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();
    write_external_config(&temp);
    seed_session(&temp, &server);

    LogoutCommand {
        api_url: None,
        yes: true,
        offline: true,
        peppy_dirs: Some(dirs(&temp)),
        pat: None,
    }
    .execute(&ctx())
    .expect("offline external recovery");

    assert!(storage::load(&creds_path(&temp)).unwrap().session.is_none());
}

#[test]
fn offline_logout_recovers_malformed_or_future_credentials_after_stop_proof() {
    for invalid in [
        "{ not valid JSON5",
        r#"{ version: 99, session: null, router: null, core_node_identity: null }"#,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = creds_path(&temp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, invalid).unwrap();

        LogoutCommand {
            api_url: None,
            yes: true,
            offline: true,
            peppy_dirs: Some(dirs(&temp)),
            pat: None,
        }
        .execute(&ctx())
        .expect("explicit offline recovery resets an unusable credential store");

        let recovered = storage::load(&path).unwrap();
        assert_eq!(recovered.version, 1);
        assert!(recovered.session.is_none());
        assert!(recovered.router.is_none());
        assert!(recovered.core_node_identity.is_none());
    }
}

#[test]
fn offline_logout_refuses_corrupt_router_fence_state_and_preserves_identity() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);
    std::fs::write(
        daemon::state::DaemonState::state_file_in(temp.path()),
        "{ corrupt stale state",
    )
    .unwrap();

    let error = LogoutCommand {
        api_url: None,
        yes: true,
        offline: true,
        peppy_dirs: Some(dirs(&temp)),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("corrupt state cannot prove that a managed router no longer holds the key");

    assert!(
        error
            .to_string()
            .contains("cannot prove the last managed router is stopped"),
        "{error}"
    );
    assert!(storage::load(&creds_path(&temp)).unwrap().session.is_some());
}

#[test]
fn offline_logout_refuses_pid_only_stale_state_without_a_router_launch_fence() {
    let server = MockServer::start();
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);
    write_live_state(
        &temp,
        daemon::state::RouterOwnership::PeppyManaged,
        Some(30),
    );

    let error = LogoutCommand {
        api_url: None,
        yes: true,
        offline: true,
        peppy_dirs: Some(dirs(&temp)),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("PID-only state cannot prove that the last managed router is stopped");

    assert!(error.to_string().contains("no managed-router launch fence"));
    assert!(storage::load(&creds_path(&temp)).unwrap().session.is_some());
    assert_eq!(logout.calls(), 0);
}

#[test]
fn offline_logout_requires_the_daemon_owner_lock_before_http_or_cleanup() {
    let server = MockServer::start();
    let logout = server.mock(|when, then| {
        when.method(POST).path("/logout");
        then.status(202);
    });
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);
    let dirs = dirs(&temp);
    let _daemon_owner = auth::identity::acquire_identity_owner(&dirs).unwrap();

    let error = LogoutCommand {
        api_url: None,
        yes: true,
        offline: true,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("offline cleanup cannot overlap daemon ownership");

    assert!(error.to_string().contains("offline identity ownership"));
    assert!(storage::load(&creds_path(&temp)).unwrap().session.is_some());
    assert_eq!(logout.calls(), 0);
}

#[test]
fn offline_logout_refuses_when_daemon_answers_hello_even_without_state() {
    let server = MockServer::start();
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);
    let dirs = dirs(&temp);
    let control = stub_control(&dirs, vec![hello_response()]);

    let error = LogoutCommand {
        api_url: None,
        yes: true,
        offline: true,
        peppy_dirs: Some(dirs),
        pat: None,
    }
    .execute(&ctx())
    .expect_err("a responding daemon owns identity despite missing state");

    assert!(error.to_string().contains("still answering"), "{error}");
    assert_eq!(control.join().unwrap().len(), 1);
    assert!(storage::load(&creds_path(&temp)).unwrap().session.is_some());
}

#[test]
fn whoami_remains_a_read_only_oauth_command() {
    let server = MockServer::start();
    let me = mock_me(&server);
    let temp = tempfile::tempdir().unwrap();
    seed_session(&temp, &server);

    WhoamiCommand {
        api_url: Some(server.base_url()),
        json: true,
        peppy_dirs: Some(dirs(&temp)),
        pat: None,
    }
    .execute(&ctx())
    .expect("whoami");

    assert_eq!(me.calls(), 1);
    assert!(storage::load(&creds_path(&temp)).unwrap().session.is_some());
}
