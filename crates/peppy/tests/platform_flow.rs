//! Command-level platform tests with every HTTP endpoint mocked (`httpmock`):
//! the public `/cli/auth-config`, OIDC discovery, the Zitadel device, token and
//! revocation endpoints, and the backend's `/me`, workspace, project and
//! router-peer routes. All state is isolated per test via the `peppy_dirs`
//! seam pointed at a tempdir (no `PEPPY_HOME` mutation, so tests run in
//! parallel); the credentials file, the selection, the enrollment bundle, the
//! daemon state and `peppy_config.json5` all land there. A command that can
//! ask the person is told how to ask (`Ask`), so no test depends on a
//! terminal. A running daemon is stood in for by a state file that names this
//! test process, and by a stub on the control socket where a test needs the
//! daemon to answer. The engine internals (resolver, the platform client, the
//! enrollment store, the CSR) are covered by the `auth` crate's own tests.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;

use daemon_config::consts::PeppyDirs;
use httpmock::prelude::*;
use secrecy::ExposeSecret;
use serde_json::json;

use auth::enrollment::{self, EnrollmentDocument, RouterEndpoint};
use auth::selection::{self as platform_selection, Named, PlatformSelection};
use auth::storage::{self, Credentials, ProfileCreds};
use auth::test_support;
use daemon::state::DaemonState;
use peppy::commands::Command;
use peppy::commands::platform::enroll::EnrollCommand;
use peppy::commands::platform::login::LoginCommand;
use peppy::commands::platform::logout::LogoutCommand;
use peppy::commands::platform::peers::PeersCommand;
use peppy::commands::platform::project::{ProjectCommand, ProjectCommands};
use peppy::commands::platform::router::{RouterAction, RouterCommand, RouterCommands};
use peppy::commands::platform::select::Ask;
use peppy::commands::platform::status::StatusCommand;
use peppy::commands::platform::unenroll::UnenrollCommand;
use peppy::commands::platform::whoami::WhoamiCommand;
use peppy::commands::platform::workspace::{WorkspaceCommand, WorkspaceCommands};
use peppy::commands::platform::{PlatformCommand, PlatformCommands};
use peppy::context::AppContext;

const WORKSPACE: &str = "4f1b2e2c-9a71-4d0e-b3c8-0d2b9f6a11c4";
const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
const ZID: &str = "2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5";
const ROUTER_HOST: &str = "rtr-p.us-east-1.robocloud.dev.peppy.bot";
const PEERS_PATH: &str = "/api/workspace/4f1b2e2c-9a71-4d0e-b3c8-0d2b9f6a11c4/projects/550e8400-e29b-41d4-a716-446655440000/router/peers";
const ROUTER_PATH: &str = "/api/workspace/4f1b2e2c-9a71-4d0e-b3c8-0d2b9f6a11c4/projects/550e8400-e29b-41d4-a716-446655440000/router";

/// The `/cli/auth-config` answer of a platform whose issuer is `base`, with the
/// platform's own device page when `device_verification_uri` is set (API
/// contract 3.3.0 and later) and without it otherwise (an older platform).
fn cli_auth_config(base: &str, device_verification_uri: Option<&str>) -> serde_json::Value {
    let mut config = json!({
        "issuer": base,
        "client_id": "cli-client-id",
        "project_id": "proj-id",
        "scopes": "openid profile email offline_access urn:zitadel:iam:org:project:id:proj-id:aud",
    });
    if let Some(address) = device_verification_uri {
        config["device_verification_uri"] = json!(address);
    }
    config
}

/// Builds the login mocks for a current platform: one that publishes its own
/// device page, on a path apart from the identity provider's.
fn mock_login_endpoints(server: &MockServer, access_token: &str) {
    let base = server.base_url();
    mock_login_endpoints_answering(
        server,
        access_token,
        cli_auth_config(&base, Some(&format!("{base}/app/device"))),
    );
}

/// Builds the cli/auth-config + OIDC discovery + device-authorization + token mocks
/// for a server whose issuer is its own base URL, with `/cli/auth-config`
/// answering `config` and the device grant succeeding immediately. Returns the
/// device-authorization mock, so a test can tell whether the flow started.
fn mock_login_endpoints_answering<'a>(
    server: &'a MockServer,
    access_token: &str,
    config: serde_json::Value,
) -> httpmock::Mock<'a> {
    let base = server.base_url();

    server.mock(|when, then| {
        when.method(GET).path("/cli/auth-config");
        then.status(200).json_body(config);
    });
    mock_discovery(server);
    let device_authorization = server.mock(|when, then| {
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

    device_authorization
}

fn mock_discovery(server: &MockServer) {
    let base = server.base_url();
    server.mock(|when, then| {
        when.method(GET).path("/.well-known/openid-configuration");
        then.status(200).json_body(json!({
            "issuer": base,
            "device_authorization_endpoint": format!("{base}/oauth/v2/device_authorization"),
            "token_endpoint": format!("{base}/oauth/v2/token"),
            "revocation_endpoint": format!("{base}/oauth/v2/revoke"),
        }));
    });
}

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

fn mock_workspaces_and_projects(server: &MockServer) {
    server.mock(|when, then| {
        when.method(GET).path("/api/workspaces");
        then.status(200).json_body(json!([
            { "id": WORKSPACE, "name": "Alice's workspace", "tier": "free",
              "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z" }
        ]));
    });
    server.mock(|when, then| {
        when.method(GET)
            .path(format!("/api/workspace/{WORKSPACE}/projects"));
        then.status(200).json_body(json!([
            { "id": PROJECT, "workspace_id": WORKSPACE, "name": "Lab", "robot_count": 1,
              "live_session_count": 0, "created_at": "2026-10-01T00:00:00Z",
              "updated_at": "2026-10-01T00:00:00Z" },
            { "id": "p-old", "workspace_id": WORKSPACE, "name": "Old", "robot_count": 0,
              "live_session_count": 0, "created_at": "2026-10-01T00:00:00Z",
              "updated_at": "2026-10-01T00:00:00Z", "archived_at": "2026-10-02T00:00:00Z" }
        ]));
    });
}

/// The core-node name of the daemon whose state the tests write: the name a
/// machine enrolls under.
const CORE_NODE: &str = "cn-local-daemon";

/// `POST .../router/peers` of `project` in `workspace`, answering a signed
/// enrollment with a leaf valid from `test_support::ISSUED_AT` to
/// `test_support::EXPIRES_AT`. Only a signing request for [`CORE_NODE`] is
/// answered, so a call count of one proves the CLI posted what it minted,
/// under the name of the running daemon.
fn mock_enroll_in<'a>(
    server: &'a MockServer,
    workspace: &str,
    project: &str,
    zid: &str,
) -> httpmock::Mock<'a> {
    let path = format!("/api/workspace/{workspace}/projects/{project}/router/peers");
    let project = project.to_string();
    let zid = zid.to_string();
    let leaf = test_support::leaf();
    server.mock(move |when, then| {
        when.method(POST).path(path).is_true(|request| {
            test_support::enrollment_request_common_name(request.body().as_ref()).as_deref()
                == Some(CORE_NODE)
        });
        then.status(201).json_body(json!({
            "peer": { "id": "peer-1", "name": "robot-7", "certificate_cn": "robot-7",
                      "status": "unknown", "certificate_expires_at": "2027-01-01T00:00:00Z",
                      "created_at": "2026-10-03T00:00:00Z" },
            "certificate": leaf,
            "chain": "-----BEGIN CERTIFICATE-----\nissuer\n-----END CERTIFICATE-----\n",
            "trust_anchor": "-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n",
            "address": { "host": ROUTER_HOST, "port": 7447 },
            "zenoh_id": zid,
            "namespace": project,
            "zenoh_config": "{ mode: \"router\" }",
        }));
    })
}

/// [`mock_enroll_in`] for `PROJECT` of `WORKSPACE`.
fn mock_enroll<'a>(server: &'a MockServer, zid: &str) -> httpmock::Mock<'a> {
    mock_enroll_in(server, WORKSPACE, PROJECT, zid)
}

/// Every enrollment request, in any project, refused. Registered by a test
/// that expects no enrollment, so its call count is the evidence.
fn mock_any_enrollment(server: &MockServer) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(POST).path_includes("/router/peers");
        then.status(500);
    })
}

fn ctx() -> Arc<AppContext> {
    Arc::new(AppContext::from_current_dir().expect("cwd is readable"))
}

fn creds_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("conf").join("credentials.json5")
}

