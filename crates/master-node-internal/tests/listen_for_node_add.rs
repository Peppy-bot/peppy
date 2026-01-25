mod common;

use common::{
    CALLER_INSTANCE_ID, NodeAddSource, TEST_GIT_HASH, send_node_add_and_wait,
    start_master_node_with_mock_messenger, write_peppy_json5,
};
use config::consts::{NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH, logs_dir_add};
use config::node::QoSProfile;
use config::test_helpers;
use git2::{Repository, Signature};
use gix_url::Url as GitUrl;
use master_node::encoding::{NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse};
use master_node::names;
use peppylib::ActionMessenger;
use std::io::Write;
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

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");
    // `add` only adds the node to the NodeStack but doesn't spawn any instance
    assert_eq!(entity.instances().len(), 0);

    // Verify the node was copied to the peppy storage directory
    let snapshot_path = add_result.snapshot_path.as_path();
    let root_path = entity.root_path();
    assert_eq!(
        snapshot_path, root_path,
        "snapshot_path should match copied node path"
    );
    assert!(
        root_path != source_dir.path(),
        "node should be copied to a different location, got: {}",
        root_path.display()
    );
    assert!(
        root_path.exists(),
        "copied node directory should exist: {}",
        root_path.display()
    );
    assert!(
        root_path.join(NODE_CONFIG_FILE).exists(),
        "config file should be present at the root of the node folder: {}",
        root_path.join(NODE_CONFIG_FILE).display()
    );

    // Verify the path follows the expected naming convention: <node_name>_<tag>_<uuid>
    let folder_name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have folder name");
    assert!(
        folder_name.starts_with(&format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}_")),
        "folder name should start with '<node_name>_<tag>_', got: {}",
        folder_name
    );

    // Clean up the copied directory
    let _ = std::fs::remove_dir_all(root_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_success() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = test_helpers::create_nodes_git_repo(&git_repo_temp_dir);

    const TARGET_NODE_NAME: &str = "uvc_camera";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_REPO_PATH: &str = "nodes/uvc_camera";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");
    assert_eq!(entity.instances().len(), 0);

    let snapshot_path = add_result.snapshot_path.as_path();
    let root_path = entity.root_path();
    assert_eq!(snapshot_path, root_path);
    assert!(root_path.exists(), "snapshot path should exist");
    assert!(
        root_path.join(NODE_CONFIG_FILE).exists(),
        "config file should exist in snapshot"
    );

    // Verify the path follows the expected naming convention: <node_name>_<tag>_<uuid>
    let folder_name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have folder name");
    assert!(
        folder_name.starts_with(&format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}_")),
        "folder name should start with '<node_name>_<tag>_', got: {}",
        folder_name
    );

    // Clean up the copied directory
    let _ = std::fs::remove_dir_all(root_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_with_ref_success() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = create_versioned_nodes_git_repo(&git_repo_temp_dir);

    const TARGET_NODE_NAME: &str = "uvc_camera";
    const TARGET_REPO_PATH: &str = "nodes/uvc_camera";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let add_result_head = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    let snapshot_path_head = node_stack
        .find(TARGET_NODE_NAME, "0.2.0")
        .expect("node should exist in stack")
        .root_path()
        .to_path_buf();
    let _ = std::fs::remove_dir_all(&snapshot_path_head);

    let snapshot_path_ref = node_stack
        .find(TARGET_NODE_NAME, "0.1.0")
        .expect("node should exist in stack")
        .root_path()
        .to_path_buf();
    let _ = std::fs::remove_dir_all(&snapshot_path_ref);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_http_add_success() {
    const TARGET_NODE_NAME: &str = "http_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );

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
        &started_master.caller_handle,
        &started_master.master_node_name,
        NodeAddSource::Http(url),
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

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");
    assert_eq!(entity.instances().len(), 0);

    let snapshot_path = add_result.snapshot_path.as_path();
    let root_path = entity.root_path();
    assert_eq!(snapshot_path, root_path);
    assert!(root_path.exists(), "snapshot path should exist");
    assert!(
        root_path.join(NODE_CONFIG_FILE).exists(),
        "config file should exist in snapshot"
    );

    let copied_file = root_path.join("test_file.txt");
    assert!(copied_file.exists(), "test_file.txt should be copied");
    let copied_content = std::fs::read_to_string(&copied_file).expect("should read copied file");
    assert_eq!(
        copied_content, test_file_content,
        "file content should match"
    );

    // Verify the path follows the expected naming convention: <node_name>_<tag>_<uuid>
    let folder_name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have folder name");
    assert!(
        folder_name.starts_with(&format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}_")),
        "folder name should start with '<node_name>_<tag>_', got: {}",
        folder_name
    );

    // Clean up the copied directory
    let _ = std::fs::remove_dir_all(root_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_config_found() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    std::fs::remove_file(source_dir.path().join(NODE_CONFIG_FILE))
        .expect("failed to remove peppy.json5 config file");

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_git_hash_mismatch_fails() {
    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "git_hash_mismatch_node",
            tag: "0.1.0",
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
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_invalid_config_fails() {
    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{ manifest: [unclosed"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_start_cmd_fails() {
    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "no_start_cmd_node",
            tag: "0.1.0",
            language: "rust",
        },
        parameters: {}
    }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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
            .map(|msg| msg.contains("start_cmd"))
            .unwrap_or(false),
        "error message should mention start_cmd, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_dependency_not_resolved() {
    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Try to add a consumer node that depends on a non-existent provider
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "consumer_node",
            tag: "1.0.0",
            language: "rust",
            start_cmd: ["sleep", "10"],
        },
        interfaces: {
            subscribes_to: {
                topics: [
                    {
                        id: "sensor_input",
                        node: "non_existent_node",
                        name: "sensor_data",
                        tag: "1.0.0"
                    }
                ]
            }
        }
    }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_same_tags_overwrites_when_no_dependents() {
    const NODE_NAME: &str = "overwrite_node";
    const NODE_TAG: &str = "1.0.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");

    // First add: no interfaces
    let peppy_json5_v1 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }},
            parameters: {{}}
        }}"#
    );
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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
    let entity = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should exist after v1");
    assert_eq!(entity.instances().len(), 0);
    let copied_path_v1 = entity.root_path().to_path_buf();

    // Second add: same name+tag but different interfaces -> should overwrite.
    let peppy_json5_v2 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                exposes: {{
                    topics: [{{ name: "/example" }}]
                }}
            }}
        }}"#
    );
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    let add_v2 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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
    assert_eq!(entity.instances().len(), 0, "should not have any instances");
    assert_eq!(
        entity.root_path(),
        add_v2.snapshot_path.as_path(),
        "node stack should point to the new snapshot path"
    );
    assert_ne!(
        entity.root_path(),
        copied_path_v1.as_path(),
        "node stack should not point to the previous snapshot path"
    );
    assert!(
        !copied_path_v1.exists(),
        "previous snapshot should be removed from storage: {}",
        copied_path_v1.display()
    );
    assert!(
        entity
            .config()
            .interfaces
            .exposes
            .as_ref()
            .and_then(|exposes| exposes.topics.as_ref())
            .is_some_and(|topics| topics.iter().any(|topic| topic.name == "/example")),
        "node should have updated interfaces from the overwritten config"
    );

    // Clean up
    let _ = std::fs::remove_dir_all(entity.root_path());

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_same_tags_fails_when_node_has_dependents() {
    const DEPENDENCY_NODE_NAME: &str = "lidar";
    const DEPENDENCY_NODE_TAG: &str = "1.0.0";
    const DEPENDENT_NODE_NAME: &str = "brain";
    const DEPENDENT_NODE_TAG: &str = "1.0.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let dependency_source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependency_source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependent_source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let dependency_peppy_json5_v1 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                exposes: {{
                    services: [
                        {{ name: "reset_sensor" }}
                    ]
                }}
            }}
        }}"#
    );
    write_peppy_json5(dependency_source_dir_v1.path(), &dependency_peppy_json5_v1);

    let dependency_add_v1 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    let dependent_peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{DEPENDENT_NODE_NAME}",
                tag: "{DEPENDENT_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                subscribes_to: {{
                    services: [
                        {{
                          id: "reset_sensor_sub",
                          node: "{DEPENDENCY_NODE_NAME}",
                          name: "reset_sensor",
                          tag: "{DEPENDENCY_NODE_TAG}"
                        }}
                    ]
                }}
            }}
        }}"#
    );
    write_peppy_json5(dependent_source_dir.path(), &dependent_peppy_json5);

    let dependent_add = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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
    let dependency_entity = node_stack
        .find(DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG)
        .expect("dependency should exist");
    let dependency_snapshot_path = dependency_entity.root_path().to_path_buf();

    // Overwrite attempt: same name+tag but different interfaces should fail due to dependent nodes.
    let dependency_peppy_json5_v2 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                exposes: {{
                    services: [
                        {{ name: "new_service" }}
                    ]
                }}
            }}
        }}"#
    );
    write_peppy_json5(dependency_source_dir_v2.path(), &dependency_peppy_json5_v2);

    let dependency_add_v2 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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
    let dependency_entity = node_stack
        .find(DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG)
        .expect("dependency should still exist");
    assert_eq!(
        dependency_entity.root_path(),
        dependency_snapshot_path.as_path(),
        "dependency should still point to the original snapshot path"
    );
    assert!(
        node_stack.contains(DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG),
        "dependent node should still exist after failed overwrite"
    );

    // Clean up snapshots created by successful adds
    let _ = std::fs::remove_dir_all(&dependency_snapshot_path);
    let _ = std::fs::remove_dir_all(&dependent_add.snapshot_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_different_tags_create_two_entities() {
    const NODE_NAME: &str = "versioned_node";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5_v1 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "1.0.0",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    let peppy_json5_v2 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "2.0.0",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    let add_v2 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    // Clean up copied directories
    if let Some(entity) = node_stack.find(NODE_NAME, "1.0.0") {
        let _ = std::fs::remove_dir_all(entity.root_path());
    }
    if let Some(entity) = node_stack.find(NODE_NAME, "2.0.0") {
        let _ = std::fs::remove_dir_all(entity.root_path());
    }

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_copies_files_to_storage() {
    const TARGET_NODE_NAME: &str = "copy_test_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

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

    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");

    let copied_path = entity.root_path();
    assert_eq!(
        add_result.snapshot_path.as_path(),
        copied_path,
        "snapshot_path should match copied node path"
    );

    // Verify the file was copied
    let copied_file = copied_path.join("test_file.txt");
    assert!(copied_file.exists(), "test_file.txt should be copied");
    let content = std::fs::read_to_string(&copied_file).expect("should read copied file");
    assert_eq!(content, test_file_content, "file content should match");

    // Verify the subdirectory and nested file were copied
    let copied_nested = copied_path.join("subdir").join("nested_file.txt");
    assert!(copied_nested.exists(), "nested file should be copied");
    let nested_content = std::fs::read_to_string(&copied_nested).expect("should read nested file");
    assert_eq!(
        nested_content, "nested content",
        "nested content should match"
    );

    // Clean up
    let _ = std::fs::remove_dir_all(copied_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_runs_add_cmd() {
    const TARGET_NODE_NAME: &str = "add_cmd_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // add_cmd creates a marker file to prove it was executed
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                add_cmd: ["touch", "{ADD_CMD_MARKER_FILE}"],
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");

    let copied_path = entity.root_path();

    // Verify that add_cmd was executed on the COPIED folder (not the source)
    // by checking the marker file exists only in the copied path
    let marker_file = copied_path.join(ADD_CMD_MARKER_FILE);
    assert!(
        marker_file.exists(),
        "add_cmd should have created marker file at {}",
        marker_file.display()
    );

    // Verify add_cmd did NOT run on the source directory
    let source_marker = source_dir.path().join(ADD_CMD_MARKER_FILE);
    assert!(
        !source_marker.exists(),
        "add_cmd should NOT have created marker file in source dir at {}",
        source_marker.display()
    );

    // Clean up
    let _ = std::fs::remove_dir_all(copied_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_add_cmd_failure_fails_add() {
    const TARGET_NODE_NAME: &str = "add_cmd_fail_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // add_cmd that will fail (non-existent command)
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                add_cmd: ["this_command_does_not_exist_12345"],
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    // Node should not be in the stack
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when add_cmd fails"
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_add_cmd_nonzero_exit_fails_add() {
    const TARGET_NODE_NAME: &str = "add_cmd_exit_fail_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // add_cmd that exits with non-zero status
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                add_cmd: ["sh", "-c", "exit 1"],
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    // Node should not be in the stack
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when add_cmd fails"
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_streams_stdout_and_stderr() {
    const TARGET_NODE_NAME: &str = "stream_output_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const STDOUT_MARKER: &str = "peppy_add_stdout_marker";
    const STDERR_MARKER: &str = "peppy_add_stderr_marker";

    let started_master = start_master_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                add_cmd: ["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2"],
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    // Use wildcard caller IDs so mock pub/sub can match feedback topics with "*" segments.
    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    let _ = std::fs::remove_dir_all(&add_result.snapshot_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_fingerprint_mismatch() {
    const TARGET_NODE_NAME: &str = "fingerprint_mismatch_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    // Write the config file only (without fingerprint)
    let config_path = source_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&config_path, &peppy_json5).expect("failed to write peppy.json5");

    // Create a wrong fingerprint that won't match the actual peppy.json5 content
    config::fingerprint::create_wrong_codegen_fingerprint(
        &config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_writes_log_file() {
    const TARGET_NODE_NAME: &str = "log_file_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const STDOUT_MARKER: &str = "peppy_logfile_stdout_marker";
    const STDERR_MARKER: &str = "peppy_logfile_stderr_marker";

    let started_master = start_master_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                add_cmd: ["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2"],
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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
    let log_dir = logs_dir_add();
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

    // Clean up log file
    let _ = std::fs::remove_file(&add_result.log_path);

    // Clean up snapshot directory
    let _ = std::fs::remove_dir_all(&add_result.snapshot_path);

    started_master.task.abort();
}

/// Tests that a new goal can be processed after a previous action was abandoned
/// (goal accepted but result never polled).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_abandoned_action_does_not_block_next_goal() {
    const FIRST_NODE_NAME: &str = "abandoned_node";
    const FIRST_NODE_TAG: &str = "0.1.0";
    const SECOND_NODE_NAME: &str = "second_node";
    const SECOND_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    // Create first node source directory
    let first_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let first_peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{FIRST_NODE_NAME}",
                tag: "{FIRST_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(first_source_dir.path(), &first_peppy_json5);

    // Write git hash file for first node
    let first_peppy_dir = first_source_dir.path().join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&first_peppy_dir).expect("failed to create .peppy dir");
    std::fs::write(first_peppy_dir.join("git.hash"), TEST_GIT_HASH)
        .expect("failed to write git hash");

    // Send first goal but DON'T wait for result (simulating abandoned action)
    let first_goal = NodeAddGoal::new(first_source_dir.path(), TEST_GIT_HASH);
    let first_goal_payload = first_goal.encode().expect("failed to encode goal");

    let first_action_handle = ActionMessenger::send_goal(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_ADD_ACTION,
        Some(&started_master.master_node_name),
        None,
        first_goal_payload,
        QoSProfile::default(),
        GOAL_TIMEOUT,
    )
    .await
    .expect("first goal should be sent");

    // Verify first goal was accepted
    let first_goal_response_payload = first_action_handle.goal_response().payload().to_bytes();
    let first_goal_response = NodeAddGoalResponse::decode(&first_goal_response_payload)
        .expect("failed to decode first goal response");
    assert!(
        first_goal_response.accepted,
        "first goal should be accepted"
    );

    // Wait for the first action to complete (but don't poll for result)
    // The add operation should complete quickly since there's no add_cmd
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Now send second goal - this should succeed even though we never polled
    // for the first action's result
    let second_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let second_peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{SECOND_NODE_NAME}",
                tag: "{SECOND_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(second_source_dir.path(), &second_peppy_json5);

    let second_add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    // Clean up snapshot directories
    if let Some(entity) = node_stack.find(FIRST_NODE_NAME, FIRST_NODE_TAG) {
        let _ = std::fs::remove_dir_all(entity.root_path());
    }
    let _ = std::fs::remove_dir_all(&second_add_result.snapshot_path);

    started_master.task.abort();
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

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5_v1 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
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

    let entity_v1 = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should exist after v1");
    let snapshot_v1 = entity_v1.root_path().to_path_buf();

    let instance_id_1 = config::node::Name::new(INSTANCE_1).expect("valid instance id 1");
    let instance_id_2 = config::node::Name::new(INSTANCE_2).expect("valid instance id 2");
    node_stack
        .add_instance(NODE_NAME, NODE_TAG, Some(&instance_id_1), None)
        .expect("add_instance for instance 1 should succeed");
    node_stack
        .add_instance(NODE_NAME, NODE_TAG, Some(&instance_id_2), None)
        .expect("add_instance for instance 2 should succeed");

    let instance_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_master.shared_messenger));

    let (called_tx_1, called_rx_1) = oneshot::channel::<()>();
    let called_tx_1 = Arc::new(Mutex::new(Some(called_tx_1)));
    let allow_shutdown_1 = Arc::new(Notify::new());
    let allow_shutdown_1_clone = Arc::clone(&allow_shutdown_1);
    let mut shutdown_endpoint_1 = ServiceMessenger::listen(
        &instance_messenger,
        &started_master.master_node_name,
        INSTANCE_1,
        NODE_NAME,
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service for instance 1");
    let shutdown_task_1 = tokio::spawn({
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
                        Ok(payload.to_bytes())
                    }
                })
                .await
        }
    });

    let (called_tx_2, called_rx_2) = oneshot::channel::<()>();
    let called_tx_2 = Arc::new(Mutex::new(Some(called_tx_2)));
    let allow_shutdown_2 = Arc::new(Notify::new());
    let allow_shutdown_2_clone = Arc::clone(&allow_shutdown_2);
    let mut shutdown_endpoint_2 = ServiceMessenger::listen(
        &instance_messenger,
        &started_master.master_node_name,
        INSTANCE_2,
        NODE_NAME,
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service for instance 2");
    let shutdown_task_2 = tokio::spawn({
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
                        Ok(payload.to_bytes())
                    }
                })
                .await
        }
    });

    // Ensure shutdown services are fully registered before starting the overwrite.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5_v2 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                exposes: {{
                    topics: [{{ name: "/example" }}]
                }}
            }}
        }}"#
    );
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    // Use wildcard caller IDs so mock pub/sub can match feedback topics with "*" segments.
    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();

    let caller_handle = started_master.caller_handle.clone();
    let master_node_name = started_master.master_node_name.clone();
    let source_path_v2 = source_dir_v2.path().to_path_buf();
    let add_task = tokio::spawn(async move {
        send_node_add_and_wait(
            &caller_handle,
            &master_node_name,
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
    let entity_mid = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should still exist while waiting for shutdown 1 to complete");
    assert_eq!(
        entity_mid.root_path(),
        snapshot_v1.as_path(),
        "node should not be overwritten before instance 1 is shutdown"
    );

    // Allow instance 1 shutdown response, then wait for instance 2 shutdown request.
    allow_shutdown_1.notify_one();

    tokio::time::timeout(Duration::from_secs(5), called_rx_2)
        .await
        .expect("shutdown request for instance 2 should arrive within timeout")
        .expect("shutdown channel for instance 2 should not be dropped");
    let entity_mid = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should still exist while waiting for shutdown 2 to complete");
    assert_eq!(
        entity_mid.root_path(),
        snapshot_v1.as_path(),
        "node should not be overwritten before instance 2 is shutdown"
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

    let entity_v2 = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should exist after overwrite");
    assert_eq!(
        entity_v2.root_path(),
        add_v2.snapshot_path.as_path(),
        "node stack should point to the new snapshot path"
    );
    assert_eq!(
        entity_v2.instances().len(),
        0,
        "instances should be stopped before overwrite completes"
    );
    assert!(
        !snapshot_v1.exists(),
        "previous snapshot should be removed from storage: {}",
        snapshot_v1.display()
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

    // Clean up log files and snapshot directory created by successful adds.
    let _ = std::fs::remove_file(&add_v1.log_path);
    let _ = std::fs::remove_file(&add_v2.log_path);
    let _ = std::fs::remove_dir_all(&add_v2.snapshot_path);

    shutdown_task_1.abort();
    shutdown_task_2.abort();
    started_master.task.abort();
}
