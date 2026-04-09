mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, NodeAddSource, TEST_GIT_HASH, create_tar_zst_from_dir,
    send_node_add_and_wait, send_node_add_and_wait_with_env, send_node_add_and_wait_with_force,
    send_node_add_and_wait_with_variant, spawn_real_running_instance, spawn_real_stuck_instance,
    start_core_node_with_mock_messenger, write_peppy_json5,
};
use config::consts::{
    DEFAULT_ALPINE_BASE_IMAGE, NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH,
};
use config::node::QoSProfile;
use config::test_helpers;
use core_node::encoding::{NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse};
use core_node::names;
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

const ADD_CMD_MARKER_FILE: &str = "add_cmd_executed.marker";
const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_TIMEOUT: Duration = Duration::from_secs(120);
const CONTAINER_RESULT_TIMEOUT: Duration = Duration::from_secs(300);

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

/// Creates a minimal node bundle (peppy.json5 + tar.zst) suitable for HTTP source tests.
/// Returns the temp directory (must be kept alive) and the compressed bundle bytes.
fn create_minimal_http_bundle(node_name: &str, node_tag: &str) -> (TempDir, Vec<u8>) {
    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{node_name}",
                tag: "{node_tag}",
            }},
            interfaces: {{}},
            execution: {{
                language: "rust",
                start_cmd: ["sleep", "10"]
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
            schema_version: 1,
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
                start_cmd: ["sleep", "10"]
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
            schema_version: 1,
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
                start_cmd: ["sleep", "10"]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_fs_add_success() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    // `add` only adds the node to the NodeStack but doesn't spawn any instance
    assert_eq!(
        entity_instance_count(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG),
        0
    );

    // Verify the node was archived to the peppy storage directory
    let snapshot_path = add_result.snapshot_path.as_path();
    let root_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(
        snapshot_path, root_path,
        "snapshot_path should match archive path"
    );
    assert!(
        root_path != source_dir.path(),
        "node should be archived to a different location, got: {}",
        root_path.display()
    );
    assert!(
        root_path.exists(),
        "node archive should exist: {}",
        root_path.display()
    );
    assert!(
        archive_contains_entry(&root_path, NODE_CONFIG_FILE),
        "config file should be present in the archive"
    );

    // Verify the path follows the expected naming convention: <node_name>_<tag>.tar.zst
    let file_name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have file name");
    assert_eq!(
        file_name,
        format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}.tar.zst"),
        "archive file name should be '<node_name>_<tag>.tar.zst', got: {}",
        file_name
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_with_container_success() {
    const TARGET_NODE_NAME: &str = "container_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "TARGET_NODE_NAME",
            tag: "TARGET_NODE_TAG",
        },
        // Using `container` let `peppy` manage the node internally
        execution: {
            language: "rust",
            container: {
                def_file: "apptainer.def",
            }
        }
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);
    let apptainer_def = r#"
Bootstrap: docker
From: {DEFAULT_ALPINE_BASE_IMAGE}

%labels
    Name {TARGET_NODE_NAME}
    Version {TARGET_NODE_TAG}

%runscript
    echo "Running {TARGET_NODE_NAME}:{TARGET_NODE_TAG}"
"#
    .replace("{DEFAULT_ALPINE_BASE_IMAGE}", DEFAULT_ALPINE_BASE_IMAGE)
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    std::fs::write(source_dir.path().join("apptainer.def"), &apptainer_def)
        .expect("failed to write apptainer definition");

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        CONTAINER_RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    // `add` only adds the node to the NodeStack but doesn't spawn any instance
    assert_eq!(
        entity_instance_count(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG),
        0
    );

    // Verify the .sif image was stored in the peppy storage directory
    let snapshot_path = add_result.snapshot_path.as_path();
    let root_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(
        snapshot_path, root_path,
        "snapshot_path should match root_path"
    );
    assert!(
        root_path != source_dir.path(),
        "node should be stored in a different location than the source, got: {}",
        root_path.display()
    );
    assert!(
        root_path.exists(),
        "node .sif image should exist: {}",
        root_path.display()
    );

    // Container nodes store the .sif image directly, not wrapped in a tar.zst archive
    let file_name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have file name");
    assert_eq!(
        file_name,
        format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}.sif"),
        "stored image should be '<node_name>_<tag>.sif', got: {}",
        file_name
    );

    // Verify the log file path is returned and points to the correct directory
    assert!(
        !add_result.log_path.as_os_str().is_empty(),
        "log_path should not be empty"
    );
    assert!(
        add_result.log_path.exists(),
        "log file should exist at {:?}",
        add_result.log_path
    );

    let log_dir = started_core_node.peppy_dirs.logs_dir_add();
    assert!(
        add_result.log_path.starts_with(&log_dir),
        "log file should be in logs_dir_add(), expected to start with {:?}, got {:?}",
        log_dir,
        add_result.log_path
    );

    // Verify the log file name follows the expected pattern: <node_name>_<tag>_<timestamp>.log
    let log_filename = add_result
        .log_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have log filename");
    assert!(
        log_filename.starts_with(&format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}_")),
        "log filename should start with '<node_name>_<tag>_', got: {}",
        log_filename
    );
    assert!(
        log_filename.ends_with(".log"),
        "log filename should end with '.log', got: {}",
        log_filename
    );

    // Verify that container build output was streamed to the log file.
    // Apptainer build always writes status messages to stderr/stdout which our
    // streaming infrastructure captures with [stdout]/[stderr] prefixes.
    let log_content =
        std::fs::read_to_string(&add_result.log_path).expect("should be able to read log file");
    assert!(
        log_content.contains("[stdout]") || log_content.contains("[stderr]"),
        "log file should contain streamed build output with [stdout]/[stderr] prefixes, got:\n{}",
        log_content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_success() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = test_helpers::create_nodes_git_repo(&git_repo_temp_dir);

    const TARGET_NODE_NAME: &str = "uvc_camera";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_REPO_PATH: &str = "nodes/uvc_camera";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: TARGET_REPO_PATH,
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    assert_eq!(
        entity_instance_count(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG),
        0
    );

    let snapshot_path = add_result.snapshot_path.as_path();
    let root_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(snapshot_path, root_path);
    assert!(root_path.exists(), "archive should exist");
    assert!(
        archive_contains_entry(&root_path, NODE_CONFIG_FILE),
        "config file should exist in archive"
    );

    // Verify the path follows the expected naming convention: <node_name>_<tag>.tar.zst
    let file_name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have file name");
    assert_eq!(
        file_name,
        format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}.tar.zst"),
        "archive file name should be '<node_name>_<tag>.tar.zst', got: {}",
        file_name
    );

    // Verify the log file was renamed to the canonical <node_name>_<tag>_<timestamp>.log format
    let log_dir = started_core_node.peppy_dirs.logs_dir_add();
    assert!(
        add_result.log_path.starts_with(&log_dir),
        "log file should be in logs_dir_add(), expected to start with {:?}, got {:?}",
        log_dir,
        add_result.log_path
    );
    let log_filename = add_result
        .log_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have log filename");
    assert!(
        log_filename.starts_with(&format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}_")),
        "log filename should start with '<node_name>_<tag>_', got: {}",
        log_filename
    );
    assert!(
        log_filename.ends_with(".log"),
        "log filename should end with '.log', got: {}",
        log_filename
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_with_ref_success() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = create_versioned_nodes_git_repo(&git_repo_temp_dir);

    const TARGET_NODE_NAME: &str = "uvc_camera";
    const TARGET_REPO_PATH: &str = "nodes/uvc_camera";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let add_result_head = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url: repo_url.clone(),
            repo_path: TARGET_REPO_PATH,
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result_head.success,
        "node_add should succeed, got error: {:?}",
        add_result_head.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, "0.2.0"));

    let add_result_ref = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: TARGET_REPO_PATH,
            repo_ref: Some("v0.1.0"),
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result_ref.success,
        "node_add should succeed, got error: {:?}",
        add_result_ref.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, "0.1.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_http_add_success() {
    const TARGET_NODE_NAME: &str = "http_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "new_service" }
                    ]
                }
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);

    let manifest_path = bundle_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&manifest_path, &peppy_json5).expect("failed to write manifest");

    let test_file_content = "hello from http";
    let test_file_path = bundle_dir.path().join("test_file.txt");
    std::fs::write(&test_file_path, test_file_content).expect("failed to write test file");

    let mut tar_data = Vec::new();
    {
        let mut tar_builder = tar::Builder::new(&mut tar_data);
        tar_builder
            .append_path_with_name(&manifest_path, NODE_CONFIG_FILE)
            .expect("failed to append manifest to tar");
        tar_builder
            .append_path_with_name(&test_file_path, "test_file.txt")
            .expect("failed to append test file to tar");
        tar_builder.finish().expect("failed to finish tar");
    }

    let bundle_path = bundle_dir.path().join("http_node.tar.zst");
    let bundle_file = std::fs::File::create(&bundle_path).expect("failed to create bundle file");
    let mut encoder = zstd::Encoder::new(bundle_file, 0).expect("failed to create zstd encoder");
    encoder
        .write_all(&tar_data)
        .expect("failed to write compressed bundle");
    encoder.finish().expect("failed to finish encoder");
    let bundle_bytes = std::fs::read(&bundle_path).expect("failed to read bundle");

    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/http_node.tar.zst"))
            .respond_with(status_code(200).body(bundle_bytes)),
    );
    let url = url::Url::parse(&server.url("/bundles/http_node.tar.zst").to_string())
        .expect("http bundle url should parse");

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Http { url, sha256: None },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    assert_eq!(
        entity_instance_count(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG),
        0
    );

    let snapshot_path = add_result.snapshot_path.as_path();
    let root_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(snapshot_path, root_path);
    assert!(root_path.exists(), "archive should exist");
    assert!(
        archive_contains_entry(&root_path, NODE_CONFIG_FILE),
        "config file should exist in archive"
    );

    assert!(
        archive_contains_entry(&root_path, "test_file.txt"),
        "test_file.txt should be in the archive"
    );
    let copied_content = read_file_from_archive(&root_path, "test_file.txt");
    assert_eq!(
        copied_content, test_file_content,
        "file content should match"
    );

    // Verify the path follows the expected naming convention: <node_name>_<tag>.tar.zst
    let file_name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have file name");
    assert_eq!(
        file_name,
        format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}.tar.zst"),
        "archive file name should be '<node_name>_<tag>.tar.zst', got: {}",
        file_name
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_http_add_rejects_wrong_sha256() {
    const TARGET_NODE_NAME: &str = "http_sha_bad";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let (_bundle_dir, bundle_bytes) = create_minimal_http_bundle(TARGET_NODE_NAME, TARGET_NODE_TAG);
    let (_server, url) = serve_bundle_over_http(bundle_bytes);

    let wrong_sha256 = "a".repeat(64);
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Http {
            url,
            sha256: Some(wrong_sha256),
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should fail with wrong sha256"
    );
    assert!(
        add_result
            .error_message
            .as_deref()
            .map(|msg| msg.contains("checksum mismatch"))
            .unwrap_or(false),
        "error should mention checksum mismatch, got: {:?}",
        add_result.error_message
    );
    assert!(!node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_http_add_accepts_correct_sha256() {
    const TARGET_NODE_NAME: &str = "http_sha_ok";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let (_bundle_dir, bundle_bytes) = create_minimal_http_bundle(TARGET_NODE_NAME, TARGET_NODE_TAG);

    use sha2::{Digest, Sha256};
    let correct_sha256: String = Sha256::digest(&bundle_bytes)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();

    let (_server, url) = serve_bundle_over_http(bundle_bytes);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Http {
            url,
            sha256: Some(correct_sha256),
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed with correct sha256, got error: {:?}",
        add_result.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_config_found() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            interfaces: {
                topics: {
                    emits: [{ name: "/example" }]
                }
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    std::fs::remove_file(source_dir.path().join(NODE_CONFIG_FILE))
        .expect("failed to remove peppy.json5 config file");

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should not succeed, the config file is missing",
    );

    assert!(!node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 1, "root");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_git_hash_mismatch_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "git_hash_mismatch_node",
            tag: "0.1.0",
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let peppy_dir = source_dir.path().join(config::consts::PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir).expect("failed to create .peppy dir");
    std::fs::write(peppy_dir.join("git.hash"), "wrong-hash\n")
        .expect("failed to write wrong git hash file");

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when git hash mismatches"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("git hash mismatch"))
            .unwrap_or(false),
        "error message should indicate git hash mismatch, got: {:?}",
        add_result.error_message
    );
    assert!(!node_stack.contains("git_hash_mismatch_node", "0.1.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_invalid_config_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{ manifest: [unclosed"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail for invalid json5"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Failed to parse node config"))
            .unwrap_or(false),
        "error message should indicate parse failure, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_start_cmd_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "no_start_cmd_node",
            tag: "0.1.0",
        },
        execution: {
            language: "rust",
        },
    }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when start_cmd is missing"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("process"))
            .unwrap_or(false),
        "error message should mention process, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_dependency_not_resolved() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Try to add a consumer node that depends on a non-existent provider
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "consumer_node",
            tag: "1.0.0",
            depends_on: {
                nodes: [
                    { name: "non_existent_node", tag: "1.0.0", local_id: "non_existent_node" }
                ]
            },
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"],
        },
    }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when dependencies are missing"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Failed to add node"))
            .unwrap_or(false),
        "error message should indicate add failure, got: {:?}",
        add_result.error_message
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("does not exist in the stack"))
            .unwrap_or(false),
        "error message should indicate missing dependency, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_same_tags_overwrites_when_no_dependents() {
    const NODE_NAME: &str = "overwrite_node";
    const NODE_TAG: &str = "1.0.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");

    // First add: no interfaces
    let peppy_json5_v1 = r#"{
            schema_version: 1,
            manifest: {
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
            },
            interfaces: {
                topics: {
                    emits: [{ name: "wrong_topic_name" }]
                }
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{NODE_NAME}", NODE_NAME)
    .replace("{NODE_TAG}", NODE_TAG);
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add v1 should complete");

    assert!(
        add_v1.success,
        "node_add v1 should succeed, got error: {:?}",
        add_v1.error_message
    );

    assert_eq!(node_stack.len(), 2, "root + v1");
    assert_eq!(entity_instance_count(&node_stack, NODE_NAME, NODE_TAG), 0);
    let copied_path_v1 = entity_artifact_path(&node_stack, NODE_NAME, NODE_TAG);

    // Second add: same name+tag but different interfaces -> should overwrite.
    let peppy_json5_v2 = r#"{
            schema_version: 1,
            manifest: {
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
            },
            interfaces: {
                topics: {
                    emits: [{ name: "/example" }]
                }
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{NODE_NAME}", NODE_NAME)
    .replace("{NODE_TAG}", NODE_TAG);
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    let add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add v2 should complete");

    assert!(
        add_v2.success,
        "node_add should overwrite when there are no dependents, got error: {:?}",
        add_v2.error_message
    );

    assert_eq!(node_stack.len(), 2, "stack should be unchanged");
    let entity = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should exist after v2 overwrite");
    let entity_guard = entity.read();
    assert_eq!(
        entity_guard.instances().len(),
        0,
        "should not have any instances"
    );
    let entity_root = entity_guard
        .artifact_path()
        .expect("entity should be built")
        .to_path_buf();
    assert_eq!(
        entity_root.as_path(),
        add_v2.snapshot_path.as_path(),
        "node stack should point to the new snapshot path"
    );
    // With deterministic archive naming, v1 and v2 produce the same path.
    assert_eq!(
        entity_root.as_path(),
        copied_path_v1.as_path(),
        "deterministic archive path should be the same for both adds"
    );
    assert!(entity_root.exists(), "archive should exist after overwrite");
    assert!(
        entity_guard
            .config()
            .interfaces
            .topics
            .as_ref()
            .and_then(|t| t.emits.as_ref())
            .is_some_and(|topics| topics.iter().any(|topic| topic.name == "/example")),
        "node should have updated interfaces from the overwritten config"
    );
    drop(entity_guard);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_same_tags_fails_when_node_has_dependents() {
    const DEPENDENCY_NODE_NAME: &str = "lidar";
    const DEPENDENCY_NODE_TAG: &str = "1.0.0";
    const DEPENDENT_NODE_NAME: &str = "brain";
    const DEPENDENT_NODE_TAG: &str = "1.0.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let dependency_source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependency_source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependent_source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let dependency_peppy_json5_v1 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
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
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v1.path(), &dependency_peppy_json5_v1);

    let dependency_add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add v1 should complete");
    assert!(
        dependency_add_v1.success,
        "dependency node_add v1 should succeed, got error: {:?}",
        dependency_add_v1.error_message
    );

    let dependent_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENT_NODE_NAME}",
                tag: "{DEPENDENT_NODE_TAG}",
                depends_on: {
                    nodes: [
                        { name: "{DEPENDENCY_NODE_NAME}", tag: "{DEPENDENCY_NODE_TAG}", local_id: "{DEPENDENCY_NODE_NAME}" }
                    ]
                },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          local_node_id: "{DEPENDENCY_NODE_NAME}",
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENT_NODE_NAME}", DEPENDENT_NODE_NAME)
    .replace("{DEPENDENT_NODE_TAG}", DEPENDENT_NODE_TAG)
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependent_source_dir.path(), &dependent_peppy_json5);

    let dependent_add = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependent_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependent node_add should complete");
    assert!(
        dependent_add.success,
        "dependent node_add should succeed, got error: {:?}",
        dependent_add.error_message
    );

    assert_eq!(node_stack.len(), 3, "root + dependency + dependent");
    let dependency_snapshot_path =
        entity_artifact_path(&node_stack, DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG);

    // Overwrite attempt: same name+tag but different interfaces should fail due to dependent nodes.
    let dependency_peppy_json5_v2 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "new_service" }
                    ]
                }
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v2.path(), &dependency_peppy_json5_v2);

    let dependency_add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add v2 should complete");

    assert!(
        !dependency_add_v2.success,
        "overwriting an existing node should fail when it has dependents"
    );
    assert!(
        dependency_add_v2
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Cannot overwrite node"))
            .unwrap_or(false),
        "error message should indicate overwrite is not allowed, got: {:?}",
        dependency_add_v2.error_message
    );

    assert_eq!(node_stack.len(), 3, "stack should be unchanged");
    assert_eq!(
        entity_artifact_path(&node_stack, DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG).as_path(),
        dependency_snapshot_path.as_path(),
        "dependency should still point to the original snapshot path"
    );
    assert!(
        node_stack.contains(DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG),
        "dependent node should still exist after failed overwrite"
    );

    // Path equality alone isn't enough — assert the live entity config still
    // exposes the v1-only interface (`reset_sensor`) and does NOT expose the
    // v2-only interface (`new_service`). This proves the failed overwrite
    // truly preserved the original revision rather than just the path.
    {
        let handle = node_stack
            .find(DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG)
            .expect("dependency entity should exist");
        let guard = handle.read();
        let services = guard
            .config()
            .interfaces
            .services
            .as_ref()
            .expect("services section should be present");
        let exposed: Vec<&str> = services
            .exposes
            .as_ref()
            .map(|v| v.iter().map(|s| s.name.as_str()).collect())
            .unwrap_or_default();
        assert!(
            exposed.contains(&"reset_sensor"),
            "v1-only service `reset_sensor` should still be exposed; got {:?}",
            exposed
        );
        assert!(
            !exposed.contains(&"new_service"),
            "v2-only service `new_service` should NOT be exposed; got {:?}",
            exposed
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_different_tags_create_two_entities() {
    const NODE_NAME: &str = "versioned_node";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5_v1 = r#"{
            schema_version: 1,
            manifest: {
                name: "{NODE_NAME}",
                tag: "1.0.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "new_service" }
                    ]
                }
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{NODE_NAME}", NODE_NAME);
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add v1 should complete");

    assert!(
        add_v1.success,
        "node_add v1 should succeed, got error: {:?}",
        add_v1.error_message
    );

    let peppy_json5_v2 = r#"{
            schema_version: 1,
            manifest: {
                name: "{NODE_NAME}",
                tag: "2.0.0",
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
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{NODE_NAME}", NODE_NAME);
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    let add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add v2 should complete");

    assert!(
        add_v2.success,
        "node_add v2 should succeed, got error: {:?}",
        add_v2.error_message
    );

    assert_eq!(node_stack.len(), 3, "root + two versions");
    assert!(node_stack.contains(NODE_NAME, "1.0.0"));
    assert!(node_stack.contains(NODE_NAME, "2.0.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_copies_files_to_storage() {
    const TARGET_NODE_NAME: &str = "copy_test_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create a temporary source directory with some files
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let test_file_content = "test file content";
    std::fs::write(source_dir.path().join("test_file.txt"), test_file_content)
        .expect("failed to write test file");

    // Create a subdirectory with a file
    let sub_dir = source_dir.path().join("subdir");
    std::fs::create_dir(&sub_dir).expect("failed to create subdir");
    std::fs::write(sub_dir.join("nested_file.txt"), "nested content")
        .expect("failed to write nested file");

    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let archive_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(
        add_result.snapshot_path.as_path(),
        archive_path.as_path(),
        "snapshot_path should match archive path"
    );

    // Verify the file was archived
    assert!(
        archive_contains_entry(&archive_path, "test_file.txt"),
        "test_file.txt should be in the archive"
    );
    let content = read_file_from_archive(&archive_path, "test_file.txt");
    assert_eq!(content, test_file_content, "file content should match");

    // Verify the subdirectory and nested file were archived
    assert!(
        archive_contains_entry(&archive_path, "subdir/nested_file.txt"),
        "nested file should be in the archive"
    );
    let nested_content = read_file_from_archive(&archive_path, "subdir/nested_file.txt");
    assert_eq!(
        nested_content, "nested content",
        "nested content should match"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_runs_add_cmd() {
    const TARGET_NODE_NAME: &str = "add_cmd_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // add_cmd creates a marker file to prove it was executed
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                add_cmd: ["touch", "{ADD_CMD_MARKER_FILE}"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG)
    .replace("{ADD_CMD_MARKER_FILE}", ADD_CMD_MARKER_FILE);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let archive_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);

    // Verify that add_cmd was executed in the working directory (not the source)
    // by checking the marker file exists in the archive
    assert!(
        archive_contains_entry(&archive_path, ADD_CMD_MARKER_FILE),
        "add_cmd should have created marker file in the archive"
    );

    // Verify add_cmd did NOT run on the source directory
    let source_marker = source_dir.path().join(ADD_CMD_MARKER_FILE);
    assert!(
        !source_marker.exists(),
        "add_cmd should NOT have created marker file in source dir at {}",
        source_marker.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_cmd_failure_fails_add() {
    const TARGET_NODE_NAME: &str = "add_cmd_fail_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // add_cmd that will fail (non-existent command)
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                add_cmd: ["this_command_does_not_exist_12345"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when add_cmd fails"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("add_cmd failed"))
            .unwrap_or(false),
        "error message should mention add_cmd failure, got: {:?}",
        add_result.error_message
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("this_command_does_not_exist_12345"))
            .unwrap_or(false),
        "error message should include the command that failed, got: {:?}",
        add_result.error_message
    );

    // Node should not be in the stack
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when add_cmd fails"
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_cmd_nonzero_exit_fails_add() {
    const TARGET_NODE_NAME: &str = "add_cmd_exit_fail_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // add_cmd that exits with non-zero status
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                add_cmd: ["sh", "-c", "exit 1"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when add_cmd exits with non-zero status"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("add_cmd failed"))
            .unwrap_or(false),
        "error message should mention add_cmd failure, got: {:?}",
        add_result.error_message
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("exit 1"))
            .unwrap_or(false),
        "error message should include the command that failed, got: {:?}",
        add_result.error_message
    );

    // Node should not be in the stack
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when add_cmd fails"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_streams_stdout_and_stderr() {
    const TARGET_NODE_NAME: &str = "stream_output_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const STDOUT_MARKER: &str = "peppy_add_stdout_marker";
    const STDERR_MARKER: &str = "peppy_add_stderr_marker";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                add_cmd: ["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG)
    .replace("{STDOUT_MARKER}", STDOUT_MARKER)
    .replace("{STDERR_MARKER}", STDERR_MARKER);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    // Use wildcard caller IDs so mock pub/sub can match feedback topics with "*" segments.
    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Some(feedback_tx),
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }
    let saw_stdout = feedback
        .iter()
        .any(|entry| entry.is_stdout() && entry.line.trim() == STDOUT_MARKER);
    let saw_stderr = feedback
        .iter()
        .any(|entry| entry.is_stderr() && entry.line.trim() == STDERR_MARKER);

    assert!(saw_stdout, "stdout feedback should include marker");
    assert!(saw_stderr, "stderr feedback should include marker");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_fingerprint_mismatch() {
    const TARGET_NODE_NAME: &str = "fingerprint_mismatch_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    // Write the config file only (without fingerprint)
    let config_path = source_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&config_path, &peppy_json5).expect("failed to write peppy.json5");

    // Create a wrong fingerprint that won't match the actual peppy.json5 content
    config::fingerprint::create_wrong_codegen_fingerprint(
        &config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when fingerprint mismatches"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Codegen fingerprint verification failed"))
            .unwrap_or(false),
        "error message should indicate fingerprint verification failure, got: {:?}",
        add_result.error_message
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("fingerprint mismatch"))
            .unwrap_or(false),
        "error message should mention fingerprint mismatch, got: {:?}",
        add_result.error_message
    );

    // Node should not be in the stack
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when fingerprint mismatches"
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_writes_log_file() {
    const TARGET_NODE_NAME: &str = "log_file_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const STDOUT_MARKER: &str = "peppy_logfile_stdout_marker";
    const STDERR_MARKER: &str = "peppy_logfile_stderr_marker";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                add_cmd: ["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG)
    .replace("{STDOUT_MARKER}", STDOUT_MARKER)
    .replace("{STDERR_MARKER}", STDERR_MARKER);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    // Verify the log file path is returned
    assert!(
        !add_result.log_path.as_os_str().is_empty(),
        "log_path should not be empty"
    );

    // Verify the log file exists
    assert!(
        add_result.log_path.exists(),
        "log file should exist at {:?}",
        add_result.log_path
    );

    // Verify the log file is in the expected directory
    let log_dir = started_core_node.peppy_dirs.logs_dir_add();
    assert!(
        add_result.log_path.starts_with(&log_dir),
        "log file should be in logs_dir_add(), expected to start with {:?}, got {:?}",
        log_dir,
        add_result.log_path
    );

    // Verify the log file name follows the expected pattern: <node_name>_<tag>_<timestamp>.log
    let log_filename = add_result
        .log_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have log filename");
    assert!(
        log_filename.starts_with(&format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}_")),
        "log filename should start with '<node_name>_<tag>_', got: {}",
        log_filename
    );
    assert!(
        log_filename.ends_with(".log"),
        "log filename should end with '.log', got: {}",
        log_filename
    );

    // Verify the log file contains expected content
    let log_content =
        std::fs::read_to_string(&add_result.log_path).expect("should be able to read log file");

    // Check that stdout marker is present with correct prefix
    assert!(
        log_content.contains(&format!("[stdout] {}", STDOUT_MARKER)),
        "log file should contain stdout marker with [stdout] prefix, got:\n{}",
        log_content
    );

    // Check that stderr marker is present with correct prefix
    assert!(
        log_content.contains(&format!("[stderr] {}", STDERR_MARKER)),
        "log file should contain stderr marker with [stderr] prefix, got:\n{}",
        log_content
    );
}

/// Tests that a new goal can be processed after a previous action was abandoned
/// (goal accepted but result never polled).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_abandoned_action_does_not_block_next_goal() {
    const FIRST_NODE_NAME: &str = "abandoned_node";
    const FIRST_NODE_TAG: &str = "0.1.0";
    const SECOND_NODE_NAME: &str = "second_node";
    const SECOND_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create first node source directory
    let first_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let first_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{FIRST_NODE_NAME}",
                tag: "{FIRST_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{FIRST_NODE_NAME}", FIRST_NODE_NAME)
    .replace("{FIRST_NODE_TAG}", FIRST_NODE_TAG);
    write_peppy_json5(first_source_dir.path(), &first_peppy_json5);

    // Write git hash file for first node
    let first_peppy_dir = first_source_dir.path().join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&first_peppy_dir).expect("failed to create .peppy dir");
    std::fs::write(first_peppy_dir.join("git.hash"), TEST_GIT_HASH)
        .expect("failed to write git hash");

    // Send first goal but DON'T wait for result (simulating abandoned action)
    let first_goal = NodeAddGoal::new(
        first_source_dir.path(),
        TEST_GIT_HASH,
        RESULT_TIMEOUT.as_secs(),
    );
    let first_goal_payload = first_goal.encode().expect("failed to encode goal");

    let first_action_handle = ActionMessenger::send_goal(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        names::NODE_ADD_ACTION,
        Some(&started_core_node.core_node_name),
        None,
        first_goal_payload,
        QoSProfile::default(),
        GOAL_TIMEOUT,
    )
    .await
    .expect("first goal should be sent");

    // Verify first goal was accepted
    let first_goal_response_payload = first_action_handle.goal_response().payload();
    let first_goal_response = NodeAddGoalResponse::decode(&first_goal_response_payload)
        .expect("failed to decode first goal response");
    assert!(
        first_goal_response.accepted,
        "first goal should be accepted"
    );

    // `node_add` only advances the entity to `Added` now — wait for that,
    // not `Ready`. Build is a separate goal which we deliberately do NOT
    // send for the first node (the whole point of this test is that the
    // first action's result is never requested and the second goal is
    // still processed correctly).
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(handle) = node_stack.find(FIRST_NODE_NAME, FIRST_NODE_TAG) {
                let guard = handle.read();
                if matches!(guard.stage(), node_stack::NodeStage::Added { .. }) {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("first node never reached Added within 30s");
    drop(first_action_handle);

    // Now send second goal - this should succeed even though we never polled
    // for the first action's result
    let second_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let second_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{SECOND_NODE_NAME}",
                tag: "{SECOND_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{SECOND_NODE_NAME}", SECOND_NODE_NAME)
    .replace("{SECOND_NODE_TAG}", SECOND_NODE_TAG);
    write_peppy_json5(second_source_dir.path(), &second_peppy_json5);

    let second_add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        second_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("second node_add request should complete");

    assert!(
        second_add_result.success,
        "second node_add should succeed even after first action was abandoned, got error: {:?}",
        second_add_result.error_message
    );

    // Verify both nodes are in the stack
    assert!(
        node_stack.contains(FIRST_NODE_NAME, FIRST_NODE_TAG),
        "first node should be in stack (action completed even though result wasn't polled)"
    );
    assert!(
        node_stack.contains(SECOND_NODE_NAME, SECOND_NODE_TAG),
        "second node should be in stack"
    );
    assert_eq!(node_stack.len(), 3, "root + first + second nodes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_same_node_shutdown_existing_instances() {
    use peppylib::messaging::{MessengerHandle, SHUTDOWN_SERVICE, ServiceMessenger};
    use std::sync::Arc;
    use tokio::sync::{Mutex, Notify, oneshot};

    const NODE_NAME: &str = "readd_node";
    const NODE_TAG: &str = "0.1.0";
    const INSTANCE_1: &str = "readd_instance_1";
    const INSTANCE_2: &str = "readd_instance_2";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5_v1 = r#"{
            schema_version: 1,
            manifest: {
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{NODE_NAME}", NODE_NAME)
    .replace("{NODE_TAG}", NODE_TAG);
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add v1 should complete");
    assert!(
        add_v1.success,
        "node_add v1 should succeed, got error: {:?}",
        add_v1.error_message
    );

    let snapshot_v1 = entity_artifact_path(&node_stack, NODE_NAME, NODE_TAG);
    // Capture the v1 archive bytes so we can later detect whether the v2
    // overwrite actually mutated the on-disk artifact mid-overwrite. With
    // deterministic archive naming v1 and v2 share the same path; only the
    // *bytes* can prove the overwrite ordering invariant.
    let snapshot_v1_bytes =
        std::fs::read(&snapshot_v1).expect("v1 archive should be readable on disk");

    let instance_id_1 = config::node::Name::new(INSTANCE_1).expect("valid instance id 1");
    let instance_id_2 = config::node::Name::new(INSTANCE_2).expect("valid instance id 2");
    let _running_1 =
        spawn_real_running_instance(&started_core_node, NODE_NAME, NODE_TAG, &instance_id_1).await;
    let _running_2 =
        spawn_real_running_instance(&started_core_node, NODE_NAME, NODE_TAG, &instance_id_2).await;

    let instance_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));

    let (called_tx_1, called_rx_1) = oneshot::channel::<()>();
    let called_tx_1 = Arc::new(Mutex::new(Some(called_tx_1)));
    let allow_shutdown_1 = Arc::new(Notify::new());
    let allow_shutdown_1_clone = Arc::clone(&allow_shutdown_1);
    let mut shutdown_endpoint_1 = ServiceMessenger::listen(
        &instance_messenger,
        &started_core_node.core_node_name,
        INSTANCE_1,
        NODE_NAME,
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service for instance 1");
    let _shutdown_task_1 = AbortOnDrop(peppylib::runtime::spawn({
        let called_tx_1 = Arc::clone(&called_tx_1);
        async move {
            shutdown_endpoint_1
                .handle_requests(move |context| {
                    let called_tx_1 = Arc::clone(&called_tx_1);
                    let allow_shutdown_1_clone = Arc::clone(&allow_shutdown_1_clone);
                    async move {
                        let payload = context.message().payload();
                        if let Some(tx) = called_tx_1.lock().await.take() {
                            let _ = tx.send(());
                        }
                        allow_shutdown_1_clone.notified().await;
                        Ok(payload)
                    }
                })
                .await
        }
    }));

    let (called_tx_2, called_rx_2) = oneshot::channel::<()>();
    let called_tx_2 = Arc::new(Mutex::new(Some(called_tx_2)));
    let allow_shutdown_2 = Arc::new(Notify::new());
    let allow_shutdown_2_clone = Arc::clone(&allow_shutdown_2);
    let mut shutdown_endpoint_2 = ServiceMessenger::listen(
        &instance_messenger,
        &started_core_node.core_node_name,
        INSTANCE_2,
        NODE_NAME,
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service for instance 2");
    let _shutdown_task_2 = AbortOnDrop(peppylib::runtime::spawn({
        let called_tx_2 = Arc::clone(&called_tx_2);
        async move {
            shutdown_endpoint_2
                .handle_requests(move |context| {
                    let called_tx_2 = Arc::clone(&called_tx_2);
                    let allow_shutdown_2_clone = Arc::clone(&allow_shutdown_2_clone);
                    async move {
                        let payload = context.message().payload();
                        if let Some(tx) = called_tx_2.lock().await.take() {
                            let _ = tx.send(());
                        }
                        allow_shutdown_2_clone.notified().await;
                        Ok(payload)
                    }
                })
                .await
        }
    }));

    // Ensure shutdown services are fully registered before starting the overwrite.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5_v2 = r#"{
            schema_version: 1,
            manifest: {
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{NODE_NAME}", NODE_NAME)
    .replace("{NODE_TAG}", NODE_TAG);
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    // Drop a v2-only marker into the source directory so the rebuilt
    // archive bytes diverge from v1. Without this the deterministic
    // archive naming would produce identical artifact bytes and the
    // mid-overwrite assertions below couldn't distinguish "still v1" from
    // "already v2".
    std::fs::write(
        source_dir_v2.path().join("v2_marker.txt"),
        b"v2-only payload",
    )
    .expect("write v2 marker");

    // Use wildcard caller IDs so mock pub/sub can match feedback topics with "*" segments.
    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();

    let caller_handle = started_core_node.caller_handle.clone();
    let core_node_name = started_core_node.core_node_name.clone();
    let source_path_v2 = source_dir_v2.path().to_path_buf();
    let add_task = tokio::spawn(async move {
        send_node_add_and_wait(
            &caller_handle,
            &core_node_name,
            &source_path_v2,
            GOAL_TIMEOUT,
            RESULT_TIMEOUT,
            Some(feedback_tx),
        )
        .await
    });

    // Wait for the first shutdown request and ensure the stack is not overwritten yet.
    tokio::time::timeout(Duration::from_secs(5), called_rx_1)
        .await
        .expect("shutdown request for instance 1 should arrive within timeout")
        .expect("shutdown channel for instance 1 should not be dropped");
    assert_eq!(
        entity_artifact_path(&node_stack, NODE_NAME, NODE_TAG).as_path(),
        snapshot_v1.as_path(),
        "node should not be overwritten before instance 1 is shutdown"
    );
    // Path equality is not enough — verify the on-disk archive bytes still
    // match v1 (so we can be sure the v2 build hasn't yet rewritten them).
    assert_eq!(
        std::fs::read(&snapshot_v1).expect("v1 archive should still exist"),
        snapshot_v1_bytes,
        "archive bytes should still match v1 before instance 1 shutdown completes"
    );

    // Allow instance 1 shutdown response, then wait for instance 2 shutdown request.
    allow_shutdown_1.notify_one();

    tokio::time::timeout(Duration::from_secs(5), called_rx_2)
        .await
        .expect("shutdown request for instance 2 should arrive within timeout")
        .expect("shutdown channel for instance 2 should not be dropped");
    assert_eq!(
        entity_artifact_path(&node_stack, NODE_NAME, NODE_TAG).as_path(),
        snapshot_v1.as_path(),
        "node should not be overwritten before instance 2 is shutdown"
    );
    assert_eq!(
        std::fs::read(&snapshot_v1).expect("v1 archive should still exist"),
        snapshot_v1_bytes,
        "archive bytes should still match v1 between the two instance shutdowns"
    );

    // Allow instance 2 shutdown response so the overwrite can proceed.
    allow_shutdown_2.notify_one();

    let add_v2 = add_task
        .await
        .expect("node_add overwrite task should join")
        .expect("node_add overwrite request should complete");

    assert!(
        add_v2.success,
        "node_add overwrite should succeed, got error: {:?}",
        add_v2.error_message
    );

    assert_eq!(
        entity_artifact_path(&node_stack, NODE_NAME, NODE_TAG).as_path(),
        add_v2.snapshot_path.as_path(),
        "node stack should point to the new snapshot path"
    );
    assert_eq!(
        entity_instance_count(&node_stack, NODE_NAME, NODE_TAG),
        0,
        "instances should be stopped before overwrite completes"
    );
    // With deterministic archive naming, v1 and v2 produce the same path.
    // The archive was overwritten, so it should still exist.
    assert!(
        add_v2.snapshot_path.exists(),
        "archive should exist after overwrite"
    );
    // After overwrite the archive bytes must have changed (v1 had no
    // `v2_marker.txt`, v2 does), proving the artifact was actually
    // replaced rather than just left in place.
    let snapshot_v2_bytes =
        std::fs::read(&add_v2.snapshot_path).expect("v2 archive should be readable");
    assert_ne!(
        snapshot_v2_bytes, snapshot_v1_bytes,
        "v2 archive should differ from v1 (the v2 source includes v2_marker.txt)"
    );

    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }
    let expected_instance_1 = format!("{INSTANCE_1} has been stopped");
    let expected_instance_2 = format!("{INSTANCE_2} has been stopped");
    let saw_instance_1 = feedback
        .iter()
        .any(|entry| entry.is_stdout() && entry.line.trim() == expected_instance_1.as_str());
    let saw_instance_2 = feedback
        .iter()
        .any(|entry| entry.is_stdout() && entry.line.trim() == expected_instance_2.as_str());
    assert!(saw_instance_1, "should emit stop feedback for instance 1");
    assert!(saw_instance_2, "should emit stop feedback for instance 2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_uses_env_overrides_for_path() {
    // Emulates a real case scenario where the caller environment differs from the already-running
    // daemon environment. In practice, users often "install a tool then source it" (e.g.
    // `. "$HOME/.cargo/env"`), but that only affects their shell, not the daemon. We model this by
    // passing a PATH override in the goal on the second attempt.
    const TARGET_NODE_NAME: &str = "the_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const STDOUT_MARKER: &str = "peppy_logfile_stdout_marker";
    const STDERR_MARKER: &str = "peppy_logfile_stderr_marker";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                add_cmd: ["printout {STDOUT_MARKER}; printout {STDERR_MARKER} 1>&2"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG)
    .replace("{STDOUT_MARKER}", STDOUT_MARKER)
    .replace("{STDERR_MARKER}", STDERR_MARKER);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    // `printout` does not exist in the system when this is run
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "The command should fail, printout does not exist: {:?}",
        add_result.error_message
    );

    // Create a temp bin directory with a `printout` script
    let bin_dir = tempfile::tempdir().expect("failed to create temp bin dir");
    let printout_path = bin_dir.path().join("printout");
    std::fs::write(&printout_path, "#!/bin/sh\necho \"$@\"\n")
        .expect("failed to write printout script");

    // Make it executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&printout_path)
            .expect("failed to get printout metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&printout_path, perms)
            .expect("failed to set printout permissions");
    }

    // Pass the bin directory in PATH via env overrides to simulate the caller having an updated
    // PATH without restarting the daemon.
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_dir.path().display(), current_path);
    let env_vars = vec![("PATH".to_string(), new_path)];

    let add_result = send_node_add_and_wait_with_env(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
        env_vars,
    )
    .await
    .expect("node_add request should succeed");

    // Now the command should succeed, since `printout` is available in the PATH override
    assert!(
        add_result.success,
        "The command should succeed, got error: {:?}",
        add_result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_injects_runtime_env_vars() {
    const TARGET_NODE_NAME: &str = "runtime_env_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                add_cmd: [
                    "sh",
                    "-c",
                    "test -n \"$PEPPY_APPTAINER_BIN\" && test \"$PEPPY_NODE_NAME\" = \"{TARGET_NODE_NAME}\" && test \"$PEPPY_NODE_TAG\" = \"{TARGET_NODE_TAG}\""
                ],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed when runtime env vars are injected, got error: {:?}",
        add_result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_fails_runs_add_cmd_on_missing_node_dependency() {
    // If there is a missing dependency, the NODE_ADD_ACTION should fail with a MissingDependency error
    // BEFORE running add_cmd. This mimics real nodes (e.g. fake_video_reconstruction) where
    // `cargo build` fails because peppygen interfaces are incomplete when dependencies are missing.
    const TARGET_NODE_NAME: &str = "add_cmd_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let marker_dir = tempfile::tempdir().expect("failed to create temp marker dir");
    let marker_path = marker_dir.path().join(ADD_CMD_MARKER_FILE);

    // add_cmd creates a marker file then fails (simulating a build that fails due to
    // incomplete peppygen interfaces from missing dependencies). We use an absolute path
    // for the marker so it survives the copied-dir cleanup on failure.
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
          name: "TARGET_NODE_NAME",
          tag: "TARGET_NODE_TAG",
          depends_on: {
            nodes: [
              { name: "fake_uvc_camera", tag: "0.1.0", local_id: "fake_uvc_camera" }
            ]
          },
        },
        interfaces: {
          topics: {
            consumes: [
              {
                local_node_id: "fake_uvc_camera",
                name: "video_stream"
              },
            ],
          },
        },
        execution: {
          language: "rust",
          add_cmd: ["sh", "-c", "touch MARKER_PATH && exit 1"],
          start_cmd: ["sleep", "10"]
        },
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG)
    .replace("MARKER_PATH", &marker_path.to_string_lossy());
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should fail when dependency is missing"
    );

    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("does not exist in the stack"))
            .unwrap_or(false),
        "error message should indicate missing dependency, got: {:?}",
        add_result.error_message
    );

    // add_cmd should NOT have been executed — dependency validation must happen before add_cmd
    assert!(
        !marker_path.exists(),
        "add_cmd should NOT have been executed when dependency is missing"
    );

    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when dependency is missing"
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_fails_on_missing_interface_even_when_dependency_exists() {
    // The dependency node (fake_uvc_camera:0.1.0) exists in the stack but emits a
    // DIFFERENT topic name than what the dependent node subscribes to. The node add should
    // fail with a MissingInterface error BEFORE running add_cmd. This mimics the real
    // scenario where `fake_uvc_camera` is added first, but `fake_video_reconstruction`
    // fails because the interface names don't match.
    const DEPENDENCY_NODE_NAME: &str = "fake_uvc_camera";
    const DEPENDENCY_NODE_TAG: &str = "0.1.0";
    const TARGET_NODE_NAME: &str = "fake_video_reconstruction";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Step 1: Add the dependency node that emits a topic with a DIFFERENT name
    // than what the dependent node will subscribe to.
    let dep_source_dir = tempfile::tempdir().expect("failed to create temp dep source dir");
    let dep_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dep_source_dir.path(), &dep_peppy_json5);

    let dep_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dep_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add request should complete");

    assert!(
        dep_result.success,
        "dependency node_add should succeed, got error: {:?}",
        dep_result.error_message
    );
    assert!(
        node_stack.contains(DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG),
        "dependency node should be in the stack"
    );
    assert_eq!(node_stack.len(), 2, "root + dependency");

    // Step 2: Add the dependent node that subscribes to a topic name that the
    // dependency does NOT emit (node name+tag matches, but interface doesn't).
    let target_source_dir = tempfile::tempdir().expect("failed to create temp target source dir");
    let marker_dir = tempfile::tempdir().expect("failed to create temp marker dir");
    let marker_path = marker_dir.path().join(ADD_CMD_MARKER_FILE);

    let target_peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
          name: "TARGET_NODE_NAME",
          tag: "TARGET_NODE_TAG",
          depends_on: {
            nodes: [
              { name: "DEPENDENCY_NODE_NAME", tag: "DEPENDENCY_NODE_TAG", local_id: "DEPENDENCY_NODE_NAME" }
            ]
          },
        },
        interfaces: {
          topics: {
            consumes: [
              {
                local_node_id: "DEPENDENCY_NODE_NAME",
                name: "video_stream"
              },
            ],
          },
        },
        execution: {
          language: "rust",
          add_cmd: ["sh", "-c", "touch MARKER_PATH && exit 1"],
          start_cmd: ["sleep", "10"]
        },
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG)
    .replace("DEPENDENCY_NODE_NAME", DEPENDENCY_NODE_NAME)
    .replace("DEPENDENCY_NODE_TAG", DEPENDENCY_NODE_TAG)
    .replace("MARKER_PATH", &marker_path.to_string_lossy());
    write_peppy_json5(target_source_dir.path(), &target_peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        target_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when interface is not exposed by dependency"
    );

    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("is not exposed"))
            .unwrap_or(false),
        "error message should indicate missing interface, got: {:?}",
        add_result.error_message
    );

    // add_cmd should NOT have been executed — interface validation must happen before add_cmd
    assert!(
        !marker_path.exists(),
        "add_cmd should NOT have been executed when interface is missing"
    );

    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "dependent node should not be added when interface is missing"
    );
    assert_eq!(node_stack.len(), 2, "root + dependency only");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_reports_excluded_dirs_in_feedback() {
    const TARGET_NODE_NAME: &str = "excluded_dirs_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Create directories that should be excluded from the copy
    for dir_name in [".venv", "target", "node_modules", "__pycache__"] {
        std::fs::create_dir(source_dir.path().join(dir_name))
            .expect("failed to create excluded dir");
    }

    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Some(feedback_tx),
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }

    let excluded_feedback = feedback.iter().find(|entry| {
        entry.is_stdout() && entry.line.starts_with("Excluded directories from copy:")
    });
    assert!(
        excluded_feedback.is_some(),
        "feedback should include excluded directories message, got: {feedback:?}"
    );

    let line = &excluded_feedback.unwrap().line;
    for expected in [".venv", "__pycache__", "node_modules", "target"] {
        assert!(
            line.contains(expected),
            "excluded dirs feedback should mention '{expected}', got: {line}"
        );
    }

    // Verify excluded directories were not included in the archive
    let entries = list_archive_entries(&add_result.snapshot_path);
    for dir_name in [".venv", "target", "node_modules", "__pycache__"] {
        assert!(
            !entries
                .iter()
                .any(|e| e.starts_with(&format!("{dir_name}/")) || e == dir_name),
            "{dir_name} should not be in the archive, entries: {entries:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_container_build_failure_includes_stderr_in_error() {
    const TARGET_NODE_NAME: &str = "broken_container_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "TARGET_NODE_NAME",
            tag: "TARGET_NODE_TAG",
        },
        execution: {
            language: "rust",
            container: {
                def_file: "apptainer.def",
            }
        }
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    // Write a deliberately broken definition file so apptainer build fails with
    // a diagnostic message on stderr.
    let broken_def = "\
Bootstrap: invalid_bootstrap_agent_that_does_not_exist
From: nowhere

%runscript
    echo broken
";
    std::fs::write(source_dir.path().join("apptainer.def"), broken_def)
        .expect("failed to write broken apptainer definition");

    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        CONTAINER_RESULT_TIMEOUT,
        Some(feedback_tx),
    )
    .await
    .expect("node_add request should complete");

    // The build should fail
    assert!(
        !add_result.success,
        "node_add should fail with a broken def file"
    );

    // The error message should mention the container build failure. The
    // exact wording comes from NodeEntity::build, which wraps the apptainer
    // failure as a `BuildFailed` error containing "apptainer build failed".
    let error_msg = add_result
        .error_message
        .as_ref()
        .expect("error_message should be present");
    assert!(
        error_msg.contains("apptainer build failed"),
        "error should mention apptainer build failure, got: {}",
        error_msg
    );

    // The error message should include the stderr tail from apptainer so the user
    // sees WHY the build failed, not just the exit code.
    assert!(
        error_msg.contains("stderr"),
        "error should include stderr output from apptainer build, got: {}",
        error_msg
    );

    // Node should not be in the stack
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when container build fails"
    );

    // Verify the log file was written and contains build output
    assert!(
        add_result.log_path.exists(),
        "log file should exist even on failure: {:?}",
        add_result.log_path
    );
    let log_content =
        std::fs::read_to_string(&add_result.log_path).expect("should be able to read log file");
    assert!(
        log_content.contains("[stdout]") || log_content.contains("[stderr]"),
        "log file should contain streamed build output, got:\n{}",
        log_content
    );

    // Verify feedback was streamed to the CLI during the build
    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }
    assert!(
        !feedback.is_empty(),
        "feedback should have been streamed during the container build"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_logs_error_on_spawn_failure() {
    const TARGET_NODE_NAME: &str = "add_spawn_failure_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Multi-element add_cmd with a nonexistent binary.
    // Multi-element commands are executed directly (not via shell), so
    // command.spawn() will fail with a "No such file or directory" error.
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                add_cmd: ["nonexistent_binary_peppy_test_xyz", "--flag"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(!add_result.success, "node_add should fail when spawn fails");

    let error_msg = add_result
        .error_message
        .as_ref()
        .expect("error_message should be present");
    assert!(
        error_msg.contains("add_cmd failed"),
        "error should mention add_cmd failure, got: {}",
        error_msg
    );
    assert!(
        error_msg.contains("nonexistent_binary_peppy_test_xyz"),
        "error should include the command that failed, got: {}",
        error_msg
    );

    // The log file should exist and contain the error
    assert!(
        add_result.log_path.exists(),
        "log file should exist at {:?}",
        add_result.log_path
    );

    let log_content =
        std::fs::read_to_string(&add_result.log_path).expect("should be able to read log file");
    assert!(
        !log_content.is_empty(),
        "log file should not be empty when a spawn failure occurs"
    );
    assert!(
        log_content.contains("[error]"),
        "log file should contain an [error] entry, got:\n{}",
        log_content
    );
    assert!(
        log_content.contains("add_cmd failed"),
        "log file should contain the failure message, got:\n{}",
        log_content
    );
    assert!(
        log_content.contains("nonexistent_binary_peppy_test_xyz"),
        "log file should contain the command that failed, got:\n{}",
        log_content
    );

    // Node should not be in the stack
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when add_cmd fails"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_same_node_with_running_instance_and_dependents_succeeds() {
    use peppylib::messaging::{MessengerHandle, SHUTDOWN_SERVICE, ServiceMessenger};
    use std::sync::Arc;
    use tokio::sync::{Mutex, Notify, oneshot};

    const DEPENDENCY_NODE_NAME: &str = "lidar_dep";
    const DEPENDENCY_NODE_TAG: &str = "1.0.0";
    const DEPENDENT_NODE_NAME: &str = "brain_dep";
    const DEPENDENT_NODE_TAG: &str = "1.0.0";
    const INSTANCE_ID: &str = "lidar_dep_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let dependency_source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependency_source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependent_source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let dependency_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
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
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v1.path(), &dependency_peppy_json5);

    let dependency_add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add v1 should complete");
    assert!(
        dependency_add_v1.success,
        "dependency node_add v1 should succeed: {:?}",
        dependency_add_v1.error_message
    );

    let dependent_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENT_NODE_NAME}",
                tag: "{DEPENDENT_NODE_TAG}",
                depends_on: {
                    nodes: [
                        { name: "{DEPENDENCY_NODE_NAME}", tag: "{DEPENDENCY_NODE_TAG}", local_id: "{DEPENDENCY_NODE_NAME}" }
                    ]
                },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          local_node_id: "{DEPENDENCY_NODE_NAME}",
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENT_NODE_NAME}", DEPENDENT_NODE_NAME)
    .replace("{DEPENDENT_NODE_TAG}", DEPENDENT_NODE_TAG)
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependent_source_dir.path(), &dependent_peppy_json5);

    let dependent_add = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependent_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependent node_add should complete");
    assert!(
        dependent_add.success,
        "dependent node_add should succeed: {:?}",
        dependent_add.error_message
    );

    // Add a fake running instance to the dependency node
    let instance_id = config::node::Name::new(INSTANCE_ID).expect("valid instance id");
    let _running = spawn_real_running_instance(
        &started_core_node,
        DEPENDENCY_NODE_NAME,
        DEPENDENCY_NODE_TAG,
        &instance_id,
    )
    .await;

    // Mock the shutdown service for the running instance
    let instance_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let (called_tx, called_rx) = oneshot::channel::<()>();
    let called_tx = Arc::new(Mutex::new(Some(called_tx)));
    let allow_shutdown = Arc::new(Notify::new());
    let allow_shutdown_clone = Arc::clone(&allow_shutdown);
    let mut shutdown_endpoint = ServiceMessenger::listen(
        &instance_messenger,
        &started_core_node.core_node_name,
        INSTANCE_ID,
        DEPENDENCY_NODE_NAME,
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service");
    let _shutdown_task = AbortOnDrop(peppylib::runtime::spawn({
        let called_tx = Arc::clone(&called_tx);
        async move {
            shutdown_endpoint
                .handle_requests(move |context| {
                    let called_tx = Arc::clone(&called_tx);
                    let allow_shutdown_clone = Arc::clone(&allow_shutdown_clone);
                    async move {
                        let payload = context.message().payload();
                        if let Some(tx) = called_tx.lock().await.take() {
                            let _ = tx.send(());
                        }
                        allow_shutdown_clone.notified().await;
                        Ok(payload)
                    }
                })
                .await
        }
    }));

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Re-add the dependency with the same interface
    write_peppy_json5(dependency_source_dir_v2.path(), &dependency_peppy_json5);

    let caller_handle = started_core_node.caller_handle.clone();
    let core_node_name = started_core_node.core_node_name.clone();
    let source_path_v2 = dependency_source_dir_v2.path().to_path_buf();
    let add_task = tokio::spawn(async move {
        send_node_add_and_wait(
            &caller_handle,
            &core_node_name,
            &source_path_v2,
            GOAL_TIMEOUT,
            RESULT_TIMEOUT,
            None,
        )
        .await
    });

    // Wait for shutdown to be requested, then allow it to complete
    tokio::time::timeout(Duration::from_secs(5), called_rx)
        .await
        .expect("shutdown request should arrive within timeout")
        .expect("shutdown channel should not be dropped");
    allow_shutdown.notify_one();

    let add_v2 = add_task
        .await
        .expect("node_add re-add task should join")
        .expect("node_add re-add request should complete");

    assert!(
        add_v2.success,
        "re-adding a node with same interface should succeed even when dependents exist, got: {:?}",
        add_v2.error_message
    );

    assert_eq!(
        entity_instance_count(&node_stack, DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG),
        0,
        "running instance should have been stopped"
    );
    assert!(
        node_stack.contains(DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG),
        "dependent node should still be in the stack"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_same_node_changing_interface_with_running_instance_and_dependents_fails() {
    // The instance is stopped first (shutdown succeeds), then push_config fails because
    // the new interface breaks the dependent. The stack is preserved with the old config.
    use peppylib::messaging::{MessengerHandle, SHUTDOWN_SERVICE, ServiceMessenger};
    use std::sync::Arc;

    const DEPENDENCY_NODE_NAME: &str = "lidar_iface";
    const DEPENDENCY_NODE_TAG: &str = "1.0.0";
    const DEPENDENT_NODE_NAME: &str = "brain_iface";
    const DEPENDENT_NODE_TAG: &str = "1.0.0";
    const INSTANCE_ID: &str = "lidar_iface_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let dependency_source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependency_source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependent_source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let dependency_peppy_json5_v1 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
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
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v1.path(), &dependency_peppy_json5_v1);

    let dependency_add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add v1 should complete");
    assert!(
        dependency_add_v1.success,
        "dependency node_add v1 should succeed: {:?}",
        dependency_add_v1.error_message
    );

    let dependent_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENT_NODE_NAME}",
                tag: "{DEPENDENT_NODE_TAG}",
                depends_on: {
                    nodes: [
                        { name: "{DEPENDENCY_NODE_NAME}", tag: "{DEPENDENCY_NODE_TAG}", local_id: "{DEPENDENCY_NODE_NAME}" }
                    ]
                },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          local_node_id: "{DEPENDENCY_NODE_NAME}",
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENT_NODE_NAME}", DEPENDENT_NODE_NAME)
    .replace("{DEPENDENT_NODE_TAG}", DEPENDENT_NODE_TAG)
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependent_source_dir.path(), &dependent_peppy_json5);

    let dependent_add = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependent_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependent node_add should complete");
    assert!(
        dependent_add.success,
        "dependent node_add should succeed: {:?}",
        dependent_add.error_message
    );

    let snapshot_v1 = entity_artifact_path(&node_stack, DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG);

    // Add a fake running instance to the dependency node
    let instance_id = config::node::Name::new(INSTANCE_ID).expect("valid instance id");
    let _running = spawn_real_running_instance(
        &started_core_node,
        DEPENDENCY_NODE_NAME,
        DEPENDENCY_NODE_TAG,
        &instance_id,
    )
    .await;

    // Register a SHUTDOWN_SERVICE handler that responds immediately.
    // Shutdown succeeds; push_config then rejects the overwrite due to the interface change.
    let instance_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let mut shutdown_endpoint = ServiceMessenger::listen(
        &instance_messenger,
        &started_core_node.core_node_name,
        INSTANCE_ID,
        DEPENDENCY_NODE_NAME,
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service");
    let _shutdown_task = AbortOnDrop(peppylib::runtime::spawn(async move {
        shutdown_endpoint
            .handle_requests(|context| async move { Ok(context.message().payload()) })
            .await
    }));

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Try to overwrite with a different interface (new_service instead of reset_sensor).
    let dependency_peppy_json5_v2 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "new_service" }
                    ]
                }
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v2.path(), &dependency_peppy_json5_v2);

    let add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add overwrite request should complete");

    assert!(
        !add_v2.success,
        "overwriting with a changed interface should fail when dependents exist"
    );
    assert!(
        add_v2
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Cannot overwrite node"))
            .unwrap_or(false),
        "error should indicate the overwrite is blocked: {:?}",
        add_v2.error_message
    );

    // Old config must be preserved, snapshot path unchanged
    assert_eq!(
        entity_artifact_path(&node_stack, DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG).as_path(),
        snapshot_v1.as_path(),
        "snapshot path should be unchanged after failed overwrite"
    );
    // Shutdown succeeded, so the instance was stopped before push_config rejected the overwrite
    assert_eq!(
        entity_instance_count(&node_stack, DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG),
        0,
        "running instance should have been stopped before push_config rejected the overwrite"
    );
    assert!(
        node_stack.contains(DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG),
        "dependent node should still be in the stack after failed overwrite"
    );

    // The dependency's interface must remain the v1 shape: `reset_sensor`
    // is still exposed and the v2 `new_service` was never spliced in.
    {
        let handle = node_stack
            .find(DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG)
            .expect("dependency entity missing");
        let guard = handle.read();
        let exposes = guard
            .config()
            .interfaces
            .services
            .as_ref()
            .and_then(|s| s.exposes.as_ref())
            .expect("v1 services.exposes should be present");
        let names: Vec<&str> = exposes.iter().map(|svc| svc.name.as_str()).collect();
        assert!(
            names.contains(&"reset_sensor"),
            "v1 `reset_sensor` should still be exposed after failed overwrite, got: {:?}",
            names
        );
        assert!(
            !names.contains(&"new_service"),
            "v2 `new_service` must not have leaked through the failed overwrite, got: {:?}",
            names
        );
    }
}

