macro_rules! assert_rendered {
    ($cond:expr, $rendered:expr, $($arg:tt)+) => {
        if !$cond {
            eprintln!("rendered output:\n{}", $rendered);
            panic!($($arg)+);
        }
    };
}

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use super::types::InterfaceArtifact;

pub const STUB_NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "generated_node",
    tag: "v1",
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

pub use config_test_support::assert_contains_all;

/// Returns a stable, shared target directory for test compilations so that
/// dependencies are compiled once and reused across all clippy/build tests.
///
/// Rooted at [`config_test_support::test_data_root`] (the disk-backed test root),
/// NOT `PeppyDirs::default()`: the latter resolves to `/tmp/.peppy` in dev, and a
/// tens-of-GB cargo target dir on `/tmp` tmpfs exhausts it and makes `ld` SIGBUS
/// mid-link.
fn stable_test_target_dir() -> PathBuf {
    config_test_support::test_data_root().join("cache/rust/test-targets")
}

/// Stable per-crate-dir hash used to derive a unique package name. Canonicalizes
/// so the same directory always hashes identically regardless of how the path was
/// spelled.
///
/// Deliberately duplicates `crate_identity_hash` in the integration-test helpers
/// (`tests/helpers.rs`): that module is a separate compilation unit and this one is
/// `#[cfg(test)]`-gated in the lib `src`, so they cannot share a private item
/// without leaking test-only scaffolding into the non-test API.
fn crate_identity_hash(crate_dir: &Path) -> u64 {
    let stable_identity = crate_dir
        .canonicalize()
        .unwrap_or_else(|_| crate_dir.to_path_buf());
    let mut hasher = DefaultHasher::new();
    stable_identity.hash(&mut hasher);
    hasher.finish()
}

/// Rewrites the generated `peppygen` crate's `Cargo.toml` `package.name` to a
/// per-test-unique value before it is built.
///
/// The generator names every node's library `peppygen` (`peppygen v0.1.0`). These
/// checks build that crate directly into the shared test target dir, so without a
/// unique name cargo treats two structurally different `peppygen v0.1.0` crates as
/// one interchangeable unit and may reuse (or clobber) a single cached rlib. A
/// generated-code regression in one test could then be masked by another test's
/// already-fresh artifact, defeating the point of these compile/clippy checks. A
/// unique name per crate dir keeps each test's peppygen a distinct cargo unit; the
/// heavy shared deps (peppylib, tokio, capnp, ...) keep their names and are still
/// reused across builds.
///
/// The crate is built standalone here (nothing consumes it by name), so unlike the
/// wrapper-consumed case in `tests/helpers.rs` no `package = "..."` consumer alias
/// is needed. Idempotent; a no-op if the peppygen `Cargo.toml` is absent.
fn rename_peppygen_package(peppygen_dir: &Path) {
    let unique = format!("peppygen_{:016x}", crate_identity_hash(peppygen_dir));
    let peppygen_cargo = peppygen_dir.join("Cargo.toml");
    let Ok(contents) = fs::read_to_string(&peppygen_cargo) else {
        return;
    };
    if contents.contains(&format!("name = \"{unique}\"")) {
        return;
    }
    let renamed = contents.replacen("name = \"peppygen\"", &format!("name = \"{unique}\""), 1);
    assert_ne!(
        renamed,
        contents,
        "expected `name = \"peppygen\"` in generated peppygen Cargo.toml at {}",
        peppygen_cargo.display()
    );
    fs::write(&peppygen_cargo, renamed)
        .expect("failed to rewrite peppygen Cargo.toml package name");
}

/// Runs `cargo clippy` on a generated crate.
///
/// Uses a shared target directory so heavy dependencies are compiled once and
/// reused across all checks. Two protections cover its two distinct hazards:
/// `rename_peppygen_package` gives this test's peppygen a distinct cargo unit so a
/// neighbouring test's cached rlib cannot be reused in its place (correctness),
/// and the exclusive file lock serializes the cargo invocations so parallel
/// processes do not exhaust file descriptors (sccache "Too many open files"). The
/// lock alone does not prevent the reuse: identity is decided in the target dir, so
/// even serialized builds can pick up a stale same-named unit.
pub fn run_clippy(output_dir: &Path) {
    let target_dir = stable_test_target_dir();
    fs::create_dir_all(&target_dir).expect("failed to create stable test target directory");
    rename_peppygen_package(output_dir);

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
/// directory, per-test peppygen rename, and file lock as [`run_clippy`].
pub fn run_cargo_build(output_dir: &Path) {
    let target_dir = stable_test_target_dir();
    fs::create_dir_all(&target_dir).expect("failed to create stable test target directory");
    rename_peppygen_package(output_dir);

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
