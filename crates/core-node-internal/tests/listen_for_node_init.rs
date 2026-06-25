mod common;

use common::{CALLER_INSTANCE_ID, start_core_node_with_mock_messenger};
use config::consts::{
    DEFAULT_PYTHON_BASE_IMAGE, DEFAULT_RUST_BASE_IMAGE, NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR,
    PEPPYGEN_OUTPUT_PATH, PEPPYLIB_OUTPUT_PATH,
};
use config::node::Toolchain;
use config_test_support::assert_contains_all;
use core_node_api::encoding::NodeInitRequest;
use peppylib::core_node::transport::poll_node_init;
use std::fs;
use std::time::Duration;
use tempfile::tempdir;

const NODE_INIT_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_init_rust_success() {
    const NODE_NAME: &str = "example_node";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_root = tempdir().expect("failed to create temp nodes root directory");

    let response = poll_node_init(
        &NodeInitRequest::new(
            nodes_root.path(),
            NODE_NAME,
            "abc123",
            false,
            Toolchain::Cargo,
        ),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        NODE_INIT_TIMEOUT,
    )
    .await
    .expect("node_init request should complete");

    assert!(
        response.success,
        "node_init should succeed, got error: {}",
        response.error_message
    );

    let node_dir = nodes_root.path().join(NODE_NAME);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_init_rust_container_success() {
    const NODE_NAME: &str = "example_node";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_root = tempdir().expect("failed to create temp nodes root directory");

    let response = poll_node_init(
        &NodeInitRequest::new(
            nodes_root.path(),
            NODE_NAME,
            "abc123",
            true,
            Toolchain::Cargo,
        ),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        NODE_INIT_TIMEOUT,
    )
    .await
    .expect("node_init request should complete");

    assert!(
        response.success,
        "node_init should succeed, got error: {}",
        response.error_message
    );

    let node_dir = nodes_root.path().join(NODE_NAME);
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

    let peppygen_dir = node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        peppygen_dir.display()
    );

    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    assert!(
        node_config_path.exists(),
        "node config should exist at {}",
        node_config_path.display()
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
        &[
            "Bootstrap: docker",
            &format!("From: {DEFAULT_RUST_BASE_IMAGE}"),
        ],
    );

    let node_config =
        fs::read_to_string(&node_config_path).expect("failed to read generated peppy.json5");
    assert_contains_all(&node_config, &["container:"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_init_python_success() {
    const NODE_NAME: &str = "example_node";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_root = tempdir().expect("failed to create temp nodes root directory");

    let response = poll_node_init(
        &NodeInitRequest::new(nodes_root.path(), NODE_NAME, "abc123", false, Toolchain::Uv),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        NODE_INIT_TIMEOUT,
    )
    .await
    .expect("node_init request should complete");

    assert!(
        response.success,
        "node_init should succeed, got error: {}",
        response.error_message
    );

    let node_dir = nodes_root.path().join(NODE_NAME);
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

    let init_py_path = node_dir.join(format!("src/{NODE_NAME}/__init__.py"));
    assert!(
        init_py_path.exists(),
        "src/{}/__init__.py should exist at {}",
        NODE_NAME,
        init_py_path.display()
    );

    let main_py_path = node_dir.join(format!("src/{NODE_NAME}/__main__.py"));
    assert!(
        main_py_path.exists(),
        "src/{}/__main__.py should exist at {}",
        NODE_NAME,
        main_py_path.display()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_init_python_container_success() {
    const NODE_NAME: &str = "example_node";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_root = tempdir().expect("failed to create temp nodes root directory");

    let response = poll_node_init(
        &NodeInitRequest::new(nodes_root.path(), NODE_NAME, "abc123", true, Toolchain::Uv),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        NODE_INIT_TIMEOUT,
    )
    .await
    .expect("node_init request should complete");

    assert!(
        response.success,
        "node_init should succeed, got error: {}",
        response.error_message
    );

    let node_dir = nodes_root.path().join(NODE_NAME);
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

    let init_py_path = node_dir.join(format!("src/{NODE_NAME}/__init__.py"));
    assert!(
        init_py_path.exists(),
        "src/{}/__init__.py should exist at {}",
        NODE_NAME,
        init_py_path.display()
    );

    let main_py_path = node_dir.join(format!("src/{NODE_NAME}/__main__.py"));
    assert!(
        main_py_path.exists(),
        "src/{}/__main__.py should exist at {}",
        NODE_NAME,
        main_py_path.display()
    );

    let peppygen_dir = node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        peppygen_dir.display()
    );

    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    assert!(
        node_config_path.exists(),
        "node config should exist at {}",
        node_config_path.display()
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
        &[
            "Bootstrap: docker",
            &format!("From: {DEFAULT_PYTHON_BASE_IMAGE}"),
        ],
    );

    let node_config =
        fs::read_to_string(&node_config_path).expect("failed to read generated peppy.json5");
    assert_contains_all(&node_config, &["container:"]);
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

    let response = poll_node_init(
        &NodeInitRequest::new(
            nodes_root.path(),
            NODE_NAME,
            "abc123",
            false,
            Toolchain::Cargo,
        ),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        NODE_INIT_TIMEOUT,
    )
    .await
    .expect("node_init request should complete");

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
