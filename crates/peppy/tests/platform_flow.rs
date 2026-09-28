//! Command-level platform tests with every HTTP endpoint mocked (`httpmock`):
//! the public `/cli/auth-config`, OIDC discovery, the Zitadel device, token and
//! revocation endpoints, and the backend's `/me`, workspace, project and
//! router-peer routes. All state is isolated per test via the `peppy_dirs`
//! seam pointed at a tempdir (no `PEPPY_HOME` mutation, so tests run in
//! parallel); the credentials file, the context, the enrollment bundle and
//! `peppy_config.json5` all land there. A command that can ask the person is
//! told how to ask (`Ask`), so no test depends on a terminal. A running daemon is stood in for by a
//! stub on the control socket. The engine internals (resolver, the platform
//! client, the enrollment store, the CSR) are covered by the `auth` crate's
//! own tests.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;

use daemon_config::consts::PeppyDirs;
use httpmock::prelude::*;
use secrecy::ExposeSecret;
use serde_json::json;

use auth::context::{self as platform_context, CONTEXT_VERSION, Named, PlatformContext};
use auth::enrollment::{self, EnrollmentBundle, EnrollmentDocument, RouterEndpoint};
use auth::storage::{self, Credentials, ProfileCreds};
use daemon::state::DaemonState;
use peppy::commands::Command;
use peppy::commands::platform::configure::ConfigureCommand;
use peppy::commands::platform::context::{ContextAction, ContextCommand, ContextCommands};
use peppy::commands::platform::enroll::EnrollCommand;
use peppy::commands::platform::login::LoginCommand;
use peppy::commands::platform::logout::LogoutCommand;
use peppy::commands::platform::peers::PeersCommand;
use peppy::commands::platform::projects::ProjectsCommand;
use peppy::commands::platform::router::{RouterAction, RouterCommand, RouterCommands};
use peppy::commands::platform::select::Ask;
use peppy::commands::platform::status::StatusCommand;
use peppy::commands::platform::unenroll::UnenrollCommand;
use peppy::commands::platform::whoami::WhoamiCommand;
use peppy::commands::platform::workspaces::WorkspacesCommand;
use peppy::commands::platform::{PlatformCommand, PlatformCommands};
use peppy::context::AppContext;

const WORKSPACE: &str = "4f1b2e2c-9a71-4d0e-b3c8-0d2b9f6a11c4";
const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
const ZID: &str = "2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5";
const ROUTER_HOST: &str = "rtr-p.us-east-1.robocloud.dev.peppy.bot";
const PEERS_PATH: &str = "/api/workspace/4f1b2e2c-9a71-4d0e-b3c8-0d2b9f6a11c4/projects/550e8400-e29b-41d4-a716-446655440000/router/peers";
const ROUTER_PATH: &str = "/api/workspace/4f1b2e2c-9a71-4d0e-b3c8-0d2b9f6a11c4/projects/550e8400-e29b-41d4-a716-446655440000/router";

/// Builds the cli/auth-config + OIDC discovery + device-authorization + token mocks
/// for a server whose issuer is its own base URL, with the device grant
/// succeeding immediately.
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
    mock_discovery(server);
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

