#![allow(dead_code)]

mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, NodeAddSource, TEST_GIT_HASH, build_staged_node,
    create_tar_zst_from_dir, send_node_add_and_wait, send_node_add_and_wait_with_force,
    send_node_add_and_wait_with_variant, spawn_real_running_instance, spawn_real_stuck_instance,
    start_core_node_with_mock_messenger, wait_until_service_reachable, write_peppy_json5,
};
use config::consts::{NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH};
use config::node::QoSProfile;
use config::test_helpers;
use core_node::names;
use core_node_api::encoding::{NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse};
use git2::{Repository, Signature};
use gix_url::Url as GitUrl;
use peppylib::ActionMessenger;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use {
    httptest::Expectation, httptest::Server, httptest::matchers::request,
    httptest::responders::status_code,
};

const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Returns the current `artifact_path` of the entity at `(name, tag)` as an owned
/// `PathBuf`. Panics if the entity is missing or is not yet `Built`.
///
/// The entity lookup uses the new `Arc<RwLock<NodeEntity>>` API: most call
/// sites used to call `entity.root_path()` directly; this helper encapsulates
/// the `read()` + `artifact_path().expect(...)` boilerplate.
fn entity_artifact_path(node_stack: &node_stack::NodeStack, name: &str, tag: &str) -> PathBuf {
    node_stack
        .find(name, tag)
        .expect("entity should exist")
        .read()
        .artifact_path()
        .expect("entity should be built")
        .to_path_buf()
}

/// Returns the number of tracked instances for the entity at `(name, tag)`.
fn entity_instance_count(node_stack: &node_stack::NodeStack, name: &str, tag: &str) -> usize {
    node_stack
        .find(name, tag)
        .expect("entity should exist")
        .read()
        .instances()
        .len()
}

