#![allow(dead_code)]

use config::consts::{
    NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH, PYTHON_MAX_VERSION, PYTHON_MIN_VERSION, PeppyDirs,
};
use config::node::PeppygenLanguage;
use generator::generate_peppygen_lib;
use peppylib::messaging::Iface;
use peppylib::messaging::{ActionMessenger, NODE_HEALTH_SERVICE, SHUTDOWN_SERVICE};
use peppylib::{MessengerHandle, ServiceMessenger};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;
use std::{fs, thread, time::Duration};
use tempfile::TempDir;
use tokio::time::sleep;

/// Returns a `PeppyDirs` instance suitable for use in tests.
///
/// Uses `PeppyDirs::default()` since generator tests need access to the real
/// shared cache directories for deploying vendored crates and Python packages.
pub fn test_peppy_dirs() -> PeppyDirs {
    PeppyDirs::default()
}

pub const STUB_NODE_CONFIG: &str = r#"{
  peppy_schema: "node_v1",
  manifest: {
    name: "generated_node",
    tag: "v1"
  },
  execution: {
    language: "rust",
    run_cmd: ["./target/release/generated_node"]
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

pub fn init_test_env<G: Default>(
    temp_dir: &TempDir,
    node_config: &str,
) -> (
    G,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let (output_dir, user_node, peppy_node_config_path) = prepare_directories(temp_dir);
    fs::write(&peppy_node_config_path, node_config).unwrap();
    (G::default(), output_dir, user_node, peppy_node_config_path)
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
            .stdin(Stdio::null())
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
        let tokio_dependency_line =
            "tokio = { version = \"1\", features = [\"macros\", \"rt-multi-thread\", \"time\"] }\n";
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
        if let Some(limit) = timeout
            && start.elapsed() > limit
        {
            let _ = child.kill();
            let _ = child.wait();
            let mut stdout = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                let _ = out.read_to_end(&mut stdout);
            }
            let mut stderr = Vec::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_end(&mut stderr);
            }
            panic!(
                "process timed out after {:?} for project at {}\nstdout:\n{}\nstderr:\n{}",
                limit,
                dir.display(),
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr),
            );
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

/// Returns a stable, shared target directory for test compilations so that sccache can
/// reuse the same dir across runs
fn stable_test_target_dir() -> std::path::PathBuf {
    PeppyDirs::default().root().join("cache/rust/test-targets")
}

pub fn compile_project(dir: impl AsRef<Path>) {
    let dir = dir.as_ref();
    let target_dir = stable_test_target_dir();
    fs::create_dir_all(&target_dir).expect("failed to create stable test target directory");

    // Hold an exclusive file lock across both the cargo build and the binary copy.
    // This prevents a parallel test's build from overwriting the `user_node` binary
    // in the shared target dir between our build finishing and the copy completing.
    let lock_file = fs::File::create(target_dir.join(".compile.lock"))
        .expect("failed to create compile lock file");
    lock_file.lock().expect("failed to acquire compile lock");

    let cargo_output = Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TARGET_DIR", &target_dir)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to invoke cargo build on generated crate");
    assert!(
        cargo_output.status.success(),
        "cargo build failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        cargo_output.status.code(),
        String::from_utf8_lossy(&cargo_output.stdout),
        String::from_utf8_lossy(&cargo_output.stderr)
    );

    // Copy the binary to the project's local target dir so that spawn_cargo_run
    // can execute it directly without cargo, avoiding lock contention at runtime.
    let binary = target_dir.join("debug").join("user_node");
    if binary.exists() {
        let local_bin_dir = dir.join("target").join("debug");
        fs::create_dir_all(&local_bin_dir).expect("failed to create local target/debug dir");
        fs::copy(&binary, local_bin_dir.join("user_node"))
            .expect("failed to copy compiled binary to local target dir");
    }
    // Lock released on drop
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