/// `POST .../router/peers` answering a signed enrollment for any request.
fn mock_enroll<'a>(server: &'a MockServer, zid: &str) -> httpmock::Mock<'a> {
    let zid = zid.to_string();
    server.mock(move |when, then| {
        // Only a PEM signing request is answered, so a call count of one proves
        // the CLI posted what it minted.
        when.method(POST)
            .path(PEERS_PATH)
            .body_includes("-----BEGIN CERTIFICATE REQUEST-----");
        then.status(201).json_body(json!({
            "peer": { "id": "peer-1", "name": "robot-7", "certificate_cn": "robot-7",
                      "status": "unknown", "certificate_expires_at": "2027-01-01T00:00:00Z",
                      "created_at": "2026-10-03T00:00:00Z" },
            "certificate": "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n",
            "chain": "-----BEGIN CERTIFICATE-----\nissuer\n-----END CERTIFICATE-----\n",
            "trust_anchor": "-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n",
            "zenoh_id": zid,
            "namespace": PROJECT,
            "zenoh_config": format!("{{ connect: {{ endpoints: [\"tls/{ROUTER_HOST}:7447\"] }} }}"),
        }));
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

/// Writes an enrollment in `PROJECT` under `dir`, as a previous `enroll` would
/// have, with placeholder PEM material.
fn write_enrollment(dir: &tempfile::TempDir, peer_id: &str, zid: &str) {
    let bundle = EnrollmentBundle {
        document: EnrollmentDocument {
            version: enrollment::ENROLLMENT_VERSION,
            api_url: "https://api.example".into(),
            workspace_id: WORKSPACE.into(),
            project_id: PROJECT.into(),
            peer_id: peer_id.into(),
            peer_name: "robot-7".into(),
            zenoh_id: pmi::RouterId::parse(zid).unwrap(),
            namespace: config::namespace::Namespace::parse(PROJECT).unwrap(),
            router: RouterEndpoint::parse(ROUTER_HOST, 7447).unwrap(),
            certificate_expires_at: 9_999_999_999,
            enrolled_at: 1_700_000_000,
        },
        peer_key_pem: storage::secret("key".into()),
        peer_certificate_pem: "cert".into(),
        trust_anchor_pem: "ca".into(),
        chain_pem: "chain".into(),
        platform_zenoh_config: "{}".into(),
    };
    enrollment::save(&dirs(dir), &bundle).expect("write enrollment");
}

/// Writes a daemon state file recording this generation under `namespace` and
/// `router_id`, with this test process as the pid (so `is_running` holds) and a
/// managed router (so commands poke the control socket).
fn write_daemon_state(dir: &tempfile::TempDir, namespace: &str, router_id: Option<&str>) {
    let state = DaemonState::new(
        "cn-local-daemon",
        "127.0.0.1",
        7447,
        "test-git-hash",
        30,
        config::namespace::Namespace::parse(namespace).expect("valid namespace"),
        router_id.map(|id| pmi::RouterId::parse(id).unwrap()),
        true,
    );
    let path = DaemonState::state_file_in(dir.path());
    std::fs::create_dir_all(path.parent().expect("state file has a parent"))
        .expect("state file dir");
    DaemonState::write_to(&path, &state).expect("write daemon state");
}

/// A stub daemon on the control socket under `dir`. It answers the first poke
/// with `restarting`, then rewrites the daemon state to the identity the
/// enrollment now prescribes (as the rebuilt generation would), and answers
/// the second poke with `reply`. Returns the request lines it saw.
fn stub_restarting_daemon(
    dir: &tempfile::TempDir,
    namespace: &'static str,
    router_id: Option<&'static str>,
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
        let state = DaemonState::new(
            "cn-local-daemon",
            "127.0.0.1",
            7447,
            "test-git-hash",
            30,
            config::namespace::Namespace::parse(namespace).unwrap(),
            router_id.map(|id| pmi::RouterId::parse(id).unwrap()),
            true,
        );
        DaemonState::write_to(&DaemonState::state_file_in(&root), &state).expect("rewrite state");
        answer(&mut line, reply);
        seen.push(line.trim().to_string());
        seen
    })
}

// ─── login ───────────────────────────────────────────────────────────────

#[test]
fn login_persists_credentials_and_resolves_identity() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let me = mock_me(&server);

    let dir = tempfile::tempdir().expect("temp dir");
    let path = creds_path(&dir);

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        no_configure: true,
        ask: Ask::Never,
        peppy_dirs: Some(dirs(&dir)),
    }
    .execute(&ctx())
    .expect("login needs no daemon");

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
        "login never touches the daemon"
    );
}