fn dirs(dir: &tempfile::TempDir) -> PeppyDirs {
    PeppyDirs::new(dir.path())
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

/// Writes an enrollment in `PROJECT` of `WORKSPACE` under `dir`, as a previous
/// `enroll` would have, with a leaf that is valid now and placeholder material
/// for the rest.
fn write_enrollment(dir: &tempfile::TempDir, peer_id: &str, zid: &str) {
    test_support::write_enrollment(
        &dirs(dir),
        EnrollmentDocument {
            workspace_id: WORKSPACE.into(),
            peer_id: peer_id.into(),
            zenoh_id: pmi::RouterId::parse(zid).unwrap(),
            router: RouterEndpoint::parse(ROUTER_HOST, 7447).unwrap(),
            ..test_support::enrollment_document()
        },
    );
}

/// The state of a daemon generation of [`CORE_NODE`] under `namespace`, with
/// this test process as the pid (so `is_running` holds): with a managed router
/// under `router_id` (so commands poke the control socket), or with an
/// operator-run router when `router_id` is `None`.
fn daemon_state(namespace: &str, router_id: Option<&str>) -> DaemonState {
    DaemonState::new(
        CORE_NODE,
        "127.0.0.1",
        7447,
        "test-git-hash",
        30,
        config::namespace::Namespace::parse(namespace).expect("valid namespace"),
        router_id.map(|id| pmi::RouterId::parse(id).expect("valid router id")),
    )
}

/// Writes the state of a running daemon under `dir`, with a managed router
/// under `router_id`.
fn write_daemon_state(dir: &tempfile::TempDir, namespace: &str, router_id: &str) {
    write_state(dir, &daemon_state(namespace, Some(router_id)));
}

/// Writes the state of a running daemon under `dir`, with an operator-run
/// router: it has no control socket.
fn write_external_daemon_state(dir: &tempfile::TempDir) {
    write_state(dir, &daemon_state("local", None));
}

fn write_state(dir: &tempfile::TempDir, state: &DaemonState) {
    let path = DaemonState::state_file_in(dir.path());
    std::fs::create_dir_all(path.parent().expect("state file has a parent"))
        .expect("state file dir");
    DaemonState::write_to(&path, state).expect("write daemon state");
}

/// Writes the state of a daemon that died under `dir`: a pid outside the
/// valid range names no live process.
fn write_dead_daemon_state(dir: &tempfile::TempDir) {
    let mut stale = daemon_state("local", Some("7f3a9c1e"));
    stale.daemon_pid = Some(u32::MAX);
    write_state(dir, &stale);
}

/// A tempdir with a running daemon under `local` and no session, ready for a
/// login that enrolls. No stub answers the control socket, so the poke after
/// an enrollment finds no daemon there, which the command notes.
fn daemon_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    write_daemon_state(&dir, "local", "7f3a9c1e");
    dir
}

/// A tempdir with a seeded session and a running daemon under `local`, ready
/// for a command that enrolls. The poke after the enrollment finds no daemon
/// on the control socket, as for [`daemon_dir`].
fn enrollable_dir(server: &MockServer) -> tempfile::TempDir {
    let dir = authenticated_dir(server);
    write_daemon_state(&dir, "local", "7f3a9c1e");
    dir
}

/// A stub daemon on the control socket under `dir`. It answers the first poke
/// with `restarting`, then rewrites the daemon state to the identity the
/// enrollment now prescribes (as the rebuilt generation would), and answers
/// the second poke with `reply`. Returns the request lines it saw.
fn stub_restarting_daemon(
    dir: &tempfile::TempDir,
    namespace: &'static str,
    router_id: &'static str,
    reply: &'static str,
) -> std::thread::JoinHandle<Vec<String>> {
    let peppy_dirs = dirs(dir);
    let runtime = peppy_dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let socket = runtime.join("federation_control.sock");
    let listener = UnixListener::bind(&socket).expect("bind stub control socket");
    let root = dir.path().to_path_buf();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let answer = |line: &mut String, reply: &str| {
            let (mut stream, _) = listener.accept().expect("accept poke");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            line.clear();
            reader.read_line(line).expect("read poke request");
            stream.write_all(reply.as_bytes()).expect("reply");
        };
        let mut line = String::new();
        answer(&mut line, "{\"status\":\"restarting\"}\n");
        seen.push(line.trim().to_string());
        DaemonState::write_to(
            &DaemonState::state_file_in(&root),
            &daemon_state(namespace, Some(router_id)),
        )
        .expect("rewrite state");
        answer(&mut line, reply);
        seen.push(line.trim().to_string());
        seen
    })
}

// ─── login ───────────────────────────────────────────────────────────────

/// A login against `server` with state under `dir`, with no flag, the restart
/// prompt skipped and nobody to ask. A test sets what it is about with struct
/// update syntax.
fn login(server: &MockServer, dir: &tempfile::TempDir) -> LoginCommand {
    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        workspace: None,
        project: None,
        no_enroll: false,
        yes: true,
        ask: Ask::Never,
        peppy_dirs: Some(dirs(dir)),
    }
}

/// [`login`] signing in only.
fn login_only(server: &MockServer, dir: &tempfile::TempDir) -> LoginCommand {
    LoginCommand {
        no_enroll: true,
        ..login(server, dir)
    }
}

#[test]
fn login_persists_credentials_and_resolves_identity() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);

    login_only(&server, &dir)
        .execute(&ctx())
        .expect("a login that signs in only needs no daemon");

    let creds = storage::load(&path).expect("load creds");
    let pc = creds.session.as_ref().expect("session present");
    assert_eq!(pc.access_token.expose_secret(), "access-token-1");
    assert_eq!(pc.refresh_token.expose_secret(), "the-refresh-token");
    assert_eq!(pc.subject, "user-123");
    assert_eq!(pc.username, "alice");
    assert_eq!(pc.issuer, server.base_url());
    assert_eq!(pc.client_id, "cli-client-id");
    assert!(me.calls() >= 1, "GET /me should have been called");
    assert!(
        !dirs(&dir).runtime_config_dir().exists(),
        "a login that signs in only never touches the daemon"
    );
}

#[test]
fn login_seeds_peppy_config_with_resource_servers_block() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-3");
    let _me = mock_me(&server);
    let dir = tempfile::tempdir().expect("temp dir");

    login_only(&server, &dir).execute(&ctx()).expect("login");

    // A login on a machine that never ran the daemon still seeds
    // peppy_config.json5 with the resource_servers block (the build's default
    // URL, the dev backend in this debug test build).
    let config = std::fs::read_to_string(dir.path().join("conf").join("peppy_config.json5"))
        .expect("peppy_config.json5 seeded");
    assert!(config.contains("resource_servers"), "{config}");
    assert!(
        config.contains(daemon_config::peppy_config::DEFAULT_API_URL),
        "{config}"
    );
}

#[test]
fn login_writes_credentials_file_0600() {
    use std::os::unix::fs::PermissionsExt;

    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-2");
    let _me = mock_me(&server);
    let dir = tempfile::tempdir().expect("temp dir");

    login_only(&server, &dir).execute(&ctx()).expect("login");

    let mode = std::fs::metadata(creds_path(&dir))
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "credentials must be owner-only");
}

/// A platform older than API contract 3.3.0 does not publish its device page.
/// The login is refused before the device flow starts, says why, and stores
/// nothing.
#[test]
fn login_refuses_an_older_platform_before_the_flow_starts() {
    let server = MockServer::start();
    let device_authorization = mock_login_endpoints_answering(
        &server,
        "unused-token",
        cli_auth_config(&server.base_url(), None),
    );
    let dir = tempfile::tempdir().expect("temp dir");

    let err = login_only(&server, &dir)
        .execute(&ctx())
        .expect_err("a platform older than 3.3.0 cannot be signed in to");

    assert!(
        err.to_string().contains(
            "This platform is older than API contract 3.3.0 and does not publish its own \
             device page"
        ),
        "{err}"
    );
    assert_eq!(device_authorization.calls(), 0);
    assert!(!creds_path(&dir).exists(), "a refused login stores nothing");
}

/// A device page the CLI cannot print safely stops the login before the device
/// flow starts, so no code is ever issued for it.
#[test]
fn login_refuses_an_untrusted_device_page_before_the_flow_starts() {
    for (address, reason) in [
        ("ftp://app.example.test/device", "unsupported URL scheme"),
        (
            "https://app.example.test/device?user_code=ABCD-EFGH",
            "query string or fragment",
        ),
    ] {
        let server = MockServer::start();
        let device_authorization = mock_login_endpoints_answering(
            &server,
            "unused-token",
            cli_auth_config(&server.base_url(), Some(address)),
        );
        let dir = tempfile::tempdir().expect("temp dir");

        let err = login_only(&server, &dir)
            .execute(&ctx())
            .expect_err("an untrusted device page is refused");

        assert!(err.to_string().contains(reason), "{address}: {err}");
        assert_eq!(device_authorization.calls(), 0, "{address}");
        assert!(
            !creds_path(&dir).exists(),
            "{address}: a refused login stores nothing"
        );
    }
}

// ─── login: the enrollment ───────────────────────────────────────────────

const FIELD: &str = "p-field";
const SECOND_WORKSPACE: &str = "ws-b";
const ARM: &str = "p-arm";

