#![cfg(feature = "multi_daemon_e2e")]

use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use config::consts::{PEPPY_CONFIG_ENV, PEPPY_HOME_ENV};
use daemon_config::peppy_config::{
    ExternalZenohConfig, ManagedZenohConfig, PeppyConfig, ZenohConfig,
};
use pmi::{ZenohAdapter, ZenohNetProtocol, render_router_config};
use testcontainers::core::client::docker_client_instance;
use testcontainers::core::{AccessMode, CmdWaitFor, ExecCommand, Host, Mount};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

const TIMEOUT: Duration = Duration::from_secs(60);
const IMAGE_OVERRIDE_ENV: &str = "PEPPY_MULTI_DAEMON_E2E_IMAGE";
const MANAGED_ROUTER_PORT: u16 = 7447;
const FEDERATION_LISTENER_PORT: u16 = 7449;
const CONTAINER_ROUTER_CONFIG: &str = "/etc/peppy/router.json5";
const CONTAINER_PEPPY_BINARY: &str = "/usr/local/bin/peppy";

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

fn assert_federated_json_row(json: &str, endpoint: &str, core_node: &str) {
    let document: serde_json::Value = serde_json::from_str(json.trim())
        .unwrap_or_else(|error| panic!("invalid federation JSON ({error}):\n{json}"));
    let rows = document["federated_routers"]
        .as_array()
        .expect("federated_routers must be a JSON array");
    let row = rows
        .iter()
        .find(|row| row["endpoint"].as_str() == Some(endpoint))
        .unwrap_or_else(|| panic!("endpoint {endpoint} missing from federation JSON:\n{json}"));
    assert_eq!(row["core_node"].as_str(), Some(core_node), "{json}");
    assert_eq!(row["status"].as_str(), Some("federated"), "{json}");
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

fn managed_daemon_config(core_node: &str, listen_endpoint: Option<&str>) -> String {
    let mut managed = ManagedZenohConfig::default();
    // The pinned router links below do not need a backend. Keep the armed
    // federation task's failed backend resolution from delaying startup.
    managed.federation.connect_timeout_secs = 1;
    managed.federation.listen_endpoint = listen_endpoint.map(str::to_string);
    let config = PeppyConfig {
        core_node_name: Some(core_node.to_string()),
        zenoh: ZenohConfig::Managed(managed),
        ..PeppyConfig::default()
    };
    serde_json::to_string(&config).expect("full daemon config should serialize")
}

fn write_router_pin(path: &Path, connect_endpoints: Vec<String>) {
    let config = render_router_config(
        ZenohNetProtocol::Tcp,
        "0.0.0.0",
        MANAGED_ROUTER_PORT,
        true,
        connect_endpoints,
        Vec::new(),
        None,
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
    extra_hosts: &'a [(&'a str, &'a str)],
    network: Option<&'a str>,
    publish: Option<(u16, u16)>,
    federation_identity: Option<&'a Path>,
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
                !output.success() || !output.text.contains(needle),
                "{needle} unexpectedly became visible in {}:\n{}",
                self.name,
                output.text
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
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

/// User-defined Docker networks are created outside testcontainers so each
/// test can put daemons on deliberately isolated bridges. Drop removes every
/// successfully created network, including when an assertion unwinds.
struct Networks {
    docker: PathBuf,
    names: Vec<String>,
}

impl Networks {
    fn create(names: impl IntoIterator<Item = String>) -> Self {
        let mut networks = Self {
            docker: executable_on_path("docker"),
            names: Vec::new(),
        };
        for name in names {
            let output = Command::new(&networks.docker)
                .args(["network", "create", &name])
                .output()
                .unwrap_or_else(|error| panic!("creating Docker network {name}: {error}"));
            if !output.status.success() {
                panic!(
                    "creating Docker network {name} failed: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            networks.names.push(name);
        }
        networks
    }
}

impl Drop for Networks {
    fn drop(&mut self) {
        for name in self.names.iter().rev() {
            let output = Command::new(&self.docker)
                .args(["network", "rm", name])
                .output();
            match output {
                Ok(output) if output.status.success() => {}
                Ok(output) => eprintln!(
                    "removing Docker network {name} failed: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
                Err(error) => eprintln!("removing Docker network {name} failed: {error}"),
            }
        }
    }
}

fn run_peppy_on_host(peppy_home: &Path, args: &[&str]) -> ExecOutput {
    let output = Command::new(env!("CARGO_BIN_EXE_peppy"))
        .args(args)
        .env(PEPPY_HOME_ENV, peppy_home)
        .env_remove(PEPPY_CONFIG_ENV)
        .output()
        .unwrap_or_else(|error| panic!("running host peppy command {args:?}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    ExecOutput {
        exit_code: output.status.code().map(i64::from),
        text: format!("{}{}", stdout, String::from_utf8_lossy(&output.stderr)),
        stdout,
    }
}

fn free_host_port() -> u16 {
    TcpListener::bind((docker_host_gateway_ipv4(), 0))
        .expect("bind a free Docker host-gateway port")
        .local_addr()
        .expect("read the free Docker host-gateway port")
        .port()
}

/// Memoized: the gateway is constant for the host, and shelling out to
/// `docker network inspect` on every lookup is needless. A failed lookup
/// panics before anything is cached, so retries recompute.
fn docker_host_gateway_ipv4() -> Ipv4Addr {
    static GATEWAY: OnceLock<Ipv4Addr> = OnceLock::new();
    *GATEWAY.get_or_init(|| {
        let docker = executable_on_path("docker");
        let output = Command::new(docker)
            .args([
                "network",
                "inspect",
                "bridge",
                "--format",
                "{{(index .IPAM.Config 0).Gateway}}",
            ])
            .output()
            .expect("inspect Docker's default bridge gateway");
        assert!(
            output.status.success(),
            "inspecting Docker's host gateway failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("Docker's default bridge gateway must be IPv4")
    })
}

struct FederationPki {
    _root: tempfile::TempDir,
    bundle_a: PathBuf,
    bundle_b: PathBuf,
    bundle_c: PathBuf,
}

fn issue_bundle(ca_home: &Path, host: &str, out: &Path) {
    let out_arg = out.to_str().expect("federation bundle path must be UTF-8");
    require_success(
        run_peppy_on_host(
            ca_home,
            &[
                "federation",
                "ca",
                "issue",
                "--host",
                host,
                "--out",
                out_arg,
            ],
        ),
        &format!("issuing federation identity for {host}"),
    );
}

fn create_federation_pki() -> FederationPki {
    let root = tempfile::tempdir().expect("create federation PKI directory");
    let fleet_home = root.path().join("fleet-ca-home");
    std::fs::create_dir_all(&fleet_home).expect("create fleet CA home");
    require_success(
        run_peppy_on_host(&fleet_home, &["federation", "ca", "init"]),
        "initializing fleet CA",
    );
    let bundle_a = root.path().join("bundle-a");
    let bundle_b = root.path().join("bundle-b");
    issue_bundle(&fleet_home, "robo-fed-a.peppy.test", &bundle_a);
    issue_bundle(&fleet_home, "robo-fed-b.peppy.test", &bundle_b);

    let rogue_home = root.path().join("rogue-ca-home");
    std::fs::create_dir_all(&rogue_home).expect("create rogue CA home");
    require_success(
        run_peppy_on_host(&rogue_home, &["federation", "ca", "init"]),
        "initializing rogue CA",
    );
    let bundle_c = root.path().join("bundle-c");
    issue_bundle(&rogue_home, "robo-fed-c.peppy.test", &bundle_c);

    FederationPki {
        _root: root,
        bundle_a,
        bundle_b,
        bundle_c,
    }
}

async fn start_daemon(
    launch: &DaemonLaunch,
    name: &str,
    hostname: &str,
    config: &str,
    options: DaemonOptions<'_>,
) -> Daemon {
    let mut request = GenericImage::new(launch.image_name.as_str(), launch.image_tag.as_str())
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
    for &(hostname, address) in options.extra_hosts {
        let host = if address == "host-gateway" {
            Host::Addr(IpAddr::V4(docker_host_gateway_ipv4()))
        } else {
            Host::Addr(address.parse().unwrap_or_else(|error| {
                panic!("invalid extra-host address {address} for {hostname}: {error}")
            }))
        };
        request = request.with_host(hostname, host);
    }
    if let Some(network) = options.network {
        request = request.with_network(network);
    }
    if let Some((host_port, container_port)) = options.publish {
        request = request
            .with_mapped_port(host_port, container_port.into())
            .with_host_config_modifier(|host_config| {
                if let Some(port_bindings) = host_config.port_bindings.as_mut() {
                    for bindings in port_bindings.values_mut().flatten() {
                        for binding in bindings {
                            binding.host_ip = Some(docker_host_gateway_ipv4().to_string());
                        }
                    }
                }
            });
    }
    if let Some(identity) = options.federation_identity {
        request = request.with_mount(read_only_bind(identity, "/data/conf/federation"));
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

/// External mode is the shared-router architecture: both container daemons dial
/// one operator-run host router, and peppy owns none of its router lifecycle.
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

/// The multi-machine architecture: every "machine" (container) runs a genuinely
/// managed router and keeps the default peer topology. Operator-pinned
/// `ZENOH_CONFIG` files link those routers directly: B dials A, while the
/// collision daemon dials both A and B so its name claim does not depend on
/// multi-hop relay.
///
/// The federation task still boots armed, but this test has no backend and its
/// one-second resolution attempt fails before the daemon proceeds standalone.
/// The static cross-machine links belong to zenohd itself, not that task, and a
/// pinned config survives peppy's router watchdog restarts unchanged.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn federated_router_peer_topology_daemons_are_enumerated_and_collisions_are_refused() {
    let (launch, suffix) = DaemonLaunch::detect().await;
    let zenohd_binary = bundled_zenohd_binary();

    let router_pins = tempfile::tempdir().expect("create pinned router config directory");
    let router_a_pin = router_pins.path().join("router-a.json5");
    write_router_pin(&router_a_pin, Vec::new());

    let daemon_a = start_daemon(
        &launch,
        &format!("peppy-fed-a-{suffix}"),
        "robo-fed-a",
        &managed_daemon_config("daemon-a", None),
        DaemonOptions {
            managed_router: Some(ManagedRouterMount {
                zenohd_binary: &zenohd_binary,
                config: Some(&router_a_pin),
            }),
            ..DaemonOptions::default()
        },
    )
    .await;
    let daemon_a_ip = daemon_a.bridge_ip().await;

    let router_b_pin = router_pins.path().join("router-b.json5");
    write_router_pin(
        &router_b_pin,
        vec![format!("tcp/{daemon_a_ip}:{MANAGED_ROUTER_PORT}")],
    );
    let daemon_b = start_daemon(
        &launch,
        &format!("peppy-fed-b-{suffix}"),
        "robo-fed-b",
        &managed_daemon_config("daemon-b", None),
        DaemonOptions {
            managed_router: Some(ManagedRouterMount {
                zenohd_binary: &zenohd_binary,
                config: Some(&router_b_pin),
            }),
            ..DaemonOptions::default()
        },
    )
    .await;
    let daemon_b_ip = daemon_b.bridge_ip().await;

    // Cross-visibility must hold from BOTH sides: a resolves b through
    // router A ← router B, and b resolves a through the same link dialed the
    // other way.
    let from_a = daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-fed-a)")
                && text.contains("Core node: daemon-b (host: robo-fed-b)")
        })
        .await;
    assert!(
        from_a.find("Core node: daemon-a").expect("local section")
            < from_a.find("Core node: daemon-b").expect("remote section"),
        "local section must be first:\n{from_a}"
    );
    daemon_b
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-fed-a)")
                && text.contains("Core node: daemon-b (host: robo-fed-b)")
        })
        .await;

    // The stack_list service itself must answer across the federation link,
    // not just the presence enumeration.
    let targeted = require_success(
        daemon_a.stack_list(Some("daemon-b")).await,
        "targeting daemon-b across the router federation",
    )
    .text;
    assert!(targeted.contains("Core node: daemon-b (host: robo-fed-b)"));
    assert!(
        !targeted.contains("Core node: daemon-a"),
        "explicit targeting must render one section:\n{targeted}"
    );

    // Name claims must be enforced across the federation. C dials both live
    // routers explicitly, avoiding any dependence on multi-hop router relay.
    let collision_pin = router_pins.path().join("router-c.json5");
    write_router_pin(
        &collision_pin,
        vec![
            format!("tcp/{daemon_a_ip}:{MANAGED_ROUTER_PORT}"),
            format!("tcp/{daemon_b_ip}:{MANAGED_ROUTER_PORT}"),
        ],
    );
    let collision = start_daemon(
        &launch,
        &format!("peppy-fed-c-{suffix}"),
        "robo-fed-c",
        &managed_daemon_config("daemon-a", None),
        DaemonOptions {
            managed_router: Some(ManagedRouterMount {
                zenohd_binary: &zenohd_binary,
                config: Some(&collision_pin),
            }),
            ..DaemonOptions::default()
        },
    )
    .await;
    let collision_status = collision.wait_for_exit().await;
    assert_ne!(
        collision_status, 0,
        "colliding daemon must fail startup across the federation"
    );
    let collision_logs = collision.logs().await;
    assert!(
        collision_logs.contains("core node name 'daemon-a' is already in use"),
        "collision error missing:\n{collision_logs}"
    );
}

/// User-managed federation is always mTLS. This covers the complete CLI flow,
/// including PKI generation, automatic peer naming, strict peer verification,
/// a rogue fleet CA, and name-based removal.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn federation_federate_establishes_mtls_and_remove_tears_it_down() {
    let (launch, suffix) = DaemonLaunch::detect().await;
    let zenohd_binary = bundled_zenohd_binary();
    let pki = create_federation_pki();
    let listener = format!("tls/0.0.0.0:{FEDERATION_LISTENER_PORT}");

    let daemon_b = start_daemon(
        &launch,
        &format!("peppy-mtls-b-{suffix}"),
        "robo-fed-b",
        &managed_daemon_config("daemon-b", Some(&listener)),
        DaemonOptions {
            managed_router: Some(ManagedRouterMount {
                zenohd_binary: &zenohd_binary,
                config: None,
            }),
            federation_identity: Some(&pki.bundle_b),
            ..DaemonOptions::default()
        },
    )
    .await;
    // A only needs B's address, available right after start; both daemons can
    // then finish booting side by side.
    let daemon_b_address = daemon_b.bridge_ip().await.to_string();

    let daemon_a = start_daemon(
        &launch,
        &format!("peppy-mtls-a-{suffix}"),
        "robo-fed-a",
        &managed_daemon_config("daemon-a", None),
        DaemonOptions {
            managed_router: Some(ManagedRouterMount {
                zenohd_binary: &zenohd_binary,
                config: None,
            }),
            extra_hosts: &[("robo-fed-b.peppy.test", daemon_b_address.as_str())],
            federation_identity: Some(&pki.bundle_a),
            ..DaemonOptions::default()
        },
    )
    .await;
    tokio::join!(
        daemon_a.wait_for_stack(|text| text.contains("Core node: daemon-a (host: robo-fed-a)")),
        daemon_b.wait_for_stack(|text| text.contains("Core node: daemon-b (host: robo-fed-b)")),
    );

    let plain_endpoint = format!("tcp/robo-fed-b.peppy.test:{FEDERATION_LISTENER_PORT}");
    let plain = daemon_a
        .exec_peppy(&["federation", "federate", &plain_endpoint])
        .await;
    assert!(
        !plain.success() && plain.text.to_ascii_lowercase().contains("tls"),
        "plain TCP federation must be rejected with a TLS error: {plain:?}"
    );

    let endpoint = format!("tls/robo-fed-b.peppy.test:{FEDERATION_LISTENER_PORT}");
    let federated = require_success(
        daemon_a
            .exec_peppy(&["federation", "federate", &endpoint])
            .await,
        "federating daemon A with daemon B",
    )
    .text;
    assert!(
        federated.contains("daemon-b"),
        "federate must report the discovered core-node name:\n{federated}"
    );

    let list = require_success(
        daemon_a.exec_peppy(&["federation", "list"]).await,
        "listing daemon A federation",
    )
    .text;
    assert!(
        list.contains("Federated routers"),
        "router section missing:\n{list}"
    );
    assert!(
        list.contains("Visible core nodes"),
        "core-node section missing:\n{list}"
    );
    assert!(
        list.contains("daemon-b") && list.contains(&endpoint),
        "discovered peer row missing:\n{list}"
    );
    assert!(
        list.to_ascii_lowercase().contains("platform-backend")
            && list.to_ascii_lowercase().contains("logged out"),
        "logged-out platform backend row missing:\n{list}"
    );
    assert!(
        list.contains("daemon-a") && list.contains("daemon-b"),
        "both core nodes must be visible:\n{list}"
    );
    let list_json = require_success(
        daemon_a.exec_peppy(&["federation", "list", "--json"]).await,
        "listing daemon A federation as JSON",
    )
    .stdout;
    assert_federated_json_row(&list_json, &endpoint, "daemon-b");

    daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-fed-a)")
                && text.contains("Core node: daemon-b (host: robo-fed-b)")
        })
        .await;
    daemon_b
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-fed-a)")
                && text.contains("Core node: daemon-b (host: robo-fed-b)")
        })
        .await;
    let targeted = require_success(
        daemon_a.stack_list(Some("daemon-b")).await,
        "targeting daemon B across the mTLS federation",
    )
    .text;
    assert!(targeted.contains("Core node: daemon-b (host: robo-fed-b)"));

    let daemon_c = start_daemon(
        &launch,
        &format!("peppy-mtls-c-{suffix}"),
        "robo-fed-c",
        &managed_daemon_config("daemon-c", None),
        DaemonOptions {
            managed_router: Some(ManagedRouterMount {
                zenohd_binary: &zenohd_binary,
                config: None,
            }),
            extra_hosts: &[("robo-fed-b.peppy.test", daemon_b_address.as_str())],
            federation_identity: Some(&pki.bundle_c),
            ..DaemonOptions::default()
        },
    )
    .await;
    daemon_c
        .wait_for_stack(|text| text.contains("Core node: daemon-c (host: robo-fed-c)"))
        .await;
    let rogue = daemon_c
        .exec_peppy(&["federation", "federate", &endpoint])
        .await;
    let rogue_lower = rogue.text.to_ascii_lowercase();
    assert!(
        !rogue.success()
            && (rogue_lower.contains("unknownissuer")
                || rogue_lower.contains("unknown issuer")
                || rogue_lower.contains("unverifiable")),
        "a peer signed by a rogue CA must fail verification: {rogue:?}"
    );
    daemon_c
        .assert_stack_absent_for("Core node: daemon-b", Duration::from_secs(5))
        .await;

    require_success(
        daemon_a
            .exec_peppy(&["federation", "remove", "daemon-b", "--yes"])
            .await,
        "removing daemon B by its discovered name",
    );
    let after_remove = require_success(
        daemon_a.exec_peppy(&["federation", "list"]).await,
        "listing daemon A federation after removal",
    )
    .text;
    assert!(
        !after_remove.contains(&endpoint),
        "removed endpoint must disappear from federation list:\n{after_remove}"
    );
    daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-fed-a)")
                && !text.contains("Core node: daemon-b")
        })
        .await;
}

