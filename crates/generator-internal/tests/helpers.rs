#![allow(dead_code)]

use config::consts::{
    NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH, PEPPYLIB_OUTPUT_PATH, PYTHON_MAX_VERSION,
    PYTHON_MIN_VERSION, PeppyDirs,
};
use config::node::PeppygenLanguage;
use generator::generate_peppygen_lib;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{ActionMessenger, NODE_HEALTH_SERVICE, SHUTDOWN_SERVICE};
use peppylib::{MessengerHandle, ServiceMessenger};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
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

/// Root for per-test scratch directories, placed under `$HOME` rather than the
/// system temp dir because `/tmp` is frequently a size-quota'd `tmpfs` on Linux
/// dev/CI machines. Each Python node materialises a venv plus several copies of
/// the (large) compiled `peppylib` shared object during `uv sync`; at full test
/// parallelism that transient peak trips the per-user tmpfs quota. `$HOME` lives
/// on the roomy backing disk instead.
///
/// Hand out scratch from here via [`TempDir::new_in`] so it is removed when the
/// guard drops — normal completion and panics both clean up and nothing is
/// carried between runs. As a backstop for runs hard-killed before their guards
/// could run, the first call per test binary reclaims leftovers older than
/// [`STALE_TEST_TMP_AGE`]; that age floor keeps concurrently-running test
/// binaries from deleting each other's live dirs.
///
/// Note: the shared `peppylib` `.so` cache itself still lives under
/// [`test_peppy_dirs`] (the global `.peppy`); only the per-test copies move here.
pub fn test_tmp_root() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    let root = PathBuf::from(home).join(".peppy/test-tmp");
    fs::create_dir_all(&root).expect("create ~/.peppy/test-tmp/");

    static RECLAIM: std::sync::Once = std::sync::Once::new();
    RECLAIM.call_once(|| reclaim_stale_test_tmp(&root));

    root
}

/// Scratch older than this is treated as abandoned by an earlier run and is
/// safe to delete. Far longer than any real test run (which finishes in
/// minutes), so an in-flight run is never affected.
const STALE_TEST_TMP_AGE: Duration = Duration::from_secs(60 * 60);