/// Two workspaces. The first has the projects Lab and Field, the second has
/// the project Arm.
fn mock_two_workspaces(server: &MockServer) -> httpmock::Mock<'_> {
    let project = |id: &str, workspace: &str, name: &str| {
        json!({ "id": id, "workspace_id": workspace, "name": name, "robot_count": 0,
                "live_session_count": 0, "created_at": "2026-10-01T00:00:00Z",
                "updated_at": "2026-10-01T00:00:00Z" })
    };
    let first = json!([
        project(PROJECT, WORKSPACE, "Lab"),
        project(FIELD, WORKSPACE, "Field")
    ]);
    let second = json!([project(ARM, SECOND_WORKSPACE, "Arm")]);
    server.mock(move |when, then| {
        when.method(GET)
            .path(format!("/api/workspace/{WORKSPACE}/projects"));
        then.status(200).json_body(first);
    });
    server.mock(move |when, then| {
        when.method(GET)
            .path(format!("/api/workspace/{SECOND_WORKSPACE}/projects"));
        then.status(200).json_body(second);
    });
    server.mock(|when, then| {
        when.method(GET).path("/api/workspaces");
        then.status(200).json_body(json!([
            { "id": WORKSPACE, "name": "Alice's workspace", "tier": "free",
              "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z" },
            { "id": SECOND_WORKSPACE, "name": "Robotics lab", "tier": "team",
              "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z" }
        ]));
    })
}

/// Writes a selection made against `server` by `subject`: `workspace`, and
/// `project` when there is one.
fn write_selection(
    dir: &tempfile::TempDir,
    server: &MockServer,
    subject: &str,
    workspace: (&str, &str),
    project: Option<(&str, &str)>,
) -> PlatformSelection {
    let selection = PlatformSelection {
        api_origin: auth::profile::normalize_api_origin(&server.base_url()).unwrap(),
        subject: subject.into(),
        workspace: Named {
            id: workspace.0.into(),
            name: workspace.1.into(),
        },
        project: project.map(|(id, name)| Named {
            id: id.into(),
            name: name.into(),
        }),
        ..test_support::platform_selection()
    };
    platform_selection::save(&dirs(dir), &selection).expect("write selection");
    selection
}

fn stored_selection(dir: &tempfile::TempDir) -> Option<PlatformSelection> {
    platform_selection::load(&dirs(dir)).expect("the selection file parses")
}

/// The project of the selection under `dir`, `None` when no project is
/// selected.
fn selected_project(dir: &tempfile::TempDir) -> Option<String> {
    stored_selection(dir)
        .and_then(|selection| selection.project)
        .map(|project| project.id)
}

fn enrolled_project(dir: &tempfile::TempDir) -> Option<String> {
    enrollment::load(&dirs(dir))
        .expect("the bundle parses")
        .map(|enrollment| enrollment.document.project_id)
}

fn has_a_session(dir: &tempfile::TempDir) -> bool {
    storage::load(&creds_path(dir))
        .expect("load creds")
        .session
        .is_some()
}

/// The whole login: sign in, pick the only workspace and project with no
/// question, enroll under the name of the running daemon, wait for it to come
/// back under the new identity, and verify the link with a second poke.
#[test]
fn login_enrolls_in_the_only_project_with_no_question() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_workspaces_and_projects(&server);
    let enroll = mock_enroll(&server, ZID);
    let dir = daemon_dir();
    let stub = stub_restarting_daemon(
        &dir,
        PROJECT,
        ZID,
        "{\"status\":\"ok\",\"applied\":\"tls/rtr-p.us-east-1.robocloud.dev.peppy.bot:7447\"}\n",
    );

    login(&server, &dir).execute(&ctx()).expect("login");

    assert_eq!(enroll.calls(), 1);
    assert_eq!(stub.join().unwrap(), ["refederate", "refederate"]);
    assert_eq!(
        enrolled_project(&dir).as_deref(),
        Some(PROJECT),
        "the archived project does not count"
    );
    let selection = stored_selection(&dir).expect("the project became the selection");
    assert_eq!(selection.workspace.id, WORKSPACE);
    assert_eq!(selection.workspace.name, "Alice's workspace");
    assert_eq!(selected_project(&dir).as_deref(), Some(PROJECT));
    assert_eq!(selection.subject, "user-123");
    assert_eq!(
        selection.api_origin,
        auth::profile::normalize_api_origin(&server.base_url()).unwrap()
    );
}

#[test]
fn login_asks_for_the_workspace_then_the_project() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_two_workspaces(&server);
    let enroll = mock_enroll_in(&server, WORKSPACE, FIELD, ZID);
    let dir = daemon_dir();

    // Workspace 1 of 2, then project 2 of 2.
    LoginCommand {
        ask: Ask::scripted([0, 1]),
        ..login(&server, &dir)
    }
    .execute(&ctx())
    .expect("login");

    assert_eq!(enroll.calls(), 1);
    assert_eq!(enrolled_project(&dir).as_deref(), Some(FIELD));
    assert_eq!(selected_project(&dir).as_deref(), Some(FIELD));
}

/// A workspace with one project asks one question only.
#[test]
fn login_does_not_ask_for_the_only_project_of_the_selected_workspace() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_two_workspaces(&server);
    let enroll = mock_enroll_in(&server, SECOND_WORKSPACE, ARM, ZID);
    let dir = daemon_dir();

    LoginCommand {
        ask: Ask::scripted([1]),
        ..login(&server, &dir)
    }
    .execute(&ctx())
    .expect("login");

    assert_eq!(enroll.calls(), 1);
    assert_eq!(enrolled_project(&dir).as_deref(), Some(ARM));
    let selection = stored_selection(&dir).expect("selection");
    assert_eq!(selection.workspace.id, SECOND_WORKSPACE);
}

#[test]
fn login_with_flags_asks_nothing() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_two_workspaces(&server);
    let enroll = mock_enroll_in(&server, WORKSPACE, FIELD, ZID);
    let dir = daemon_dir();

    LoginCommand {
        workspace: Some("Alice's workspace".to_string()),
        project: Some("Field".to_string()),
        ..login(&server, &dir)
    }
    .execute(&ctx())
    .expect("the flags name the project, by name");

    assert_eq!(enroll.calls(), 1);
    assert_eq!(enrolled_project(&dir).as_deref(), Some(FIELD));
}

/// The last entry of each menu signs in only, and so does a cancelled menu.
/// The command succeeds: the person decided.
#[test]
fn login_signs_in_only_when_the_person_declines_the_enrollment() {
    for (answers, what) in [
        (vec![2], "the last entry of the workspace menu"),
        (vec![0, 2], "the last entry of the project menu"),
        (vec![], "a cancelled menu"),
    ] {
        let server = MockServer::start();
        mock_login_endpoints(&server, "access-token-1");
        let _me = mock_me(&server);
        mock_two_workspaces(&server);
        let any_enrollment = mock_any_enrollment(&server);
        let dir = daemon_dir();

        LoginCommand {
            ask: Ask::scripted(answers),
            ..login(&server, &dir)
        }
        .execute(&ctx())
        .unwrap_or_else(|e| panic!("{what}: {e}"));

        assert_eq!(any_enrollment.calls(), 0, "{what}");
        assert_eq!(enrolled_project(&dir), None, "{what}");
        assert_eq!(stored_selection(&dir), None, "{what}");
        assert!(has_a_session(&dir), "{what}: the session is kept");
    }
}

/// With more than one workspace and nobody to ask, the machine is not
/// enrolled: the command fails, keeps the session, and names the command that
/// enrolls.
#[test]
fn login_with_nobody_to_ask_keeps_the_session_and_names_enroll() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_two_workspaces(&server);
    let dir = daemon_dir();

    let err = login(&server, &dir)
        .execute(&ctx())
        .expect_err("nobody selects the workspace");

    let message = err.to_string();
    assert!(
        message.starts_with("signed in, but this machine is not enrolled: more than one workspace"),
        "{message}"
    );
    assert!(
        message.contains("peppy platform enroll --workspace <id|name>"),
        "{message}"
    );
    assert!(has_a_session(&dir), "the session is kept");
    assert_eq!(stored_selection(&dir), None);
    assert_eq!(enrolled_project(&dir), None);
}

/// A login that enrolls needs the daemon. With none running it stops before
/// the device flow starts, so the person never approves a code for nothing.
#[test]
fn login_without_a_running_daemon_fails_before_the_device_flow() {
    for (write_state, what) in [
        (None, "no state file"),
        (
            Some(write_dead_daemon_state as fn(&tempfile::TempDir)),
            "the state of a daemon that died",
        ),
    ] {
        let server = MockServer::start();
        let device_authorization = mock_login_endpoints_answering(
            &server,
            "unused-token",
            cli_auth_config(
                &server.base_url(),
                Some(&format!("{}/app/device", server.base_url())),
            ),
        );
        let dir = tempfile::tempdir().expect("temp dir");
        if let Some(write_state) = write_state {
            write_state(&dir);
        }

        let err = login(&server, &dir)
            .execute(&ctx())
            .expect_err("no daemon to enroll");

        let message = err.to_string();
        assert!(
            message.contains("no peppy daemon is running"),
            "{what}: {message}"
        );
        assert!(message.contains("peppy service serve"), "{what}: {message}");
        assert!(message.contains("--no-enroll"), "{what}: {message}");
        assert_eq!(device_authorization.calls(), 0, "{what}");
        assert!(!creds_path(&dir).exists(), "{what}: nothing is stored");
    }
}