/// The two daemons have no shared bridge. B's published listener is the only
/// route A can use, standing in for a WAN or NAT edge.
#[cfg_attr(not(target_os = "linux"), ignore = "requires a Linux Docker host")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn federation_across_isolated_networks_federates_via_published_endpoint() {
    let (launch, suffix) = DaemonLaunch::detect().await;
    let zenohd_binary = bundled_zenohd_binary();
    let network_a = format!("peppy-fed-net-a-{suffix}");
    let network_b = format!("peppy-fed-net-b-{suffix}");
    let _networks = Networks::create([network_a.clone(), network_b.clone()]);
    let pki = create_federation_pki();
    let published_port = free_host_port();
    let listener = format!("tls/0.0.0.0:{FEDERATION_LISTENER_PORT}");

    // The two daemons live on isolated bridges and share no launch inputs, so
    // boot them together.
    let name_a = format!("peppy-wan-a-{suffix}");
    let name_b = format!("peppy-wan-b-{suffix}");
    let config_a = managed_daemon_config("daemon-a", None);
    let config_b = managed_daemon_config("daemon-b", Some(&listener));
    let (daemon_a, daemon_b) = tokio::join!(
        start_daemon(
            &launch,
            &name_a,
            "robo-fed-a",
            &config_a,
            DaemonOptions {
                managed_router: Some(ManagedRouterMount {
                    zenohd_binary: &zenohd_binary,
                    config: None,
                }),
                extra_hosts: &[("robo-fed-b.peppy.test", "host-gateway")],
                network: Some(&network_a),
                federation_identity: Some(&pki.bundle_a),
                ..DaemonOptions::default()
            },
        ),
        start_daemon(
            &launch,
            &name_b,
            "robo-fed-b",
            &config_b,
            DaemonOptions {
                managed_router: Some(ManagedRouterMount {
                    zenohd_binary: &zenohd_binary,
                    config: None,
                }),
                network: Some(&network_b),
                publish: Some((published_port, FEDERATION_LISTENER_PORT)),
                federation_identity: Some(&pki.bundle_b),
                ..DaemonOptions::default()
            },
        ),
    );
    tokio::join!(
        daemon_a.wait_for_stack(|text| text.contains("Core node: daemon-a (host: robo-fed-a)")),
        daemon_b.wait_for_stack(|text| text.contains("Core node: daemon-b (host: robo-fed-b)")),
    );
    let daemon_b_internal_ip = daemon_b.bridge_ip().await;

    let isolated_endpoint = format!("tls/{daemon_b_internal_ip}:{FEDERATION_LISTENER_PORT}");
    let isolated_started = Instant::now();
    let isolated = daemon_a
        .exec_peppy(&["federation", "federate", &isolated_endpoint])
        .await;
    let isolated_lower = isolated.text.to_ascii_lowercase();
    assert!(
        !isolated.success()
            && isolated_lower.contains("connect to")
            && (isolated_lower.contains("timed out") || isolated_lower.contains("failed")),
        "the isolated bridge must fail at TCP reachability, not certificate naming: {isolated:?}"
    );
    assert!(
        isolated_started.elapsed() < Duration::from_secs(20),
        "the isolated reachability failure must remain bounded: {isolated:?}"
    );
    require_success(
        daemon_a
            .exec_peppy(&["federation", "remove", &isolated_endpoint])
            .await,
        "removing the failed internal-bridge endpoint",
    );

    let published_endpoint = format!("tls/robo-fed-b.peppy.test:{published_port}");
    let federated = require_success(
        daemon_a
            .exec_peppy(&["federation", "federate", &published_endpoint])
            .await,
        "federating across the published endpoint",
    )
    .text;
    assert!(
        federated.contains("daemon-b"),
        "published federation must discover daemon B:\n{federated}"
    );
    let list_json = require_success(
        daemon_a.exec_peppy(&["federation", "list", "--json"]).await,
        "listing the published federation as JSON",
    )
    .stdout;
    assert_federated_json_row(&list_json, &published_endpoint, "daemon-b");

    daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-fed-a)")
                && text.contains("Core node: daemon-b (host: robo-fed-b)")
        })
        .await;
    daemon_b
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-fed-a)")
                && text.contains("Core node: daemon-b (host: robo-fed-b)")
        })
        .await;
    let targeted = require_success(
        daemon_a.stack_list(Some("daemon-b")).await,
        "targeting daemon B over the published federation",
    )
    .text;
    assert!(targeted.contains("Core node: daemon-b (host: robo-fed-b)"));

    require_success(
        daemon_a
            .exec_peppy(&["federation", "remove", "daemon-b"])
            .await,
        "removing the published federation",
    );
    daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-fed-a)")
                && !text.contains("Core node: daemon-b")
        })
        .await;
}
