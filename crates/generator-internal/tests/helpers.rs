use config::consts::{NODE_CONFIG_FILE, NODE_CONFIG_FINGERPRINT_FILE, PEPPYGEN_OUTPUT_PATH};
use config::runtime::RuntimeConfig;
use generator::RustGenerator;
use peppylib::messaging::SHUTDOWN_SERVICE;
use peppylib::{MessengerHandle, ServiceMessenger};
use pmi::{Messenger, MessengerAdapter, MessengerBackend, ZenohAdapter, ZenohNetProtocol};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;
use std::{fs, thread, time::Duration};
use tempfile::TempDir;
use tokio::time::sleep;

pub const STUB_NODE_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: {
    name: "generated_node",
    tag: "0.1.0",
    launch_cmd: ["cargo", "run", "--release"]
  },
  logging: {
    min_level: "info",
    format: "text"
  }
}
"#;

pub fn prepare_directories(
    temp_dir: &TempDir,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let user_node = temp_dir.path().join("user_node");
    let output_dir = user_node.join(PEPPYGEN_OUTPUT_PATH);
    let peppy_node_config = user_node.join(NODE_CONFIG_FILE);
    fs::create_dir_all(&output_dir).unwrap();
    fs::create_dir_all(&user_node).unwrap();
    fs::write(&peppy_node_config, STUB_NODE_CONFIG).unwrap();
    (output_dir, user_node, peppy_node_config)
}

pub fn init_test_env(
    temp_dir: &TempDir,
) -> (
    RustGenerator,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let (output_dir, user_node, peppy_node_config_path) = prepare_directories(temp_dir);
    (
        RustGenerator::new(),
        output_dir,
        user_node,
        peppy_node_config_path,
    )
}

pub fn copy_config_to_output(user_node: &Path, output_dir: &Path) -> std::path::PathBuf {
    let source = user_node.join(NODE_CONFIG_FILE);
    let destination = output_dir.join(NODE_CONFIG_FILE);
    fs::copy(&source, &destination).unwrap();
    destination
}

pub fn init_cargo_user_node(to_dir: impl AsRef<Path>) {
    let crate_dir = to_dir.as_ref();
    fs::create_dir_all(crate_dir).expect("failed to create user node directory");
    let cargo_toml_path = crate_dir.join("Cargo.toml");

    if !cargo_toml_path.exists() {
        Command::new("cargo")
            .arg("init")
            .arg("--bin")
            .arg("--vcs")
            .arg("none")
            .current_dir(crate_dir)
            .output()
            .expect("failed to invoke cargo init for user node");
    }

    let manifest_contents =
        fs::read_to_string(&cargo_toml_path).expect("failed to read user node Cargo.toml");

    let mut updated_manifest = manifest_contents.clone();

    if !updated_manifest
        .lines()
        .any(|line| line.trim_start().starts_with("tokio"))
    {
        let tokio_dependency_line = "tokio = { version = \"1.47.0\", features = [\"macros\", \"rt-multi-thread\", \"time\"] }\n";
        updated_manifest = insert_dependency_line(&updated_manifest, tokio_dependency_line);
    }

    if !updated_manifest
        .lines()
        .any(|line| line.trim_start().starts_with("peppygen"))
    {
        let dependency_line = format!("peppygen = {{ path = \"{}\" }}\n", PEPPYGEN_OUTPUT_PATH);
        updated_manifest = insert_dependency_line(&updated_manifest, &dependency_line);
    }

    if updated_manifest != manifest_contents {
        fs::write(&cargo_toml_path, updated_manifest)
            .expect("failed to write user node Cargo.toml");
    }
}

pub fn spawn_cargo_run(dir: &std::path::Path, env_vars: &[(&str, &str)]) -> std::process::Child {
    // Run the compiled binary directly to avoid cargo's global package-cache lock contention.
    // `compile_project` must be called beforehand to ensure the binary exists.
    let binary_path = dir.join("target").join("debug").join("user_node");
    let use_binary = binary_path.exists();
    let mut command = if use_binary {
        Command::new(&binary_path)
    } else {
        Command::new("cargo")
    };
    command
        .env("CARGO_NET_OFFLINE", "true")
        .args((!use_binary).then_some("run"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(dir);

    for &(key, value) in env_vars {
        command.env(key, value);
    }

    command.spawn().expect("failed to spawn cargo run")
}

pub fn wait_for_child(
    child: &mut std::process::Child,
    timeout: Option<Duration>,
    dir: &std::path::Path,
) -> std::process::Output {
    let start = Instant::now();
    loop {
        if let Some(limit) = timeout {
            if start.elapsed() > limit {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "process timed out after {:?} for project at {}",
                    limit,
                    dir.display()
                );
            }
        }

        if let Some(status) = child
            .try_wait()
            .expect("failed to poll process status for generated project")
        {
            let mut stdout = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                out.read_to_end(&mut stdout)
                    .expect("failed to capture cargo stdout");
            }
            let mut stderr = Vec::new();
            if let Some(mut err) = child.stderr.take() {
                err.read_to_end(&mut stderr)
                    .expect("failed to capture cargo stderr");
            }
            return std::process::Output {
                status,
                stdout,
                stderr,
            };
        }

        thread::sleep(Duration::from_millis(50));
    }
}

