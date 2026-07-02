#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::PeppygenLanguage;
use core_node::nodes_repo_cache_path;
use daemon_config::consts::PeppyDirs;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// Writes a node config file and the corresponding fingerprint file expected by `node_add`.
pub fn write_peppy_json5(dir: &Path, content: &str) {
    let config_path = dir.join(NODE_CONFIG_FILE);
    std::fs::write(&config_path, content).expect("failed to write peppy.json5");
    config::fingerprint::create_codegen_fingerprint(&config_path, Path::new(PEPPYGEN_OUTPUT_PATH));
}

pub fn create_tar_zst_from_dir(source_dir: &Path, archive_path: &Path, archive_root_name: &str) {
    let bundle_file = std::fs::File::create(archive_path).expect("failed to create bundle file");
    let encoder =
        zstd::stream::write::Encoder::new(bundle_file, 0).expect("failed to create zstd encoder");
    let mut tar_builder = tar::Builder::new(encoder);
    tar_builder
        .append_dir_all(archive_root_name, source_dir)
        .expect("failed to append source dir to tar");
    tar_builder.finish().expect("failed to finish tar");
    let encoder = tar_builder
        .into_inner()
        .expect("failed to finish tar encoder");
    encoder.finish().expect("failed to finalize zstd stream");
}

/// Builder for `nodes.json5` and `interfaces.json5` cache fixtures. Tests call
/// [`TestPackagesCache::fs_entry`] / `git_entry` / `interface_git_entry` to
/// declare discovered items, then [`TestPackagesCache::write`] to serialize
/// the files under `peppy_dirs.cache_dir()`.
#[derive(Default)]
pub struct TestPackagesCache {
    entries: Vec<serde_json::Value>,
    interfaces: Vec<serde_json::Value>,
}

