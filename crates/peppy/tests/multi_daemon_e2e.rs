#![cfg(feature = "multi_daemon_e2e")]

//! Multi-daemon docker e2e: the platform-only federation architecture.
//!
//! Topology under test: N containerized peppy daemons, ONE hub zenohd
//! container shaped exactly like platform-backend's shared router (a `tls/`
//! listener requiring a client certificate chained to the committed dev CA),
//! and a mock platform-backend HTTP server running in the test process. Each
//! daemon authenticates with a PAT (or seeded OAuth credentials), pulls
//! `{endpoint, protocol, workspace_id, reconnect_after_secs}` from the mock,
//! and federates its managed router to the hub over mTLS with the dev client
//! leaf the debug build embeds.
//!
//! The hub's server leaf is a committed fixture
//! (`tests/fixtures/platform-hub/`), minted once from platform-backend's
//! dev-pki (`PEPPY_ROUTER_SANS=hub.peppy.test PEPPY_SKIP_CLIENT_LEAF=1
//! PEPPY_ZENOH_CERTS_DIR=... cargo run -p dev-pki --bin gen_dev_certs`) so it
//! chains to the same dev CA the daemon embeds; the harness asserts that CA
//! byte-equality at startup so a rotated CA fails loudly instead of as a
//! handshake mystery.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use config::consts::{PEPPY_CONFIG_ENV, PEPPY_HOME_ENV};
use daemon_config::peppy_config::{
    ExternalZenohConfig, ManagedZenohConfig, PeppyConfig, ZenohConfig,
};
use pmi::{RouterLinks, TlsConfig, ZenohAdapter, ZenohNetProtocol, render_router_config};
use testcontainers::core::client::docker_client_instance;
use testcontainers::core::{AccessMode, CmdWaitFor, ExecCommand, Host, Mount};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

const TIMEOUT: Duration = Duration::from_secs(60);
const IMAGE_OVERRIDE_ENV: &str = "PEPPY_MULTI_DAEMON_E2E_IMAGE";
const MANAGED_ROUTER_PORT: u16 = 7447;
const HUB_PORT: u16 = 7447;
const HUB_HOSTNAME: &str = "hub.peppy.test";
/// The `*.localhost` name containers resolve to the docker host gateway; the
/// mock platform binds `0.0.0.0` on the host, and `*.localhost` is what keeps
/// plain-http backend URLs acceptable to the CLI's scheme check.
const MOCK_BACKEND_HOSTNAME: &str = "mock.backend.localhost";
const CONTAINER_ROUTER_CONFIG: &str = "/etc/peppy/router.json5";
const CONTAINER_PEPPY_BINARY: &str = "/usr/local/bin/peppy";
/// Two distinct workspaces (stable UUIDs) plus the PATs that map to them in
/// the mock platform.
const WORKSPACE_ONE: &str = "550e8400-e29b-41d4-a716-446655440000";
const WORKSPACE_TWO: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
const PAT_WORKSPACE_ONE: &str = "pat-workspace-one";
const PAT_WORKSPACE_TWO: &str = "pat-workspace-two";
/// A seeded-OAuth access token the mock accepts for workspace one.
const SESSION_TOKEN_WORKSPACE_ONE: &str = "session-token-workspace-one";

async fn require_docker() {
    let client = docker_client_instance()
        .await
        .expect("a Docker client must be constructible on the test host");
    client
        .ping()
        .await
        .expect("the Docker daemon must be reachable on the test host");
}

#[derive(Debug)]
struct ExecOutput {
    exit_code: Option<i64>,
    /// Stdout alone, used for machine-readable command output.
    stdout: String,
    /// Stdout followed by stderr, both lossily decoded.
    text: String,
}

impl ExecOutput {
    fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Panics unless the command exited 0. Returns the output so callers can pick
/// `.text` or `.stdout`.
fn require_success(output: ExecOutput, operation: &str) -> ExecOutput {
    if !output.success() {
        panic!(
            "{operation} failed (exit code {:?}):\n{}",
            output.exit_code, output.text
        );
    }
    output
}

fn external_daemon_config(core_node: &str, router_port: u16) -> String {
    let mut config = PeppyConfig {
        core_node_name: Some(core_node.to_string()),
        ..PeppyConfig::default()
    };
    config.zenoh = ZenohConfig::External(ExternalZenohConfig {
        endpoint: format!("tcp/host.docker.internal:{router_port}"),
    });
    serde_json::to_string(&config).expect("full daemon config should serialize")
}

fn managed_daemon_config(core_node: &str) -> String {
    let config = PeppyConfig {
        core_node_name: Some(core_node.to_string()),
        zenoh: ZenohConfig::Managed(ManagedZenohConfig::default()),
        ..PeppyConfig::default()
    };
    serde_json::to_string(&config).expect("full daemon config should serialize")
}

/// Writes an operator-pinned standalone zenohd config: the shape an operator
/// hands the daemon via `ZENOH_CONFIG`, which peppy must never rewrite.
fn write_standalone_router_pin(path: &Path) {
    let config = render_router_config(
        ZenohNetProtocol::Tcp,
        "0.0.0.0",
        MANAGED_ROUTER_PORT,
        true,
        RouterLinks::default(),
    );
    std::fs::write(path, config).expect("write pinned zenohd config");
}

fn executable_on_path(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} must be installed on the Docker test host"))
}