/// When a running node instance does not respond to SHUTDOWN_SERVICE (e.g. the process is frozen),
/// `shutdown_existing_instances` times out and the add must fail with a descriptive error.
/// The instance and stack must remain untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_same_node_with_running_instance_and_dependents_fails_on_stopped_node_stuck() {
    use peppylib::messaging::{MessengerHandle, SHUTDOWN_SERVICE, ServiceMessenger};
    use std::sync::Arc;
    use tokio::sync::Notify;

    const DEPENDENCY_NODE_NAME: &str = "lidar_stuck";
    const DEPENDENCY_NODE_TAG: &str = "1.0.0";
    const DEPENDENT_NODE_NAME: &str = "brain_stuck";
    const DEPENDENT_NODE_TAG: &str = "1.0.0";
    const INSTANCE_ID: &str = "lidar_stuck_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let dependency_source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependency_source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependent_source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let dependency_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v1.path(), &dependency_peppy_json5);

    let dependency_add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add v1 should complete");
    assert!(
        dependency_add_v1.success,
        "dependency node_add v1 should succeed: {:?}",
        dependency_add_v1.error_message
    );

    let dependent_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{DEPENDENT_NODE_NAME}",
                tag: "{DEPENDENT_NODE_TAG}",
                depends_on: {
                    nodes: [
                        { name: "{DEPENDENCY_NODE_NAME}", tag: "{DEPENDENCY_NODE_TAG}", local_id: "{DEPENDENCY_NODE_NAME}" }
                    ]
                },
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENT_NODE_NAME}", DEPENDENT_NODE_NAME)
    .replace("{DEPENDENT_NODE_TAG}", DEPENDENT_NODE_TAG)
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependent_source_dir.path(), &dependent_peppy_json5);

    let dependent_add = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependent_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependent node_add should complete");
    assert!(
        dependent_add.success,
        "dependent node_add should succeed: {:?}",
        dependent_add.error_message
    );

    // Spawn a real running instance WITHOUT the auto-shutdown listener so
    // the production shutdown path observes a stuck process that never
    // responds or terminates.
    let instance_id = config::node::Name::new(INSTANCE_ID).expect("valid instance id");
    let _running = spawn_real_stuck_instance(
        &started_core_node,
        DEPENDENCY_NODE_NAME,
        DEPENDENCY_NODE_TAG,
        &instance_id,
    )
    .await;

    // Register a SHUTDOWN_SERVICE handler that blocks forever — simulates a frozen/unresponsive node.
    // `notify_one` is never called, so the handler never returns, causing the poll to time out.
    let instance_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let never_unblock = Arc::new(Notify::new());
    let never_unblock_clone = Arc::clone(&never_unblock);
    let mut shutdown_endpoint = ServiceMessenger::listen(
        &instance_messenger,
        &started_core_node.core_node_name,
        INSTANCE_ID,
        DEPENDENCY_NODE_NAME,
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service");
    let _shutdown_task = AbortOnDrop(peppylib::runtime::spawn(async move {
        shutdown_endpoint
            .handle_requests(move |context| {
                let never_unblock_clone = Arc::clone(&never_unblock_clone);
                async move {
                    let payload = context.message().payload();
                    // Block forever — the node never acknowledges the shutdown
                    never_unblock_clone.notified().await;
                    Ok(payload)
                }
            })
            .await
    }));

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Re-add with the same interface. The shutdown poll will time out after SHUTDOWN_TIMEOUT (5 s).
    write_peppy_json5(dependency_source_dir_v2.path(), &dependency_peppy_json5);
    let add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_v2.success,
        "node_add should fail when the instance does not respond to shutdown"
    );
    assert!(
        add_v2
            .error_message
            .as_deref()
            .map(|msg| msg.contains("failed to shutdown node instance"))
            .unwrap_or(false),
        "error should describe the shutdown failure: {:?}",
        add_v2.error_message
    );

    // The instance was never removed — shutdown did not complete
    assert_eq!(
        entity_instance_count(&node_stack, DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG),
        1,
        "running instance should still be present when shutdown timed out"
    );
    assert!(
        node_stack.contains(DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG),
        "dependent node should still be in the stack"
    );
}