/// Builds a minimal `peppy.json5` document string for a node with the given
/// name, tag, and optional `(name, tag)` dependencies.
fn minimal_node_config(name: &str, tag: &str, deps: &[(&str, &str)]) -> String {
    let depends_on = if deps.is_empty() {
        String::new()
    } else {
        let nodes = deps
            .iter()
            .map(|(n, t)| format!(r#"{{ name: "{n}", tag: "{t}", local_id: "{n}" }}"#))
            .collect::<Vec<_>>()
            .join(", ");
        format!(r#"depends_on: {{ nodes: [{nodes}] }},"#)
    };
    format!(
        r#"{{
            peppy_schema: "node_v1",
            manifest: {{
                name: "{name}",
                tag: "{tag}",
                {depends_on}
            }},
            execution: {{
                language: "rust",
                run_cmd: ["sleep", "10"]
            }}
        }}"#
    )
}

/// Creates a minimal node bundle (peppy.json5 + tar.zst) suitable for HTTP source tests.
/// Returns the temp directory (must be kept alive) and the compressed bundle bytes.
fn create_minimal_http_bundle(node_name: &str, node_tag: &str) -> (TempDir, Vec<u8>) {
    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    let peppy_json5 = format!(
        r#"{{
            peppy_schema: "node_v1",
            manifest: {{
                name: "{node_name}",
                tag: "{node_tag}",
            }},
            interfaces: {{}},
            execution: {{
                language: "rust",
                run_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    let manifest_path = bundle_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&manifest_path, &peppy_json5).expect("failed to write manifest");
    let bundle_path = bundle_dir.path().join("node.tar.zst");
    create_tar_zst_from_dir(bundle_dir.path(), &bundle_path, ".");
    let bundle_bytes = std::fs::read(&bundle_path).expect("failed to read bundle");
    (bundle_dir, bundle_bytes)
}

/// Starts an HTTP mock server serving `bundle_bytes` at `/bundles/node.tar.zst`.
/// Returns the server (must be kept alive) and the parsed URL.
fn serve_bundle_over_http(bundle_bytes: Vec<u8>) -> (Server, url::Url) {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/node.tar.zst"))
            .respond_with(status_code(200).body(bundle_bytes)),
    );
    let url = url::Url::parse(&server.url("/bundles/node.tar.zst").to_string())
        .expect("http bundle url should parse");
    (server, url)
}

/// Returns true if the given tar.zst archive contains an entry matching `entry_name`.
fn archive_contains_entry(archive_path: &Path, entry_name: &str) -> bool {
    let file = std::fs::File::open(archive_path).expect("failed to open archive");
    let decoder = zstd::stream::read::Decoder::new(file).expect("failed to create decoder");
    let mut archive = tar::Archive::new(decoder);
    archive
        .entries()
        .expect("failed to read entries")
        .any(|entry| {
            let entry = entry.expect("failed to read entry");
            let path = entry.path().expect("failed to read entry path");
            let path_str = path.to_string_lossy();
            let normalized = path_str.trim_start_matches("./");
            normalized == entry_name
        })
}

/// Reads a file from a tar.zst archive and returns its contents as a String.
fn read_file_from_archive(archive_path: &Path, entry_name: &str) -> String {
    let file = std::fs::File::open(archive_path).expect("failed to open archive");
    let decoder = zstd::stream::read::Decoder::new(file).expect("failed to create decoder");
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().expect("failed to read entries") {
        let mut entry = entry.expect("failed to read entry");
        let path = entry.path().expect("failed to read entry path");
        let path_str = path.to_string_lossy();
        let normalized = path_str.trim_start_matches("./").to_string();
        if normalized == entry_name {
            let mut contents = String::new();
            entry
                .read_to_string(&mut contents)
                .expect("failed to read entry contents");
            return contents;
        }
    }
    panic!(
        "entry '{}' not found in archive {}",
        entry_name,
        archive_path.display()
    );
}

/// Lists all file entries in a tar.zst archive (normalized paths without leading "./").
fn list_archive_entries(archive_path: &Path) -> Vec<String> {
    let file = std::fs::File::open(archive_path).expect("failed to open archive");
    let decoder = zstd::stream::read::Decoder::new(file).expect("failed to create decoder");
    let mut archive = tar::Archive::new(decoder);
    archive
        .entries()
        .expect("failed to read entries")
        .filter_map(|entry| {
            let entry = entry.expect("failed to read entry");
            if entry.header().entry_type().is_dir() {
                return None;
            }
            let path = entry.path().expect("failed to read path");
            let s = path.to_string_lossy().trim_start_matches("./").to_string();
            if s.is_empty() { None } else { Some(s) }
        })
        .collect()
}

fn create_versioned_nodes_git_repo(to_path: impl AsRef<Path>) -> PathBuf {
    let base_path = to_path.as_ref();
    let repo_path = base_path.join("versioned_peppy_nodes_repo.git");
    std::fs::create_dir_all(&repo_path).expect("failed to create repo directory");

    let repo = Repository::init(&repo_path).expect("failed to init repository");
    let signature =
        Signature::now("Peppy", "peppy@example.com").expect("failed to create signature");

    let uvc_dir = repo_path.join("nodes/uvc_camera");
    std::fs::create_dir_all(&uvc_dir).expect("failed to create uvc directories");

    let rel_config_path = Path::new("nodes/uvc_camera").join(NODE_CONFIG_FILE);

    std::fs::write(
        repo_path.join(&rel_config_path),
        r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
            },
            interfaces: {
                topics: {
                    emits: [{ name: "/example" }]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#,
    )
    .expect("failed to write uvc node v0.1.0");

    let mut index = repo.index().expect("failed to open index");
    index
        .add_path(&rel_config_path)
        .expect("failed to add uvc node");
    index.write().expect("failed to write index");

    let tree_id = index.write_tree().expect("failed to write tree");
    let tree = repo.find_tree(tree_id).expect("failed to find tree");
    let commit_v1 = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "uvc_camera v0.1.0",
            &tree,
            &[],
        )
        .expect("failed to commit v0.1.0");
    let commit_v1 = repo
        .find_commit(commit_v1)
        .expect("failed to find v0.1.0 commit");
    repo.tag("v0.1.0", commit_v1.as_object(), &signature, "v0.1.0", false)
        .expect("failed to create v0.1.0 tag");

    std::fs::write(
        repo_path.join(&rel_config_path),
        r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "uvc_camera",
                tag: "0.2.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "reset_sensor" }
                    ]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#,
    )
    .expect("failed to write uvc node v0.2.0");

    let mut index = repo.index().expect("failed to open index");
    index
        .add_path(&rel_config_path)
        .expect("failed to add uvc node");
    index.write().expect("failed to write index");

    let tree_id = index.write_tree().expect("failed to write tree");
    let tree = repo.find_tree(tree_id).expect("failed to find tree");
    let commit_v2 = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "uvc_camera v0.2.0",
            &tree,
            &[&commit_v1],
        )
        .expect("failed to commit v0.2.0");
    let commit_v2 = repo
        .find_commit(commit_v2)
        .expect("failed to find v0.2.0 commit");
    repo.tag("v0.2.0", commit_v2.as_object(), &signature, "v0.2.0", false)
        .expect("failed to create v0.2.0 tag");

    repo_path
}

/// Creates a git repository containing a single invalid `peppy.json5`
/// (missing required `execution` field and no default variant).
fn create_git_repo_with_invalid_config(base_path: &Path) -> PathBuf {
    let repo_path = base_path.join("invalid_config_repo.git");
    std::fs::create_dir_all(&repo_path).expect("create repo dir");

    let repo = Repository::init(&repo_path).expect("init repo");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("create signature");

    let config_rel = Path::new("nodes/bad_node/peppy.json5");
    std::fs::create_dir_all(repo_path.join("nodes/bad_node")).expect("create node dir");
    // Invalid config: has manifest but no execution and no default variant.
    std::fs::write(
        repo_path.join(config_rel),
        r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "bad_node",
                tag: "0.1.0",
            },
            interfaces: {},
        }"#,
    )
    .expect("write invalid config");

    let mut index = repo.index().expect("open index");
    index.add_path(config_rel).expect("add config");
    index.write().expect("write index");

    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "initial commit",
        &tree,
        &[],
    )
    .expect("commit");

    repo_path
}

#[path = "listen_for_node_add/basic_sources.rs"]
mod basic_sources;

#[path = "listen_for_node_add/repo_sources.rs"]
mod repo_sources;

#[path = "listen_for_node_add/failures.rs"]
mod failures;

#[path = "listen_for_node_add/replacement_and_lifecycle.rs"]
mod replacement_and_lifecycle;

#[path = "listen_for_node_add/variants.rs"]
mod variants;

#[path = "listen_for_node_add/concurrency_and_force.rs"]
mod concurrency_and_force;