#[test]
fn login_seeds_peppy_config_with_resource_servers_block() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-3");
    let _me = mock_me(&server);
    let dir = tempfile::tempdir().expect("temp dir");

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        no_configure: true,
        ask: Ask::Never,
        peppy_dirs: Some(dirs(&dir)),
    }
    .execute(&ctx())
    .expect("login");

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

    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        no_configure: true,
        ask: Ask::Never,
        peppy_dirs: Some(dirs(&dir)),
    }
    .execute(&ctx())
    .expect("login");

    let mode = std::fs::metadata(creds_path(&dir))
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "credentials must be owner-only");
}

// ─── context ─────────────────────────────────────────────────────────────

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

/// Writes a context selected against `server` by `subject`.
fn write_context(
    dir: &tempfile::TempDir,
    server: &MockServer,
    subject: &str,
    workspace: (&str, &str),
    project: (&str, &str),
) -> PlatformContext {
    let context = PlatformContext {
        version: CONTEXT_VERSION,
        api_origin: auth::profile::normalize_api_origin(&server.base_url()).unwrap(),
        subject: subject.into(),
        workspace: Named {
            id: workspace.0.into(),
            name: workspace.1.into(),
        },
        project: Named {
            id: project.0.into(),
            name: project.1.into(),
        },
        selected_at: 1_700_000_000,
    };
    platform_context::save(&dirs(dir), &context).expect("write context");
    context
}

fn stored_context(dir: &tempfile::TempDir) -> Option<PlatformContext> {
    platform_context::load(&dirs(dir)).expect("the context file parses")
}

fn answers(text: &str) -> Ask {
    Ask::Reader(Box::new(std::io::Cursor::new(text.as_bytes().to_vec())))
}

fn login_with(
    server: &MockServer,
    dir: &tempfile::TempDir,
    no_configure: bool,
    ask: Ask,
) -> peppy::error::Result<()> {
    LoginCommand {
        api_url: Some(server.base_url()),
        no_browser: true,
        no_configure,
        ask,
        peppy_dirs: Some(dirs(dir)),
    }
    .execute(&ctx())
}

#[test]
fn login_selects_the_only_workspace_and_project_with_no_question() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_workspaces_and_projects(&server);
    let dir = tempfile::tempdir().expect("temp dir");

    login_with(&server, &dir, false, Ask::Never).expect("login");

    let context = stored_context(&dir).expect("a context was selected");
    assert_eq!(context.workspace.id, WORKSPACE);
    assert_eq!(context.workspace.name, "Alice's workspace");
    assert_eq!(
        context.project.id, PROJECT,
        "the archived project does not count"
    );
    assert_eq!(context.subject, "user-123");
    assert_eq!(
        context.api_origin,
        auth::profile::normalize_api_origin(&server.base_url()).unwrap()
    );
}

#[test]
fn login_asks_when_there_is_more_than_one_choice() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_two_workspaces(&server);
    let dir = tempfile::tempdir().expect("temp dir");

    // Workspace 1 of 2, then project 2 of 2.
    login_with(&server, &dir, false, answers("1\n2\n")).expect("login");

    let context = stored_context(&dir).expect("a context was selected");
    assert_eq!(context.workspace.id, WORKSPACE);
    assert_eq!(context.project.id, FIELD);
    assert_eq!(context.project.name, "Field");
}

/// A workspace with one project asks one question only.
#[test]
fn login_does_not_ask_for_the_only_project_of_the_selected_workspace() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_two_workspaces(&server);
    let dir = tempfile::tempdir().expect("temp dir");

    login_with(&server, &dir, false, answers("2\n")).expect("login");

    let context = stored_context(&dir).expect("a context was selected");
    assert_eq!(context.workspace.id, SECOND_WORKSPACE);
    assert_eq!(context.project.id, ARM);
}

