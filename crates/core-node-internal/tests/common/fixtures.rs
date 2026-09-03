#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::PeppygenLanguage;
use core_node::nodes_repo_cache_path;
use daemon_config::consts::PeppyDirs;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// Publishes `root`'s `peppy_repository.json5`, which is what a repository
/// offers a daemon. Call it once the repository holds every item the test
/// expects to be found.
pub fn publish_repo_index(root: &Path) {
    core_node::publish_repository_index(root)
        .expect("a well-formed test repository can be published");
}

/// Publishes `root`'s index and commits it together with everything else in
/// the work tree, returning the branch HEAD is on.
///
/// A git repository is read from a clone, so what it publishes has to be
/// committed, not merely written.
pub fn publish_and_commit_repo_index(root: &Path) -> String {
    publish_repo_index(root);

    let repo = git2::Repository::open(root).expect("open git repository");
    let mut index = repo.index().expect("open index");
    index
        .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("stage the work tree");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let signature = git2::Signature::now("Peppy", "peppy@example.com").expect("create signature");
    let parents: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|oid| vec![repo.find_commit(oid).expect("find parent commit")])
        .unwrap_or_default();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "publish",
        &tree,
        &parent_refs,
    )
    .expect("commit the published repository");

    let head = repo.head().expect("read HEAD");
    head.shorthand().expect("HEAD is on a branch").to_owned()
}

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

/// Builder for `nodes.json5`, `launchers.json5`, and `contracts.json5` cache
/// fixtures. Tests call [`TestPackagesCache::fs_entry`] / `git_entry` /
/// `launcher_fs_entry` / `contract_git_entry` to declare discovered items,
/// then [`TestPackagesCache::write`] to serialize the files under
/// `peppy_dirs.cache_dir()`.
#[derive(Default)]
pub struct TestPackagesCache {
    entries: Vec<serde_json::Value>,
    launchers: Vec<serde_json::Value>,
    contracts: Vec<serde_json::Value>,
}

/// A distinct, valid fingerprint per `seed`, for entries whose bytes the
/// test never reads back.
pub fn seeded_sha(seed: &str) -> String {
    config::fingerprint::fingerprint_for_bytes(seed.as_bytes())
}

/// A distinct, valid commit per `seed`.
fn seeded_commit(seed: &str) -> String {
    seeded_sha(seed)[..40].to_owned()
}

/// The fingerprint of the bytes at `path`, or a seeded one when the file is
/// not readable. A pinned add verifies materialized bytes against the entry's
/// fingerprint, so a fixture whose file already exists records the real one; a
/// fixture whose file is written later (or never, for a lookup-only test)
/// falls back to `seed`.
fn fingerprint_or_seeded(path: &Path, seed: &str) -> String {
    std::fs::read(path)
        .map(|bytes| config::fingerprint::fingerprint_for_bytes(&bytes))
        .unwrap_or_else(|_| seeded_sha(seed))
}

/// The `links` object of a node cache entry that declares no contract or
/// pairing link, serialized from the type `repo refresh` writes so a change
/// to its shape reaches every hand-written fixture.
pub fn empty_links() -> serde_json::Value {
    serde_json::to_value(core_node::DeclaredLinks::default()).expect("serialize empty links")
}

/// The `origin` object of a cache entry for an item on this machine, in the
/// shape `repo refresh` writes it. The one spelling of that shape for every
/// test binary, so a change to it is a change here rather than in each
/// hand-written fixture.
pub fn fs_origin(path: &Path) -> serde_json::Value {
    serde_json::json!({
        "source_type": "fs",
        "path": path.to_string_lossy(),
    })
}

/// The commit `repo_ref` points at in the repository at `repo_url`, so a
/// fixture pins the bytes the test actually committed.
///
/// Falls back to a seeded value when `repo_url` is not a repository on this
/// machine, which is the case for fixtures that only ever exercise lookup
/// and never materialize.
fn head_commit_of(repo_url: &str, repo_ref: &str, seed: &str) -> String {
    let path = repo_url.strip_prefix("file://").unwrap_or(repo_url);
    let Ok(repo) = git2::Repository::open(path) else {
        return seeded_commit(seed);
    };
    repo.revparse_single(repo_ref)
        .or_else(|_| repo.revparse_single("HEAD"))
        .ok()
        .and_then(|object| object.peel_to_commit().ok())
        .map(|commit| commit.id().to_string())
        .unwrap_or_else(|| seeded_commit(seed))
}

/// The `origin` object of a cache entry for an item held by a remote, in
/// the shape `repo refresh` writes it. `path` is repository-relative and
/// points at the file that declares the item.
pub fn git_origin(repo_url: &str, repo_ref: &str, path: &str, seed: &str) -> serde_json::Value {
    serde_json::json!({
        "source_type": "git",
        "repo_url": repo_url,
        "repo_ref": repo_ref,
        "commit": head_commit_of(repo_url, repo_ref, seed),
        "path": path,
    })
}