/// Locates the bundled zenohd built by pmi's `build_zenoh` feature. Cargo puts
/// it under this profile's `build/pmi-*/out` directory; a neighboring binary,
/// an explicit environment override, and PATH remain useful for packaged runs.
fn bundled_zenohd_binary() -> PathBuf {
    let current_exe = std::env::current_exe().expect("resolve current test executable");
    if let Some(directory) = current_exe.parent() {
        let candidate = directory.join("zenohd");
        if candidate.is_file() {
            return candidate;
        }
    }

    if let Some(candidate) = std::env::var_os("ZENOHD_BINARY_PATH").map(PathBuf::from)
        && candidate.is_file()
    {
        return candidate;
    }

    let mut built_candidates = current_exe
        .parent()
        .and_then(Path::parent)
        .map(|profile| profile.join("build"))
        .and_then(|build| std::fs::read_dir(build).ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("pmi-"))
        .map(|entry| entry.path().join("out/zenohd"))
        .filter(|candidate| candidate.is_file())
        .collect::<Vec<_>>();
    built_candidates.sort_by_key(|candidate| {
        candidate
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    built_candidates
        .pop()
        .unwrap_or_else(|| executable_on_path("zenohd"))
}

/// Uses the runner's Ubuntu release so a host-built binary never targets a
/// newer glibc than the container provides. Non-Ubuntu runners must select a
/// compatible image explicitly with `PEPPY_MULTI_DAEMON_E2E_IMAGE`.
fn host_compatible_image() -> (String, String) {
    if let Ok(image) = std::env::var(IMAGE_OVERRIDE_ENV)
        && !image.trim().is_empty()
    {
        return split_image_reference(image.trim());
    }

    let release = std::fs::read_to_string("/etc/os-release")
        .expect("read /etc/os-release or set PEPPY_MULTI_DAEMON_E2E_IMAGE");
    let value = |key: &str| {
        release.lines().find_map(|line| {
            let (candidate, value) = line.split_once('=')?;
            (candidate == key).then(|| value.trim_matches('"').to_string())
        })
    };
    let distro = value("ID").expect("/etc/os-release must contain ID");
    let version = value("VERSION_ID").expect("/etc/os-release must contain VERSION_ID");
    assert_eq!(
        distro, "ubuntu",
        "set {IMAGE_OVERRIDE_ENV} to a container image compatible with this {distro} runner"
    );
    (String::from("ubuntu"), version)
}

/// `GenericImage` wants the name and tag separately and joins them back with a
/// colon. Splitting at the last colon (unless it belongs to a registry port)
/// reproduces the original reference, `name@sha256` digests included.
fn split_image_reference(reference: &str) -> (String, String) {
    match reference.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name.to_string(), tag.to_string()),
        _ => (reference.to_string(), String::from("latest")),
    }
}

struct DaemonLaunch {
    image_name: String,
    image_tag: String,
    peppy_binary: PathBuf,
    apptainer_dir: PathBuf,
    newuidmap: PathBuf,
}

impl DaemonLaunch {
    /// Resolves everything a daemon container needs from the test host, plus a
    /// per-run container-name suffix. Panics before any container starts when
    /// Docker or a host prerequisite is missing.
    async fn detect() -> (Self, String) {
        require_docker().await;
        let (image_name, image_tag) = host_compatible_image();
        let launch = Self {
            image_name,
            image_tag,
            peppy_binary: PathBuf::from(env!("CARGO_BIN_EXE_peppy")),
            apptainer_dir: containers::Apptainer::resolve_apptainer_dir()
                .expect("the test-built daemon should have a host Apptainer installation"),
            newuidmap: executable_on_path("newuidmap"),
        };
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_millis()
        );
        (launch, suffix)
    }
}

#[derive(Clone, Copy)]
struct ManagedRouterMount<'a> {
    zenohd_binary: &'a Path,
    config: Option<&'a Path>,
}

#[derive(Default)]
struct DaemonOptions<'a> {
    managed_router: Option<ManagedRouterMount<'a>>,
    /// Extra `/etc/hosts` entries: `(hostname, host)`, where
    /// [`Host::HostGateway`] resolves to the docker host gateway.
    extra_hosts: Vec<(String, Host)>,
    /// Extra environment variables for the daemon process.
    env: Vec<(String, String)>,
    /// A host directory mounted read-write at `/data/conf` (the container's
    /// `conf/` dir), for seeding `credentials.json5`. Read-write because login
    /// and the daemon's own pulls rewrite the file atomically.
    conf_dir: Option<&'a Path>,
    publish: Option<u16>,
}

fn read_only_bind(host_path: &Path, container_path: &str) -> Mount {
    Mount::bind_mount(host_path.display().to_string(), container_path)
        .with_access_mode(AccessMode::ReadOnly)
}

/// A running daemon container. The guard owns cleanup: dropping it removes the
/// container (panic unwinds included), so failed assertions cannot leak
/// containers.
struct Daemon {
    name: String,
    container: ContainerAsync<GenericImage>,
}

impl Daemon {
    async fn exec_peppy(&self, args: &[&str]) -> ExecOutput {
        let cmd = std::iter::once(CONTAINER_PEPPY_BINARY)
            .chain(args.iter().copied())
            .collect::<Vec<_>>();
        let mut result = self
            .container
            .exec(ExecCommand::new(cmd).with_cmd_ready_condition(CmdWaitFor::exit()))
            .await
            .unwrap_or_else(|error| panic!("failed to exec peppy in {}: {error}", self.name));
        let exit_code = result
            .exit_code()
            .await
            .unwrap_or_else(|error| panic!("peppy exit code in {}: {error}", self.name));
        let stdout = result
            .stdout_to_vec()
            .await
            .unwrap_or_else(|error| panic!("peppy stdout in {}: {error}", self.name));
        let stderr = result
            .stderr_to_vec()
            .await
            .unwrap_or_else(|error| panic!("peppy stderr in {}: {error}", self.name));
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        ExecOutput {
            exit_code,
            text: format!("{}{}", stdout, String::from_utf8_lossy(&stderr)),
            stdout,
        }
    }

    async fn stack_list(&self, target: Option<&str>) -> ExecOutput {
        let mut args = vec!["stack", "list"];
        if let Some(target) = target {
            args.extend(["--core-node", target]);
        }
        self.exec_peppy(&args).await
    }