/// A machine that is enrolled keeps its enrollment: the login signs in, and
/// needs no daemon and no selection.
#[test]
fn login_on_an_enrolled_machine_keeps_the_enrollment() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    let workspaces = server.mock(|when, then| {
        when.method(GET).path("/api/workspaces");
        then.status(500);
    });
    let any_enrollment = mock_any_enrollment(&server);
    let dir = tempfile::tempdir().expect("temp dir");
    write_enrollment(&dir, "peer-old", "7f3a9c1e");

    login(&server, &dir).execute(&ctx()).expect("login");

    assert!(has_a_session(&dir));
    assert_eq!(workspaces.calls(), 0, "nothing is selected");
    assert_eq!(any_enrollment.calls(), 0);
    assert_eq!(
        enrollment::load(&dirs(&dir))
            .unwrap()
            .unwrap()
            .document
            .peer_id,
        "peer-old"
    );
}

/// Flags that name a project on an enrolled machine ask to move it, which a
/// login does not do. It stops before the device flow and names the commands
/// that do.
#[test]
fn login_with_flags_on_an_enrolled_machine_fails_before_the_device_flow() {
    let server = MockServer::start();
    let device_authorization = mock_login_endpoints_answering(
        &server,
        "unused-token",
        cli_auth_config(
            &server.base_url(),
            Some(&format!("{}/app/device", server.base_url())),
        ),
    );
    let dir = daemon_dir();
    write_enrollment(&dir, "peer-old", "7f3a9c1e");

    let err = LoginCommand {
        project: Some("Field".to_string()),
        ..login(&server, &dir)
    }
    .execute(&ctx())
    .expect_err("a login does not move an enrolled machine");

    let message = err.to_string();
    assert!(message.contains("already enrolled"), "{message}");
    assert!(message.contains("--no-enroll"), "{message}");
    assert!(
        message.contains("peppy platform enroll --replace"),
        "{message}"
    );
    assert_eq!(device_authorization.calls(), 0);
}

#[test]
fn login_no_enroll_selects_and_enrolls_nothing() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    let workspaces = mock_two_workspaces(&server);
    let any_enrollment = mock_any_enrollment(&server);
    let dir = daemon_dir();

    login_only(&server, &dir).execute(&ctx()).expect("login");

    assert_eq!(stored_selection(&dir), None);
    assert_eq!(workspaces.calls(), 0);
    assert_eq!(any_enrollment.calls(), 0);
}

#[test]
fn login_keeps_the_selection_of_the_same_identity() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_two_workspaces(&server);
    let dir = daemon_dir();
    let before = write_selection(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        Some((FIELD, "Field")),
    );

    LoginCommand {
        ask: Ask::scripted([]),
        ..login(&server, &dir)
    }
    .execute(&ctx())
    .expect("the person declines the enrollment");

    assert_eq!(stored_selection(&dir), Some(before));
}

#[test]
fn login_as_a_different_identity_replaces_the_selection() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_workspaces_and_projects(&server);
    let _enroll = mock_enroll(&server, ZID);
    let dir = daemon_dir();
    write_selection(
        &dir,
        &server,
        "someone-else",
        ("ws-x", "Their workspace"),
        Some(("p-x", "Theirs")),
    );

    login(&server, &dir).execute(&ctx()).expect("login");

    let selection = stored_selection(&dir).expect("a selection was made");
    assert_eq!(selection.subject, "user-123");
    assert_eq!(selected_project(&dir).as_deref(), Some(PROJECT));
}

/// The old selection belongs to a different account, so it does not stay when
/// the person signs in only.
#[test]
fn login_as_a_different_identity_removes_the_selection_also_when_it_signs_in_only() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_two_workspaces(&server);
    let dir = daemon_dir();
    write_selection(
        &dir,
        &server,
        "someone-else",
        ("ws-x", "Their workspace"),
        Some(("p-x", "Theirs")),
    );

    LoginCommand {
        ask: Ask::scripted([]),
        ..login(&server, &dir)
    }
    .execute(&ctx())
    .expect("login");

    assert_eq!(stored_selection(&dir), None);
}

// ─── workspace / project ─────────────────────────────────────────────────

fn workspace_command(
    server: &MockServer,
    dir: &tempfile::TempDir,
    command: WorkspaceCommands,
    ask: Ask,
) -> peppy::error::Result<()> {
    WorkspaceCommand {
        command,
        api_url: Some(server.base_url()),
        ask,
        peppy_dirs: Some(dirs(dir)),
    }
    .execute(&ctx())
}

fn project_command(
    server: &MockServer,
    dir: &tempfile::TempDir,
    command: ProjectCommands,
    ask: Ask,
) -> peppy::error::Result<()> {
    ProjectCommand {
        command,
        api_url: Some(server.base_url()),
        ask,
        peppy_dirs: Some(dirs(dir)),
    }
    .execute(&ctx())
}

fn use_workspace(workspace: &str) -> WorkspaceCommands {
    WorkspaceCommands::Use {
        workspace: Some(workspace.to_string()),
    }
}

fn use_project(project: Option<&str>, workspace: Option<&str>) -> ProjectCommands {
    ProjectCommands::Use {
        project: project.map(str::to_string),
        workspace: workspace.map(str::to_string),
    }
}

/// The project stays selected when `use` selects its workspace again. A
/// different workspace starts with no project.
#[test]
fn workspace_use_keeps_the_project_of_the_same_workspace_only() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);
    write_selection(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        Some((FIELD, "Field")),
    );

    workspace_command(&server, &dir, use_workspace(WORKSPACE), Ask::Never)
        .expect("the same workspace, by id");
    assert_eq!(selected_project(&dir).as_deref(), Some(FIELD));

    workspace_command(&server, &dir, use_workspace("Robotics lab"), Ask::Never)
        .expect("a different workspace, by name");
    let selection = stored_selection(&dir).expect("selection");
    assert_eq!(selection.workspace.id, SECOND_WORKSPACE);
    assert_eq!(selection.project, None);
}

#[test]
fn workspace_use_asks_when_there_is_more_than_one() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);

    workspace_command(
        &server,
        &dir,
        WorkspaceCommands::Use { workspace: None },
        Ask::scripted([1]),
    )
    .expect("the person selects");

    let selection = stored_selection(&dir).expect("selection");
    assert_eq!(selection.workspace.id, SECOND_WORKSPACE);
    assert_eq!(selection.subject, "user-123");
    assert_eq!(selection.project, None);
}

#[test]
fn workspace_use_with_nobody_to_ask_names_the_argument() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);

    let use_with = |ask| {
        workspace_command(
            &server,
            &dir,
            WorkspaceCommands::Use { workspace: None },
            ask,
        )
    };

    let err = use_with(Ask::Never).expect_err("nobody to ask");
    assert!(
        err.to_string()
            .contains("peppy platform workspace use <id|name>"),
        "{err}"
    );
    let err = use_with(Ask::scripted([])).expect_err("a cancelled menu selects nothing");
    assert_eq!(err.to_string(), "no workspace was selected");
    assert_eq!(
        stored_selection(&dir),
        None,
        "a failed selection writes nothing"
    );
}

/// `use` with a project alone looks in the selected workspace, and moves
/// neither the machine nor the daemon.
#[test]
fn project_use_looks_in_the_selected_workspace() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);
    write_selection(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        Some((PROJECT, "Lab")),
    );
    write_enrollment(&dir, "peer-1", ZID);

    project_command(&server, &dir, use_project(Some("Field"), None), Ask::Never)
        .expect("the account has two workspaces, and the selection names the one to look in");

    let selection = stored_selection(&dir).expect("selection");
    assert_eq!(selection.workspace.id, WORKSPACE);
    assert_eq!(selected_project(&dir).as_deref(), Some(FIELD));
    assert_eq!(
        enrolled_project(&dir).as_deref(),
        Some(PROJECT),
        "a selection does not move the machine"
    );
    assert!(
        !dirs(&dir).runtime_config_dir().exists(),
        "a selection never pokes the daemon"
    );
}

/// `--workspace` alone selects the only project of that workspace, and asks
/// when the workspace has more than one.
#[test]
fn project_use_with_a_workspace_flag_selects_its_only_project() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);
    write_selection(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        Some((PROJECT, "Lab")),
    );

    project_command(
        &server,
        &dir,
        use_project(None, Some("Robotics lab")),
        Ask::Never,
    )
    .expect("one project, no question");
    assert_eq!(selected_project(&dir).as_deref(), Some(ARM));

    let err = project_command(
        &server,
        &dir,
        use_project(None, Some(WORKSPACE)),
        Ask::Never,
    )
    .expect_err("two projects, nobody to ask");
    assert!(
        err.to_string()
            .contains("peppy platform project use <id|name>"),
        "{err}"
    );
    assert_eq!(selected_project(&dir).as_deref(), Some(ARM));

    project_command(
        &server,
        &dir,
        use_project(None, Some(WORKSPACE)),
        Ask::scripted([1]),
    )
    .expect("the person selects");
    assert_eq!(selected_project(&dir).as_deref(), Some(FIELD));
}

/// With no selected workspace, `use` asks for the workspace first.
#[test]
fn project_use_with_no_selected_workspace_asks_for_it() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);

    let err = project_command(&server, &dir, use_project(Some("Field"), None), Ask::Never)
        .expect_err("which workspace?");
    assert!(err.to_string().contains("--workspace"), "{err}");
    assert_eq!(stored_selection(&dir), None);

    project_command(
        &server,
        &dir,
        use_project(None, None),
        Ask::scripted([0, 1]),
    )
    .expect("the workspace, then the project");
    let selection = stored_selection(&dir).expect("selection");
    assert_eq!(selection.workspace.id, WORKSPACE);
    assert_eq!(selected_project(&dir).as_deref(), Some(FIELD));
}

