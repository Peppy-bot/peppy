#![cfg(feature = "multi_daemon_e2e")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use config::consts::{PEPPY_CONFIG_ENV, PEPPY_HOME_ENV};
use daemon_config::peppy_config::{LocalNodesTopology, PeppyConfig, ZenohdConfig};
use pmi::ZenohAdapter;

const TIMEOUT: Duration = Duration::from_secs(60);
const IMAGE_OVERRIDE_ENV: &str = "PEPPY_MULTI_DAEMON_E2E_IMAGE";

struct Containers {
    names: Vec<String>,
}

impl Containers {
    fn new() -> Self {
        Self { names: Vec::new() }
    }

    fn track(&mut self, name: String) {
        self.names.push(name);
    }
}

impl Drop for Containers {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = Command::new("docker").args(["rm", "-f", name]).output();
        }
    }
}

fn run_docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to execute docker {args:?}: {error}"))
}

fn require_success(output: Output, operation: &str) -> String {
    if !output.status.success() {
        panic!(
            "{operation} failed (status {}):\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn daemon_config(core_node: &str, router_port: u16) -> String {
    let mut config = PeppyConfig {
        core_node_name: Some(core_node.to_string()),
        ..PeppyConfig::default()
    };
    config.zenoh.local_nodes_topology = LocalNodesTopology::Router;
    config.zenoh.zenohd = ZenohdConfig::External {
        endpoint: format!("tcp/host.docker.internal:{router_port}"),
    };
    serde_json::to_string(&config).expect("full daemon config should serialize")
}

fn executable_on_path(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} must be installed on the Docker test host"))
}

/// Uses the runner's Ubuntu release so a host-built binary never targets a
/// newer glibc than the container provides. Non-Ubuntu runners must select a
/// compatible image explicitly with `PEPPY_MULTI_DAEMON_E2E_IMAGE`.
fn host_compatible_image() -> String {
    if let Ok(image) = std::env::var(IMAGE_OVERRIDE_ENV)
        && !image.trim().is_empty()
    {
        return image;
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
    format!("ubuntu:{version}")
}

struct DaemonLaunch<'a> {
    image: &'a str,
    router_port: u16,
    peppy_binary: &'a Path,
    apptainer_dir: &'a Path,
    newuidmap: &'a Path,
}

fn start_daemon(
    containers: &mut Containers,
    launch: &DaemonLaunch<'_>,
    name: &str,
    hostname: &str,
    core_node: &str,
) {
    // Track before `docker run`: Docker can create the named container and
    // still return a start failure, and the no-`--rm` collision case must not
    // leak that stopped container.
    containers.track(name.to_string());
    let config = daemon_config(core_node, launch.router_port);
    let mount = format!("{}:/usr/local/bin/peppy:ro", launch.peppy_binary.display());
    let apptainer_mount = format!("{}:/opt/peppy-apptainer:ro", launch.apptainer_dir.display());
    let newuidmap_mount = format!("{}:/usr/local/bin/newuidmap:ro", launch.newuidmap.display());
    let output = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            name,
            "--hostname",
            hostname,
            "--add-host=host.docker.internal:host-gateway",
            "-v",
            &mount,
            "-v",
            &apptainer_mount,
            "-v",
            &newuidmap_mount,
            "-e",
            &format!("{PEPPY_HOME_ENV}=/data"),
            "-e",
            "PEPPY_APPTAINER_DIR=/opt/peppy-apptainer",
            "-e",
            &format!("{PEPPY_CONFIG_ENV}={config}"),
            launch.image,
            "/usr/local/bin/peppy",
            "service",
            "serve",
        ])
        .output()
        .unwrap_or_else(|error| panic!("failed to start daemon container {name}: {error}"));
    require_success(output, &format!("starting container {name}"));
}

fn stack_list(container: &str, target: Option<&str>) -> Output {
    let mut command = Command::new("docker");
    command.args(["exec", container, "/usr/local/bin/peppy", "stack", "list"]);
    if let Some(target) = target {
        command.args(["--core-node", target]);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to exec stack list in {container}: {error}"))
}

fn wait_for_stack(container: &str, predicate: impl Fn(&str) -> bool) -> String {
    let started = Instant::now();
    let mut last = String::new();
    while started.elapsed() < TIMEOUT {
        let output = stack_list(container, None);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        last = format!("{stdout}{stderr}");
        if output.status.success() && predicate(&last) {
            return last;
        }
        thread::sleep(Duration::from_millis(250));
    }
    let logs_output = run_docker(&["logs", container]);
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&logs_output.stdout),
        String::from_utf8_lossy(&logs_output.stderr)
    );
    panic!(
        "timed out waiting for stack list in {container}; last output:\n{last}\ncontainer logs:\n{logs}"
    );
}

fn wait_for_exit(container: &str) -> i32 {
    let started = Instant::now();
    let mut last = String::new();
    while started.elapsed() < TIMEOUT {
        let output = run_docker(&[
            "inspect",
            "--format",
            "{{.State.Status}} {{.State.ExitCode}}",
            container,
        ]);
        if output.status.success() {
            last = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Some(code) = last.strip_prefix("exited ") {
                return code
                    .parse()
                    .unwrap_or_else(|_| panic!("invalid container exit state: {last}"));
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {container} to exit; last state: {last}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_container_daemons_are_enumerated_and_collisions_are_refused() {
    require_success(run_docker(&["version"]), "checking Docker availability");
    let image = host_compatible_image();

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
        image: &image,
        router_port,
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
    let daemon_a = format!("peppy-md-a-{suffix}");
    let daemon_b = format!("peppy-md-b-{suffix}");
    let collision = format!("peppy-md-c-{suffix}");
    let mut containers = Containers::new();

    start_daemon(&mut containers, &launch, &daemon_a, "robo-a", "daemon-a");
    start_daemon(&mut containers, &launch, &daemon_b, "robo-b", "daemon-b");

    let both = wait_for_stack(&daemon_a, |text| {
        text.contains("Core node: daemon-a (host: robo-a)")
            && text.contains("Core node: daemon-b (host: robo-b)")
    });
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
        stack_list(&daemon_a, Some("daemon-b")),
        "targeting daemon-b from daemon-a",
    );
    assert!(targeted.contains("Core node: daemon-b (host: robo-b)"));
    assert!(
        !targeted.contains("Core node: daemon-a"),
        "explicit targeting must render one section:\n{targeted}"
    );

    start_daemon(&mut containers, &launch, &collision, "robo-c", "daemon-a");
    let collision_status = wait_for_exit(&collision);
    assert_ne!(collision_status, 0, "colliding daemon must fail startup");
    let collision_logs = require_success(
        run_docker(&["logs", &collision]),
        "reading collision container logs",
    );
    assert!(
        collision_logs.contains("core node name 'daemon-a' is already in use"),
        "collision error missing:\n{collision_logs}"
    );

    require_success(
        run_docker(&["stop", "--time", "10", &daemon_b]),
        "stopping daemon-b",
    );
    let only_a = wait_for_stack(&daemon_a, |text| {
        text.contains("Core node: daemon-a (host: robo-a)") && !text.contains("Core node: daemon-b")
    });
    assert!(
        !only_a.contains("daemon-b"),
        "stopped daemon presence must disappear:\n{only_a}"
    );
}
