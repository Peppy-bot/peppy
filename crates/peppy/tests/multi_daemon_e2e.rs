#![cfg(feature = "multi_daemon_e2e")]

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
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

struct ExecOutput {
    exit_code: Option<i64>,
    /// Stdout followed by stderr, both lossily decoded.
    text: String,
}

impl ExecOutput {
    fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

fn require_success(output: ExecOutput, operation: &str) -> String {
    if !output.success() {
        panic!(
            "{operation} failed (exit code {:?}):\n{}",
            output.exit_code, output.text
        );
    }
    output.text
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
    let mut managed = ManagedZenohConfig::default();
    // The pinned router links below do not need a backend. Keep the armed
    // federation task's failed backend resolution from delaying startup.
    managed.federation.connect_timeout_secs = 1;
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

struct DaemonLaunch<'a> {
    image_name: &'a str,
    image_tag: &'a str,
    peppy_binary: &'a Path,
    apptainer_dir: &'a Path,
    newuidmap: &'a Path,
}

#[derive(Clone, Copy)]
struct ManagedRouterMount<'a> {
    zenohd_binary: &'a Path,
    config: &'a Path,
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
    async fn stack_list(&self, target: Option<&str>) -> ExecOutput {
        let mut cmd = vec![CONTAINER_PEPPY_BINARY, "stack", "list"];
        if let Some(target) = target {
            cmd.extend(["--core-node", target]);
        }
        let mut result = self
            .container
            .exec(ExecCommand::new(cmd).with_cmd_ready_condition(CmdWaitFor::exit()))
            .await
            .unwrap_or_else(|error| panic!("failed to exec stack list in {}: {error}", self.name));
        let exit_code = result
            .exit_code()
            .await
            .unwrap_or_else(|error| panic!("stack list exit code in {}: {error}", self.name));
        let stdout = result
            .stdout_to_vec()
            .await
            .unwrap_or_else(|error| panic!("stack list stdout in {}: {error}", self.name));
        let stderr = result
            .stderr_to_vec()
            .await
            .unwrap_or_else(|error| panic!("stack list stderr in {}: {error}", self.name));
        ExecOutput {
            exit_code,
            text: format!(
                "{}{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            ),
        }
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
    launch: &DaemonLaunch<'_>,
    name: &str,
    hostname: &str,
    config: &str,
    managed_router: Option<ManagedRouterMount<'_>>,
) -> Daemon {
    let mut request = GenericImage::new(launch.image_name, launch.image_tag)
        .with_container_name(name)
        .with_hostname(hostname)
        .with_host("host.docker.internal", Host::HostGateway)
        .with_mount(read_only_bind(launch.peppy_binary, CONTAINER_PEPPY_BINARY))
        .with_mount(read_only_bind(launch.apptainer_dir, "/opt/peppy-apptainer"))
        .with_mount(read_only_bind(launch.newuidmap, "/usr/local/bin/newuidmap"))
        .with_env_var(PEPPY_HOME_ENV, "/data")
        .with_env_var("PEPPY_APPTAINER_DIR", "/opt/peppy-apptainer")
        .with_env_var(PEPPY_CONFIG_ENV, config)
        .with_cmd([CONTAINER_PEPPY_BINARY, "service", "serve"]);
    if let Some(router) = managed_router {
        request = request
            .with_mount(read_only_bind(
                router.zenohd_binary,
                "/usr/local/bin/zenohd",
            ))
            .with_mount(read_only_bind(router.config, CONTAINER_ROUTER_CONFIG))
            .with_env_var("ZENOH_CONFIG", CONTAINER_ROUTER_CONFIG);
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_container_daemons_are_enumerated_and_collisions_are_refused() {
    require_docker().await;
    let (image_name, image_tag) = host_compatible_image();

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

    let peppy_binary = Path::new(env!("CARGO_BIN_EXE_peppy"));
    let apptainer_dir = containers::Apptainer::resolve_apptainer_dir()
        .expect("the test-built daemon should have a host Apptainer installation");
    let newuidmap = executable_on_path("newuidmap");
    let launch = DaemonLaunch {
        image_name: &image_name,
        image_tag: &image_tag,
        peppy_binary,
        apptainer_dir: &apptainer_dir,
        newuidmap: &newuidmap,
    };
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_millis()
    );

    let daemon_a = start_daemon(
        &launch,
        &format!("peppy-md-a-{suffix}"),
        "robo-a",
        &external_daemon_config("daemon-a", router_port),
        None,
    )
    .await;
    let daemon_b = start_daemon(
        &launch,
        &format!("peppy-md-b-{suffix}"),
        "robo-b",
        &external_daemon_config("daemon-b", router_port),
        None,
    )
    .await;

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
    );
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
        None,
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn federated_router_peer_topology_daemons_are_enumerated_and_collisions_are_refused() {
    require_docker().await;
    let (image_name, image_tag) = host_compatible_image();

    let peppy_binary = Path::new(env!("CARGO_BIN_EXE_peppy"));
    let zenohd_binary = bundled_zenohd_binary();
    let apptainer_dir = containers::Apptainer::resolve_apptainer_dir()
        .expect("the test-built daemon should have a host Apptainer installation");
    let newuidmap = executable_on_path("newuidmap");
    let launch = DaemonLaunch {
        image_name: &image_name,
        image_tag: &image_tag,
        peppy_binary,
        apptainer_dir: &apptainer_dir,
        newuidmap: &newuidmap,
    };
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_millis()
    );

    let router_pins = tempfile::tempdir().expect("create pinned router config directory");
    let router_a_pin = router_pins.path().join("router-a.json5");
    write_router_pin(&router_a_pin, Vec::new());

    let daemon_a = start_daemon(
        &launch,
        &format!("peppy-fed-a-{suffix}"),
        "robo-fed-a",
        &managed_daemon_config("daemon-a"),
        Some(ManagedRouterMount {
            zenohd_binary: &zenohd_binary,
            config: &router_a_pin,
        }),
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
        &managed_daemon_config("daemon-b"),
        Some(ManagedRouterMount {
            zenohd_binary: &zenohd_binary,
            config: &router_b_pin,
        }),
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
    );
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
        &managed_daemon_config("daemon-a"),
        Some(ManagedRouterMount {
            zenohd_binary: &zenohd_binary,
            config: &collision_pin,
        }),
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