#[test]
fn project_use_needs_a_session() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().expect("temp dir");
    let err = project_command(&server, &dir, use_project(None, None), Ask::Never)
        .expect_err("no session");
    assert!(err.to_string().contains("peppy platform login"), "{err}");
}

#[test]
fn show_list_and_clear_the_selection() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);

    for json in [false, true] {
        workspace_command(&server, &dir, WorkspaceCommands::Show { json }, Ask::Never)
            .expect("workspace show with no selection");
        project_command(&server, &dir, ProjectCommands::Show { json }, Ask::Never)
            .expect("project show with no selection");
    }
    project_command(&server, &dir, ProjectCommands::Clear, Ask::Never)
        .expect("project clear with no selection");

    write_selection(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        Some((FIELD, "Field")),
    );
    for json in [false, true] {
        workspace_command(&server, &dir, WorkspaceCommands::Show { json }, Ask::Never)
            .expect("workspace show");
        workspace_command(&server, &dir, WorkspaceCommands::List { json }, Ask::Never)
            .expect("workspace list");
        project_command(&server, &dir, ProjectCommands::Show { json }, Ask::Never)
            .expect("project show");
        project_command(
            &server,
            &dir,
            ProjectCommands::List {
                workspace: None,
                json,
            },
            Ask::Never,
        )
        .expect("project list: the selected workspace of two");
    }

    project_command(&server, &dir, ProjectCommands::Clear, Ask::Never).expect("project clear");
    let selection = stored_selection(&dir).expect("the workspace stays selected");
    assert_eq!(selection.workspace.id, WORKSPACE);
    assert_eq!(selection.project, None);
    for json in [false, true] {
        project_command(&server, &dir, ProjectCommands::Show { json }, Ask::Never)
            .expect("project show with a workspace alone");
    }

    workspace_command(&server, &dir, WorkspaceCommands::Clear, Ask::Never)
        .expect("workspace clear");
    assert_eq!(stored_selection(&dir), None);
    workspace_command(&server, &dir, WorkspaceCommands::Clear, Ask::Never)
        .expect("clear two times");
}

/// A selection of a different identity is not the selection of this session.
#[test]
fn a_selection_of_a_different_identity_is_not_used() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = enrollable_dir(&server);
    write_selection(
        &dir,
        &server,
        "someone-else",
        (WORKSPACE, "Alice's workspace"),
        Some((FIELD, "Field")),
    );

    let err = enroll_in(&server, &dir, false).expect_err("no selection, more than one workspace");
    assert!(
        err.to_string().contains("peppy platform workspace use"),
        "{err}"
    );
}

#[test]
fn enroll_uses_the_selected_project() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let enroll = mock_enroll(&server, ZID);
    let dir = enrollable_dir(&server);
    write_selection(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        Some((PROJECT, "Lab")),
    );

    enroll_in(&server, &dir, false).expect("the selection names the project");

    assert_eq!(enroll.calls(), 1);
    assert_eq!(enrolled_project(&dir).as_deref(), Some(PROJECT));
}

/// A workspace selected alone is the default workspace of `enroll`, which
/// then takes the only project of it.
#[test]
fn enroll_uses_the_only_project_of_a_workspace_selected_alone() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let enroll = mock_enroll_in(&server, SECOND_WORKSPACE, ARM, ZID);
    let dir = enrollable_dir(&server);
    write_selection(
        &dir,
        &server,
        "user-123",
        (SECOND_WORKSPACE, "Robotics lab"),
        None,
    );

    enroll_in(&server, &dir, false).expect("one project in the selected workspace");

    assert_eq!(enroll.calls(), 1);
    assert_eq!(enrolled_project(&dir).as_deref(), Some(ARM));
}

/// The machine is enrolled in one project and the selection names a different
/// one: `peers` and `router` act on the selected project.
#[test]
fn peers_and_router_use_the_selected_project_before_the_enrollment() {
    let server = MockServer::start();
    let field_router = format!("/api/workspace/{WORKSPACE}/projects/{FIELD}/router");
    let field_peers = server.mock(|when, then| {
        when.method(GET).path(format!("{field_router}/peers"));
        then.status(200).json_body(json!([]));
    });
    server.mock(|when, then| {
        when.method(GET).path(field_router.as_str());
        then.status(200)
            .json_body(router_body("running", "connected"));
    });
    let field_restart = server.mock(|when, then| {
        when.method(POST).path(format!("{field_router}/restart"));
        then.status(202)
            .json_body(router_body("restarting", "connected"));
    });
    let enrolled_restart = mock_router_action(&server, "restart", "restarting");
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-1", ZID);
    write_selection(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        Some((FIELD, "Field")),
    );

    PeersCommand {
        api_url: Some(server.base_url()),
        workspace: None,
        project: None,
        json: false,
        peppy_dirs: Some(dirs(&dir)),
    }
    .execute(&ctx())
    .expect("peers");
    router_command(&server, &dir, RouterAction::Restart, None).expect("restart");

    assert_eq!(field_peers.calls(), 1);
    assert_eq!(field_restart.calls(), 1);
    assert_eq!(enrolled_restart.calls(), 0);
}

#[test]
fn a_refusal_on_the_selected_project_names_project_use() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path(format!(
            "/api/workspace/{WORKSPACE}/projects/p-gone/router/peers"
        ));
        then.status(404)
            .header("content-type", "application/problem+json")
            .json_body(json!({ "type": "about:blank", "title": "Not Found", "status": 404 }));
    });
    let dir = authenticated_dir(&server);
    write_selection(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        Some(("p-gone", "Gone")),
    );

    let err = PeersCommand {
        api_url: Some(server.base_url()),
        workspace: None,
        project: None,
        json: false,
        peppy_dirs: Some(dirs(&dir)),
    }
    .execute(&ctx())
    .expect_err("the project is gone");
    assert_eq!(
        err.to_string(),
        "Not Found. The selected project can be out of date. Run `peppy platform project use`."
    );
}

// ─── logout ──────────────────────────────────────────────────────────────

#[test]
fn logout_revokes_both_tokens_and_keeps_the_enrollment() {
    let server = MockServer::start();
    mock_discovery(&server);
    let revoke_refresh = server.mock(|when, then| {
        when.method(POST)
            .path("/oauth/v2/revoke")
            .body_includes("token=seeded-refresh")
            .body_includes("token_type_hint=refresh_token");
        then.status(200);
    });
    let revoke_access = server.mock(|when, then| {
        when.method(POST)
            .path("/oauth/v2/revoke")
            .body_includes("token=seeded-access")
            .body_includes("token_type_hint=access_token");
        then.status(200);
    });
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-1", ZID);
    write_selection(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        Some((PROJECT, "Lab")),
    );

    LogoutCommand {
        api_url: Some(server.base_url()),
        peppy_dirs: Some(dirs(&dir)),
    }
    .execute(&ctx())
    .expect("logout");

    assert_eq!(revoke_refresh.calls(), 1);
    assert_eq!(revoke_access.calls(), 1);
    let after = storage::load(&creds_path(&dir)).expect("load creds");
    assert!(after.session.is_none(), "the session is cleared");
    assert_eq!(
        platform_selection::load(&dirs(&dir)).unwrap(),
        None,
        "the selection goes with the session"
    );
    assert!(
        enrollment::load(&dirs(&dir)).unwrap().is_some(),
        "logout leaves the enrollment in place"
    );
    assert!(
        !dirs(&dir).runtime_config_dir().exists(),
        "logout never pokes the daemon"
    );
}

#[test]
fn logout_clears_the_session_when_revocation_fails() {
    let server = MockServer::start();
    mock_discovery(&server);
    server.mock(|when, then| {
        when.method(POST).path("/oauth/v2/revoke");
        then.status(500);
    });
    let dir = authenticated_dir(&server);

    LogoutCommand {
        api_url: Some(server.base_url()),
        peppy_dirs: Some(dirs(&dir)),
    }
    .execute(&ctx())
    .expect("logout is best effort at the issuer");

    assert!(storage::load(&creds_path(&dir)).unwrap().session.is_none());
}

#[test]
fn logout_heals_a_malformed_credentials_file() {
    // A malformed (unversioned) credentials file fails to parse with
    // `AuthError::Auth`. Logout treats that as "already logged out", but it must
    // still rewrite the file to a clean default so the bad file does not linger.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir conf");
    std::fs::write(
        &path,
        r#"{ session: { api_url: "http://x", issuer: "http://y", client_id: "c",
            access_token: "a", refresh_token: "r", expires_at: 1, token_type: "Bearer",
            scope: "openid" } }"#,
    )
    .expect("write malformed creds");

    LogoutCommand {
        // Never contacted: the malformed path returns "Not logged in" before any
        // call. A dummy keeps the test independent of build-default URLs.
        api_url: Some("http://127.0.0.1:9".to_string()),
        peppy_dirs: Some(dirs(&dir)),
    }
    .execute(&ctx())
    .expect("logout tolerates a malformed file");

    let after = storage::load(&path).expect("malformed file must be healed, not left on disk");
    assert!(after.session.is_none(), "healed file is logged out");
}