/// With nobody to ask, the sign-in is still good: the session is kept, no
/// context is written, and the command succeeds.
#[test]
fn login_with_nobody_to_ask_succeeds_and_writes_no_context() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_two_workspaces(&server);
    let dir = tempfile::tempdir().expect("temp dir");

    login_with(&server, &dir, false, Ask::Never).expect("the sign-in does not fail");

    assert_eq!(stored_context(&dir), None);
    let creds = storage::load(&creds_path(&dir)).expect("load creds");
    assert!(creds.session.is_some(), "the session is kept");
}

#[test]
fn login_keeps_the_context_of_the_same_identity() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    let workspaces = mock_two_workspaces(&server);
    let dir = tempfile::tempdir().expect("temp dir");
    let before = write_context(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        (FIELD, "Field"),
    );

    login_with(&server, &dir, false, Ask::Never).expect("login");

    assert_eq!(stored_context(&dir), Some(before));
    assert_eq!(workspaces.calls(), 0, "nothing is selected again");
}

#[test]
fn login_as_a_different_identity_replaces_the_context() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_workspaces_and_projects(&server);
    let dir = tempfile::tempdir().expect("temp dir");
    write_context(
        &dir,
        &server,
        "someone-else",
        ("ws-x", "Their workspace"),
        ("p-x", "Theirs"),
    );

    login_with(&server, &dir, false, Ask::Never).expect("login");

    let context = stored_context(&dir).expect("a context was selected");
    assert_eq!(context.subject, "user-123");
    assert_eq!(context.project.id, PROJECT);
}

/// The old context belongs to a different account, so it does not stay when
/// the selection cannot complete.
#[test]
fn login_as_a_different_identity_removes_the_context_also_with_nobody_to_ask() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    mock_two_workspaces(&server);
    let dir = tempfile::tempdir().expect("temp dir");
    write_context(
        &dir,
        &server,
        "someone-else",
        ("ws-x", "Their workspace"),
        ("p-x", "Theirs"),
    );

    login_with(&server, &dir, false, Ask::Never).expect("login");

    assert_eq!(stored_context(&dir), None);
}

#[test]
fn login_no_configure_selects_nothing() {
    let server = MockServer::start();
    mock_login_endpoints(&server, "access-token-1");
    let _me = mock_me(&server);
    let workspaces = mock_two_workspaces(&server);
    let dir = tempfile::tempdir().expect("temp dir");

    login_with(&server, &dir, true, answers("1\n1\n")).expect("login");

    assert_eq!(stored_context(&dir), None);
    assert_eq!(workspaces.calls(), 0);
}

fn configure_with(
    server: &MockServer,
    dir: &tempfile::TempDir,
    workspace: Option<&str>,
    project: Option<&str>,
    ask: Ask,
) -> peppy::error::Result<()> {
    ConfigureCommand {
        api_url: Some(server.base_url()),
        workspace: workspace.map(str::to_string),
        project: project.map(str::to_string),
        ask,
        peppy_dirs: Some(dirs(dir)),
    }
    .execute(&ctx())
}

fn context_command(
    server: &MockServer,
    dir: &tempfile::TempDir,
    action: ContextAction,
    ask: Ask,
) -> peppy::error::Result<()> {
    ContextCommand {
        action,
        api_url: Some(server.base_url()),
        ask,
        peppy_dirs: Some(dirs(dir)),
    }
    .execute(&ctx())
}

/// `configure` always runs the selection, also when a context exists.
#[test]
fn configure_replaces_the_context() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);
    write_context(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        (PROJECT, "Lab"),
    );

    configure_with(&server, &dir, None, None, answers("2\n")).expect("configure");

    let context = stored_context(&dir).expect("context");
    assert_eq!(context.workspace.id, SECOND_WORKSPACE);
    assert_eq!(context.project.id, ARM);
}