impl TestPackagesCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// `absolute_path` is the directory containing `peppy.json5`. The
    /// cache stores the manifest file path (path-points-at-file
    /// convention), so we join `NODE_CONFIG_FILE` here.
    ///
    /// The fingerprint follows [`fingerprint_or_seeded`]: the file's bytes
    /// when it exists, a seeded value otherwise.
    pub fn fs_entry(mut self, name: &str, tag: &str, absolute_path: impl AsRef<Path>) -> Self {
        let manifest_path = absolute_path.as_ref().join(NODE_CONFIG_FILE);
        let sha256 = fingerprint_or_seeded(&manifest_path, &format!("{name}:{tag}"));
        self.entries.push(serde_json::json!({
            "node_name": name,
            "node_tag": tag,
            "sha256": sha256,
            "origin": fs_origin(&manifest_path),
            "links": empty_links(),
        }));
        self
    }

    /// `path_in_repo` is the directory containing `peppy.json5` within
    /// the checked-out repo. We join `NODE_CONFIG_FILE` so the cache
    /// records the manifest file path.
    ///
    /// The fingerprint follows [`fingerprint_or_seeded`], read from the
    /// repository's worktree copy of the manifest when `repo_url` is a path on
    /// this machine; a remote or lookup-only fixture gets a seeded value.
    pub fn git_entry(
        mut self,
        name: &str,
        tag: &str,
        repo_url: &str,
        resolved_ref: &str,
        path_in_repo: &str,
    ) -> Self {
        let manifest_path = Path::new(path_in_repo).join(NODE_CONFIG_FILE);
        let local_repo = repo_url.strip_prefix("file://").unwrap_or(repo_url);
        let sha256 = fingerprint_or_seeded(
            &Path::new(local_repo).join(&manifest_path),
            &format!("{name}:{tag}"),
        );
        self.entries.push(serde_json::json!({
            "node_name": name,
            "node_tag": tag,
            "sha256": sha256,
            "origin": git_origin(
                repo_url,
                resolved_ref,
                &manifest_path.to_string_lossy(),
                &format!("{name}:{tag}"),
            ),
            "links": empty_links(),
        }));
        self
    }

    /// Adds a `launchers.json5` entry for a filesystem-sourced launcher.
    /// `absolute_path` points at the launcher `.json5` file itself. The
    /// fingerprint follows [`fingerprint_or_seeded`]; launcher resolution
    /// materializes the origin's path without a drift check, so a seeded
    /// value is harmless for a file written later.
    pub fn launcher_fs_entry(mut self, name: &str, absolute_path: impl AsRef<Path>) -> Self {
        let path = absolute_path.as_ref();
        let sha256 = fingerprint_or_seeded(path, name);
        self.launchers.push(serde_json::json!({
            "launcher_name": name,
            "sha256": sha256,
            "origin": fs_origin(path),
        }));
        self
    }

    /// Adds a `contracts.json5` entry for a git-sourced contract. `body`
    /// is the on-disk contract JSON5 (assumed already committed at
    /// `path_in_repo` inside `repo_url`); its sha256 is computed here so
    /// the cache fingerprint matches what `ensure_checkout` will read.
    pub fn contract_git_entry(
        mut self,
        name: &str,
        tag: &str,
        repo_url: &str,
        resolved_ref: &str,
        path_in_repo: &str,
        body: &str,
    ) -> Self {
        self.contracts.push(serde_json::json!({
            "contract_name": name,
            "tag": tag,
            "sha256": config::fingerprint::fingerprint_for_bytes(body.as_bytes()),
            "origin": git_origin(repo_url, resolved_ref, path_in_repo, &format!("{name}:{tag}")),
        }));
        self
    }

    /// Adds a `contracts.json5` entry for a filesystem-sourced contract.
    /// `body` is the on-disk contract JSON5 (assumed already written at
    /// `absolute_path`); its sha256 is computed here so the cache
    /// fingerprint matches what `resolve_contract_doc` reads back.
    pub fn contract_fs_entry(
        mut self,
        name: &str,
        tag: &str,
        absolute_path: impl AsRef<Path>,
        body: &str,
    ) -> Self {
        self.contracts.push(serde_json::json!({
            "contract_name": name,
            "tag": tag,
            "sha256": config::fingerprint::fingerprint_for_bytes(body.as_bytes()),
            "origin": fs_origin(absolute_path.as_ref()),
        }));
        self
    }

    pub fn write(self, peppy_dirs: &daemon_config::consts::PeppyDirs) {
        let cache_dir = peppy_dirs.cache_dir();
        std::fs::create_dir_all(&cache_dir).expect("failed to create cache dir");
        let content =
            serde_json::to_string_pretty(&self.entries).expect("failed to serialize cache entries");
        std::fs::write(nodes_repo_cache_path(peppy_dirs), content)
            .expect("failed to write nodes.json5 fixture");
        write_or_clear(
            &core_node::launchers_repo_cache_path(peppy_dirs),
            &self.launchers,
            "launchers.json5",
        );
        write_or_clear(
            &core_node::contracts_repo_cache_path(peppy_dirs),
            &self.contracts,
            "contracts.json5",
        );
    }
}

/// Writes `entries` to `path`, or removes any stale file left by an earlier
/// fixture when the set is empty. An absent cache file and an empty one are
/// not the same thing to the readers under test, so the empty case clears
/// rather than writing `[]`.
fn write_or_clear(path: &Path, entries: &[serde_json::Value], label: &str) {
    if entries.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("failed to remove stale {label} fixture: {e}"),
        }
        return;
    }
    let content = serde_json::to_string_pretty(entries)
        .unwrap_or_else(|e| panic!("failed to serialize {label} cache entries: {e}"));
    std::fs::write(path, content)
        .unwrap_or_else(|e| panic!("failed to write {label} fixture: {e}"));
}

/// Convenience helper: writes `peppy.json5` under `dir` but skips the
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
/// The returned [`TempDir`] owns the directory and deletes it, including the
/// multi-GB cargo `target/`, when it drops, so test runs never accumulate
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
        generator::NodeTree::Source,
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