#[warn(dead_code)]
pub fn compile_project(dir: impl AsRef<Path>) {
    let cargo_output = Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(dir)
        .output()
        .expect("failed to invoke cargo build on generated crate");
    assert!(
        cargo_output.status.success(),
        "cargo build failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        cargo_output.status.code(),
        String::from_utf8_lossy(&cargo_output.stdout),
        String::from_utf8_lossy(&cargo_output.stderr)
    );
}

pub fn insert_dependency_line(contents: &str, dependency_line: &str) -> String {
    let header = "[dependencies]";
    if let Some(section_start) = contents.find(header) {
        let after_header = contents[section_start..]
            .find('\n')
            .map(|offset| section_start + offset + 1)
            .unwrap_or(contents.len());
        let insert_pos = contents[after_header..]
            .find("\n[")
            .map(|offset| after_header + offset)
            .unwrap_or(contents.len());

        let mut updated = contents.to_string();
        if insert_pos > 0 && !updated[..insert_pos].ends_with('\n') {
            updated.insert(insert_pos, '\n');
        }
        updated.insert_str(insert_pos, dependency_line);
        updated
    } else {
        let mut updated = contents.to_string();
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str("[dependencies]\n");
        updated.push_str(dependency_line);
        updated
    }
}

/// Context for waiting on service reachability in tests.
pub struct WaitContext<'a> {
    pub messenger: &'a MessengerHandle,
    pub bound_master_node: &'a str,
    pub caller_instance_id: &'a str,
    pub target_master_node: Option<&'a str>,
}

pub fn write_codegen_fingerprint(peppy_config_path: impl AsRef<Path>) {
    let peppy_config_path = peppy_config_path.as_ref();
    let fingerprint = RuntimeConfig::generate_peppy_config_fingerprint(peppy_config_path)
        .expect("failed to generate peppy.json5 fingerprint");
    let peppy_config_dir = peppy_config_path.parent().unwrap_or_else(|| Path::new("."));
    let fingerprint_path = peppy_config_dir
        .join(PEPPYGEN_OUTPUT_PATH)
        .join(NODE_CONFIG_FINGERPRINT_FILE);
    fs::write(&fingerprint_path, fingerprint).expect("failed to write codegen fingerprint");
}

pub async fn wait_for_service_reachable_or_exit(
    ctx: &WaitContext<'_>,
    target_node_name: &str,
    target_service_name: &str,
    target_instance_id: Option<&str>,
    child: &mut std::process::Child,
    dir: &std::path::Path,
    timeout: Duration,
) {
    let start = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .expect("failed to poll process status for generated project")
        {
            let output = wait_for_child(child, None, dir);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!(
                "process exited before `{}` became reachable (status: {:?}) for project at {}\nstdout:\n{}\nstderr:\n{}",
                target_service_name,
                status.code(),
                dir.display(),
                stdout,
                stderr
            );
        }

        let reachable = ServiceMessenger::is_reachable(
            ctx.messenger,
            ctx.bound_master_node,
            ctx.caller_instance_id,
            target_node_name,
            target_service_name,
            ctx.target_master_node,
            target_instance_id,
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "failed to check reachability for service `{}` (node={}, instance={:?}) for project at {}: {}",
                target_service_name,
                target_node_name,
                target_instance_id,
                dir.display(),
                err
            )
        });

        if reachable {
            return;
        }

        if start.elapsed() > timeout {
            panic!(
                "timed out after {:?} waiting for `{}` to become reachable (node={}, instance={:?}) for project at {}",
                timeout,
                target_service_name,
                target_node_name,
                target_instance_id,
                dir.display()
            );
        }

        sleep(Duration::from_millis(50)).await;
    }
}