// ---------------------------------------------------------------------------
// Variant tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_local_source() {
    const ROOT_NODE_NAME: &str = "robot_brain";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create root node directory with variant inside it
    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("mock_node");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    // Root node config with a "mock" variant pointing to a subdirectory
    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "robot_brain",
            tag: "0.1.0",
            variants: [
                { name: "mock", source: { local: "mock_node" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Variant config — only defines runtime (no manifest, no interfaces)
    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "mock",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with variant should succeed");

    assert!(
        add_result.success,
        "variant node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    // Variant should be in the stack under the root node's name:tag
    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    // The config in the stack should have root's interfaces but variant's runtime
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.start_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the variant"
    );
    drop(entity_guard);
}

/// `node sync` must fingerprint the variant's own peppy.json5,
/// not the temporary merged config, so that `node add` verification passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_local_source_after_sync() {
    use core_node::encoding::NodeSyncRequest;

    const ROOT_NODE_NAME: &str = "synced_robot";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create root node directory with variant inside it
    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("mock_node");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    // Root node config with a "mock" variant pointing to a subdirectory
    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "synced_robot",
            tag: "0.1.0",
            variants: [
                { name: "mock", source: { local: "mock_node" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    // Write configs WITHOUT pre-baked fingerprints — sync will generate them.
    let root_config_path = root_dir.join(NODE_CONFIG_FILE);
    std::fs::write(&root_config_path, root_config).expect("failed to write root config");

    // Variant config — only defines execution (no manifest, no interfaces)
    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    let variant_config_path = variant_dir.join(NODE_CONFIG_FILE);
    std::fs::write(&variant_config_path, variant_config).expect("failed to write variant config");

    // Step 1: Run node sync — this generates peppygen + fingerprint for root and variant.
    let sync_response = NodeSyncRequest::new(&root_dir, TEST_GIT_HASH)
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        sync_response.success,
        "node_sync should succeed, got error: {}",
        sync_response.error_message
    );

    // Sanity: variant .peppy directory should exist after sync
    assert!(
        variant_dir.join(PEPPY_OUTPUT_DIR).exists(),
        "variant .peppy directory should exist after sync"
    );

    // Step 2: Run node add with the variant.
    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "mock",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with variant should succeed");

    assert!(
        add_result.success,
        "variant node_add after sync should succeed, got error: {:?}",
        add_result.error_message
    );

    // Verify the node is in the stack with the expected merged config
    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.start_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the variant"
    );
    drop(entity_guard);

    // Verify that the fingerprint stored by sync matches the variant's peppy.json5 content
    let stored_fingerprint =
        config::fingerprint::read_codegen_fingerprint(&variant_config_path, PEPPYGEN_OUTPUT_PATH)
            .expect("variant fingerprint should be readable after sync");
    let expected_fingerprint =
        config::fingerprint::fingerprint_for_bytes(variant_config.as_bytes());
    assert_eq!(
        stored_fingerprint, expected_fingerprint,
        "stored fingerprint should match the variant's peppy.json5 content"
    );
}

/// Variant-only nodes (no execution at root, only in variants) must work with
/// `node sync` + `node add --variant`. Sync skips peppygen generation for the
/// root when it has no execution block, so only the variant directory gets a
/// `.peppy/git.hash`. The `node add` verification must use the resolved variant
/// path, not the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_only_node_after_sync() {
    use core_node::encoding::{NodeInfoRequest, NodeSyncRequest};

    const ROOT_NODE_NAME: &str = "variant_only_robot";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create root node directory with variant inside it
    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("mock_node");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    // Root config has NO execution block — only manifest with variants + interfaces.
    // This is the variant-only pattern: the root defines the contract, variants
    // provide the implementation. A "default" variant is required when there is
    // no execution block at root.
    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "variant_only_robot",
            tag: "0.1.0",
            variants: [
                { name: "default", source: { local: "mock_node" } },
                { name: "mock", source: { local: "mock_node" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        }
    }"#;
    let root_config_path = root_dir.join(NODE_CONFIG_FILE);
    std::fs::write(&root_config_path, root_config).expect("failed to write root config");

    // Variant config defines execution (the implementation)
    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    let variant_config_path = variant_dir.join(NODE_CONFIG_FILE);
    std::fs::write(&variant_config_path, variant_config).expect("failed to write variant config");

    // Step 1: Run node sync — generates peppygen only for the variant (not root,
    // since root has no execution block).
    let sync_response = NodeSyncRequest::new(&root_dir, TEST_GIT_HASH)
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        sync_response.success,
        "node_sync should succeed, got error: {}",
        sync_response.error_message
    );

    // Root should have .peppy/git.hash (sync always writes it alongside the
    // manifest) but no peppygen output (no execution block at root).
    let root_peppy_dir = root_dir.join(PEPPY_OUTPUT_DIR);
    assert!(
        root_peppy_dir.exists(),
        "root .peppy directory should exist after sync (git.hash lives here)"
    );
    assert!(
        root_peppy_dir.join("git.hash").exists(),
        "root .peppy/git.hash should exist after sync"
    );
    assert!(
        !root_dir.join(PEPPYGEN_OUTPUT_PATH).exists(),
        "root should NOT have peppygen output (no execution block)"
    );

    // Variant should have .peppy directory after sync.
    assert!(
        variant_dir.join(PEPPY_OUTPUT_DIR).exists(),
        "variant .peppy directory should exist after sync"
    );

    // Step 2: Preflight node_info check — mirrors what the CLI does before add.
    // Must succeed for variant-only nodes (auto-resolves the default variant).
    let info_response = NodeInfoRequest::new(core_node::encoding::NodeSource::Fs(root_dir.clone()))
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_info preflight should succeed for variant-only nodes");

    assert_eq!(info_response.config.manifest.name.as_str(), ROOT_NODE_NAME);
    assert_eq!(info_response.config.manifest.tag.as_str(), ROOT_NODE_TAG);

    // Step 3: Run node add with the variant — must succeed despite no .peppy variant file at the repo root.
    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "mock",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with variant should succeed");

    assert!(
        add_result.success,
        "variant-only node_add after sync should succeed, got error: {:?}",
        add_result.error_message
    );

    // Verify the node is in the stack with the expected merged config
    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.start_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the variant"
    );
}

