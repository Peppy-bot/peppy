mod common;

use common::{CALLER_INSTANCE_ID, start_core_node_with_mock_messenger};
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::Toolchain;
use config_test_support::assert_contains_all;
use core_node_api::encoding::{NodeInitRequest, NodeInitResponse};
use daemon_config::consts::{
    DEFAULT_PYTHON_BASE_IMAGE, DEFAULT_RUST_BASE_IMAGE, PEPPY_OUTPUT_DIR, PEPPYLIB_OUTPUT_PATH,
};
use peppylib::core_node::transport::poll;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

const NODE_INIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Sends `request` to the `node_init` service of the started core node and
/// returns the response. Panics only when the request itself fails.
async fn send_node_init(
    started_core_node: &common::StartedCoreNode,
    request: &NodeInitRequest,
) -> NodeInitResponse {
    poll(
        request,
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        NODE_INIT_TIMEOUT,
    )
    .await
    .expect("node_init request should complete")
}

/// Asserts the files every initialized node has, whatever its toolchain: the
/// node directory with its git hash, its node config, the generated peppygen
/// output with its codegen fingerprint, and its `.gitignore`.
fn assert_node_scaffold(node_dir: &Path) {
    assert!(
        node_dir.exists(),
        "node_dir should exist at {}",
        node_dir.display()
    );

    let git_hash_file = node_dir.join(PEPPY_OUTPUT_DIR).join("git.hash");
    assert!(
        git_hash_file.exists(),
        "git.hash file should exist at {}",
        git_hash_file.display()
    );

    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    assert!(
        node_config_path.exists(),
        "node config should exist at {}",
        node_config_path.display()
    );

    let peppygen_dir = node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        peppygen_dir.display()
    );

    assert!(
        config::fingerprint::read_codegen_fingerprint(&node_config_path, PEPPYGEN_OUTPUT_PATH)
            .is_ok(),
        "fingerprint file should exist in peppygen directory"
    );

    let gitignore_path = node_dir.join(".gitignore");
    assert!(
        gitignore_path.exists(),
        ".gitignore should exist at {}",
        gitignore_path.display()
    );
}

/// Asserts the Cargo project of a Rust node: a `Cargo.toml` that depends on
/// the generated peppygen crate, and a `src/main.rs`.
fn assert_cargo_project(node_dir: &Path) {
    let cargo_toml_path = node_dir.join("Cargo.toml");
    assert!(
        cargo_toml_path.exists(),
        "node Cargo.toml should exist at {}",
        cargo_toml_path.display()
    );
    let cargo_toml =
        fs::read_to_string(&cargo_toml_path).expect("failed to read generated Cargo.toml");
    assert!(
        cargo_toml.contains("peppygen"),
        "Cargo.toml should contain peppygen dependency, got:\n{}",
        cargo_toml
    );
    assert!(
        cargo_toml.contains(PEPPYGEN_OUTPUT_PATH),
        "Cargo.toml should reference generated peppygen path, got:\n{}",
        cargo_toml
    );

    let main_rs_path = node_dir.join("src/main.rs");
    assert!(
        main_rs_path.exists(),
        "src/main.rs should exist at {}",
        main_rs_path.display()
    );
}

/// Asserts the uv project of the Python node `node_name`: a `pyproject.toml`
/// that depends on the generated peppygen package and the deployed peppylib,
/// and the `src/<node_name>` package with its `__init__.py` and `__main__.py`.
fn assert_uv_project(node_dir: &Path, node_name: &str) {
    let pyproject_toml_path = node_dir.join("pyproject.toml");
    assert!(
        pyproject_toml_path.exists(),
        "pyproject.toml should exist at {}",
        pyproject_toml_path.display()
    );
    let pyproject_toml =
        fs::read_to_string(&pyproject_toml_path).expect("failed to read generated pyproject.toml");
    assert!(
        pyproject_toml.contains("peppygen"),
        "pyproject.toml should contain peppygen dependency, got:\n{}",
        pyproject_toml
    );
    assert!(
        pyproject_toml.contains(PEPPYGEN_OUTPUT_PATH),
        "pyproject.toml should reference generated peppygen path, got:\n{}",
        pyproject_toml
    );
    assert!(
        pyproject_toml.contains("peppylib"),
        "pyproject.toml should contain peppylib dependency, got:\n{}",
        pyproject_toml
    );
    assert!(
        pyproject_toml.contains(PEPPYLIB_OUTPUT_PATH),
        "pyproject.toml should reference deployed peppylib path, got:\n{}",
        pyproject_toml
    );

    let init_py_path = node_dir.join(format!("src/{node_name}/__init__.py"));
    assert!(
        init_py_path.exists(),
        "src/{}/__init__.py should exist at {}",
        node_name,
        init_py_path.display()
    );

    let main_py_path = node_dir.join(format!("src/{node_name}/__main__.py"));
    assert!(
        main_py_path.exists(),
        "src/{}/__main__.py should exist at {}",
        node_name,
        main_py_path.display()
    );
}

