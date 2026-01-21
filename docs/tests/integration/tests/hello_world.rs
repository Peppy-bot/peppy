use docs_integration_tests::{peppy_binary, workspace_root};
use peppy::test_support::ServeCommandEmulation;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct NodeSetup {
    node_dir: PathBuf,
    daemon_state_path: PathBuf,
    #[allow(dead_code)]
    temp_dir: tempfile::TempDir,
    #[allow(dead_code)]
    rt: tokio::runtime::Runtime,
    #[allow(dead_code)]
    serve: ServeCommandEmulation,
}

fn setup_node_with_snippets(
    node_name: &str,
    main_rs_path: &Path,
    peppy_json5_path: &Path,
) -> NodeSetup {
    let peppy = peppy_binary();

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_zenoh())
        .expect("failed to start serve emulation");
    let daemon_state_path = serve.daemon_state_path().to_path_buf();

    // Create a node in a tempdir with `peppy node init`
    let temp_dir = tempfile::tempdir().expect("failed to create temp directory");
    let nodes_root = temp_dir.path();

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

    // Copy snippet files to the node
    let node_dir = nodes_root.join(node_name);

    let dest_main = node_dir.join("src").join("main.rs");
    fs::copy(main_rs_path, &dest_main).expect("failed to copy main.rs");

    let dest_peppy_json = node_dir.join("peppy.json5");
    fs::copy(peppy_json5_path, &dest_peppy_json).expect("failed to copy peppy.json5");

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
        node_dir,
        daemon_state_path,
        temp_dir,
        rt,
        serve,
    }
}

#[test]
fn hello_world() {
    const NODE_NAME: &str = "my_first_node";
    let peppy = peppy_binary();

    let workspace = workspace_root();
    let snippets_dir = workspace.join("docs/src/content/docs/guides/snippets/hello_world");
    let main_rs_path = snippets_dir.join("main.rs");
    let peppy_json5_path = snippets_dir.join("peppy.json5");

    let setup = setup_node_with_snippets(NODE_NAME, &main_rs_path, &peppy_json5_path);
    let node_dir = &setup.node_dir;
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