/// After sync, modifying the variant's peppy.json5 must cause a fingerprint
/// mismatch on the next `node add`, blocking the stale variant from being added.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_fingerprint_mismatch_after_sync() {
    use core_node::encoding::NodeSyncRequest;

    const ROOT_NODE_NAME: &str = "stale_variant_robot";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("mock_node");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "stale_variant_robot",
            tag: "0.1.0",
            variants: [
                { name: "mock", source: { local: "mock_node" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    std::fs::write(root_dir.join(NODE_CONFIG_FILE), root_config)
        .expect("failed to write root config");

    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    std::fs::write(variant_dir.join(NODE_CONFIG_FILE), variant_config)
        .expect("failed to write variant config");

    // Step 1: Sync — generates peppygen and fingerprint for both root and variant.
    let sync_response = NodeSyncRequest::new(&root_dir, TEST_GIT_HASH)
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        sync_response.success,
        "node_sync should succeed, got error: {}",
        sync_response.error_message
    );

    // Step 2: Modify the variant config after sync (simulates user editing without re-syncing).
    let modified_variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "99"]
        }
    }"#;
    std::fs::write(variant_dir.join(NODE_CONFIG_FILE), modified_variant_config)
        .expect("failed to write modified variant config");

    // Step 3: node add should fail — fingerprint no longer matches.
    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "mock",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when variant config was modified after sync"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Codegen fingerprint verification failed"))
            .unwrap_or(false),
        "error should indicate fingerprint verification failure, got: {:?}",
        add_result.error_message
    );

    // Node should not be in the stack
    assert!(
        !node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG),
        "node should not be added when variant fingerprint mismatches"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_with_fs_archive_variant_uses_archived_root() {
    const ROOT_NODE_NAME: &str = "archive_robot_brain";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    let archived_root_dir = bundle_dir.path().join("archived_root");
    let archived_variant_dir = archived_root_dir.join("mock_node");
    std::fs::create_dir_all(&archived_variant_dir).expect("failed to create archived variant dir");

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "archive_robot_brain",
            tag: "0.1.0",
            variants: [
                { name: "mock", source: { local: "./mock_node" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&archived_root_dir, root_config);
    let peppy_dir = archived_root_dir.join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir).expect("failed to create peppy output dir");
    std::fs::write(peppy_dir.join("git.hash"), TEST_GIT_HASH).expect("failed to write git hash");

    write_peppy_json5(
        &archived_variant_dir,
        r#"{
            schema_version: 1,
            execution: {
                language: "rust",
                start_cmd: ["sleep", "5"]
            }
        }"#,
    );

    let host_decoy_variant_dir = bundle_dir.path().join("mock_node");
    std::fs::create_dir_all(&host_decoy_variant_dir)
        .expect("failed to create host decoy variant dir");
    write_peppy_json5(
        &host_decoy_variant_dir,
        r#"{
            schema_version: 1,
            execution: {
                language: "rust",
                start_cmd: ["sleep", "99"]
            }
        }"#,
    );

    let bundle_path = bundle_dir.path().join("archive_robot_brain.tar.zst");
    create_tar_zst_from_dir(&archived_root_dir, &bundle_path, "root_node");

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        bundle_path.as_path(),
        "mock",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with archive variant should succeed");

    assert!(
        add_result.success,
        "archive variant node_add should succeed, got error: {:?}",
        add_result.error_message
    );
    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.start_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the archived variant, not the host decoy"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_not_found() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("real_variant");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            variants: [
                { name: "real", source: { local: "real_variant2" } }
            ]
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "nonexistent",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("request should complete");

    assert!(
        !add_result.success,
        "node_add should fail for nonexistent variant"
    );
    assert!(
        add_result
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("variant 'nonexistent' not found"),
        "error should mention the missing variant: {:?}",
        add_result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_interface_mismatch() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("bad_variant");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            variants: [
                { name: "bad", source: { local: "bad_variant" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "sensor_data", message_format: { temperature: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Variant defines DIFFERENT interfaces
    let variant_config = r#"{
        schema_version: 1,
        interfaces: {
            topics: {
                emits: [
                    { name: "different_topic", message_format: { speed: "f32" } }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "bad",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("request should complete");

    assert!(
        !add_result.success,
        "node_add should fail for interface mismatch"
    );
    assert!(
        add_result
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("VariantInterfaceMismatch"),
        "error should mention interface mismatch: {:?}",
        add_result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_matching_interfaces_different_order() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("good_variant");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            variants: [
                { name: "good", source: { local: "good_variant" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "topic_a", message_format: { x: "f64", y: "f64" } },
                    { name: "topic_b" }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Variant defines the SAME interfaces but in different order
    let variant_config = r#"{
        schema_version: 1,
        interfaces: {
            topics: {
                emits: [
                    { name: "topic_b" },
                    { name: "topic_a", message_format: { y: "f64", x: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "good",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with matching interfaces should succeed");

    assert!(
        add_result.success,
        "variant with matching interfaces (different order) should succeed, got error: {:?}",
        add_result.error_message
    );
    assert!(node_stack.contains("test_node", "0.1.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_no_interfaces() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("minimal_variant");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            variants: [
                { name: "minimal", source: { local: "minimal_variant" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "data", message_format: { value: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Variant has NO interfaces (omitted entirely) — should be accepted
    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "minimal",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with variant without interfaces should succeed");

    assert!(
        add_result.success,
        "variant without interfaces should succeed, got error: {:?}",
        add_result.error_message
    );
    assert!(node_stack.contains("test_node", "0.1.0"));

    let entity = node_stack
        .find("test_node", "0.1.0")
        .expect("node should exist");
    assert!(
        entity.read().config().interfaces.topics.is_some(),
        "root interfaces should be used even when variant has none"
    );
}

/// Verifies that `NodeAddGoal` encode/decode roundtrips are lossless for every
/// `NodeSource` variant (`Fs`, `Git`, `Http`) used as either the primary source
/// or as a variant, as well as the case where no variant is set.
#[test]
fn listen_for_node_add_variant_encoding_roundtrip() {
    use core_node::encoding::NodeSource;

    // Name-based variant (Fs)
    let goal = NodeAddGoal::new("/some/path", "test-hash", 60).with_variant_name("mock");
    let encoded = goal.encode().expect("encoding should succeed");
    let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
    assert!(
        matches!(&decoded.variant, Some(NodeSource::Fs(p)) if p.to_string_lossy() == "mock"),
        "expected Fs(\"mock\"), got {:?}",
        decoded.variant
    );
    assert_eq!(decoded.git_hash, "test-hash");
    assert_eq!(decoded.timeout_secs, 60);

    // Git-based variant
    let git_url = GitUrl::try_from("https://github.com/example/repo.git").unwrap();
    let goal_git =
        NodeAddGoal::new("/some/path", "test-hash", 60).with_variant_source(NodeSource::Git {
            repo_url: git_url.clone(),
            repo_path: "brain".to_string(),
            repo_ref: Some("main".to_string()),
        });
    let encoded = goal_git.encode().expect("encoding should succeed");
    let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
    assert!(
        matches!(&decoded.variant, Some(NodeSource::Git { repo_path, repo_ref, .. }) if repo_path == "brain" && repo_ref.as_deref() == Some("main")),
        "expected Git variant, got {:?}",
        decoded.variant
    );

    // Http-based source
    let url = url::Url::parse("https://example.com/node.tar.zst").unwrap();
    let source_sha256 = "a".repeat(64);
    let goal_http_source =
        NodeAddGoal::new_http(url.clone(), Some(source_sha256.clone()), "test-hash", 60);
    let encoded = goal_http_source.encode().expect("encoding should succeed");
    let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
    assert!(
        matches!(&decoded.source, NodeSource::Http { url: u, sha256 } if u.as_str() == "https://example.com/node.tar.zst" && sha256.as_deref() == Some(source_sha256.as_str())),
        "expected Http source with sha256, got {:?}",
        decoded.source
    );

    // Http-based variant
    let url = url::Url::parse("https://example.com/variant.tar.zst").unwrap();
    let variant_sha256 = "b".repeat(64);
    let goal_http =
        NodeAddGoal::new("/some/path", "test-hash", 60).with_variant_source(NodeSource::Http {
            url: url.clone(),
            sha256: Some(variant_sha256.clone()),
        });
    let encoded = goal_http.encode().expect("encoding should succeed");
    let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
    assert!(
        matches!(&decoded.variant, Some(NodeSource::Http { url: u, sha256 }) if u.as_str() == "https://example.com/variant.tar.zst" && sha256.as_deref() == Some(variant_sha256.as_str())),
        "expected Http variant, got {:?}",
        decoded.variant
    );

    // Without variant
    let goal_no_variant = NodeAddGoal::new("/some/path", "test-hash", 60);
    let encoded = goal_no_variant.encode().expect("encoding should succeed");
    let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
    assert_eq!(decoded.variant, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_manifest_ignored_warning() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("variant_with_manifest");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            variants: [
                { name: "custom", source: { local: "variant_with_manifest" } }
            ]
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Variant defines a manifest — it should be ignored with a warning
    let variant_config = r#"{
        schema_version: 1,
        manifest: {
            name: "overridden_name",
            tag: "9.9.9",
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "custom",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Some(feedback_tx),
    )
    .await
    .expect("node_add with variant should succeed");

    assert!(
        add_result.success,
        "variant node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    // Collect feedback and verify the manifest-ignored warning was emitted
    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }

    let has_manifest_warning = feedback.iter().any(|f| {
        f.is_stderr()
            && f.line.contains("manifest")
            && f.line.contains("ignored")
            && f.line.contains("custom")
    });
    assert!(
        has_manifest_warning,
        "should emit a warning about variant manifest being ignored, got feedback: {:?}",
        feedback.iter().map(|f| &f.line).collect::<Vec<_>>()
    );

    // Verify the root manifest was used, not the variant's
    let entity = started_core_node
        .node_stack
        .find("test_node", "0.1.0")
        .expect("node should be in stack under root's name:tag");
    let entity_guard = entity.read();
    assert_eq!(entity_guard.config().manifest.name.as_str(), "test_node");
    assert_eq!(entity_guard.config().manifest.tag, "0.1.0");
}

/// When a root node defines a "default" variant and omits `runtime`, adding
/// the node without `--variant` should auto-resolve the default variant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_default_variant_auto_resolved() {
    const ROOT_NODE_NAME: &str = "uvc_camera";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("uvc_camera");
    let default_variant_dir = root_dir.join("variants").join("default");
    let mujoco_variant_dir = root_dir.join("variants").join("mujoco");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&default_variant_dir).unwrap();
    std::fs::create_dir_all(&mujoco_variant_dir).unwrap();

    // Root config: has a "default" variant, NO runtime
    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "uvc_camera",
            tag: "0.1.0",
            variants: [
                { name: "default", source: { local: "./variants/default" } },
                { name: "mujoco", source: { local: "./variants/mujoco" } },
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "image", qos_profile: "sensor_data", message_format: { width: "u32", height: "u32" } }
                ]
            }
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Default variant config
    let default_variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "7"]
        }
    }"#;
    write_peppy_json5(&default_variant_dir, default_variant_config);

    // Mujoco variant config
    let mujoco_variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "python",
            start_cmd: ["sleep", "3"]
        }
    }"#;
    write_peppy_json5(&mujoco_variant_dir, mujoco_variant_config);

    // Add WITHOUT specifying a variant — should auto-resolve "default"
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with default variant should succeed");

    assert!(
        add_result.success,
        "default variant node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.start_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "7".to_string()],
        "execution should come from the default variant"
    );
}

/// When a root node has a "default" variant but an explicit `--variant mujoco`
/// is requested, the explicit variant should be used instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_default_variant_explicit_other() {
    const ROOT_NODE_NAME: &str = "uvc_camera2";
    const ROOT_NODE_TAG: &str = "0.2.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("uvc_camera2");
    let default_variant_dir = parent_dir
        .path()
        .join("uvc_camera2")
        .join("variants")
        .join("default");
    let mujoco_variant_dir = parent_dir
        .path()
        .join("uvc_camera2")
        .join("variants")
        .join("mujoco");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&default_variant_dir).unwrap();
    std::fs::create_dir_all(&mujoco_variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "uvc_camera2",
            tag: "0.2.0",
            variants: [
                { name: "default", source: { local: "./variants/default" } },
                { name: "mujoco", source: { local: "./variants/mujoco" } },
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "image", qos_profile: "sensor_data", message_format: { width: "u32", height: "u32" } }
                ]
            }
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    let default_variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "7"]
        }
    }"#;
    write_peppy_json5(&default_variant_dir, default_variant_config);

    let mujoco_variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "python",
            start_cmd: ["sleep", "3"]
        }
    }"#;
    write_peppy_json5(&mujoco_variant_dir, mujoco_variant_config);

    // Add with explicit --variant mujoco
    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "mujoco",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with explicit mujoco variant should succeed");

    assert!(
        add_result.success,
        "explicit variant node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert_eq!(
        config.execution.start_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "3".to_string()],
        "execution should come from the mujoco variant, not the default"
    );
}

/// A root config that defines both an `execution` block AND a "default" variant
/// is invalid — execution must come from the default variant, not the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_execution_with_default_variant_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("uvc_camera");
    let default_variant_dir = root_dir.join("variants").join("default");
    let mujoco_variant_dir = root_dir.join("variants").join("mujoco");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&default_variant_dir).unwrap();
    std::fs::create_dir_all(&mujoco_variant_dir).unwrap();

    // Root config: has BOTH execution AND a "default" variant — this is invalid.
    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "uvc_camera",
            tag: "0.1.0",
            variants: [
                { name: "default", source: { local: "./variants/default" } },
                { name: "mujoco", source: { local: "./variants/mujoco" } },
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "image", qos_profile: "sensor_data", message_format: { width: "u32", height: "u32" } }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "7"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when both execution and a default variant are defined"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("execution"))
            .unwrap_or(false),
        "error message should mention execution, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");
}