/// Asserts the container setup of a node initialized with a container: an
/// `apptainer.def` that bootstraps from `base_image`, and a `container:`
/// section in the node config.
fn assert_container_definition(node_dir: &Path, base_image: &str) {
    let apptainer_def_path = node_dir.join("apptainer.def");
    assert!(
        apptainer_def_path.exists(),
        "apptainer.def should exist at {}",
        apptainer_def_path.display()
    );
    let apptainer_def =
        fs::read_to_string(&apptainer_def_path).expect("failed to read generated apptainer.def");
    assert_contains_all(
        &apptainer_def,
        &["Bootstrap: docker", &format!("From: {base_image}")],
    );

    let node_config = fs::read_to_string(node_dir.join(NODE_CONFIG_FILE))
        .expect("failed to read generated peppy.json5");
    assert_contains_all(&node_config, &["container:"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_init_rust_success() {
    const NODE_NAME: &str = "example_node";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_root = tempdir().expect("failed to create temp nodes root directory");

    let response = send_node_init(
        &started_core_node,
        &NodeInitRequest::new(
            nodes_root.path(),
            NODE_NAME,
            "abc123",
            false,
            Toolchain::Cargo,
        ),
    )
    .await;

    assert!(
        response.success,
        "node_init should succeed, got error: {}",
        response.error_message
    );

    let node_dir = nodes_root.path().join(NODE_NAME);
    assert_node_scaffold(&node_dir);
    assert_cargo_project(&node_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_init_rust_container_success() {
    const NODE_NAME: &str = "example_node";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_root = tempdir().expect("failed to create temp nodes root directory");

    let response = send_node_init(
        &started_core_node,
        &NodeInitRequest::new(
            nodes_root.path(),
            NODE_NAME,
            "abc123",
            true,
            Toolchain::Cargo,
        ),
    )
    .await;

    assert!(
        response.success,
        "node_init should succeed, got error: {}",
        response.error_message
    );

    let node_dir = nodes_root.path().join(NODE_NAME);
    assert_node_scaffold(&node_dir);
    assert_cargo_project(&node_dir);
    assert_container_definition(&node_dir, DEFAULT_RUST_BASE_IMAGE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_init_python_success() {
    const NODE_NAME: &str = "example_node";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_root = tempdir().expect("failed to create temp nodes root directory");

    let response = send_node_init(
        &started_core_node,
        &NodeInitRequest::new(nodes_root.path(), NODE_NAME, "abc123", false, Toolchain::Uv),
    )
    .await;

    assert!(
        response.success,
        "node_init should succeed, got error: {}",
        response.error_message
    );

    let node_dir = nodes_root.path().join(NODE_NAME);
    assert_node_scaffold(&node_dir);
    assert_uv_project(&node_dir, NODE_NAME);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_init_python_container_success() {
    const NODE_NAME: &str = "example_node";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_root = tempdir().expect("failed to create temp nodes root directory");

    let response = send_node_init(
        &started_core_node,
        &NodeInitRequest::new(nodes_root.path(), NODE_NAME, "abc123", true, Toolchain::Uv),
    )
    .await;

    assert!(
        response.success,
        "node_init should succeed, got error: {}",
        response.error_message
    );

    let node_dir = nodes_root.path().join(NODE_NAME);
    assert_node_scaffold(&node_dir);
    assert_uv_project(&node_dir, NODE_NAME);
    assert_container_definition(&node_dir, DEFAULT_PYTHON_BASE_IMAGE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_init_fails_if_directory_exists() {
    const NODE_NAME: &str = "existing_node";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_root = tempdir().expect("failed to create temp nodes root directory");
    let node_dir = nodes_root.path().join(NODE_NAME);

    fs::create_dir_all(&node_dir).expect("failed to pre-create node directory");
    let sentinel_path = node_dir.join("sentinel.txt");
    fs::write(&sentinel_path, "do not overwrite").expect("failed to write sentinel file");

    assert!(
        !node_dir.join(NODE_CONFIG_FILE).exists(),
        "precondition: node config should not exist"
    );
    assert!(
        !node_dir.join(".gitignore").exists(),
        "precondition: .gitignore should not exist"
    );
    assert!(
        !node_dir.join(PEPPYGEN_OUTPUT_PATH).exists(),
        "precondition: peppygen output should not exist"
    );

    let response = send_node_init(
        &started_core_node,
        &NodeInitRequest::new(
            nodes_root.path(),
            NODE_NAME,
            "abc123",
            false,
            Toolchain::Cargo,
        ),
    )
    .await;

    assert!(!response.success, "node_init should fail");
    assert!(
        response
            .error_message
            .contains("Node directory already exists"),
        "error should mention existing directory, got: {}",
        response.error_message
    );

    assert!(
        sentinel_path.exists(),
        "sentinel should still exist at {}",
        sentinel_path.display()
    );
    assert!(
        !node_dir.join(NODE_CONFIG_FILE).exists(),
        "node config should not be created when node_dir already exists"
    );
    assert!(
        !node_dir.join(".gitignore").exists(),
        ".gitignore should not be created when node_dir already exists"
    );
    assert!(
        !node_dir.join(PEPPYGEN_OUTPUT_PATH).exists(),
        "peppygen output should not be created when node_dir already exists"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_init_fails_if_node_directory_cannot_be_created() {
    const NODE_NAME: &str = "unreachable_node";

    let started_core_node = start_core_node_with_mock_messenger().await;

    // The nodes root is a regular file, so the node directory under it does
    // not exist yet and cannot be created either.
    let scratch = tempdir().expect("failed to create temp scratch directory");
    let nodes_root = scratch.path().join("not_a_directory");
    fs::write(&nodes_root, "a file, not a directory").expect("failed to write nodes root file");

    let response = send_node_init(
        &started_core_node,
        &NodeInitRequest::new(&nodes_root, NODE_NAME, "abc123", false, Toolchain::Cargo),
    )
    .await;

    assert!(!response.success, "node_init should fail");
    assert!(
        response
            .error_message
            .starts_with("Failed to create node directory: "),
        "error should say the node directory could not be created, got: {}",
        response.error_message
    );
    assert_eq!(
        fs::read_to_string(&nodes_root).expect("failed to read nodes root file"),
        "a file, not a directory",
        "the nodes root file should be left untouched"
    );
}