pub fn run_generate_peppygen_lib_test(
    language: PeppygenLanguage,
    json_config_content: &str,
) -> (TempDir, std::path::PathBuf) {
    let temp_dir = TempDir::new().expect("failed to create temp directory");
    let node_dir = temp_dir.path();

    // Write the peppy.json5 config
    let config_path = node_dir.join(config::consts::NODE_CONFIG_FILE);
    fs::write(&config_path, json_config_content).expect("failed to write peppy.json5");

    // Generate the library
    let peppy_dirs = PeppyDirs::default();
    generate_peppygen_lib(
        language,
        node_dir,
        Vec::new(),
        "test-hash",
        &peppy_dirs,
        Default::default(),
        None,
    )
    .expect("failed to generate library");

    // Verify the generated library structure exists
    let peppygen_dir = node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        peppygen_dir.display()
    );

    // Check that the fingerprint was created
    let config_path = node_dir.join(NODE_CONFIG_FILE);
    let fingerprint =
        config::fingerprint::read_codegen_fingerprint(&config_path, PEPPYGEN_OUTPUT_PATH)
            .expect("fingerprint file should exist in peppygen directory");
    assert!(!fingerprint.is_empty(), "fingerprint should not be empty");

    // Check that the git.hash was created
    let git_hash_path = node_dir
        .join(config::consts::PEPPY_OUTPUT_DIR)
        .join("git.hash");
    let git_hash_content =
        fs::read_to_string(&git_hash_path).expect("git.hash file should exist in .peppy directory");
    assert_eq!(
        git_hash_content, "test-hash",
        "git.hash should contain the expected hash value"
    );

    (temp_dir, peppygen_dir)
}

/// Context for waiting on service reachability in tests.
pub struct WaitContext<'a> {
    pub messenger: &'a MessengerHandle,
    pub bound_core_node: &'a str,
    pub caller_instance_id: &'a str,
    pub to_core_node: Option<&'a str>,
}

pub async fn wait_for_service_reachable_or_exit(
    ctx: &WaitContext<'_>,
    to_node_name: &str,
    to_service_name: &str,
    to_instance_id: Option<&str>,
    child: &mut std::process::Child,
    dir: &std::path::Path,
) {
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
                to_service_name,
                status.code(),
                dir.display(),
                stdout,
                stderr
            );
        }

        let reachable = ServiceMessenger::is_reachable(
            ctx.messenger,
            ctx.bound_core_node,
            ctx.caller_instance_id,
            to_node_name,
            Iface::native(),
            to_service_name,
            ctx.to_core_node,
            to_instance_id,
        )

        .await
        .unwrap_or_else(|err| {
            panic!(
                "failed to check reachability for service `{}` (node={}, instance={:?}) for project at {}: {}",
                to_service_name,
                to_node_name,
                to_instance_id,
                dir.display(),
                err
            )
        });

        if reachable {
            return;
        }

        sleep(Duration::from_millis(50)).await;
    }
}

pub async fn wait_for_action_service_reachable_or_exit(
    ctx: &WaitContext<'_>,
    to_node_name: &str,
    to_action_name: &str,
    to_instance_id: Option<&str>,
    child: &mut std::process::Child,
    dir: &std::path::Path,
) {
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
                to_action_name,
                status.code(),
                dir.display(),
                stdout,
                stderr
            );
        }

        let reachable = ActionMessenger::is_reachable(
            ctx.messenger,
            ctx.bound_core_node,
            ctx.caller_instance_id,
            to_node_name,
            Iface::native(),
            to_action_name,
            ctx.to_core_node,
            to_instance_id,
        )

        .await
        .unwrap_or_else(|err| {
            panic!(
                "failed to check reachability for action `{}` (node={}, instance={:?}) for project at {}: {}",
                to_action_name,
                to_node_name,
                to_instance_id,
                dir.display(),
                err
            )
        });

        if reachable {
            break;
        }

        sleep(Duration::from_millis(50)).await;
    }
}

pub async fn wait_for_shutdown_service_reachable_or_exit(
    ctx: &WaitContext<'_>,
    to_node_name: &str,
    to_instance_id: &str,
    child: &mut std::process::Child,
    dir: &std::path::Path,
) {
    wait_for_service_reachable_or_exit(
        ctx,
        to_node_name,
        SHUTDOWN_SERVICE,
        Some(to_instance_id),
        child,
        dir,
    )
    .await;
}

