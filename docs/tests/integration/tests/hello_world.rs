use docs_integration_tests::{peppy_binary, workspace_root};
use peppy::test_support::ServeCommandEmulation;
use std::path::{Path, PathBuf};
use std::process::Command;

struct NodeSetup {
    daemon_state_path: PathBuf,
    #[allow(dead_code)]
    temp_dir: tempfile::TempDir,
    #[allow(dead_code)]
    rt: tokio::runtime::Runtime,
    #[allow(dead_code)]
    serve: ServeCommandEmulation,
}

fn setup_node(node_name: &str, node_dir: impl AsRef<Path>) -> NodeSetup {
    let peppy = peppy_binary();

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_zenoh())
        .expect("failed to start serve emulation");
    let daemon_state_path = serve.daemon_state_path().to_path_buf();

    // Create a node in a tempdir with `peppy node init`
    let temp_dir = tempfile::tempdir().expect("failed to create temp directory");
    let nodes_root = temp_dir.path();

    // Run cargo clean to ensure a fresh build state
    Command::new("cargo")
        .arg("clean")
        .current_dir(&node_dir)
        .output()
        .ok();

    let init_output = Command::new(peppy)
        .args(["node", "init", node_name])
        .env("PEPPY_DAEMON_STATE_FILE", daemon_state_path.as_os_str())
        .current_dir(nodes_root)
        .output()
        .expect("failed to run peppy node init");

    assert!(
        init_output.status.success(),
        "peppy node init failed: {}",
        String::from_utf8_lossy(&init_output.stderr)
    );

    // Run `peppy node sync` to regenerate peppygen after config change
    let sync_output = Command::new(peppy)
        .args(["node", "sync"])
        .env("PEPPY_DAEMON_STATE_FILE", daemon_state_path.as_os_str())
        .current_dir(&node_dir)
        .output()
        .expect("failed to run peppy node sync");

    assert!(
        sync_output.status.success(),
        "peppy node sync failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sync_output.stdout),
        String::from_utf8_lossy(&sync_output.stderr)
    );

    NodeSetup {
        daemon_state_path,
        temp_dir,
        rt,
        serve,
    }
}

#[test]
fn hello_world() {
    const NODE_NAME: &str = "hello_world";
    let peppy = peppy_binary();

    let workspace = workspace_root();
    let node_dir = workspace.join("docs/src/content/docs/guides/snippets/rust/hello_world");

    let setup = setup_node(NODE_NAME, &node_dir);
    let daemon_state_path = &setup.daemon_state_path;

    // 3. Run `peppy node add .` in that folder and check that the output is valid
    let add_output = Command::new(peppy)
        .args(["node", "add", "."])
        .env("PEPPY_DAEMON_STATE_FILE", daemon_state_path.as_os_str())
        .current_dir(&node_dir)
        .output()
        .expect("failed to run peppy node add");

    assert!(
        add_output.status.success(),
        "peppy node add failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add_output.stdout),
        String::from_utf8_lossy(&add_output.stderr)
    );

    // 4. Run `peppy node start my_first_node:0.1.0` and check that the command succeeds
    let start_output = Command::new(peppy)
        .args(["node", "start", &format!("{NODE_NAME}:0.1.0")])
        .env("PEPPY_DAEMON_STATE_FILE", daemon_state_path.as_os_str())
        .current_dir(&node_dir)
        .output()
        .expect("failed to run peppy node start");

    assert!(
        start_output.status.success(),
        "peppy node start failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start_output.stdout),
        String::from_utf8_lossy(&start_output.stderr)
    );
}

#[test]
fn hello_world_param() {
    const NODE_NAME: &str = "hello_world_param";
    let peppy = peppy_binary();

    let workspace = workspace_root();
    let node_dir = workspace.join("docs/src/content/docs/guides/snippets/rust/hello_world_param");

    let setup = setup_node(NODE_NAME, &node_dir);
    let daemon_state_path = &setup.daemon_state_path;

    let add_output = Command::new(peppy)
        .args(["node", "add", "."])
        .env("PEPPY_DAEMON_STATE_FILE", daemon_state_path.as_os_str())
        .current_dir(&node_dir)
        .output()
        .expect("failed to run peppy node add");

    assert!(
        add_output.status.success(),
        "peppy node add failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add_output.stdout),
        String::from_utf8_lossy(&add_output.stderr)
    );

    let start_output = Command::new(peppy)
        .args(["node", "start", &format!("{NODE_NAME}:0.1.0")])
        .env("PEPPY_DAEMON_STATE_FILE", daemon_state_path.as_os_str())
        .current_dir(&node_dir)
        .output()
        .expect("failed to run peppy node start");

    assert!(
        start_output.status.success(),
        "peppy node start failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start_output.stdout),
        String::from_utf8_lossy(&start_output.stderr)
    );
}

#[test]
fn first_node() {
    const NODE_NAME: &str = "first_node";
    let peppy = peppy_binary();

    let workspace = workspace_root();
    let node_dir = workspace.join("docs/src/content/docs/guides/snippets/rust/first_node");

    let setup = setup_node(NODE_NAME, &node_dir);
    let daemon_state_path = &setup.daemon_state_path;

    let add_output = Command::new(peppy)
        .args(["node", "add", "."])
        .env("PEPPY_DAEMON_STATE_FILE", daemon_state_path.as_os_str())
        .current_dir(&node_dir)
        .output()
        .expect("failed to run peppy node add");

    assert!(
        add_output.status.success(),
        "peppy node add failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add_output.stdout),
        String::from_utf8_lossy(&add_output.stderr)
    );

    let start_output = Command::new(peppy)
        .args(["node", "start", &format!("{NODE_NAME}:0.1.0")])
        .env("PEPPY_DAEMON_STATE_FILE", daemon_state_path.as_os_str())
        .current_dir(&node_dir)
        .output()
        .expect("failed to run peppy node start");

    assert!(
        start_output.status.success(),
        "peppy node start failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start_output.stdout),
        String::from_utf8_lossy(&start_output.stderr)
    );
}
