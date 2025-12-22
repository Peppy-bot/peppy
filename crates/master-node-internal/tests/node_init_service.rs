mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use config::node::NodeConfigParser;
use config::peppy_config::BuildSystem;
use master_node::encoding::NodeInitRequest;
use std::time::Duration;
use tempfile::Builder;

// Long running test
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_init_rust_success() {
    let (client, _server) = setup_test_master_node().await;

    let temp_dir = Builder::new()
        .prefix("node_init")
        .tempdir()
        .expect("failed to create tempdir");
    let node_root_dir = temp_dir.path().to_path_buf();
    let node_name = "my_rust_node";

    let request =
        NodeInitRequest::new(&node_root_dir, node_name).with_build_system(BuildSystem::Cargo);

    let node_init_response = request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(
        node_init_response.success,
        "node_init should succeed, got error: {}",
        node_init_response.error_message
    );
    assert!(
        node_init_response.error_message.is_empty(),
        "expected empty error message, got: {}",
        node_init_response.error_message
    );

    // Verify the node directory was created
    let node_dir = node_root_dir.join(node_name);
    assert!(
        node_dir.exists(),
        "expected node directory to be created at {}",
        node_dir.display()
    );

    // Verify peppy.json5 was created
    assert!(
        node_dir.join(config::consts::NODE_CONFIG_FILE).exists(),
        "expected peppy.json5 to be created"
    );

    // Verify peppy.json5 can be parsed
    let peppy_config =
        NodeConfigParser::from_path(&node_dir.join(config::consts::NODE_CONFIG_FILE))
            .expect("peppy.json5 should be valid");
    assert_eq!(peppy_config.manifest.name.as_str(), node_name);

    // Verify Cargo.toml was created
    let cargo_toml_path = node_dir.join("Cargo.toml");
    assert!(
        cargo_toml_path.exists(),
        "expected Cargo.toml to be created at {}",
        cargo_toml_path.display()
    );

    // Verify Cargo.toml contains the node name
    let cargo_content =
        std::fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
    assert!(
        cargo_content.contains(&format!("name = \"{}\"", node_name)),
        "Cargo.toml should contain the node name"
    );

    // Verify Cargo.toml contains peppygen dependency
    assert!(
        cargo_content.contains(config::consts::PEPPYGEN_OUTPUT_PATH),
        "Cargo.toml should contain peppygen dependency path, got: {}",
        cargo_content
    );

    // Verify src/main.rs was created
    assert!(
        node_dir.join("src/main.rs").exists(),
        "expected src/main.rs to be created"
    );

    // Verify .gitignore was created
    assert!(
        node_dir.join(".gitignore").exists(),
        "expected .gitignore to be created"
    );

    // Verify peppygen was generated
    let peppygen_dir = node_dir.join(config::consts::PEPPYGEN_OUTPUT_PATH);
    assert!(
        peppygen_dir.join("Cargo.toml").exists(),
        "expected peppygen Cargo.toml at {}",
        peppygen_dir.display()
    );
    assert!(
        peppygen_dir.join("src/lib.rs").exists(),
        "expected peppygen src/lib.rs at {}",
        peppygen_dir.display()
    );

    // Compile the project and check that the compilation went fine
    let cargo_output = std::process::Command::new("cargo")
        .arg("build")
        .current_dir(&node_dir)
        .output()
        .expect("failed to invoke cargo build on generated node");
    assert!(
        cargo_output.status.success(),
        "cargo build failed for generated node with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        cargo_output.status.code(),
        String::from_utf8_lossy(&cargo_output.stdout),
        String::from_utf8_lossy(&cargo_output.stderr)
    );

    todo!("Actually run the project and check that the node is spinning")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_init_python_success() {
    let (client, _server) = setup_test_master_node().await;

    let temp_dir = Builder::new()
        .prefix("node_init")
        .tempdir()
        .expect("failed to create tempdir");
    let node_root_dir = temp_dir.path().to_path_buf();
    let node_name = "my_python_node";

    let request =
        NodeInitRequest::new(&node_root_dir, node_name).with_build_system(BuildSystem::Python);

    let node_init_response = request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(
        node_init_response.success,
        "node_init should succeed, got error: {}",
        node_init_response.error_message
    );

    // Verify the node directory was created
    let node_dir = node_root_dir.join(node_name);
    assert!(
        node_dir.exists(),
        "expected node directory to be created at {}",
        node_dir.display()
    );

    // Verify peppy.json5 was created
    assert!(
        node_dir.join(config::consts::NODE_CONFIG_FILE).exists(),
        "expected peppy.json5 to be created"
    );

    // Verify pyproject.toml was created
    let pyproject_path = node_dir.join("pyproject.toml");
    assert!(
        pyproject_path.exists(),
        "expected pyproject.toml to be created at {}",
        pyproject_path.display()
    );

    // Verify pyproject.toml contains the node name
    let pyproject_content =
        std::fs::read_to_string(&pyproject_path).expect("failed to read pyproject.toml");
    assert!(
        pyproject_content.contains(&format!("name = \"{}\"", node_name)),
        "pyproject.toml should contain the node name"
    );

    // Verify main.py was created
    assert!(
        node_dir.join("main.py").exists(),
        "expected main.py to be created"
    );

    // Verify .gitignore was created
    assert!(
        node_dir.join(".gitignore").exists(),
        "expected .gitignore to be created"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_init_fails_if_directory_exists() {
    let (client, _server) = setup_test_master_node().await;

    let temp_dir = Builder::new()
        .prefix("node_init")
        .tempdir()
        .expect("failed to create tempdir");
    let node_root_dir = temp_dir.path().to_path_buf();
    let node_name = "existing_node";

    // Pre-create the node directory
    let node_dir = node_root_dir.join(node_name);
    std::fs::create_dir_all(&node_dir).expect("failed to create existing node directory");

    let request =
        NodeInitRequest::new(&node_root_dir, node_name).with_build_system(BuildSystem::Cargo);

    let node_init_response = request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(
        !node_init_response.success,
        "node_init should fail when directory exists"
    );
    assert!(
        node_init_response.error_message.contains("already exists"),
        "error message should indicate directory already exists, got: {}",
        node_init_response.error_message
    );
}
