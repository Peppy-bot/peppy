use super::common::setup;
use peppy::test_support::LogCapture;
use std::time::Duration;

use config::node::Toolchain;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeInitBuilder, NodeName};

#[test]
fn node_cargo_init_command_success() {
    let (_rt, serve, node_ctx, node_dir) = setup();
    assert!(
        !serve.core_node_name().is_empty(),
        "core_node_name should not be empty"
    );

    let node_name = "test_node";

    // Set up logging for the node command
    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    // Execute the node create command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            toolchain: Toolchain::Cargo,
            to_dir: None,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node create command should succeed");

    // Verify the node directory was created
    let created_node_dir = node_dir.path().join(node_name);
    assert!(
        created_node_dir.exists(),
        "node directory should exist at {}",
        created_node_dir.display()
    );

    // Verify peppy.json5 was created
    assert!(
        created_node_dir.join("peppy.json5").exists(),
        "peppy.json5 should exist in the node directory"
    );

    // Verify Cargo.toml was created (for Rust build system)
    assert!(
        created_node_dir.join("Cargo.toml").exists(),
        "Cargo.toml should exist in the node directory"
    );

    // Verify src/main.rs was created
    assert!(
        created_node_dir.join("src/main.rs").exists(),
        "src/main.rs should exist in the node directory"
    );

    // The lib/main split: setup lives in src/lib.rs (importable by tests),
    // main.rs delegates to it, and a smoke test boots the node through the
    // generated harness.
    let lib_rs = std::fs::read_to_string(created_node_dir.join("src/lib.rs"))
        .expect("src/lib.rs should exist in the node directory");
    assert!(
        lib_rs.contains("pub async fn setup"),
        "src/lib.rs should scaffold the importable setup entry point:\n{lib_rs}"
    );
    let main_rs = std::fs::read_to_string(created_node_dir.join("src/main.rs"))
        .expect("src/main.rs should be readable");
    assert!(
        main_rs.contains(&format!("{node_name}::setup")),
        "src/main.rs should delegate to the library's setup:\n{main_rs}"
    );
    let smoke = std::fs::read_to_string(created_node_dir.join("tests/smoke.rs"))
        .expect("tests/smoke.rs should exist in the node directory");
    assert!(
        smoke.contains("fixtures::harness::Harness")
            && smoke.contains(&format!("{node_name}::setup")),
        "tests/smoke.rs should boot the node through the generated harness:\n{smoke}"
    );
    let cargo_toml = std::fs::read_to_string(created_node_dir.join("Cargo.toml"))
        .expect("Cargo.toml should be readable");
    assert!(
        cargo_toml.contains("[dev-dependencies]")
            && cargo_toml.contains(r#"features = ["testing"]"#),
        "Cargo.toml should carry the testing-featured peppygen dev-dependency:\n{cargo_toml}"
    );

    // Verify .gitignore was created
    assert!(
        created_node_dir.join(".gitignore").exists(),
        ".gitignore should exist in the node directory"
    );

    // Verify the logs contain success message
    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Successfully created node '{}'", node_name)),
        "logs should contain success message. Logs:\n{}",
        logs
    );
}

#[test]
fn node_cargo_init_with_container_generates_apptainer_def() {
    let (_rt, _serve, node_ctx, node_dir) = setup();
    let node_name = "test_node";

    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            toolchain: Toolchain::Cargo,
            to_dir: None,
            with_container: true,
        },
    }
    .execute(&node_ctx)
    .expect("node create command should succeed");

    let created_node_dir = node_dir.path().join(node_name);
    assert!(
        created_node_dir.join("apptainer.def").exists(),
        "apptainer.def should exist when --with-container is set"
    );
}

#[test]
fn node_cargo_init_without_container_omits_apptainer_def() {
    let (_rt, _serve, node_ctx, node_dir) = setup();
    let node_name = "test_node";

    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            toolchain: Toolchain::Cargo,
            to_dir: None,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node create command should succeed");

    let created_node_dir = node_dir.path().join(node_name);
    assert!(
        !created_node_dir.join("apptainer.def").exists(),
        "apptainer.def should not exist when --with-container is not set"
    );
}

