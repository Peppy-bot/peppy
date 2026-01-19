use docs_integration_tests::{peppy_binary, workspace_root, TestServeHandle};
use std::fs;
use std::process::Command;

#[test]
fn hello_world() {
    const NODE_NAME: &str = "hello_world";
    // TODO find a way to pass HOST and PORT to every `peppy` command in the env vars instead of using `daemon_state.json`
    let peppy = peppy_binary();
    println!("peppy binary at: {}", peppy.display());

    // Start `peppy service serve` on a random port in the background
    let serve_handle = TestServeHandle::start();
    println!(
        "peppy service serve started on port: {}",
        serve_handle.port()
    );

    // 1. Create a node in a tempdir with `peppy node init`
    let temp_dir = tempfile::tempdir().expect("failed to create temp directory");
    let node_dir = temp_dir.path();
    println!("Created temp directory at: {}", node_dir.display());

    let init_output = Command::new(peppy)
        .args(["node", "init", &NODE_NAME])
        .current_dir(node_dir)
        .output()
        .expect("failed to run peppy node init");

    assert!(
        init_output.status.success(),
        "peppy node init failed: {}",
        String::from_utf8_lossy(&init_output.stderr)
    );
    println!("peppy node init succeeded");

    // 2. Copy snippet files to the node
    let workspace = workspace_root();
    let snippets_dir = workspace.join("docs/src/content/docs/guides/snippets/hello_world");

    let src_main = snippets_dir.join("main.rs");
    let dest_main = node_dir.join("src/main.rs");
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

    // 3. Run `peppy node add .` in that folder and check that the output is valid
    let add_output = Command::new(peppy)
        .args(["node", "add", "."])
        .current_dir(node_dir)
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
        .current_dir(node_dir)
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

    drop(serve_handle);
}