    async fn wait_for_stack(&self, predicate: impl Fn(&str) -> bool) -> String {
        let started = Instant::now();
        let mut last = String::new();
        while started.elapsed() < TIMEOUT {
            let output = self.stack_list(None).await;
            last = output.text;
            if output.exit_code == Some(0) && predicate(&last) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let logs = self.logs().await;
        panic!(
            "timed out waiting for stack list in {}; last output:\n{last}\ncontainer logs:\n{logs}",
            self.name
        );
    }

    async fn assert_stack_absent_for(&self, needle: &str, duration: Duration) {
        let started = Instant::now();
        while started.elapsed() < duration {
            let output = self.stack_list(None).await;
            assert!(
                output.success(),
                "stack list failed while checking that {needle} stayed absent in {}:\n{}",
                self.name,
                output.text
            );
            assert!(
                !output.text.contains(needle),
                "{needle} unexpectedly became visible in {}:\n{}",
                self.name,
                output.text
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Runs `peppy platform federations --json` and parses the document.
    async fn federations_json(&self) -> serde_json::Value {
        let output = require_success(
            self.exec_peppy(&["platform", "federations", "--json"])
                .await,
            &format!("platform federations --json in {}", self.name),
        );
        serde_json::from_str(output.stdout.trim()).unwrap_or_else(|error| {
            panic!(
                "invalid federations JSON from {} ({error}):\n{}",
                self.name, output.stdout
            )
        })
    }

    /// Polls the federations report until `predicate` holds, panicking with
    /// the last document and the container logs on timeout.
    async fn wait_for_federations(
        &self,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let started = Instant::now();
        let mut last = serde_json::Value::Null;
        while started.elapsed() < TIMEOUT {
            last = self.federations_json().await;
            if predicate(&last) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let logs = self.logs().await;
        panic!(
            "timed out waiting for the federations report in {}; last document:\n{last:#}\ncontainer logs:\n{logs}",
            self.name
        );
    }

    async fn wait_for_exit(&self) -> i64 {
        let started = Instant::now();
        while started.elapsed() < TIMEOUT {
            let exit =
                self.container.exit_code().await.unwrap_or_else(|error| {
                    panic!("inspecting exit state of {}: {error}", self.name)
                });
            if let Some(code) = exit {
                return code;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let logs = self.logs().await;
        panic!(
            "timed out waiting for {} to exit\ncontainer logs:\n{logs}",
            self.name
        );
    }

    async fn logs(&self) -> String {
        let stdout = self
            .container
            .stdout_to_vec()
            .await
            .unwrap_or_else(|error| panic!("reading stdout logs of {}: {error}", self.name));
        let stderr = self
            .container
            .stderr_to_vec()
            .await
            .unwrap_or_else(|error| panic!("reading stderr logs of {}: {error}", self.name));
        format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        )
    }

    async fn bridge_ip(&self) -> Ipv4Addr {
        match self.container.get_bridge_ip_address().await {
            Ok(IpAddr::V4(ip)) => ip,
            Ok(IpAddr::V6(ip)) => panic!(
                "container {} has IPv6 bridge address {ip}, expected IPv4",
                self.name
            ),
            Err(error) => panic!("inspecting bridge IP for {}: {error}", self.name),
        }
    }

    async fn stop(&self) {
        self.container
            .stop_with_timeout(Some(10))
            .await
            .unwrap_or_else(|error| panic!("stopping {}: {error}", self.name));
    }
}

async fn start_daemon(
    launch: &DaemonLaunch,
    name: &str,
    hostname: &str,
    config: &str,
    options: DaemonOptions<'_>,
) -> Daemon {
    let mut image = GenericImage::new(launch.image_name.as_str(), launch.image_tag.as_str());
    if let Some(container_port) = options.publish {
        image = image.with_exposed_port(container_port.into());
    }
    let mut request = image
        .with_container_name(name)
        .with_hostname(hostname)
        .with_host("host.docker.internal", Host::HostGateway)
        .with_mount(read_only_bind(&launch.peppy_binary, CONTAINER_PEPPY_BINARY))
        .with_mount(read_only_bind(
            &launch.apptainer_dir,
            "/opt/peppy-apptainer",
        ))
        .with_mount(read_only_bind(
            &launch.newuidmap,
            "/usr/local/bin/newuidmap",
        ))
        .with_env_var(PEPPY_HOME_ENV, "/data")
        .with_env_var("PEPPY_APPTAINER_DIR", "/opt/peppy-apptainer")
        .with_env_var(PEPPY_CONFIG_ENV, config)
        .with_cmd([CONTAINER_PEPPY_BINARY, "service", "serve"]);
    for (hostname, host) in &options.extra_hosts {
        request = request.with_host(hostname.clone(), host.clone());
    }
    for (key, value) in &options.env {
        request = request.with_env_var(key.clone(), value.clone());
    }
    if let Some(conf_dir) = options.conf_dir {
        request = request.with_mount(Mount::bind_mount(
            conf_dir.display().to_string(),
            "/data/conf",
        ));
    }
    if let Some(router) = options.managed_router {
        request = request.with_mount(read_only_bind(
            router.zenohd_binary,
            "/usr/local/bin/zenohd",
        ));
        if let Some(config) = router.config {
            request = request
                .with_mount(read_only_bind(config, CONTAINER_ROUTER_CONFIG))
                .with_env_var("ZENOH_CONFIG", CONTAINER_ROUTER_CONFIG);
        }
    }
    let container = request
        .start()
        .await
        .unwrap_or_else(|error| panic!("starting container {name} failed: {error}"));
    Daemon {
        name: name.to_string(),
        container,
    }
}

/// The committed hub TLS fixtures: a server leaf for SAN `hub.peppy.test`
/// chained to the same dev CA the debug daemon embeds. The byte-equality check
/// turns a rotated CA into a loud fixture error instead of a handshake mystery
/// ("re-mint with gen_dev_certs" is the fix either way).
fn hub_fixture_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/platform-hub");
    let fixture_ca = std::fs::read(dir.join("peppy-dev-ca.pem")).expect("read hub fixture CA");
    let embedded_ca = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../auth-internal/dev-ca/peppy-dev-ca.pem"),
    )
    .expect("read the daemon-embedded dev CA");
    assert_eq!(
        fixture_ca, embedded_ca,
        "the hub fixtures no longer chain to the daemon-embedded dev CA; re-mint them with \
         platform-backend's gen_dev_certs (PEPPY_ROUTER_SANS=hub.peppy.test \
         PEPPY_SKIP_CLIENT_LEAF=1 PEPPY_ZENOH_CERTS_DIR=crates/peppy/tests/fixtures/platform-hub)"
    );
    dir
}

/// Starts the hub: one zenohd container shaped like platform-backend's shared
/// router (mTLS `tls/` listener, client certs required, gossip off). Returns
/// the container guard; `config_dir` owns the rendered config's lifetime.
async fn start_hub(launch: &DaemonLaunch, suffix: &str, config_dir: &Path) -> Daemon {
    let zenohd_binary = bundled_zenohd_binary();
    let fixtures = hub_fixture_dir();

    // Rendered on the host with CONTAINER paths (the mounts below place the
    // material there), mirroring platform-backend's zenoh/router.json5.
    let config = render_router_config(
        ZenohNetProtocol::Tls,
        "0.0.0.0",
        HUB_PORT,
        false,
        RouterLinks {
            upstream: None,
            tls: Some(TlsConfig {
                root_ca_certificate: Some("/etc/peppy/certs/peppy-dev-ca.pem".into()),
                enable_mtls: true,
                ..TlsConfig::server(
                    "/etc/peppy/certs/zenoh-router.pem".into(),
                    "/etc/peppy/certs/zenoh-router-key.pem".into(),
                )
            }),
        },
    );
    let config_path = config_dir.join("hub-router.json5");
    std::fs::write(&config_path, config).expect("write hub router config");

    let name = format!("peppy-hub-{suffix}");
    let container = GenericImage::new(launch.image_name.as_str(), launch.image_tag.as_str())
        .with_container_name(&name)
        .with_hostname("platform-hub")
        .with_mount(read_only_bind(&zenohd_binary, "/usr/local/bin/zenohd"))
        .with_mount(read_only_bind(&fixtures, "/etc/peppy/certs"))
        .with_mount(read_only_bind(&config_path, CONTAINER_ROUTER_CONFIG))
        .with_cmd(["/usr/local/bin/zenohd", "-c", CONTAINER_ROUTER_CONFIG])
        .start()
        .await
        .unwrap_or_else(|error| panic!("starting hub container {name} failed: {error}"));
    Daemon { name, container }
}

/// A request the mock platform observed: `(method, path, bearer)`.
type RecordedRequest = (String, String, Option<String>);

/// A minimal platform-backend stand-in running in the test process: serves
/// `GET /me`, `POST /me/cli/federation`, and `POST /logout` over HTTP/1.1
/// (`Connection: close`) on `0.0.0.0`, so containers reach it through the
/// docker gateway. httpmock binds loopback only, which containers cannot
/// reach, hence this hand-rolled responder. Bearer tokens map to workspace
/// ids; unknown bearers get 401.
struct MockPlatform {
    port: u16,
    requests: Arc<StdMutex<Vec<RecordedRequest>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for MockPlatform {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl MockPlatform {
    async fn start(hub_endpoint: &str, tokens: &[(&str, &str)]) -> Self {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", 0))
            .await
            .expect("bind the mock platform listener");
        let port = listener.local_addr().expect("mock platform address").port();
        let requests: Arc<StdMutex<Vec<RecordedRequest>>> = Arc::default();
        let recorded = Arc::clone(&requests);
        let workspaces: HashMap<String, String> = tokens
            .iter()
            .map(|(token, workspace)| (token.to_string(), workspace.to_string()))
            .collect();
        let hub_endpoint = hub_endpoint.to_string();

        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let recorded = Arc::clone(&recorded);
                let workspaces = workspaces.clone();
                let hub_endpoint = hub_endpoint.clone();
                tokio::spawn(async move {
                    let _ = serve_mock_connection(stream, recorded, workspaces, hub_endpoint).await;
                });
            }
        });
        Self {
            port,
            requests,
            server,
        }
    }

    /// The backend base URL containers use; `*.localhost` keeps the CLI's
    /// plain-http check satisfied and the extra-host entry points it at the
    /// docker gateway.
    fn api_url(&self) -> String {
        format!("http://{MOCK_BACKEND_HOSTNAME}:{}", self.port)
    }

    fn saw(&self, method: &str, path: &str) -> bool {
        self.requests
            .lock()
            .expect("mock platform request log")
            .iter()
            .any(|(m, p, _)| m == method && p == path)
    }
}

async fn serve_mock_connection(
    mut stream: tokio::net::TcpStream,
    recorded: Arc<StdMutex<Vec<RecordedRequest>>>,
    workspaces: HashMap<String, String>,
    hub_endpoint: String,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Read until the header terminator (requests here are small; 64 KiB cap).
    let mut buffer = Vec::with_capacity(1024);
    let header_end = loop {
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 64 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut bearer = None;
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.to_ascii_lowercase().as_str() {
            "authorization" => {
                bearer = value
                    .trim()
                    .strip_prefix("Bearer ")
                    .map(|token| token.to_string());
            }
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    // Drain the body so the client never sees a reset mid-write.
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    recorded.lock().expect("mock platform request log").push((
        method.clone(),
        path.clone(),
        bearer.clone(),
    ));

    let workspace = bearer.as_deref().and_then(|token| workspaces.get(token));
    let (status, payload) = match (method.as_str(), path.as_str(), workspace) {
        ("GET", "/me", Some(workspace)) => (
            "200 OK",
            serde_json::json!({
                "sub": format!("svc-{workspace}"),
                "kind": "machine",
                "username": "e2e-service",
            })
            .to_string(),
        ),
        ("POST", "/me/cli/federation", Some(workspace)) => (
            "200 OK",
            serde_json::json!({
                "endpoint": hub_endpoint,
                "protocol": "tls",
                "workspace_id": workspace,
                "reconnect_after_secs": 3000,
            })
            .to_string(),
        ),
        ("POST", "/logout", _) => ("202 Accepted", String::new()),
        ("GET", "/me", None) | ("POST", "/me/cli/federation", None) => {
            ("401 Unauthorized", String::new())
        }
        _ => ("404 Not Found", String::new()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// Everything a platform-federated daemon container needs beyond the shared
/// launch inputs.
struct PlatformDaemonSpec<'a> {
    core_node: &'a str,
    /// The hub's bridge IP for the `hub.peppy.test` extra-host entry, or
    /// `None` to map it to the container's loopback (a deterministic
    /// connection-refused hub for failure-path tests).
    hub_ip: Option<Ipv4Addr>,
    api_port: u16,
    /// `PEPPY_API_KEY` in the daemon's environment; the daemon resolves
    /// credentials from its own env, which is exactly the product behavior
    /// PAT federation requires.
    pat: Option<&'a str>,
    /// Host dir mounted read-write at `/data/conf` (seeded OAuth credentials).
    conf_dir: Option<&'a Path>,
    /// Operator-pinned `ZENOH_CONFIG` file for the managed router.
    router_pin: Option<&'a Path>,
}

async fn start_platform_daemon(
    launch: &DaemonLaunch,
    zenohd_binary: &Path,
    name: &str,
    hostname: &str,
    spec: PlatformDaemonSpec<'_>,
) -> Daemon {
    let hub_ip = spec.hub_ip.unwrap_or(Ipv4Addr::LOCALHOST);
    let mut env = vec![(
        "PEPPY_API_URL".to_string(),
        format!("http://{MOCK_BACKEND_HOSTNAME}:{}", spec.api_port),
    )];
    if let Some(pat) = spec.pat {
        env.push(("PEPPY_API_KEY".to_string(), pat.to_string()));
    }
    start_daemon(
        launch,
        name,
        hostname,
        &managed_daemon_config(spec.core_node),
        DaemonOptions {
            managed_router: Some(ManagedRouterMount {
                zenohd_binary,
                config: spec.router_pin,
            }),
            extra_hosts: vec![
                (HUB_HOSTNAME.to_string(), Host::Addr(IpAddr::V4(hub_ip))),
                (MOCK_BACKEND_HOSTNAME.to_string(), Host::HostGateway),
            ],
            env,
            conf_dir: spec.conf_dir,
            ..DaemonOptions::default()
        },
    )
    .await
}

fn hub_endpoint() -> String {
    format!("tls/{HUB_HOSTNAME}:{HUB_PORT}")
}

/// The exact `federated_core_nodes` rows the report must carry for `local`
/// seeing `remotes` (sorted) through the hub.
fn expected_rows(local: &str, remotes: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(
        remotes
            .iter()
            .map(|remote| {
                serde_json::json!({
                    "core_node": remote,
                    "via": "platform-backend",
                    "path": [local, "platform-backend", remote],
                })
            })
            .collect(),
    )
}

/// Seeds a host `conf/` dir with current OAuth credentials whose access token the
/// mock platform accepts, for the seeded-login daemon variant.
fn seed_oauth_conf_dir(dir: &Path, api_url: &str, access_token: &str) {
    let creds = auth::storage::Credentials {
        session: Some(auth::storage::ProfileCreds::with_tokens(
            api_url.to_string(),
            api_url.to_string(),
            "e2e-client".to_string(),
            "user-e2e".to_string(),
            "alice".to_string(),
            &auth::device::TokenSet {
                access_token: access_token.to_string(),
                refresh_token: "e2e-refresh".to_string(),
                expires_at: 9_999_999_999,
                token_type: "Bearer".to_string(),
                scope: "openid".to_string(),
            },
        )),
        ..Default::default()
    };
    // `storage::save` writes atomically under `<dir>/credentials.json5` when
    // handed the final path directly.
    auth::storage::save(&dir.join("credentials.json5"), &creds).expect("seed credentials");
}

/// External mode is the shared-router architecture: both container daemons dial
/// one operator-run host router, and peppy owns none of its router lifecycle.
/// This also preserves the logged-out shared-topology e2e.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_container_daemons_are_enumerated_and_collisions_are_refused() {
    let (launch, suffix) = DaemonLaunch::detect().await;

    let _router = ZenohAdapter::start_router_ephemeral_in_mode(
        "0.0.0.0",
        None,
        false,
        pmi::SubscriberBufferSizes::default(),
        None,
    )
    .await
    .expect("host zenohd should start");
    let router_port = _router.port;

    // Both daemons dial the host router independently, so boot them together.
    let name_a = format!("peppy-md-a-{suffix}");
    let name_b = format!("peppy-md-b-{suffix}");
    let config_a = external_daemon_config("daemon-a", router_port);
    let config_b = external_daemon_config("daemon-b", router_port);
    let (daemon_a, daemon_b) = tokio::join!(
        start_daemon(
            &launch,
            &name_a,
            "robo-a",
            &config_a,
            DaemonOptions::default()
        ),
        start_daemon(
            &launch,
            &name_b,
            "robo-b",
            &config_b,
            DaemonOptions::default()
        ),
    );

    let both = daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-a)")
                && text.contains("Core node: daemon-b (host: robo-b)")
        })
        .await;
    let local_position = both.find("Core node: daemon-a").expect("local section");
    let remote_position = both.find("Core node: daemon-b").expect("remote section");
    assert!(
        local_position < remote_position,
        "local section must be first:\n{both}"
    );
    let local_section = &both[local_position..remote_position];
    let remote_section = &both[remote_position..];
    assert!(
        local_section.contains("daemon-a:"),
        "daemon-a root row missing:\n{both}"
    );
    assert!(
        remote_section.contains("daemon-b:"),
        "daemon-b root row missing:\n{both}"
    );

    let targeted = require_success(
        daemon_a.stack_list(Some("daemon-b")).await,
        "targeting daemon-b from daemon-a",
    )
    .text;
    assert!(targeted.contains("Core node: daemon-b (host: robo-b)"));
    assert!(
        !targeted.contains("Core node: daemon-a"),
        "explicit targeting must render one section:\n{targeted}"
    );

    let collision = start_daemon(
        &launch,
        &format!("peppy-md-c-{suffix}"),
        "robo-c",
        &external_daemon_config("daemon-a", router_port),
        DaemonOptions::default(),
    )
    .await;
    let collision_status = collision.wait_for_exit().await;
    assert_ne!(collision_status, 0, "colliding daemon must fail startup");
    let collision_logs = collision.logs().await;
    assert!(
        collision_logs.contains("core node name 'daemon-a' is already in use"),
        "collision error missing:\n{collision_logs}"
    );

    daemon_b.stop().await;
    let only_a = daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-a)")
                && !text.contains("Core node: daemon-b")
        })
        .await;
    assert!(
        !only_a.contains("daemon-b"),
        "stopped daemon presence must disappear:\n{only_a}"
    );
}