#[test]
fn node_init_with_to_dir_creates_the_node_in_that_directory() {
    let (_rt, _serve, node_ctx, working_dir) = setup();
    let target_dir = tempfile::tempdir().expect("failed to create temp target dir");
    let node_name = "test_node";

    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            toolchain: Toolchain::Cargo,
            to_dir: Some(target_dir.path().to_path_buf()),
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node create command should succeed");

    assert!(
        target_dir
            .path()
            .join(node_name)
            .join("peppy.json5")
            .exists(),
        "the node should be created under --to-dir"
    );
    assert!(
        !working_dir.path().join(node_name).exists(),
        "--to-dir should replace the working directory as the parent of the node"
    );
    let logs = log_capture.logs();
    let expected = format!(
        "Successfully created node '{node_name}' at {}/{node_name}",
        target_dir.path().display()
    );
    assert!(
        logs.contains(&expected),
        "logs should name the --to-dir location. Logs:\n{logs}"
    );
}

#[test]
fn node_init_refuses_an_existing_node_directory() {
    let (_rt, _serve, node_ctx, node_dir) = setup();
    let node_name = "test_node";
    let existing_node_dir = node_dir.path().join(node_name);
    std::fs::create_dir(&existing_node_dir).expect("failed to create the existing node dir");

    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    let err = NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            toolchain: Toolchain::Cargo,
            to_dir: None,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect_err("node init should refuse a directory that already exists");

    let expected = format!(
        "Node directory already exists: {}",
        existing_node_dir.display()
    );
    let err_msg = err.to_string();
    assert!(
        err_msg.contains(&expected),
        "the error should carry the refusal of the daemon. Got: {err_msg}"
    );
    let logs = log_capture.logs();
    assert!(
        !logs.contains("Successfully created node"),
        "a refused init should not log a success. Logs:\n{logs}"
    );
}

#[test]
fn node_uv_init_command_success() {
    let (_rt, serve, node_ctx, node_dir) = setup();
    assert!(
        !serve.core_node_name().is_empty(),
        "core_node_name should not be empty"
    );

    let node_name = "test_node";

    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    // Use NodeInitBuilder directly with no timeout (signal-based waiting)
    // to avoid flakiness from wall-clock dependencies in CI.
    NodeInitBuilder::new(
        &node_ctx,
        NodeName::new(node_name).expect("valid node name"),
        Toolchain::Uv,
        false,
    )
    .with_timeout(None::<Duration>)
    .build()
    .expect("node create command should succeed");

    let created_node_dir = node_dir.path().join(node_name);
    assert!(
        created_node_dir.exists(),
        "node directory should exist at {}",
        created_node_dir.display()
    );

    assert!(
        created_node_dir.join("peppy.json5").exists(),
        "peppy.json5 should exist in the node directory"
    );

    assert!(
        created_node_dir.join("pyproject.toml").exists(),
        "pyproject.toml should exist in the node directory"
    );

    assert!(
        created_node_dir
            .join(format!("src/{node_name}/__init__.py"))
            .exists(),
        "src/{node_name}/__init__.py should exist in the node directory"
    );

    assert!(
        created_node_dir
            .join(format!("src/{node_name}/__main__.py"))
            .exists(),
        "src/{node_name}/__main__.py should exist in the node directory"
    );

    // The scaffolded smoke test boots the node through the generated
    // harness; pyproject's dev group carries the pytest tooling for it.
    let smoke = std::fs::read_to_string(created_node_dir.join("tests/test_smoke.py"))
        .expect("tests/test_smoke.py should exist in the node directory");
    assert!(
        smoke.contains("peppygen.fixtures")
            && smoke.contains(&format!("from {node_name}.__main__ import setup")),
        "tests/test_smoke.py should boot the node through the generated harness:\n{smoke}"
    );
    let pyproject = std::fs::read_to_string(created_node_dir.join("pyproject.toml"))
        .expect("pyproject.toml should be readable");
    assert!(
        pyproject.contains("pytest-asyncio") && pyproject.contains("asyncio_mode = \"auto\""),
        "pyproject.toml should carry the pytest dev tooling:\n{pyproject}"
    );

    assert!(
        created_node_dir.join(".gitignore").exists(),
        ".gitignore should exist in the node directory"
    );

    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Successfully created node '{}'", node_name)),
        "logs should contain success message. Logs:\n{}",
        logs
    );
}