// ─── whoami / workspace list / project list ─────────────────────────────

#[test]
fn whoami_runs_against_a_seeded_session() {
    let server = MockServer::start();
    let _me = mock_me(&server);
    let dir = authenticated_dir(&server);

    // Both the human and the --json formatter must run without error.
    for json in [false, true] {
        WhoamiCommand {
            api_url: Some(server.base_url()),
            json,
            peppy_dirs: Some(dirs(&dir)),
        }
        .execute(&ctx())
        .expect("whoami");
    }
}

#[test]
fn workspace_and_project_list_in_both_formats() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let dir = authenticated_dir(&server);

    for json in [false, true] {
        workspace_command(&server, &dir, WorkspaceCommands::List { json }, Ask::Never)
            .expect("workspace list");
        project_command(
            &server,
            &dir,
            ProjectCommands::List {
                workspace: None,
                json,
            },
            Ask::Never,
        )
        .expect("project list (the only workspace is picked)");
    }
    let err = project_command(
        &server,
        &dir,
        ProjectCommands::List {
            workspace: Some("Nope".to_string()),
            json: false,
        },
        Ask::Never,
    )
    .expect_err("an unknown workspace is refused");
    assert!(
        err.to_string().contains("Alice's workspace"),
        "lists the choices: {err}"
    );
}

#[test]
fn a_command_that_needs_a_session_fails_without_one() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().expect("temp dir");

    let err = workspace_command(
        &server,
        &dir,
        WorkspaceCommands::List { json: false },
        Ask::Never,
    )
    .expect_err("no session");
    assert!(err.to_string().contains("peppy platform login"), "{err}");
}

// ─── enroll ──────────────────────────────────────────────────────────────

fn enroll_in(
    server: &MockServer,
    dir: &tempfile::TempDir,
    replace: bool,
) -> peppy::error::Result<()> {
    EnrollCommand {
        api_url: Some(server.base_url()),
        workspace: None,
        project: None,
        replace,
        yes: true,
        peppy_dirs: Some(dirs(dir)),
    }
    .execute(&ctx())
}

/// The whole enrollment: pick the only workspace and project, post the CSR
/// under the name of the running daemon, write the bundle, poke the daemon,
/// wait for it to come back under the new identity, and verify the link with a
/// second poke.
#[test]
fn enroll_writes_the_bundle_and_waits_for_the_daemon_to_restart() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let enroll = mock_enroll(&server, ZID);
    let dir = authenticated_dir(&server);
    write_daemon_state(&dir, "local", "7f3a9c1e");
    let stub = stub_restarting_daemon(
        &dir,
        PROJECT,
        ZID,
        "{\"status\":\"ok\",\"applied\":\"tls/rtr-p.us-east-1.robocloud.dev.peppy.bot:7447\"}\n",
    );

    enroll_in(&server, &dir, false).expect("enroll");

    assert_eq!(enroll.calls(), 1);
    let requests = stub.join().expect("stub thread");
    assert_eq!(requests, ["refederate", "refederate"]);
    let enrolled = enrollment::load(&dirs(&dir))
        .expect("the bundle parses")
        .expect("enrolled");
    let d = &enrolled.document;
    assert_eq!(d.workspace_id, WORKSPACE);
    assert_eq!(d.project_id, PROJECT);
    assert_eq!(d.peer_id, "peer-1");
    assert_eq!(d.peer_name, "robot-7");
    assert_eq!(d.zenoh_id.as_str(), ZID);
    assert_eq!(d.namespace.as_str(), PROJECT);
    assert_eq!(d.router.locator(), format!("tls/{ROUTER_HOST}:7447"));
    assert_eq!(d.api_url, server.base_url());
    assert_eq!(
        enrolled.certificate,
        test_support::certificate_validity(),
        "the validity is read from the leaf the platform signed"
    );
    assert!(enrolled.certificate.renewal_due_at() < enrolled.certificate.not_after);
    let key = std::fs::read_to_string(&enrolled.peer_key).unwrap();
    assert!(
        key.starts_with("-----BEGIN PRIVATE KEY-----"),
        "a PKCS#8 key was minted"
    );
    let cert = std::fs::read_to_string(&enrolled.peer_certificate).unwrap();
    assert_eq!(
        cert.matches("-----BEGIN CERTIFICATE-----").count(),
        1,
        "the peer presents the leaf alone"
    );
    assert!(
        !cert.contains("issuer"),
        "the chain is not in the leaf file"
    );
    let chain = std::fs::read_to_string(dirs(&dir).peer_dir().join("chain.crt")).unwrap();
    assert!(chain.contains("issuer"), "the chain is kept apart: {chain}");
}

/// The daemon names this machine and joins the router, so an enrollment
/// with no daemon running stops before any call to the platform: no token is
/// refreshed and nothing is minted.
#[test]
fn enroll_without_a_running_daemon_is_refused_before_any_platform_call() {
    for (write_state, what) in [
        (None, "no state file"),
        (
            Some(write_dead_daemon_state as fn(&tempfile::TempDir)),
            "the state of a daemon that died",
        ),
    ] {
        let server = MockServer::start();
        let any_api = server.mock(|when, then| {
            when.path_includes("/");
            then.status(500);
        });
        let dir = authenticated_dir(&server);
        if let Some(write_state) = write_state {
            write_state(&dir);
        }

        let err = enroll_in(&server, &dir, false).expect_err("no daemon");

        let message = err.to_string();
        assert!(
            message.contains("no peppy daemon is running"),
            "{what}: {message}"
        );
        assert!(message.contains("core node"), "{what}: {message}");
        assert!(message.contains("peppy service serve"), "{what}: {message}");
        assert_eq!(any_api.calls(), 0, "{what}");
        assert_eq!(enrolled_project(&dir), None, "{what}");
    }
}

#[test]
fn enroll_refuses_a_second_enrollment_without_replace() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let enroll = mock_enroll(&server, ZID);
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-old", "7f3a9c1e");

    let err = enroll_in(&server, &dir, false).expect_err("already enrolled");
    assert!(err.to_string().contains("--replace"), "{err}");
    assert_eq!(enroll.calls(), 0, "nothing is minted");
    assert_eq!(
        enrollment::load(&dirs(&dir))
            .unwrap()
            .unwrap()
            .document
            .peer_id,
        "peer-old"
    );
}

#[test]
fn enroll_replace_enrolls_anew_then_removes_the_old_peer() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let enroll = mock_enroll(&server, ZID);
    let remove_old = server.mock(|when, then| {
        when.method(DELETE).path(format!("{PEERS_PATH}/peer-old"));
        then.status(202).json_body(json!({
            "phase": "running", "desired_state": "running", "can_manage_infra": true,
            "size": "micro", "entitled_sizes": ["micro"], "pending_changes": true,
            "pending_change_entries": [], "peers": []
        }));
    });
    let dir = enrollable_dir(&server);
    write_enrollment(&dir, "peer-old", "7f3a9c1e");

    enroll_in(&server, &dir, true).expect("re-enroll");

    assert_eq!(enroll.calls(), 1);
    assert_eq!(remove_old.calls(), 1);
    assert_eq!(
        enrollment::load(&dirs(&dir))
            .unwrap()
            .unwrap()
            .document
            .peer_id,
        "peer-1"
    );
}

#[test]
fn enroll_surfaces_the_platform_refusal_and_writes_nothing() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    server.mock(|when, then| {
        when.method(POST).path(PEERS_PATH);
        then.status(422)
            .header("content-type", "application/problem+json")
            .json_body(json!({
                "type": "https://peppy.bot/problems/peer-limit-reached",
                "title": "Peer limit reached", "status": 422,
                "detail": "this router admits 5 peers; remove one first",
            }));
    });
    mock_router_and_peers_with(&server, "pending_restart");
    let dir = enrollable_dir(&server);

    let err = enroll_in(&server, &dir, false).expect_err("refused");
    let message = err.to_string();
    assert!(
        message.starts_with("Peer limit reached: this router admits 5 peers; remove one first"),
        "the refusal comes first: {message}"
    );
    assert!(
        message.contains("robot-7"),
        "the peers are listed: {message}"
    );
    assert!(
        message.contains(&format!(
            "peppy platform router restart --workspace {WORKSPACE} --project {PROJECT}"
        )),
        "a removed peer keeps a slot, so the remedy is the restart: {message}"
    );
    assert!(enrollment::load(&dirs(&dir)).unwrap().is_none());
}