/// The platform architecture end to end: daemons authenticate against the
/// (mock) backend, federate their managed routers to the mTLS hub, relay
/// presence and services exclusively through it, and report the spec's exact
/// A/B and A/B/C documents with logically inferred hub paths. A same-workspace
/// name collision must also be enforced across the hub.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_relays_daemons_and_reports_inferred_paths() {
    let (launch, suffix) = DaemonLaunch::detect().await;
    let zenohd_binary = bundled_zenohd_binary();
    let scratch = tempfile::tempdir().expect("create scratch dir");
    let hub = start_hub(&launch, &suffix, scratch.path()).await;
    let hub_ip = hub.bridge_ip().await;
    let mock = MockPlatform::start(&hub_endpoint(), &[(PAT_WORKSPACE_ONE, WORKSPACE_ONE)]).await;

    let daemon_a = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-hub-a-{suffix}"),
        "robo-hub-a",
        PlatformDaemonSpec {
            core_node: "daemon-a",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;
    let daemon_b = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-hub-b-{suffix}"),
        "robo-hub-b",
        PlatformDaemonSpec {
            core_node: "daemon-b",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;

    // Cross-visibility rides the hub: neither daemon knows the other's
    // address, only the hub's.
    daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a") && text.contains("Core node: daemon-b")
        })
        .await;

    // A verifying login (the PAT path) upgrades the link to Verified, which is
    // what the report requires for `federated` (endpoint presence alone must
    // never produce it).
    require_success(
        daemon_a.exec_peppy(&["platform", "login", "--yes"]).await,
        "platform login on daemon-a",
    );
    require_success(
        daemon_b.exec_peppy(&["platform", "login", "--yes"]).await,
        "platform login on daemon-b",
    );
    assert!(
        mock.saw("POST", "/me/cli/federation"),
        "the daemons pulled the federation config from the backend"
    );

    // The A/B documents, exactly as specified, from both sides.
    let report_a = daemon_a
        .wait_for_federations(|doc| {
            doc["federated_core_nodes"] == expected_rows("daemon-a", &["daemon-b"])
        })
        .await;
    assert_eq!(
        report_a["platform_federation"],
        serde_json::json!({ "endpoint": hub_endpoint(), "status": "federated" }),
        "daemon-a must report a verified hub link:\n{report_a:#}"
    );
    assert_eq!(report_a["daemon_running"], serde_json::json!(true));
    let report_b = daemon_b
        .wait_for_federations(|doc| {
            doc["federated_core_nodes"] == expected_rows("daemon-b", &["daemon-a"])
        })
        .await;
    assert_eq!(
        report_b["platform_federation"]["status"],
        serde_json::json!("federated")
    );

    // The human report labels the topology as inferred and carries the
    // CORE NODE / VIA / PATH table.
    let human = require_success(
        daemon_a.exec_peppy(&["platform", "federations"]).await,
        "human federations report on daemon-a",
    )
    .stdout;
    for needle in [
        "CORE NODE",
        "VIA",
        "PATH",
        "daemon-a -> platform-backend -> daemon-b",
        "logically inferred",
    ] {
        assert!(human.contains(needle), "missing {needle:?} in:\n{human}");
    }

    // A/B/C: a third same-workspace daemon appears as a second sorted row.
    let daemon_c = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-hub-c-{suffix}"),
        "robo-hub-c",
        PlatformDaemonSpec {
            core_node: "daemon-c",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;
    daemon_a
        .wait_for_federations(|doc| {
            doc["federated_core_nodes"] == expected_rows("daemon-a", &["daemon-b", "daemon-c"])
        })
        .await;
    drop(daemon_c);

    // Same-workspace name collisions are enforced across the hub: the
    // colliding daemon sees the incumbent only through the platform relay.
    let collision = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-hub-x-{suffix}"),
        "robo-hub-x",
        PlatformDaemonSpec {
            core_node: "daemon-a",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;
    let collision_status = collision.wait_for_exit().await;
    assert_ne!(
        collision_status, 0,
        "a same-workspace name collision must refuse boot across the hub"
    );
    let collision_logs = collision.logs().await;
    assert!(
        collision_logs.contains("core node name 'daemon-a' is already in use"),
        "collision error missing:\n{collision_logs}"
    );
}

/// Different workspaces never see each other, even through the same hub: the
/// session namespaces differ, so presence and services stay routing-isolated.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspaces_are_invisible_to_each_other() {
    let (launch, suffix) = DaemonLaunch::detect().await;
    let zenohd_binary = bundled_zenohd_binary();
    let scratch = tempfile::tempdir().expect("create scratch dir");
    let hub = start_hub(&launch, &suffix, scratch.path()).await;
    let hub_ip = hub.bridge_ip().await;
    let mock = MockPlatform::start(
        &hub_endpoint(),
        &[
            (PAT_WORKSPACE_ONE, WORKSPACE_ONE),
            (PAT_WORKSPACE_TWO, WORKSPACE_TWO),
        ],
    )
    .await;

    let daemon_a = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-ws-a-{suffix}"),
        "robo-ws-a",
        PlatformDaemonSpec {
            core_node: "daemon-a",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;
    let daemon_b = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-ws-b-{suffix}"),
        "robo-ws-b",
        PlatformDaemonSpec {
            core_node: "daemon-b",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;
    let daemon_c = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-ws-c-{suffix}"),
        "robo-ws-c",
        PlatformDaemonSpec {
            core_node: "daemon-c",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_TWO),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;

    // Workspace one sees exactly its own peers.
    daemon_a
        .wait_for_federations(|doc| {
            doc["federated_core_nodes"] == expected_rows("daemon-a", &["daemon-b"])
        })
        .await;

    // The other workspace's daemon is federated to the same hub yet sees
    // nobody: `federated` with an empty inferred list once verified.
    require_success(
        daemon_c.exec_peppy(&["platform", "login", "--yes"]).await,
        "platform login on daemon-c",
    );
    let report_c = daemon_c
        .wait_for_federations(|doc| doc["platform_federation"]["status"] == "federated")
        .await;
    assert_eq!(
        report_c["federated_core_nodes"],
        serde_json::json!([]),
        "a different workspace must infer no peers:\n{report_c:#}"
    );

    // And it stays absent from workspace one's stack for a sustained window.
    daemon_a
        .assert_stack_absent_for("daemon-c", Duration::from_secs(5))
        .await;
    daemon_b
        .assert_stack_absent_for("daemon-c", Duration::from_secs(5))
        .await;
}

/// Stopping the hub removes cross-daemon reachability: there is no direct
/// fallback link, so presence (and the inferred rows) drain and stay gone.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_the_hub_removes_cross_daemon_reachability() {
    let (launch, suffix) = DaemonLaunch::detect().await;
    let zenohd_binary = bundled_zenohd_binary();
    let scratch = tempfile::tempdir().expect("create scratch dir");
    let hub = start_hub(&launch, &suffix, scratch.path()).await;
    let hub_ip = hub.bridge_ip().await;
    let mock = MockPlatform::start(&hub_endpoint(), &[(PAT_WORKSPACE_ONE, WORKSPACE_ONE)]).await;

    let daemon_a = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-hubstop-a-{suffix}"),
        "robo-hubstop-a",
        PlatformDaemonSpec {
            core_node: "daemon-a",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;
    let _daemon_b = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-hubstop-b-{suffix}"),
        "robo-hubstop-b",
        PlatformDaemonSpec {
            core_node: "daemon-b",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;

    daemon_a
        .wait_for_federations(|doc| {
            doc["federated_core_nodes"] == expected_rows("daemon-a", &["daemon-b"])
        })
        .await;

    hub.stop().await;

    // Reachability drains: the inferred rows empty and daemon-b leaves the
    // stack, with no direct-link fallback bringing it back. The post-stop
    // platform status is deliberately only asserted as not-federated-verified
    // here; the link-state degradation semantics live in the daemon's unit
    // tests.
    daemon_a
        .wait_for_federations(|doc| doc["federated_core_nodes"] == serde_json::json!([]))
        .await;
    daemon_a
        .wait_for_stack(|text| !text.contains("Core node: daemon-b"))
        .await;
    daemon_a
        .assert_stack_absent_for("daemon-b", Duration::from_secs(5))
        .await;
}

/// A managed daemon with no credentials stays standalone: the exact
/// logged-out report, a working local stack, and nothing ever dialed.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_logged_out_daemon_stays_standalone() {
    let (launch, suffix) = DaemonLaunch::detect().await;
    let zenohd_binary = bundled_zenohd_binary();
    let scratch = tempfile::tempdir().expect("create scratch dir");
    let hub = start_hub(&launch, &suffix, scratch.path()).await;
    let hub_ip = hub.bridge_ip().await;
    let mock = MockPlatform::start(&hub_endpoint(), &[(PAT_WORKSPACE_ONE, WORKSPACE_ONE)]).await;

    let daemon = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-loggedout-{suffix}"),
        "robo-loggedout",
        PlatformDaemonSpec {
            core_node: "daemon-solo",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: None,
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;

    // The local stack works standalone (its own core node enumerates).
    daemon
        .wait_for_stack(|text| text.contains("Core node: daemon-solo"))
        .await;

    let report = daemon.federations_json().await;
    assert_eq!(
        report,
        serde_json::json!({
            "platform_federation": { "endpoint": null, "status": "logged_out" },
            "daemon_running": true,
            "federated_core_nodes": [],
        }),
        "the logged-out report must be exact:\n{report:#}"
    );
}

/// Logout restores standalone operation: the seeded-OAuth daemon revokes its
/// session, restarts under `local`, reports `logged_out`, and drops out of its
/// former workspace's inferred rows on the other side.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_restores_standalone() {
    let (launch, suffix) = DaemonLaunch::detect().await;
    let zenohd_binary = bundled_zenohd_binary();
    let scratch = tempfile::tempdir().expect("create scratch dir");
    let hub = start_hub(&launch, &suffix, scratch.path()).await;
    let hub_ip = hub.bridge_ip().await;
    let mock = MockPlatform::start(
        &hub_endpoint(),
        &[
            (PAT_WORKSPACE_ONE, WORKSPACE_ONE),
            (SESSION_TOKEN_WORKSPACE_ONE, WORKSPACE_ONE),
        ],
    )
    .await;

    // Daemon A authenticates via seeded OAuth credentials on disk (no PAT in
    // its environment), so `platform logout` can actually end its authentication.
    let conf_dir = tempfile::tempdir().expect("create conf dir");
    seed_oauth_conf_dir(
        conf_dir.path(),
        &mock.api_url(),
        SESSION_TOKEN_WORKSPACE_ONE,
    );
    let daemon_a = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-logout-a-{suffix}"),
        "robo-logout-a",
        PlatformDaemonSpec {
            core_node: "daemon-a",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: None,
            conf_dir: Some(conf_dir.path()),
            router_pin: None,
        },
    )
    .await;
    let daemon_b = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-logout-b-{suffix}"),
        "robo-logout-b",
        PlatformDaemonSpec {
            core_node: "daemon-b",
            hub_ip: Some(hub_ip),
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;

    // Mutually visible through the hub first.
    daemon_b
        .wait_for_federations(|doc| {
            doc["federated_core_nodes"] == expected_rows("daemon-b", &["daemon-a"])
        })
        .await;

    // Log daemon A out: revokes the session at the backend, de-federates, and
    // restarts the generation under `local`.
    require_success(
        daemon_a.exec_peppy(&["platform", "logout", "--yes"]).await,
        "platform logout on daemon-a",
    );
    assert!(
        mock.saw("POST", "/logout"),
        "logout must revoke the session at the backend"
    );

    let report_a = daemon_a
        .wait_for_federations(|doc| doc["platform_federation"]["status"] == "logged_out")
        .await;
    assert_eq!(
        report_a["platform_federation"]["endpoint"],
        serde_json::Value::Null
    );
    assert_eq!(report_a["federated_core_nodes"], serde_json::json!([]));

    // And the workspace no longer sees it.
    daemon_b
        .wait_for_federations(|doc| doc["federated_core_nodes"] == serde_json::json!([]))
        .await;
    daemon_b
        .assert_stack_absent_for("daemon-a", Duration::from_secs(5))
        .await;
}

/// Login failure modes surface as actionable errors and honest report states:
/// a rejected PAT fails authentication outright, and an unreachable hub leaves
/// the link in `error`, never a false `federated`.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_failures_surface_error_states() {
    let (launch, suffix) = DaemonLaunch::detect().await;
    let zenohd_binary = bundled_zenohd_binary();
    let mock = MockPlatform::start(&hub_endpoint(), &[(PAT_WORKSPACE_ONE, WORKSPACE_ONE)]).await;

    // (a) A PAT the backend rejects: login fails before any federation poke.
    let rejected = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-badpat-{suffix}"),
        "robo-badpat",
        PlatformDaemonSpec {
            core_node: "daemon-badpat",
            hub_ip: None,
            api_port: mock.port,
            pat: Some("pat-the-backend-rejects"),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;
    rejected
        .wait_for_stack(|text| text.contains("Core node: daemon-badpat"))
        .await;
    let login = rejected.exec_peppy(&["platform", "login", "--yes"]).await;
    assert_ne!(login.exit_code, Some(0), "a rejected PAT must fail login");
    assert!(
        login.text.contains("API key rejected"),
        "the error must name the rejected key:\n{}",
        login.text
    );
    let report = rejected.federations_json().await;
    assert_ne!(
        report["platform_federation"]["status"],
        serde_json::json!("federated"),
        "a rejected PAT can never read as federated:\n{report:#}"
    );

    // (b) A valid PAT but an unreachable hub (hub.peppy.test resolves to the
    // container loopback, where nothing listens): the config applies, the
    // verify probe fails, login exits non-zero, and the report says `error`.
    let unreachable = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-nohub-{suffix}"),
        "robo-nohub",
        PlatformDaemonSpec {
            core_node: "daemon-nohub",
            hub_ip: None,
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: None,
        },
    )
    .await;
    unreachable
        .wait_for_stack(|text| text.contains("Core node: daemon-nohub"))
        .await;
    let login = unreachable
        .exec_peppy(&["platform", "login", "--yes"])
        .await;
    assert_ne!(
        login.exit_code,
        Some(0),
        "an unverifiable hub link must fail login:\n{}",
        login.text
    );
    assert!(
        login.text.contains("could not be established"),
        "the error must explain the failed link:\n{}",
        login.text
    );
    let report = unreachable
        .wait_for_federations(|doc| doc["platform_federation"]["status"] == "error")
        .await;
    assert_eq!(
        report["platform_federation"]["endpoint"],
        serde_json::json!(hub_endpoint()),
        "the applied-but-unverifiable endpoint is still reported:\n{report:#}"
    );
}

/// Operator-owned routers report `operator_managed` and infer nothing, for
/// both ownership modes: `zenoh.external` and an operator-pinned
/// `ZENOH_CONFIG` on a managed router.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_and_pinned_routers_report_operator_managed() {
    let (launch, suffix) = DaemonLaunch::detect().await;
    let zenohd_binary = bundled_zenohd_binary();
    let mock = MockPlatform::start(&hub_endpoint(), &[(PAT_WORKSPACE_ONE, WORKSPACE_ONE)]).await;

    let operator_managed_report = serde_json::json!({
        "platform_federation": { "endpoint": null, "status": "operator_managed" },
        "daemon_running": true,
        "federated_core_nodes": [],
    });

    // (a) `zenoh.external`: an operator-run host router, PAT set. Federation
    // belongs to the operator; peppy infers nothing.
    let _router = ZenohAdapter::start_router_ephemeral_in_mode(
        "0.0.0.0",
        None,
        false,
        pmi::SubscriberBufferSizes::default(),
        None,
    )
    .await
    .expect("host zenohd should start");
    let external = start_daemon(
        &launch,
        &format!("peppy-ext-{suffix}"),
        "robo-ext",
        &external_daemon_config("daemon-ext", _router.port),
        DaemonOptions {
            extra_hosts: vec![(MOCK_BACKEND_HOSTNAME.to_string(), Host::HostGateway)],
            env: vec![
                (
                    "PEPPY_API_URL".to_string(),
                    format!("http://{MOCK_BACKEND_HOSTNAME}:{}", mock.port),
                ),
                ("PEPPY_API_KEY".to_string(), PAT_WORKSPACE_ONE.to_string()),
            ],
            ..DaemonOptions::default()
        },
    )
    .await;
    external
        .wait_for_stack(|text| text.contains("Core node: daemon-ext"))
        .await;
    assert_eq!(
        external.federations_json().await,
        operator_managed_report,
        "external mode must report operator_managed exactly"
    );

    // (b) An operator-pinned `ZENOH_CONFIG` on a managed router: login prints
    // the pinned note (exit 0) and the report is the same operator_managed
    // document.
    let pins = tempfile::tempdir().expect("create pinned router config directory");
    let pin = pins.path().join("router-pinned.json5");
    write_standalone_router_pin(&pin);
    let pinned = start_platform_daemon(
        &launch,
        &zenohd_binary,
        &format!("peppy-pin-{suffix}"),
        "robo-pin",
        PlatformDaemonSpec {
            core_node: "daemon-pin",
            hub_ip: None,
            api_port: mock.port,
            pat: Some(PAT_WORKSPACE_ONE),
            conf_dir: None,
            router_pin: Some(&pin),
        },
    )
    .await;
    pinned
        .wait_for_stack(|text| text.contains("Core node: daemon-pin"))
        .await;
    let login = require_success(
        pinned.exec_peppy(&["platform", "login", "--yes"]).await,
        "platform login on the pinned daemon",
    );
    assert!(
        login.text.contains("operator-pinned ZENOH_CONFIG"),
        "login must print the pinned note:\n{}",
        login.text
    );
    assert_eq!(
        pinned.federations_json().await,
        operator_managed_report,
        "a pinned managed router must report operator_managed exactly"
    );
}
