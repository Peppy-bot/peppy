macro_rules! assert_rendered {
    ($cond:expr, $rendered:expr, $($arg:tt)+) => {
        if !$cond {
            eprintln!("rendered output:\n{}", $rendered);
            panic!($($arg)+);
        }
    };
}

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH, PeppyDirs};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use super::types::InterfaceArtifact;

pub const STUB_NODE_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: {
    name: "generated_node",
    tag: "0.1.0",
  },
  codegen: {
    language: "rust",
  },
  process: {
    start_cmd: ["./target/release/generated_node"]
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
) -> (
    G,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let (output_dir, user_node, peppy_node_config_path) = prepare_directories(temp_dir);
    (G::default(), output_dir, user_node, peppy_node_config_path)
}

pub fn copy_config_to_output(user_node: &Path, output_dir: &Path) -> std::path::PathBuf {
    let source = user_node.join(NODE_CONFIG_FILE);
    let destination = output_dir.join(NODE_CONFIG_FILE);
    fs::copy(&source, &destination).unwrap();
    destination
}

/// Converts artifacts into a vector of generated code strings.
pub fn render_artifacts(artifacts: Vec<InterfaceArtifact>) -> Vec<String> {
    artifacts
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect()
}

pub use config::test_helpers::assert_contains_all;

/// Returns a stable, shared target directory for test compilations so that
/// dependencies are compiled once and reused across all clippy/build tests.
fn stable_test_target_dir() -> PathBuf {
    PeppyDirs::default().root().join("cache/rust/test-targets")
}

/// Runs `cargo clippy` on a generated crate, using a shared target directory
/// and an exclusive file lock to prevent parallel cargo processes from
/// exhausting file descriptors (sccache "Too many open files").
pub fn run_clippy(output_dir: &Path) {
    let target_dir = stable_test_target_dir();
    fs::create_dir_all(&target_dir).expect("failed to create stable test target directory");

    let lock_file = fs::File::create(target_dir.join(".compile.lock"))
        .expect("failed to create compile lock file");
    lock_file.lock().expect("failed to acquire compile lock");

    let clippy_output = Command::new("cargo")
        .arg("clippy")
        .arg("--all-targets")
        .arg("--color")
        .arg("always")
        .arg("--")
        .arg("-D")
        .arg("warnings")
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(output_dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run cargo clippy on generated crate");
    assert!(
        clippy_output.status.success(),
        "cargo clippy failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        clippy_output.status.code(),
        String::from_utf8_lossy(&clippy_output.stdout),
        String::from_utf8_lossy(&clippy_output.stderr)
    );
}

/// Runs `cargo build` on a generated crate, using the same shared target
/// directory and file lock as [`run_clippy`].
pub fn run_cargo_build(output_dir: &Path) {
    let target_dir = stable_test_target_dir();
    fs::create_dir_all(&target_dir).expect("failed to create stable test target directory");

    let lock_file = fs::File::create(target_dir.join(".compile.lock"))
        .expect("failed to create compile lock file");
    lock_file.lock().expect("failed to acquire compile lock");

    let cargo_output = Command::new("cargo")
        .arg("build")
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(output_dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run cargo build on generated crate");
    assert!(
        cargo_output.status.success(),
        "cargo build failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        cargo_output.status.code(),
        String::from_utf8_lossy(&cargo_output.stdout),
        String::from_utf8_lossy(&cargo_output.stderr)
    );
}

/// Asserts that at least one artifact contains the given pattern.
pub fn assert_artifact_contains(artifacts: &[String], pattern: &str) {
    let rendered = artifacts.join("\n");
    assert_rendered!(
        artifacts.iter().any(|artifact| artifact.contains(pattern)),
        &rendered,
        "expected an artifact containing pattern: {:?}",
        pattern
    );
}