/// When the platform says how many slots a restart frees, the CLI takes its
/// word: it does not read the peers, and it names the restart only when the
/// number is above zero.
#[test]
fn enroll_on_a_full_router_follows_the_slots_the_platform_reports() {
    for (pending_removals, names_the_restart) in [(2, true), (0, false)] {
        let server = MockServer::start();
        mock_workspaces_and_projects(&server);
        server.mock(move |when, then| {
            when.method(POST).path(PEERS_PATH);
            then.status(422)
                .header("content-type", "application/problem+json")
                .json_body(json!({
                    "type": "https://peppy.bot/problems/peer-limit-reached",
                    "title": "Peer limit reached", "status": 422,
                    "pending_removals": pending_removals,
                    "a_member_this_cli_does_not_know": true,
                }));
        });
        let peers = server.mock(|when, then| {
            when.method(GET).path(PEERS_PATH);
            then.status(500);
        });
        let dir = enrollable_dir(&server);

        let message = enroll_in(&server, &dir, false)
            .expect_err("refused")
            .to_string();
        assert_eq!(
            message.contains("peppy platform router restart --workspace"),
            names_the_restart,
            "{pending_removals}: {message}"
        );
        assert_eq!(
            message.contains("frees no slot"),
            !names_the_restart,
            "{pending_removals}: {message}"
        );
        assert_eq!(peers.calls(), 0, "the platform gave the answer");
    }
}

/// The platform could not sign in time. The CLI gives the delay the platform
/// asked for and does not send the enrollment again.
#[test]
fn enroll_prints_the_retry_after_delay_and_does_not_retry() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let refused = server.mock(|when, then| {
        when.method(POST).path(PEERS_PATH);
        then.status(503)
            .header("content-type", "application/problem+json")
            .header("retry-after", "5")
            .json_body(json!({
                "type": "https://peppy.bot/problems/provisioner-unavailable",
                "title": "Provisioner unavailable", "status": 503,
            }));
    });
    let dir = enrollable_dir(&server);

    let err = enroll_in(&server, &dir, false).expect_err("refused");
    assert_eq!(
        err.to_string(),
        "Provisioner unavailable. Run the command again in 5 seconds."
    );
    assert_eq!(refused.calls(), 1, "the enrollment is sent once");
    assert!(enrollment::load(&dirs(&dir)).unwrap().is_none());
}

#[test]
fn enroll_in_external_mode_writes_the_bundle_without_poking() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let _enroll = mock_enroll(&server, ZID);
    let dir = authenticated_dir(&server);
    write_external_zenoh_config(&dir);
    write_external_daemon_state(&dir);

    enroll_in(&server, &dir, false).expect("external enroll");

    assert!(enrollment::load(&dirs(&dir)).unwrap().is_some());
    assert!(
        !dirs(&dir).runtime_config_dir().exists(),
        "external mode never touches the control socket"
    );
}

#[test]
fn enroll_rejects_a_zenoh_id_the_router_would_refuse() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let _enroll = mock_enroll(&server, "0abc");
    let dir = enrollable_dir(&server);

    let err = enroll_in(&server, &dir, false).expect_err("a leading zero is refused");
    assert!(err.to_string().contains("router id"), "{err}");
    assert!(enrollment::load(&dirs(&dir)).unwrap().is_none());
}

/// The router address is a member of the answer. An answer with no address
/// names no router to dial, so nothing is written.
#[test]
fn enroll_rejects_an_answer_with_no_router_address() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    server.mock(|when, then| {
        when.method(POST).path(PEERS_PATH);
        then.status(201).json_body(json!({
            "peer": { "id": "peer-1", "name": "robot-7", "certificate_cn": "robot-7",
                      "status": "unknown", "certificate_expires_at": "2027-01-01T00:00:00Z",
                      "created_at": "2026-10-03T00:00:00Z" },
            "certificate": "leaf", "chain": "issuer", "trust_anchor": "ca",
            "zenoh_id": ZID,
            "namespace": PROJECT,
            "zenoh_config": format!("{{ connect: {{ endpoints: [\"tls/{ROUTER_HOST}:7447\"] }} }}"),
        }));
    });
    let dir = enrollable_dir(&server);

    let err = enroll_in(&server, &dir, false).expect_err("no address");
    assert!(err.to_string().contains("address"), "{err}");
    assert!(enrollment::load(&dirs(&dir)).unwrap().is_none());
}

// ─── unenroll ────────────────────────────────────────────────────────────

fn unenroll_in(
    server: &MockServer,
    dir: &tempfile::TempDir,
    local_only: bool,
) -> peppy::error::Result<()> {
    UnenrollCommand {
        api_url: Some(server.base_url()),
        local_only,
        yes: true,
        peppy_dirs: Some(dirs(dir)),
    }
    .execute(&ctx())
}

#[test]
fn unenroll_removes_the_peer_and_the_bundle_and_restarts_the_daemon() {
    let server = MockServer::start();
    let remove = server.mock(|when, then| {
        when.method(DELETE).path(format!("{PEERS_PATH}/peer-1"));
        then.status(202).json_body(json!({
            "phase": "running", "desired_state": "running", "can_manage_infra": true,
            "size": "micro", "entitled_sizes": ["micro"], "pending_changes": true,
            "pending_change_entries": [], "peers": []
        }));
    });
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-1", ZID);
    write_daemon_state(&dir, PROJECT, ZID);
    let stub = stub_restarting_daemon(
        &dir,
        "local",
        "7f3a9c1e",
        "{\"status\":\"ok\",\"applied\":null}\n",
    );

    unenroll_in(&server, &dir, false).expect("unenroll");

    assert_eq!(remove.calls(), 1);
    assert_eq!(stub.join().unwrap(), ["refederate", "refederate"]);
    assert!(enrollment::load(&dirs(&dir)).unwrap().is_none());
    assert!(!dirs(&dir).peer_dir().exists());
}

#[test]
fn unenroll_local_only_skips_the_platform() {
    let server = MockServer::start();
    let remove = server.mock(|when, then| {
        when.method(DELETE).path(format!("{PEERS_PATH}/peer-1"));
        then.status(500);
    });
    // No session at all: local-only must not need one.
    let dir = tempfile::tempdir().expect("temp dir");
    write_enrollment(&dir, "peer-1", ZID);

    unenroll_in(&server, &dir, true).expect("local-only unenroll");

    assert_eq!(remove.calls(), 0);
    assert!(enrollment::load(&dirs(&dir)).unwrap().is_none());
}

#[test]
fn unenroll_without_a_session_explains_local_only() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().expect("temp dir");
    write_enrollment(&dir, "peer-1", ZID);

    let err = unenroll_in(&server, &dir, false).expect_err("needs a session");
    assert!(err.to_string().contains("--local-only"), "{err}");
    assert!(
        enrollment::load(&dirs(&dir)).unwrap().is_some(),
        "nothing was deleted"
    );
}

#[test]
fn unenroll_when_not_enrolled_is_a_no_op() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().expect("temp dir");
    unenroll_in(&server, &dir, false).expect("nothing to do");
}

// ─── status / peers ──────────────────────────────────────────────────────

fn mock_router_and_peers(server: &MockServer) {
    mock_router_and_peers_with(server, "connected");
}

/// A router with two peers, the first in `first_peer_status`, and one change
/// that waits for a restart.
fn mock_router_and_peers_with(server: &MockServer, first_peer_status: &'static str) {
    server.mock(move |when, then| {
        when.method(GET).path(ROUTER_PATH);
        then.status(200)
            .json_body(router_body("running", first_peer_status));
    });
    server.mock(move |when, then| {
        when.method(GET).path(PEERS_PATH);
        then.status(200).json_body(json!([
            { "id": "peer-1", "name": "robot-7", "certificate_cn": "robot-7",
              "status": first_peer_status, "certificate_expires_at": "2027-01-01T00:00:00Z",
              "created_at": "2026-10-03T00:00:00Z" },
            { "id": "peer-2", "name": "bench", "certificate_cn": "bench",
              "status": "unknown", "certificate_expires_at": "2027-01-01T00:00:00Z",
              "created_at": "2026-10-03T00:00:00Z" }
        ]));
    });
}

#[test]
fn status_reports_the_enrollment_the_daemon_and_the_platform() {
    let server = MockServer::start();
    mock_router_and_peers(&server);
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-1", ZID);

    for json in [false, true] {
        StatusCommand {
            api_url: Some(server.base_url()),
            json,
            peppy_dirs: Some(dirs(&dir)),
        }
        .execute(&ctx())
        .expect("status");
    }
}

#[test]
fn status_without_an_enrollment_or_a_session_still_runs() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().expect("temp dir");
    for json in [false, true] {
        StatusCommand {
            api_url: Some(server.base_url()),
            json,
            peppy_dirs: Some(dirs(&dir)),
        }
        .execute(&ctx())
        .expect("status needs nothing");
    }
}

#[test]
fn peers_default_to_the_enrolled_project() {
    let server = MockServer::start();
    mock_router_and_peers(&server);
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-1", ZID);

    for json in [false, true] {
        PeersCommand {
            api_url: Some(server.base_url()),
            workspace: None,
            project: None,
            json,
            peppy_dirs: Some(dirs(&dir)),
        }
        .execute(&ctx())
        .expect("peers");
    }

    let unenrolled = authenticated_dir(&server);
    let err = PeersCommand {
        api_url: Some(server.base_url()),
        workspace: None,
        project: None,
        json: false,
        peppy_dirs: Some(dirs(&unenrolled)),
    }
    .execute(&ctx())
    .expect_err("no enrollment and no flags");
    assert!(err.to_string().contains("--project"), "{err}");
}

// ─── router ──────────────────────────────────────────────────────────────