impl TestPackagesCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// `absolute_path` is the directory containing `peppy.json5`. The
    /// cache stores the manifest file path (path-points-at-file
    /// convention), so we join `NODE_CONFIG_FILE` here.
    pub fn fs_entry(mut self, name: &str, tag: &str, absolute_path: impl AsRef<Path>) -> Self {
        let manifest_path = absolute_path.as_ref().join(NODE_CONFIG_FILE);
        let mut m = serde_json::Map::new();
        m.insert("node_name".into(), serde_json::Value::String(name.into()));
        m.insert("node_tag".into(), serde_json::Value::String(tag.into()));
        m.insert("source_type".into(), serde_json::Value::String("fs".into()));
        m.insert(
            "path".into(),
            serde_json::Value::String(manifest_path.to_string_lossy().into_owned()),
        );
        self.entries.push(serde_json::Value::Object(m));
        self
    }

    /// `path_in_repo` is the directory containing `peppy.json5` within
    /// the checked-out repo. We join `NODE_CONFIG_FILE` so the cache
    /// records the manifest file path.
    pub fn git_entry(
        mut self,
        name: &str,
        tag: &str,
        repo_url: &str,
        resolved_ref: &str,
        path_in_repo: &str,
    ) -> Self {
        let manifest_path = Path::new(path_in_repo).join(NODE_CONFIG_FILE);
        let mut m = serde_json::Map::new();
        m.insert("node_name".into(), serde_json::Value::String(name.into()));
        m.insert("node_tag".into(), serde_json::Value::String(tag.into()));
        m.insert(
            "source_type".into(),
            serde_json::Value::String("git".into()),
        );
        m.insert(
            "source_uri".into(),
            serde_json::Value::String(repo_url.into()),
        );
        m.insert(
            "resolved_ref".into(),
            serde_json::Value::String(resolved_ref.into()),
        );
        m.insert(
            "path".into(),
            serde_json::Value::String(manifest_path.to_string_lossy().into_owned()),
        );
        self.entries.push(serde_json::Value::Object(m));
        self
    }

    /// Adds an `interfaces.json5` entry for a git-sourced interface. `body`
    /// is the on-disk interface JSON5 (assumed already committed at
    /// `path_in_repo` inside `repo_url`); its sha256 is computed here so
    /// the cache fingerprint matches what `ensure_checkout` will read.
    pub fn interface_git_entry(
        mut self,
        name: &str,
        tag: &str,
        repo_url: &str,
        resolved_ref: &str,
        path_in_repo: &str,
        body: &str,
    ) -> Self {
        let sha = config::fingerprint::fingerprint_for_bytes(body.as_bytes());
        let mut m = serde_json::Map::new();
        m.insert(
            "interface_name".into(),
            serde_json::Value::String(name.into()),
        );
        m.insert("tag".into(), serde_json::Value::String(tag.into()));
        m.insert("sha256".into(), serde_json::Value::String(sha));
        m.insert(
            "source_type".into(),
            serde_json::Value::String("git".into()),
        );
        m.insert(
            "source_uri".into(),
            serde_json::Value::String(repo_url.into()),
        );
        m.insert(
            "resolved_ref".into(),
            serde_json::Value::String(resolved_ref.into()),
        );
        m.insert(
            "path".into(),
            serde_json::Value::String(path_in_repo.into()),
        );
        self.interfaces.push(serde_json::Value::Object(m));
        self
    }

    /// Adds an `interfaces.json5` entry for a filesystem-sourced interface.
    /// `body` is the on-disk interface JSON5 (assumed already written at
    /// `absolute_path`); its sha256 is computed here so the cache
    /// fingerprint matches what `resolve_interface_doc` reads back.
    pub fn interface_fs_entry(
        mut self,
        name: &str,
        tag: &str,
        absolute_path: impl AsRef<Path>,
        body: &str,
    ) -> Self {
        let sha = config::fingerprint::fingerprint_for_bytes(body.as_bytes());
        let mut m = serde_json::Map::new();
        m.insert(
            "interface_name".into(),
            serde_json::Value::String(name.into()),
        );
        m.insert("tag".into(), serde_json::Value::String(tag.into()));
        m.insert("sha256".into(), serde_json::Value::String(sha));
        m.insert("source_type".into(), serde_json::Value::String("fs".into()));
        m.insert(
            "path".into(),
            serde_json::Value::String(absolute_path.as_ref().to_string_lossy().into_owned()),
        );
        self.interfaces.push(serde_json::Value::Object(m));
        self
    }

    pub fn write(self, peppy_dirs: &daemon_config::consts::PeppyDirs) {
        let cache_dir = peppy_dirs.cache_dir();
        std::fs::create_dir_all(&cache_dir).expect("failed to create cache dir");
        let content =
            serde_json::to_string_pretty(&self.entries).expect("failed to serialize cache entries");
        std::fs::write(nodes_repo_cache_path(peppy_dirs), content)
            .expect("failed to write nodes.json5 fixture");
        let interfaces_path = core_node::interfaces_repo_cache_path(peppy_dirs);
        if self.interfaces.is_empty() {
            match std::fs::remove_file(&interfaces_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("failed to remove stale interfaces.json5 fixture: {e}"),
            }
        } else {
            let interfaces_content = serde_json::to_string_pretty(&self.interfaces)
                .expect("failed to serialize interface cache entries");
            std::fs::write(interfaces_path, interfaces_content)
                .expect("failed to write interfaces.json5 fixture");
        }
    }
}

/// Convenience helper — writes `peppy.json5` under `dir` but skips the
/// fingerprint generation (useful for packages-cache FS fixtures that
/// aren't going through the fingerprint verification path).
pub fn write_plain_peppy_json5(dir: &Path, content: &str) {
    std::fs::create_dir_all(dir).expect("failed to create dir");
    std::fs::write(dir.join(NODE_CONFIG_FILE), content).expect("failed to write peppy.json5");
}

/// Creates a fresh test node in a new temp directory.
/// Each call creates a completely new node with its own peppygen generation
/// and cargo build, ensuring isolation between tests.
pub fn create_test_node() -> TempDir {
    init_test_node_project("example_node", "v1", true)
}