/// When a variant is fetched from a git repository, the cloned temp directory
/// does not contain `.peppy/git.hash`. The git hash verification must fall back
/// to the root source path (where `peppy node sync` wrote the hash file).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_git_variant_verifies_git_hash_at_root() {
    const ROOT_NODE_NAME: &str = "git_variant_hash_robot";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create a local git repo containing the variant's execution-only config.
    let git_repo_temp = TempDir::new().expect("failed to create git repo temp dir");
    let git_repo_path = git_repo_temp.path().join("variant_repo.git");
    std::fs::create_dir_all(&git_repo_path).expect("create git repo dir");

    let repo = Repository::init(&git_repo_path).expect("init git repo");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("create signature");

    let variant_config_rel = Path::new(NODE_CONFIG_FILE);
    std::fs::write(
        git_repo_path.join(variant_config_rel),
        r#"{
            schema_version: 1,
            execution: {
                language: "rust",
                start_cmd: ["sleep", "5"]
            }
        }"#,
    )
    .expect("write variant config to git repo");

    let mut index = repo.index().expect("open index");
    index
        .add_path(variant_config_rel)
        .expect("add variant config");
    index.write().expect("write index");

    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let commit_id = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "variant v1",
            &tree,
            &[],
        )
        .expect("commit");
    let commit = repo.find_commit(commit_id).expect("find commit");
    repo.tag("v1.0", commit.as_object(), &signature, "v1.0", false)
        .expect("create v1.0 tag");

    // Build the root node directory.  The manifest declares a variant whose
    // deployment source is the local git repository we just created.
    let parent_dir = tempfile::tempdir().expect("create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    std::fs::create_dir_all(&root_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "ROOT_NODE_NAME",
            tag: "ROOT_NODE_TAG",
            variants: [
                { name: "git_variant", source: { repo: "REPO_PATH", path: ".", ref: "v1.0" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#
    .replace("ROOT_NODE_NAME", ROOT_NODE_NAME)
    .replace("ROOT_NODE_TAG", ROOT_NODE_TAG)
    .replace("REPO_PATH", &git_repo_path.display().to_string());
    write_peppy_json5(&root_dir, &root_config);

    // The test helper (send_node_add_and_wait_with_variant) auto-provisions
    // .peppy/git.hash at the root.  The git-cloned variant temp directory will
    // NOT have this file
    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "git_variant",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        add_result.success,
        "git variant node_add should succeed (git hash verified at root, not variant temp dir): {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.start_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the git variant"
    );
}

/// `.peppy/git.hash` is always located at the root (alongside the manifest).
/// When a default variant is auto-resolved, the root's git hash must still be
/// verified.  A stale root hash must cause node_add to fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_default_fs_variant_verifies_git_hash_at_root() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create root node directory with a "default" variant subdirectory.
    let parent_dir = tempfile::tempdir().expect("create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("default_impl");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "default_variant_hash_robot",
            tag: "0.1.0",
            variants: [
                { name: "default", source: { local: "default_impl" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    // Pre-provision .peppy/git.hash at root with a STALE value before the
    // test helper runs (it only writes when the file doesn't already exist).
    // This simulates the root being modified after sync without re-syncing.
    let root_peppy_dir = root_dir.join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&root_peppy_dir).expect("create root .peppy dir");
    std::fs::write(root_peppy_dir.join("git.hash"), "stale-root-hash")
        .expect("write stale root git.hash");

    // No explicit variant — the "default" variant is auto-resolved by node_add.
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "default variant node_add should FAIL when root git.hash is stale"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("git hash mismatch"))
            .unwrap_or(false),
        "error should mention git hash mismatch, got: {:?}",
        add_result.error_message
    );
    assert_eq!(
        node_stack.len(),
        1,
        "only the core node should exist (stale root rejected)"
    );
}

/// Tests that a second goal is rejected when an action is already in progress,
/// and that the rejection message suggests using `--force`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_rejects_second_goal_when_action_in_progress() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    // Create a node with a slow add_cmd so the action stays in Running state.
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "slow_add_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                add_cmd: ["sleep", "30"],
                start_cmd: ["true"]
            }
        }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    // Create the .peppy/git.hash file so the first goal's background task does
    // not fail fast on git-hash verification (which would transition the action
    // state from Running → Completed before the second goal arrives, making the
    // rejection check non-deterministic).
    let peppy_dir = source_dir.path().join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir).expect("failed to create .peppy dir");
    std::fs::write(peppy_dir.join("git.hash"), TEST_GIT_HASH).expect("failed to write git.hash");

    // Send first goal — should be accepted and start running the slow add_cmd.
    let first_goal = NodeAddGoal::new(source_dir.path(), TEST_GIT_HASH, RESULT_TIMEOUT.as_secs());
    let first_goal_payload = first_goal.encode().expect("failed to encode goal");

    let first_action_handle = ActionMessenger::send_goal(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        names::NODE_ADD_ACTION,
        Some(&started_core_node.core_node_name),
        None,
        first_goal_payload,
        QoSProfile::default(),
        GOAL_TIMEOUT,
    )
    .await
    .expect("first goal should be sent");

    let first_response_payload = first_action_handle.goal_response().payload();
    let first_response = NodeAddGoalResponse::decode(&first_response_payload)
        .expect("failed to decode first goal response");
    assert!(first_response.accepted, "first goal should be accepted");

    // Send second goal (no force) — should be rejected.
    let second_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("second node_add request should complete");

    assert!(
        !second_result.success,
        "second goal without --force should fail"
    );
    let error_msg = second_result
        .error_message
        .as_deref()
        .expect("rejection should have an error message");
    assert!(
        error_msg.contains("action already in progress"),
        "error should mention action in progress, got: {error_msg}"
    );
    assert!(
        error_msg.contains("--force"),
        "error should suggest --force, got: {error_msg}"
    );
}