pub async fn wait_for_health_service_reachable_or_exit(
    ctx: &WaitContext<'_>,
    to_node_name: &str,
    to_instance_id: &str,
    child: &mut std::process::Child,
    dir: &std::path::Path,
) {
    wait_for_service_reachable_or_exit(
        ctx,
        to_node_name,
        NODE_HEALTH_SERVICE,
        Some(to_instance_id),
        child,
        dir,
    )
    .await;
}

pub async fn send_shutdown(
    messenger: &MessengerHandle,
    bound_core_node: &str,
    sender_instance_id: &str,
    to_node_name: &str,
    to_core_node: Option<&str>,
    to_instance_id: &str,
    timeout: Duration,
) {
    let payload = peppylib::types::Payload::from_static(b"shutdown");
    ServiceMessenger::poll(
        messenger,
        bound_core_node,
        sender_instance_id,
        to_node_name,
        Iface::native(),
        SHUTDOWN_SERVICE,
        to_core_node,
        Some(to_instance_id),
        payload,
        timeout,
    )
    .await
    .unwrap_or_else(|err| {
        panic!(
            "failed to send shutdown to node={} instance={} (project core node={}): {}",
            to_node_name, to_instance_id, bound_core_node, err
        )
    });
}

/// Like `send_shutdown` but doesn't panic if the service is unreachable
/// (e.g., the process has already exited).
pub async fn try_send_shutdown(
    messenger: &MessengerHandle,
    bound_core_node: &str,
    sender_instance_id: &str,
    to_node_name: &str,
    to_core_node: Option<&str>,
    to_instance_id: &str,
    timeout: Duration,
) {
    let payload = peppylib::types::Payload::from_static(b"shutdown");
    let _ = ServiceMessenger::poll(
        messenger,
        bound_core_node,
        sender_instance_id,
        to_node_name,
        Iface::native(),
        SHUTDOWN_SERVICE,
        to_core_node,
        Some(to_instance_id),
        payload,
        timeout,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Python-specific helpers
// ---------------------------------------------------------------------------

pub const STUB_PYTHON_NODE_CONFIG: &str = r#"{
  peppy_schema: "node_v1",
  manifest: { name: "generated_node",
    tag: "v1" },
  execution: { language: "python",
    run_cmd: ["uv", "run", "python", "main.py"]
  }
}
"#;

/// Initialises a Python user-node project at `to_dir`.
///
/// Creates a minimal `pyproject.toml` that depends on the generated `peppygen`
/// package (located at [`PEPPYGEN_OUTPUT_PATH`] relative to the project root).
pub fn init_python_user_node(to_dir: impl AsRef<Path>) {
    let project_dir = to_dir.as_ref();
    fs::create_dir_all(project_dir).expect("failed to create Python user node directory");

    let pyproject = format!(
        r#"[project]
name = "user_node"
version = "0.1.0"
requires-python = ">={PYTHON_MIN_VERSION},<{PYTHON_MAX_VERSION}"
dependencies = ["peppygen"]

[tool.uv.sources]
peppygen = {{ path = "{}" }}
"#,
        PEPPYGEN_OUTPUT_PATH
    );
    fs::write(project_dir.join("pyproject.toml"), pyproject)
        .expect("failed to write Python user node pyproject.toml");
}

/// Resolves and installs dependencies for the Python project via `uv sync`.
pub fn init_python_project_venv(dir: impl AsRef<Path>) {
    let output = Command::new("uv")
        .arg("sync")
        .current_dir(dir.as_ref())
        .stdin(Stdio::null())
        .output()
        .expect("failed to invoke uv sync on Python project");
    assert!(
        output.status.success(),
        "uv sync failed for Python project with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Spawns `uv run python main.py` inside the given project directory.
pub fn spawn_python_run(dir: &std::path::Path, env_vars: &[(&str, &str)]) -> std::process::Child {
    let mut command = Command::new("uv");
    command
        .args(["run", "python", "main.py"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(dir);

    for &(key, value) in env_vars {
        command.env(key, value);
    }

    command.spawn().expect("failed to spawn uv run python")
}