/// Creates a fresh test node in a new temp directory.
/// Each call creates a completely new node with its own peppygen generation
/// and cargo build, ensuring isolation between tests.
///
/// The returned [`TempDir`] owns the directory and deletes it — including the
/// multi-GB cargo `target/` — when it drops, so test runs never accumulate
/// build artifacts. Bind it for as long as the node is needed (e.g. for the
/// whole test body) and let it drop at scope end.
pub fn create_test_node_with_name(node_name: &str, node_tag: &str) -> TempDir {
    init_test_node_project(node_name, node_tag, true)
}

pub fn init_test_node_project(node_name: &str, node_tag: &str, build_project: bool) -> TempDir {
    // Build under the shared test-tmp root (see `config_test_support::test_tmp_root`) and keep the
    // `TempDir` guard rather than `.keep()`-ing it, so the directory and its
    // ~2 GB cargo build are reclaimed when the returned guard drops.
    let node_dir = tempfile::Builder::new()
        .prefix("peppy_test_node_")
        .tempdir_in(config_test_support::test_tmp_root())
        .expect("failed to create temp directory for test node");

    init_cargo_project(node_dir.path(), node_name);
    write_test_node_files(node_dir.path(), node_name, node_tag);

    let peppy_dirs = PeppyDirs::default();
    generator::generate_peppygen_lib(
        PeppygenLanguage::Rust,
        node_dir.path(),
        Vec::new(),
        "test-hash",
        &peppy_dirs,
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen for test node");

    if build_project {
        build_cargo_project(node_dir.path());
    }

    node_dir
}

fn init_cargo_project(node_dir: &Path, crate_name: &str) {
    let output = Command::new("cargo")
        .arg("init")
        .arg("--bin")
        .arg("--vcs")
        .arg("none")
        .arg("--name")
        .arg(crate_name)
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(node_dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to invoke `cargo init` for test node");

    assert!(
        output.status.success(),
        "`cargo init` failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_test_node_files(node_dir: &Path, crate_name: &str, node_tag: &str) {
    std::fs::write(
        node_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"

[dependencies]
peppygen = {{ path = "{PEPPYGEN_OUTPUT_PATH}" }}
"#
        ),
    )
    .expect("failed to write test node Cargo.toml");

    std::fs::write(
        node_dir.join("src/main.rs"),
        r#"use peppygen::{NodeBuilder, Parameters, Result};

fn main() -> Result<()> {
    NodeBuilder::new().run(|args: Parameters, node_runner| async {
        let _ = args;
        let _ = node_runner;
        Ok(())
    })
}
"#,
    )
    .expect("failed to write test node src/main.rs");

    // Use the pre-built binary path in run_cmd instead of "cargo run".
    // This avoids recompilation after the folder is copied to storage,
    // since cargo's fingerprinting invalidates the cache when absolute paths change.
    let binary_path = node_dir.join("target/debug").join(crate_name);
    std::fs::write(
        node_dir.join(NODE_CONFIG_FILE),
        r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "{crate_name}",
    tag: "{node_tag}",
  },
  interfaces: {
    topics: {
      emits: [
        {
          name: "hello_world",
          qos_profile: "sensor_data",
          message_format: {
            timestamp: "time",
            message: "string"
          }
        }
      ],
    }
  },
  // Avoid `build_cmd` build step here to make the `add` tests faster
  execution: {
    language: "rust",
    build_cmd: [
        "true"
    ],
    run_cmd: [
      "{binary_path}"
    ]
  },
}"#
        .replace("{crate_name}", crate_name)
        .replace("{node_tag}", node_tag)
        .replace("{binary_path}", &binary_path.display().to_string()),
    )
    .expect("failed to write test node peppy.json5");
}

fn build_cargo_project(dir: &Path) {
    let output = Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to invoke `cargo build` for test node");

    assert!(
        output.status.success(),
        "`cargo build` failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