/// Tests that `--force` aborts an in-progress action and starts a new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_force_overrides_in_progress_action() {
    const SECOND_NODE_NAME: &str = "force_add_node";
    const SECOND_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create a slow node so the first action stays running.
    let slow_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let slow_peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "slow_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                add_cmd: ["sleep", "30"],
                start_cmd: ["true"]
            }
        }"#;
    write_peppy_json5(slow_source_dir.path(), slow_peppy_json5);

    // Create the .peppy/git.hash file so the first goal's background task does
    // not fail fast on git-hash verification (same race as the rejection test).
    let peppy_dir = slow_source_dir.path().join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir).expect("failed to create .peppy dir");
    std::fs::write(peppy_dir.join("git.hash"), TEST_GIT_HASH).expect("failed to write git.hash");

    // Send first goal — starts the slow add.
    let first_goal = NodeAddGoal::new(
        slow_source_dir.path(),
        TEST_GIT_HASH,
        RESULT_TIMEOUT.as_secs(),
    );
    let first_goal_payload = first_goal.encode().expect("failed to encode goal");

    let first_action_handle = ActionMessenger::send_goal(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        names::NODE_ADD_ACTION,
        Some(&started_core_node.core_node_name),
        None,
        first_goal_payload,
        QoSProfile::default(),
        GOAL_TIMEOUT,
    )
    .await
    .expect("first goal should be sent");

    let first_response_payload = first_action_handle.goal_response().payload();
    let first_response = NodeAddGoalResponse::decode(&first_response_payload)
        .expect("failed to decode first goal response");
    assert!(first_response.accepted, "first goal should be accepted");

    // Create a fast node for the second goal.
    let fast_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let fast_peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{SECOND_NODE_NAME}",
                tag: "{SECOND_NODE_TAG}",
            }},
            execution: {{
                language: "rust",
                add_cmd: ["true"],
                start_cmd: ["true"]
            }}
        }}"#
    );
    write_peppy_json5(fast_source_dir.path(), &fast_peppy_json5);

    // Send second goal with force — should abort the slow action and succeed.
    let second_result = send_node_add_and_wait_with_force(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        fast_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("force node_add request should complete");

    assert!(
        second_result.success,
        "force node_add should succeed, got error: {:?}",
        second_result.error_message
    );

    assert!(
        node_stack.contains(SECOND_NODE_NAME, SECOND_NODE_TAG),
        "force-added node should be in the stack"
    );
}