pub async fn wait_for_shutdown_service_reachable_or_exit(
    ctx: &WaitContext<'_>,
    target_node_name: &str,
    target_instance_id: &str,
    child: &mut std::process::Child,
    dir: &std::path::Path,
    timeout: Duration,
) {
    wait_for_service_reachable_or_exit(
        ctx,
        target_node_name,
        SHUTDOWN_SERVICE,
        Some(target_instance_id),
        child,
        dir,
        timeout,
    )
    .await;
}

pub async fn wait_for_action_service_reachable_or_exit(
    ctx: &WaitContext<'_>,
    target_node_name: &str,
    target_service_name: &str,
    target_instance_id: Option<&str>,
    child: &mut std::process::Child,
    dir: &std::path::Path,
    timeout: Duration,
) {
    const INSTANCE_ID_WILDCARD: &str = "**";
    const BROADCAST_MARKER: &str = "_any_";

    let (router_host, router_port) = ctx
        .messenger
        .messaging_endpoint()
        .await
        .expect("zenoh messaging endpoint should be available for reachability checks");

    let adapter = ZenohAdapter::from_host_port(ZenohNetProtocol::Tcp, &router_host, router_port);
    let mut probe_messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
    probe_messenger
        .start_session()
        .await
        .expect("failed to start probe messenger session");

    let start = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .expect("failed to poll process status for generated project")
        {
            let output = wait_for_child(child, None, dir);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!(
                "process exited before action `{}` became reachable (status: {:?}) for project at {}\nstdout:\n{}\nstderr:\n{}",
                target_service_name,
                status.code(),
                dir.display(),
                stdout,
                stderr
            );
        }

        let caller_target_instance_segment = (ctx.caller_instance_id != INSTANCE_ID_WILDCARD)
            .then_some(ctx.caller_instance_id)
            .unwrap_or(INSTANCE_ID_WILDCARD);

        let (effective_target_master, effective_target_instance) =
            match (ctx.target_master_node, target_instance_id) {
                (Some(master), Some(instance)) => (master, instance),
                (Some(master), None) => (master, BROADCAST_MARKER),
                (None, Some(instance)) => (BROADCAST_MARKER, instance),
                (None, None) => (BROADCAST_MARKER, BROADCAST_MARKER),
            };

        let target_bound_instance_segment = (effective_target_instance != INSTANCE_ID_WILDCARD)
            .then_some(effective_target_instance);

        let target_master = target_bound_instance_segment
            .as_ref()
            .map(|_| effective_target_master)
            .unwrap_or(BROADCAST_MARKER);
        let target_instance = target_bound_instance_segment.unwrap_or(BROADCAST_MARKER);

        let service_root = format!("action/{target_node_name}/{target_service_name}");
        let request_topic = format!(
            "{}/{}/{}/{}/{}/request/reachability_probe",
            target_master,
            ctx.bound_master_node,
            target_instance,
            caller_target_instance_segment,
            service_root
        );

        let reachable = probe_messenger
            .has_matching_subscribers(&request_topic)
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "failed to check reachability for action `{}` (node={}, instance={:?}) for project at {}: {}",
                    target_service_name,
                    target_node_name,
                    target_instance_id,
                    dir.display(),
                    err
                )
            });

        if reachable {
            break;
        }

        if start.elapsed() > timeout {
            panic!(
                "timed out after {:?} waiting for action `{}` to become reachable (node={}, instance={:?}) for project at {}",
                timeout,
                target_service_name,
                target_node_name,
                target_instance_id,
                dir.display()
            );
        }

        sleep(Duration::from_millis(50)).await;
    }

    let _ = probe_messenger.stop_session().await;
}

pub async fn send_shutdown(
    messenger: &MessengerHandle,
    bound_master_node: &str,
    sender_instance_id: &str,
    target_node_name: &str,
    target_master_node: Option<&str>,
    target_instance_id: &str,
    timeout: Duration,
) {
    let payload = bytes::Bytes::from_static(b"shutdown");
    ServiceMessenger::poll(
        messenger,
        bound_master_node,
        sender_instance_id,
        target_node_name,
        SHUTDOWN_SERVICE,
        target_master_node,
        Some(target_instance_id),
        payload,
        timeout,
    )
    .await
    .unwrap_or_else(|err| {
        panic!(
            "failed to send shutdown to node={} instance={} (project master={}): {}",
            target_node_name, target_instance_id, bound_master_node, err
        )
    });
}