#[test]
fn configure_with_flags_asks_nothing() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);

    configure_with(
        &server,
        &dir,
        Some("Alice's workspace"),
        Some("Field"),
        Ask::Never,
    )
    .expect("configure by name");
    assert_eq!(stored_context(&dir).expect("context").project.id, FIELD);

    let err = configure_with(&server, &dir, None, None, Ask::Never)
        .expect_err("more than one workspace and nobody to ask");
    assert!(err.to_string().contains("--workspace"), "{err}");
    assert_eq!(
        stored_context(&dir).expect("context").project.id,
        FIELD,
        "a selection that fails writes nothing"
    );
}

#[test]
fn configure_needs_a_session() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().expect("temp dir");
    let err = configure_with(&server, &dir, None, None, Ask::Never).expect_err("no session");
    assert!(err.to_string().contains("peppy platform login"), "{err}");
}

/// `--project` alone looks in the workspace of the context.
#[test]
fn context_use_switches_the_project_in_the_workspace_of_the_context() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);
    write_context(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        (PROJECT, "Lab"),
    );
    write_enrollment(&dir, "peer-1", ZID);

    context_command(
        &server,
        &dir,
        ContextAction::Use {
            workspace: None,
            project: Some("Field".to_string()),
        },
        Ask::Never,
    )
    .expect("the account has two workspaces, and the context names the one to look in");

    let context = stored_context(&dir).expect("context");
    assert_eq!(context.workspace.id, WORKSPACE);
    assert_eq!(context.project.id, FIELD);
    assert_eq!(
        enrollment::load(&dirs(&dir))
            .unwrap()
            .unwrap()
            .document
            .project_id,
        PROJECT,
        "a switch does not move the machine"
    );
    assert!(
        !dirs(&dir).runtime_config_dir().exists(),
        "a switch never pokes the daemon"
    );
}

#[test]
fn context_show_list_and_clear() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);

    for json in [false, true] {
        context_command(&server, &dir, ContextAction::Show { json }, Ask::Never)
            .expect("show with no context");
    }
    write_context(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        (FIELD, "Field"),
    );
    for json in [false, true] {
        context_command(&server, &dir, ContextAction::Show { json }, Ask::Never).expect("show");
        context_command(&server, &dir, ContextAction::List { json }, Ask::Never).expect("list");
    }

    context_command(&server, &dir, ContextAction::Clear, Ask::Never).expect("clear");
    assert_eq!(stored_context(&dir), None);
    context_command(&server, &dir, ContextAction::Clear, Ask::Never).expect("clear two times");
}

/// `--workspace` alone selects the only project of that workspace, and asks
/// when the workspace has more than one. The project of the context is not a
/// default of a selection.
#[test]
fn context_use_with_a_workspace_alone_selects_its_project() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);
    write_context(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        (PROJECT, "Lab"),
    );
    let use_workspace = |workspace: &str, ask: Ask| {
        context_command(
            &server,
            &dir,
            ContextAction::Use {
                workspace: Some(workspace.to_string()),
                project: None,
            },
            ask,
        )
    };

    use_workspace("Robotics lab", Ask::Never).expect("one project, no question");
    assert_eq!(stored_context(&dir).expect("context").project.id, ARM);

    let err = use_workspace(WORKSPACE, Ask::Never).expect_err("two projects, nobody to ask");
    assert!(err.to_string().contains("--project"), "{err}");
    assert_eq!(stored_context(&dir).expect("context").project.id, ARM);

    use_workspace(WORKSPACE, answers("2\n")).expect("the person selects");
    assert_eq!(stored_context(&dir).expect("context").project.id, FIELD);
}

/// With no context, `--project` alone has no workspace to look in when the
/// account has more than one.
#[test]
fn context_use_with_a_project_alone_and_no_context_needs_the_workspace() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);

    let err = context_command(
        &server,
        &dir,
        ContextAction::Use {
            workspace: None,
            project: Some("Field".to_string()),
        },
        Ask::Never,
    )
    .expect_err("which workspace?");
    assert!(err.to_string().contains("--workspace"), "{err}");
    assert_eq!(stored_context(&dir), None);
}