/// Best-effort removal of stale leftovers directly under `root`. Errors are
/// ignored on purpose: reclaiming scratch must never fail a test.
fn reclaim_stale_test_tmp(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let too_old = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_TEST_TMP_AGE);
        if !too_old {
            continue;
        }
        if metadata.is_dir() {
            let _ = fs::remove_dir_all(entry.path());
        } else {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub const TEST_NODE_TAG: &str = "v1";

/// Re-exported so the communication test files can name the messaging mode for
/// their `#[case]` parameterization without reaching into the internal path.
pub use config::peppy_config::Mode;

/// Applies a messaging mode to a node's runtime config before it is written and
/// handed to a spawned node. This is the single seam the dual-mode communication
/// tests use to run the same body under both peer (gossip on) and router (gossip
/// off) mode without duplicating the body.
pub fn apply_mode(
    mut config: config::runtime::RuntimeConfig,
    mode: Mode,
) -> config::runtime::RuntimeConfig {
    config.discovery.gossip = mode.gossip();
    config
}

/// Builds a node-shaped [`SenderTarget`] with the standard test tag. Panics on
/// invalid names — tests use known-good values only.
pub fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, TEST_NODE_TAG).expect("test node target")
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

    // TEMPORARY upstream-breakage pin (2026-06-12): `time 0.3.48` and
    // `rcgen 0.14.8` are mutually incompatible (E0119: rcgen's blanket
    // `impl<T: Into<String>> From<T>` collides with a new `time` impl), and
    // these generated test projects resolve fresh — unlike the workspace,
    // which pins `time 0.3.47` in Cargo.lock and is unaffected. Constrain
    // the transitive `time` (pulled in via peppylib → pmi → zenoh → rcgen)
    // to the known-good version so the integration suites stay green.
    // Remove once upstream ships a compatible pair.
    if !updated_manifest
        .lines()
        .any(|line| line.trim_start().starts_with("time"))
    {
        let time_pin_line = "time = \"=0.3.47\"\n";
        updated_manifest = insert_dependency_line(&updated_manifest, time_pin_line);
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

/// Wraps a spawned child whose stdout/stderr are piped, draining them
/// from background threads into shared buffers. This lets a test
/// inspect stdout while the child is still running — for example, to
/// wait for a specific line to appear before sending shutdown — without
/// blocking on the pipe. Existing helpers that take a plain
/// `&mut std::process::Child` keep working against the exposed `child`
/// field.
pub struct CapturedChild {
    pub child: std::process::Child,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stdout_drainer: Option<thread::JoinHandle<()>>,
    stderr_drainer: Option<thread::JoinHandle<()>>,
}

impl CapturedChild {
    pub fn new(mut child: std::process::Child) -> Self {
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));

        let stdout_drainer = child.stdout.take().map(|pipe| {
            let buf = Arc::clone(&stdout);
            thread::spawn(move || drain_pipe(pipe, buf))
        });
        let stderr_drainer = child.stderr.take().map(|pipe| {
            let buf = Arc::clone(&stderr);
            thread::spawn(move || drain_pipe(pipe, buf))
        });

        Self {
            child,
            stdout,
            stderr,
            stdout_drainer,
            stderr_drainer,
        }
    }

    /// Kills the child, reaps its exit status, and joins both drainer
    /// threads so the captured buffers reflect every byte the child wrote
    /// before its pipes closed. Idempotent against already-exited children
    /// (`kill` on a dead pid is a no-op error we ignore) so callers can
    /// invoke this from both the timeout and post-exit paths.
    fn reap_and_join(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.stdout_drainer.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_drainer.take() {
            let _ = handle.join();
        }
    }

    /// Blocks until captured stdout contains `pattern` (as a substring).
    /// Panics if the child exits first or if `timeout` elapses.
    pub fn wait_for_stdout_contains(
        &mut self,
        pattern: &str,
        timeout: Duration,
        dir: &std::path::Path,
    ) {
        let start = Instant::now();
        let needle = pattern.as_bytes();
        loop {
            if stdout_contains(&self.stdout, needle) {
                return;
            }

            if let Some(status) = self
                .child
                .try_wait()
                .expect("failed to poll process status for generated project")
            {
                self.reap_and_join();
                let stdout = lossy_snapshot(&self.stdout);
                let stderr = lossy_snapshot(&self.stderr);
                panic!(
                    "process exited before stdout contained {:?} (status: {:?}) for project at {}\nstdout:\n{}\nstderr:\n{}",
                    pattern,
                    status.code(),
                    dir.display(),
                    stdout,
                    stderr,
                );
            }

            if start.elapsed() > timeout {
                self.reap_and_join();
                let stdout = lossy_snapshot(&self.stdout);
                let stderr = lossy_snapshot(&self.stderr);
                panic!(
                    "timed out after {:?} waiting for stdout to contain {:?} for project at {}\nstdout so far:\n{}\nstderr so far:\n{}",
                    timeout,
                    pattern,
                    dir.display(),
                    stdout,
                    stderr,
                );
            }

            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits for the child to exit and returns the captured output.
    /// Mirrors [`wait_for_child`] but pulls from the background buffers
    /// instead of reading the pipes directly (the drainer threads now
    /// own them).
    pub fn wait(
        mut self,
        timeout: Option<Duration>,
        dir: &std::path::Path,
    ) -> std::process::Output {
        let start = Instant::now();
        loop {
            if let Some(limit) = timeout
                && start.elapsed() > limit
            {
                self.reap_and_join();
                panic!(
                    "process timed out after {:?} for project at {}\nstdout:\n{}\nstderr:\n{}",
                    limit,
                    dir.display(),
                    lossy_snapshot(&self.stdout),
                    lossy_snapshot(&self.stderr),
                );
            }

            if let Some(status) = self
                .child
                .try_wait()
                .expect("failed to poll process status for generated project")
            {
                // Joining the drainer handles guarantees the buffers
                // contain every byte the child wrote before its pipes
                // closed; a fixed sleep here would race against slow
                // drainers under load.
                if let Some(handle) = self.stdout_drainer.take() {
                    let _ = handle.join();
                }
                if let Some(handle) = self.stderr_drainer.take() {
                    let _ = handle.join();
                }
                let stdout = std::mem::take(&mut *self.stdout.lock().unwrap());
                let stderr = std::mem::take(&mut *self.stderr.lock().unwrap());
                return std::process::Output {
                    status,
                    stdout,
                    stderr,
                };
            }

            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn drain_pipe<R: Read + Send + 'static>(mut pipe: R, buf: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 4096];
    while let Ok(n) = pipe.read(&mut chunk) {
        if n == 0 {
            break;
        }
        if let Ok(mut guard) = buf.lock() {
            guard.extend_from_slice(&chunk[..n]);
        }
    }
}

fn stdout_contains(buf: &Arc<Mutex<Vec<u8>>>, needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let guard = buf.lock().unwrap();
    guard.windows(needle.len()).any(|w| w == needle)
}

fn lossy_snapshot(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&buf.lock().unwrap()).into_owned()
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
    // Unqualified (not `crate::helpers::`) so this compiles both when helpers.rs
    // is included as `mod helpers` and when cargo builds it as its own test binary.
    let temp_dir = TempDir::new_in(test_tmp_root()).expect("failed to create temp directory");
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

/// Context for waiting on service reachability in tests. The harness
/// always knows the generated project's core node (`target_core_node`),
/// so reachability probes for a known instance carry the full
/// `(core_node, instance_id)` wire address.
pub struct WaitContext<'a> {
    pub messenger: &'a MessengerHandle,
    pub bound_core_node: &'a str,
    pub caller_instance_id: &'a str,
    pub target_core_node: &'a str,
}

/// Default deadline for the wait-family helpers. Long enough for slow CI
/// (zenoh discovery + queryable propagation can take a couple of seconds
/// on a cold session); short enough that a true hang (e.g. a probed
/// endpoint that will never come up) fails loudly with a clear panic
/// instead of stalling the whole test binary. Each helper accepts an
/// explicit `timeout` so call sites can opt into something larger or
/// smaller — pass [`DEFAULT_WAIT_TIMEOUT`] when no value is meaningful.
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn wait_for_service_reachable_or_exit(
    ctx: &WaitContext<'_>,
    to_node_name: &str,
    to_service_name: &str,
    target_instance_id: Option<&str>,
    child: &mut std::process::Child,
    dir: &std::path::Path,
    timeout: Duration,
) {
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            panic!(
                "timed out after {:?} waiting for service `{}` (node={}, instance={:?}) to become reachable for project at {}",
                timeout,
                to_service_name,
                to_node_name,
                target_instance_id,
                dir.display(),
            );
        }

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

        let target = target_instance_id
            .map(|inst| peppylib::messaging::ProducerRef::new(ctx.target_core_node, inst));
        let reachable = ServiceMessenger::is_reachable(
            ctx.messenger,
            ctx.bound_core_node,
            ctx.caller_instance_id,
            test_node_target(to_node_name),
            to_service_name,
            target.as_ref(),
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "failed to check reachability for service `{}` (node={}, instance={:?}) for project at {}: {}",
                to_service_name,
                to_node_name,
                target_instance_id,
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
    target_instance_id: Option<&str>,
    child: &mut std::process::Child,
    dir: &std::path::Path,
    timeout: Duration,
) {
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            panic!(
                "timed out after {:?} waiting for action `{}` (node={}, instance={:?}) to become reachable for project at {}",
                timeout,
                to_action_name,
                to_node_name,
                target_instance_id,
                dir.display(),
            );
        }

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

        let target = target_instance_id
            .map(|inst| peppylib::messaging::ProducerRef::new(ctx.target_core_node, inst));
        let reachable = ActionMessenger::is_reachable(
            ctx.messenger,
            ctx.bound_core_node,
            ctx.caller_instance_id,
            test_node_target(to_node_name),
            to_action_name,
            target.as_ref(),
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "failed to check reachability for action `{}` (node={}, instance={:?}) for project at {}: {}",
                to_action_name,
                to_node_name,
                target_instance_id,
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
    target_instance_id: &str,
    child: &mut std::process::Child,
    dir: &std::path::Path,
    timeout: Duration,
) {
    wait_for_service_reachable_or_exit(
        ctx,
        to_node_name,
        SHUTDOWN_SERVICE,
        Some(target_instance_id),
        child,
        dir,
        timeout,
    )
    .await;
}

pub async fn wait_for_health_service_reachable_or_exit(
    ctx: &WaitContext<'_>,
    to_node_name: &str,
    target_instance_id: &str,
    child: &mut std::process::Child,
    dir: &std::path::Path,
    timeout: Duration,
) {
    wait_for_service_reachable_or_exit(
        ctx,
        to_node_name,
        NODE_HEALTH_SERVICE,
        Some(target_instance_id),
        child,
        dir,
        timeout,
    )
    .await;
}

pub async fn send_shutdown(
    messenger: &MessengerHandle,
    bound_core_node: &str,
    sender_instance_id: &str,
    to_node_name: &str,
    target_core_node: &str,
    target_instance_id: &str,
    timeout: Duration,
) {
    let payload = peppylib::types::Payload::from_static(b"shutdown");
    ServiceMessenger::poll(
        messenger,
        bound_core_node,
        sender_instance_id,
        test_node_target(to_node_name),
        SHUTDOWN_SERVICE,
        Some(&peppylib::messaging::ProducerRef::new(
            target_core_node,
            target_instance_id,
        )),
        payload,
        timeout,
    )
    .await
    .unwrap_or_else(|err| {
        panic!(
            "failed to send shutdown to node={} instance={} (project core node={}): {}",
            to_node_name, target_instance_id, bound_core_node, err
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
    target_core_node: &str,
    target_instance_id: &str,
    timeout: Duration,
) {
    let payload = peppylib::types::Payload::from_static(b"shutdown");
    let _ = ServiceMessenger::poll(
        messenger,
        bound_core_node,
        sender_instance_id,
        test_node_target(to_node_name),
        SHUTDOWN_SERVICE,
        Some(&peppylib::messaging::ProducerRef::new(
            target_core_node,
            target_instance_id,
        )),
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
/// and `peppylib` packages (located at [`PEPPYGEN_OUTPUT_PATH`] and
/// [`PEPPYLIB_OUTPUT_PATH`] relative to the project root).
pub fn init_python_user_node(to_dir: impl AsRef<Path>) {
    let project_dir = to_dir.as_ref();
    fs::create_dir_all(project_dir).expect("failed to create Python user node directory");

    let pyproject = format!(
        r#"[project]
name = "user_node"
version = "0.1.0"
requires-python = ">={PYTHON_MIN_VERSION},<{PYTHON_MAX_VERSION}"
dependencies = ["peppygen", "peppylib"]

[tool.uv.sources]
peppygen = {{ path = "{PEPPYGEN_OUTPUT_PATH}" }}
peppylib = {{ path = "{PEPPYLIB_OUTPUT_PATH}" }}
"#
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
