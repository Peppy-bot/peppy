use docs_integration_tests::{peppy_binary, workspace_root};
use peppy::test_support::{override_start_cmd, ServeCommandEmulation};
use std::fs;
use std::process::Command;

#[test]
fn hello_world() {
    const NODE_NAME: &str = "my_first_node";
    let peppy = peppy_binary();
    println!("peppy binary at: {}", peppy.display());

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_zenoh())
        .expect("failed to start serve emulation");
    let daemon_state_path = serve.daemon_state_path().to_path_buf();
    println!(
        "test serve emulation started on port: {}",
        serve.messaging_port()
    );

    // 1. Create a node in a tempdir with `peppy node init`
    let temp_dir = tempfile::tempdir().expect("failed to create temp directory");
    let nodes_root = temp_dir.path();
    println!("Created temp directory at: {}", nodes_root.display());

    let init_output = Command::new(peppy)
        .args(["node", "init", &NODE_NAME])
        .env("PEPPY_DAEMON_STATE_FILE", daemon_state_path.as_os_str())
        .current_dir(nodes_root)
        .output()
        .expect("failed to run peppy node init");

    assert!(
        init_output.status.success(),
        "peppy node init failed: {}",
        String::from_utf8_lossy(&init_output.stderr)
    );
    println!("peppy node init succeeded");

    // 2. Copy snippet files to the node
    let node_dir = nodes_root.join(NODE_NAME);
    let workspace = workspace_root();
    let snippets_dir = workspace.join("docs/src/content/docs/guides/snippets/hello_world");

    let src_main = snippets_dir.join("main.rs");
    let dest_main = node_dir.join("src").join("main.rs");
    fs::copy(&src_main, &dest_main).expect("failed to copy main.rs");
    println!("Copied {} to {}", src_main.display(), dest_main.display());

    let src_peppy_json = snippets_dir.join("peppy.json5");
    let dest_peppy_json = node_dir.join("peppy.json5");
    fs::copy(&src_peppy_json, &dest_peppy_json).expect("failed to copy peppy.json5");
    println!(
        "Copied {} to {}",
        src_peppy_json.display(),
        dest_peppy_json.display()
    );

    // Avoid running `add_cmd` / `start_cmd` (network access is not available in the test runner).
    override_start_cmd(&dest_peppy_json);

    // 3. Run `peppy node add .` in that folder and check that the output is valid
    let add_output = Command::new(peppy)
        .args(["node", "add", "."])
        .env("PEPPY_DAEMON_STATE_FILE", daemon_state_path.as_os_str())
        .current_dir(&node_dir)
        .output()
        .expect("failed to run peppy node add");

    assert!(
        add_output.status.success(),
        "peppy node add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    println!(
        "peppy node add succeeded: {}",
        String::from_utf8_lossy(&add_output.stdout)
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
        "peppy node start failed: {}",
        String::from_utf8_lossy(&start_output.stderr)
    );
    println!(
        "peppy node start succeeded: {}",
        String::from_utf8_lossy(&start_output.stdout)
    );
}