fn router_body(phase: &str, first_peer_status: &str) -> serde_json::Value {
    json!({
        "phase": phase, "desired_state": "running", "can_manage_infra": true,
        "address": { "host": ROUTER_HOST, "port": 7447 },
        "size": "micro", "entitled_sizes": ["micro"],
        "pending_changes": true,
        "pending_change_entries": [ { "kind": "peer",
            "description": "peer robot-7 removed", "staged_at": "2026-09-28T10:00:00Z" } ],
        "peers": [ { "id": "peer-1", "status": first_peer_status } ]
    })
}

fn mock_router_action<'a>(server: &'a MockServer, action: &str, phase: &str) -> httpmock::Mock<'a> {
    let body = router_body(phase, "connected");
    let path = format!("{ROUTER_PATH}/{action}");
    server.mock(move |when, then| {
        when.method(POST).path(path.as_str());
        then.status(202).json_body(body);
    })
}

fn router_command(
    server: &MockServer,
    dir: &tempfile::TempDir,
    action: RouterAction,
    project: Option<&str>,
) -> peppy::error::Result<()> {
    RouterCommand {
        action,
        api_url: Some(server.base_url()),
        workspace: None,
        project: project.map(str::to_string),
        yes: true,
        peppy_dirs: Some(dirs(dir)),
    }
    .execute(&ctx())
}

/// With no flag, the router is the one of the project this machine is enrolled
/// in: no workspace or project is listed to find it.
#[test]
fn router_restart_uses_the_enrolled_project() {
    let server = MockServer::start();
    mock_router_and_peers(&server);
    let restart = mock_router_action(&server, "restart", "restarting");
    let workspaces = server.mock(|when, then| {
        when.method(GET).path("/api/workspaces");
        then.status(500);
    });
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-1", ZID);

    router_command(&server, &dir, RouterAction::Restart, None).expect("restart");

    assert_eq!(restart.calls(), 1);
    assert_eq!(workspaces.calls(), 0, "the enrollment names the project");
}

/// A flag names the project, also on a machine that is not enrolled.
#[test]
fn router_restart_takes_the_project_from_the_flag() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    mock_router_and_peers(&server);
    let restart = mock_router_action(&server, "restart", "restarting");
    let dir = authenticated_dir(&server);

    router_command(&server, &dir, RouterAction::Restart, Some("Lab")).expect("restart by name");

    assert_eq!(restart.calls(), 1);
}

/// With no flag and no enrollment the command has no router. It lists the
/// projects and sends nothing to a router.
#[test]
fn router_restart_without_a_target_lists_the_projects_and_does_nothing() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let restart = mock_router_action(&server, "restart", "restarting");
    let dir = authenticated_dir(&server);

    let err = router_command(&server, &dir, RouterAction::Restart, None)
        .expect_err("no project is selected for the person");
    let message = err.to_string();
    assert!(message.contains("--project"), "{message}");
    assert!(
        message.contains(PROJECT) && message.contains("Lab"),
        "{message}"
    );
    assert!(
        !message.contains("p-old"),
        "archived projects are not offered: {message}"
    );
    assert_eq!(restart.calls(), 0);
}

#[test]
fn router_restart_without_the_permission_says_which_permission() {
    let server = MockServer::start();
    mock_router_and_peers(&server);
    server.mock(|when, then| {
        when.method(POST).path(format!("{ROUTER_PATH}/restart"));
        then.status(403)
            .header("content-type", "application/problem+json")
            .json_body(json!({ "type": "about:blank", "title": "Forbidden", "status": 403 }));
    });
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-1", ZID);

    let err = router_command(&server, &dir, RouterAction::Restart, None).expect_err("403");
    assert!(
        err.to_string().contains("manage the infrastructure"),
        "{err}"
    );
}

/// A stopped router has nothing to restart. The command says so before it
/// sends anything, and gives the start and then the restart.
#[test]
fn router_restart_of_a_stopped_router_names_the_start_then_the_restart() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path(ROUTER_PATH);
        then.status(200)
            .json_body(router_body("stopped", "unknown"));
    });
    let restart = mock_router_action(&server, "restart", "restarting");
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-1", ZID);

    let err = router_command(&server, &dir, RouterAction::Restart, None).expect_err("stopped");
    let message = err.to_string();
    assert!(message.contains("the router is stopped"), "{message}");
    let start = message
        .find("peppy platform router start")
        .expect("the start command");
    let again = message
        .find("peppy platform router restart")
        .expect("the restart command");
    assert!(start < again, "the start comes first: {message}");
    assert!(
        message.contains("A start alone does not apply"),
        "{message}"
    );
    assert_eq!(restart.calls(), 0);
}

/// The router can stop between the read and the restart. The refusal of the
/// platform, matched on its type, gives the same answer.
#[test]
fn a_restart_the_platform_refuses_as_stopped_gives_the_same_answer() {
    let server = MockServer::start();
    mock_router_and_peers(&server);
    server.mock(|when, then| {
        when.method(POST).path(format!("{ROUTER_PATH}/restart"));
        then.status(409)
            .header("content-type", "application/problem+json")
            .json_body(json!({
                "type": "https://peppy.bot/problems/router-stopped",
                "title": "Router stopped", "status": 409,
            }));
    });
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-1", ZID);

    let err = router_command(&server, &dir, RouterAction::Restart, None).expect_err("stopped");
    let message = err.to_string();
    assert!(message.contains("peppy platform router start"), "{message}");
    assert!(
        message.contains("peppy platform router restart"),
        "{message}"
    );
}

#[test]
fn router_start_calls_the_start_route() {
    let server = MockServer::start();
    mock_router_and_peers(&server);
    let start = mock_router_action(&server, "start", "provisioning");
    let restart = mock_router_action(&server, "restart", "restarting");
    let dir = authenticated_dir(&server);
    write_enrollment(&dir, "peer-1", ZID);

    router_command(&server, &dir, RouterAction::Start, None).expect("start");

    assert_eq!(start.calls(), 1);
    assert_eq!(restart.calls(), 0);
}

// ─── the group ───────────────────────────────────────────────────────────

#[test]
fn every_platform_command_refuses_a_core_node_override() {
    let server = MockServer::start();
    // Registered before the commands run, so the call count below is evidence.
    let any_api = server.mock(|when, then| {
        when.path_includes("/");
        then.status(500);
    });
    let redirected = Arc::new(
        AppContext::from_current_dir()
            .expect("cwd is readable")
            .with_core_node_override(Some("robot-7".to_string())),
    );
    let api_url = Some(server.base_url());

    let commands: Vec<(&str, PlatformCommands)> = vec![
        (
            "login",
            PlatformCommands::Login {
                no_browser: true,
                workspace: None,
                project: None,
                no_enroll: false,
                yes: true,
            },
        ),
        ("logout", PlatformCommands::Logout),
        ("whoami", PlatformCommands::Whoami { json: false }),
        (
            "workspace show",
            PlatformCommands::Workspace {
                command: WorkspaceCommands::Show { json: false },
            },
        ),
        (
            "workspace list",
            PlatformCommands::Workspace {
                command: WorkspaceCommands::List { json: false },
            },
        ),
        (
            "workspace use",
            PlatformCommands::Workspace {
                command: WorkspaceCommands::Use { workspace: None },
            },
        ),
        (
            "workspace clear",
            PlatformCommands::Workspace {
                command: WorkspaceCommands::Clear,
            },
        ),
        (
            "project show",
            PlatformCommands::Project {
                command: ProjectCommands::Show { json: false },
            },
        ),
        (
            "project list",
            PlatformCommands::Project {
                command: ProjectCommands::List {
                    workspace: None,
                    json: false,
                },
            },
        ),
        (
            "project use",
            PlatformCommands::Project {
                command: ProjectCommands::Use {
                    project: None,
                    workspace: None,
                },
            },
        ),
        (
            "project clear",
            PlatformCommands::Project {
                command: ProjectCommands::Clear,
            },
        ),
        (
            "enroll",
            PlatformCommands::Enroll {
                workspace: None,
                project: None,
                replace: false,
                yes: true,
            },
        ),
        (
            "unenroll",
            PlatformCommands::Unenroll {
                local_only: false,
                yes: true,
            },
        ),
        ("status", PlatformCommands::Status { json: false }),
        (
            "peers",
            PlatformCommands::Peers {
                workspace: None,
                project: None,
                json: false,
            },
        ),
        (
            "router restart",
            PlatformCommands::Router {
                command: RouterCommands::Restart {
                    workspace: None,
                    project: None,
                    yes: true,
                },
            },
        ),
        (
            "router start",
            PlatformCommands::Router {
                command: RouterCommands::Start {
                    workspace: None,
                    project: None,
                },
            },
        ),
    ];

    for (name, command) in commands {
        let error = PlatformCommand {
            api_url: api_url.clone(),
            command,
        }
        .execute(&redirected)
        .expect_err(&format!("`platform {name}` must refuse --core-node"));
        assert!(
            error.to_string().contains("--core-node"),
            "`platform {name}` must name the flag it refused: {error}"
        );
    }
    assert_eq!(
        any_api.calls(),
        0,
        "the override must be refused before any command reaches the backend"
    );
}