/// Adding a node from a git source whose config has a default variant with a
/// local deployment source must succeed — fingerprint verification must not
/// be triggered because git-cloned sources never have fingerprint files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_with_default_local_variant_success() {
    const ROOT_NODE_NAME: &str = "git_default_variant_robot";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Build a local git repo containing a node with a default local variant.
    let git_repo_temp = TempDir::new().expect("failed to create git repo temp dir");
    let git_repo_path = git_repo_temp.path().join("variant_repo.git");
    std::fs::create_dir_all(&git_repo_path).expect("create git repo dir");

    let repo = Repository::init(&git_repo_path).expect("init git repo");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("create signature");

    // Root config: declares a default variant with a local deployment source.
    let root_config_rel = Path::new("nodes/robot/peppy.json5");
    let variant_config_rel = Path::new("nodes/robot/variants/default/peppy.json5");

    let root_dir = git_repo_path.join("nodes/robot");
    let variant_dir = git_repo_path.join("nodes/robot/variants/default");
    std::fs::create_dir_all(&root_dir).expect("create root node dir");
    std::fs::create_dir_all(&variant_dir).expect("create variant dir");

    let root_config = format!(
        r#"{{
        schema_version: 1,
        manifest: {{
            name: "{ROOT_NODE_NAME}",
            tag: "{ROOT_NODE_TAG}",
            variants: [
                {{ name: "default", source: {{ local: "./variants/default" }} }}
            ]
        }},
        interfaces: {{
            topics: {{
                emits: [
                    {{ name: "joint_positions", qos_profile: "sensor_data", message_format: {{ x: "f64", y: "f64" }} }}
                ]
            }}
        }}
    }}"#
    );
    std::fs::write(git_repo_path.join(root_config_rel), &root_config).expect("write root config");

    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            start_cmd: ["sleep", "5"]
        }
    }"#;
    std::fs::write(git_repo_path.join(variant_config_rel), variant_config)
        .expect("write variant config");

    let mut index = repo.index().expect("open index");
    index.add_path(root_config_rel).expect("add root config");
    index
        .add_path(variant_config_rel)
        .expect("add variant config");
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

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    // No explicit variant — the "default" variant is auto-resolved.
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: "nodes/robot",
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "git add with default local variant should succeed (no fingerprint verification): {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.start_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the default variant"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_emits_clone_feedback() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = test_helpers::create_nodes_git_repo(&git_repo_temp_dir);

    const TARGET_REPO_PATH: &str = "nodes/uvc_camera";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: TARGET_REPO_PATH,
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Some(feedback_tx),
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }

    let has_config_check = feedback
        .iter()
        .any(|f| f.is_stdout() && f.line.contains("Checking node config"));
    assert!(
        has_config_check,
        "feedback should include 'Checking node config' message, got: {:?}",
        feedback.iter().map(|f| &f.line).collect::<Vec<_>>()
    );

    let has_clone_feedback = feedback
        .iter()
        .any(|f| f.is_stdout() && f.line.contains("Cloning repository"));
    assert!(
        has_clone_feedback,
        "feedback should include 'Cloning repository' message, got: {:?}",
        feedback.iter().map(|f| &f.line).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_http_add_emits_download_feedback() {
    const TARGET_NODE_NAME: &str = "http_dl_feedback_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let (_bundle_dir, bundle_bytes) = create_minimal_http_bundle(TARGET_NODE_NAME, TARGET_NODE_TAG);
    let (server, url) = serve_bundle_over_http(bundle_bytes);

    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Http {
            url: url.clone(),
            sha256: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Some(feedback_tx),
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }

    let has_download_feedback = feedback
        .iter()
        .any(|f| f.is_stdout() && f.line.contains("Downloading bundle from"));
    assert!(
        has_download_feedback,
        "feedback should include 'Downloading bundle from' message, got: {:?}",
        feedback.iter().map(|f| &f.line).collect::<Vec<_>>()
    );

    drop(server);
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
            schema_version: 1,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_invalid_config_fails_fast() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = create_git_repo_with_invalid_config(git_repo_temp_dir.path());

    let started_core_node = start_core_node_with_mock_messenger().await;

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: "nodes/bad_node",
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should fail for invalid config"
    );
    assert!(
        add_result
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("Failed to parse node config"),
        "error should mention config parse failure, got: {:?}",
        add_result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_missing_config_fails() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = create_git_repo_with_invalid_config(git_repo_temp_dir.path());

    let started_core_node = start_core_node_with_mock_messenger().await;

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    // Point to a path that doesn't exist in the repo.
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: "nodes/nonexistent_node",
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should fail for missing config"
    );
    let err = add_result.error_message.as_deref().unwrap_or("");
    // The shallow probe reports "not found in repository"; if the probe falls
    // back (e.g. local transport doesn't support shallow fetch), the full clone
    // path reports a filesystem read error instead.
    assert!(
        err.contains("not found in repository") || err.contains("Failed to parse node config"),
        "error should mention config not found or parse failure, got: {:?}",
        add_result.error_message
    );
}