/// A context of a different identity is not the context of this session.
#[test]
fn a_context_of_a_different_identity_is_not_used() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let dir = authenticated_dir(&server);
    write_context(
        &dir,
        &server,
        "someone-else",
        (WORKSPACE, "Alice's workspace"),
        (FIELD, "Field"),
    );

    let err = enroll_in(&server, &dir, false).expect_err("no context, more than one workspace");
    assert!(
        err.to_string().contains("peppy platform configure"),
        "{err}"
    );
}

#[test]
fn enroll_uses_the_context() {
    let server = MockServer::start();
    mock_two_workspaces(&server);
    let enroll = mock_enroll(&server, ZID);
    let dir = authenticated_dir(&server);
    write_context(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        (PROJECT, "Lab"),
    );

    enroll_in(&server, &dir, false).expect("the context names the project");

    assert_eq!(enroll.calls(), 1);
    assert_eq!(
        enrollment::load(&dirs(&dir))
            .unwrap()
            .unwrap()
            .document
            .project_id,
        PROJECT
    );
}

/// The machine is enrolled in one project and the context names a different
/// one: `peers` and `router` act on the project of the context.
#[test]
fn peers_and_router_use_the_context_before_the_enrollment() {
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
    write_context(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        (FIELD, "Field"),
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
fn a_refusal_on_the_project_of_the_context_names_configure() {
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
    write_context(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        ("p-gone", "Gone"),
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
        "Not Found. The context can be out of date. Run `peppy platform configure`."
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
    write_context(
        &dir,
        &server,
        "user-123",
        (WORKSPACE, "Alice's workspace"),
        (PROJECT, "Lab"),
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
        platform_context::load(&dirs(&dir)).unwrap(),
        None,
        "the context goes with the session"
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

// ─── whoami / workspaces / projects ──────────────────────────────────────

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
fn workspaces_and_projects_list_in_both_formats() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let dir = authenticated_dir(&server);

    for json in [false, true] {
        WorkspacesCommand {
            api_url: Some(server.base_url()),
            json,
            peppy_dirs: Some(dirs(&dir)),
        }
        .execute(&ctx())
        .expect("workspaces");
        ProjectsCommand {
            api_url: Some(server.base_url()),
            workspace: None,
            json,
            peppy_dirs: Some(dirs(&dir)),
        }
        .execute(&ctx())
        .expect("projects (the only workspace is picked)");
    }
    let err = ProjectsCommand {
        api_url: Some(server.base_url()),
        workspace: Some("Nope".to_string()),
        json: false,
        peppy_dirs: Some(dirs(&dir)),
    }
    .execute(&ctx())
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

    let err = WorkspacesCommand {
        api_url: Some(server.base_url()),
        json: false,
        peppy_dirs: Some(dirs(&dir)),
    }
    .execute(&ctx())
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
        name: Some("robot-7".to_string()),
        replace,
        yes: true,
        peppy_dirs: Some(dirs(dir)),
    }
    .execute(&ctx())
}

/// The whole enrollment: pick the only workspace and project, post the CSR,
/// write the bundle, poke the daemon, wait for it to come back under the new
/// identity, and verify the link with a second poke.
#[test]
fn enroll_writes_the_bundle_and_waits_for_the_daemon_to_restart() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let enroll = mock_enroll(&server, ZID);
    let dir = authenticated_dir(&server);
    write_daemon_state(&dir, "local", Some("7f3a9c1e"));
    let stub = stub_restarting_daemon(
        &dir,
        PROJECT,
        Some(ZID),
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
    let key = std::fs::read_to_string(&enrolled.peer_key).unwrap();
    assert!(
        key.starts_with("-----BEGIN PRIVATE KEY-----"),
        "a PKCS#8 key was minted"
    );
    let cert = std::fs::read_to_string(&enrolled.peer_certificate).unwrap();
    assert_eq!(
        cert, "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n",
        "the peer presents the leaf alone"
    );
    let chain = std::fs::read_to_string(dirs(&dir).peer_dir().join("chain.crt")).unwrap();
    assert!(chain.contains("issuer"), "the chain is kept apart: {chain}");
}

#[test]
fn enroll_without_a_daemon_writes_the_bundle_and_succeeds() {
    let server = MockServer::start();
    mock_workspaces_and_projects(&server);
    let _enroll = mock_enroll(&server, ZID);
    let dir = authenticated_dir(&server);

    enroll_in(&server, &dir, false).expect("no daemon: the bundle waits for the next start");

    assert!(enrollment::load(&dirs(&dir)).unwrap().is_some());
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
    let dir = authenticated_dir(&server);
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
    let dir = authenticated_dir(&server);

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
        let dir = authenticated_dir(&server);

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
    let dir = authenticated_dir(&server);

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
    let dir = authenticated_dir(&server);

    let err = enroll_in(&server, &dir, false).expect_err("a leading zero is refused");
    assert!(err.to_string().contains("router id"), "{err}");
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
    write_daemon_state(&dir, PROJECT, Some(ZID));
    let stub = stub_restarting_daemon(
        &dir,
        "local",
        Some("7f3a9c1e"),
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
                api_url: api_url.clone(),
                no_browser: true,
                no_configure: false,
            },
        ),
        (
            "logout",
            PlatformCommands::Logout {
                api_url: api_url.clone(),
            },
        ),
        (
            "whoami",
            PlatformCommands::Whoami {
                api_url: api_url.clone(),
                json: false,
            },
        ),
        (
            "workspaces",
            PlatformCommands::Workspaces {
                api_url: api_url.clone(),
                json: false,
            },
        ),
        (
            "projects",
            PlatformCommands::Projects {
                api_url: api_url.clone(),
                workspace: None,
                json: false,
            },
        ),
        (
            "enroll",
            PlatformCommands::Enroll {
                api_url: api_url.clone(),
                workspace: None,
                project: None,
                name: None,
                replace: false,
                yes: true,
            },
        ),
        (
            "unenroll",
            PlatformCommands::Unenroll {
                api_url: api_url.clone(),
                local_only: false,
                yes: true,
            },
        ),
        (
            "status",
            PlatformCommands::Status {
                api_url: api_url.clone(),
                json: false,
            },
        ),
        (
            "peers",
            PlatformCommands::Peers {
                api_url: api_url.clone(),
                workspace: None,
                project: None,
                json: false,
            },
        ),
        (
            "router restart",
            PlatformCommands::Router {
                command: RouterCommands::Restart {
                    api_url: api_url.clone(),
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
                    api_url: api_url.clone(),
                    workspace: None,
                    project: None,
                },
            },
        ),
        (
            "configure",
            PlatformCommands::Configure {
                api_url: api_url.clone(),
                workspace: None,
                project: None,
            },
        ),
        (
            "context show",
            PlatformCommands::Context {
                command: ContextCommands::Show {
                    api_url: api_url.clone(),
                    json: false,
                },
            },
        ),
        (
            "context list",
            PlatformCommands::Context {
                command: ContextCommands::List {
                    api_url: api_url.clone(),
                    json: false,
                },
            },
        ),
        (
            "context use",
            PlatformCommands::Context {
                command: ContextCommands::Use {
                    api_url,
                    workspace: None,
                    project: None,
                },
            },
        ),
        (
            "context clear",
            PlatformCommands::Context {
                command: ContextCommands::Clear,
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
    }
    assert_eq!(
        any_api.calls(),
        0,
        "the override must be refused before any command reaches the backend"
    );
}
