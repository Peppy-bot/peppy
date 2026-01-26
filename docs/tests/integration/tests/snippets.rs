use config::node::NodeConfigParser;
use docs_integration_tests::{peppy_binary, workspace_root};
use peppy::test_support::ServeCommandEmulation;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DAEMON_STATE_ENV: &str = "PEPPY_DAEMON_STATE_FILE";
const SNIPPETS_ROOT: &str = "docs/src/content/docs/guides/snippets/rust";

struct NodeSetup {
    daemon_state_path: PathBuf,
    node_ref: String,
    _temp_dir: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
    _serve: ServeCommandEmulation,
}

fn snippet_dir(snippet_name: &str) -> PathBuf {
    workspace_root().join(SNIPPETS_ROOT).join(snippet_name)
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn peppy_output(
    peppy: &Path,
    daemon_state_path: &Path,
    current_dir: &Path,
    args: &[&str],
) -> Output {
    Command::new(peppy)
        .args(args)
        .env(DAEMON_STATE_ENV, daemon_state_path.as_os_str())
        .current_dir(current_dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run peppy {}: {e}", args.join(" ")))
}

/// Sets up the environment without syncing the main node.
/// This allows dependencies to be added before syncing a node that depends on them.
fn setup_env(peppy: &Path, node_dir: &Path) -> NodeSetup {
    let node_config = NodeConfigParser::from_path(node_dir.join("peppy.json5"))
        .expect("failed to parse peppy.json5");
    let node_name = node_config.manifest.name.as_str().to_string();
    let node_ref = format!("{}:{}", node_name, node_config.manifest.tag);

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_zenoh())
        .expect("failed to start serve emulation");
    let daemon_state_path = serve.daemon_state_path().to_path_buf();

    // Create a node in a tempdir with `peppy node init`
    let temp_dir = tempfile::tempdir().expect("failed to create temp directory");
    let nodes_root = temp_dir.path();

    let init_output = peppy_output(
        peppy,
        &daemon_state_path,
        nodes_root,
        &["node", "init", node_name.as_str()],
    );
    assert_success(&init_output, "peppy node init");

    NodeSetup {
        daemon_state_path,
        node_ref,
        _temp_dir: temp_dir,
        _rt: rt,
        _serve: serve,
    }
}

fn sync_and_add_node(peppy: &Path, daemon_state_path: &Path, node_dir: &Path, context: &str) {
    let sync_output = peppy_output(peppy, daemon_state_path, node_dir, &["node", "sync"]);
    assert_success(&sync_output, &format!("peppy node sync for {context}"));

    let add_output = peppy_output(
        peppy,
        daemon_state_path,
        node_dir,
        &["node", "add", ".", "--force"],
    );
    assert_success(&add_output, &format!("peppy node add . for {context}"));
}

fn run_snippet(snippet_name: &str, start_args: &[&str]) {
    run_snippet_with_deps(snippet_name, start_args, &[]);
}

/// Run a snippet that depends on other snippets being added first.
/// Dependencies are added to the node stack but not started.
fn run_snippet_with_deps(snippet_name: &str, start_args: &[&str], deps: &[&str]) {
    let peppy = peppy_binary();
    let node_dir = snippet_dir(snippet_name);

    let setup = setup_env(peppy, &node_dir);

    // Add dependencies first (must happen before syncing the main node)
    for dep in deps {
        let dep_dir = snippet_dir(dep);
        sync_and_add_node(peppy, &setup.daemon_state_path, &dep_dir, dep);
    }

    // Now sync and add the main node
    sync_and_add_node(peppy, &setup.daemon_state_path, &node_dir, snippet_name);

    let mut start_cmd = vec!["node", "start", setup.node_ref.as_str()];
    start_cmd.extend_from_slice(start_args);

    let start_output = peppy_output(peppy, &setup.daemon_state_path, &node_dir, &start_cmd);
    assert_success(
        &start_output,
        &format!("peppy node start {}", setup.node_ref),
    );
}

#[test]
fn hello_world() {
    run_snippet("hello_world", &[]);
}

#[test]
fn first_node() {
    run_snippet("first_node", &[]);
}

#[test]
fn standalone_node() {
    run_snippet(
        "standalone",
        &[
            "device.physical=/dev/device1",
            "device.sim=the_camera",
            "device.priority=physical",
            "video.encoding=rgb",
            "video.frame_rate=30",
            "video.resolution.width=1280",
            "video.resolution.height=720",
        ],
    );
}

// Combine both tests into one since they depend on each other and doing so avoids parallelism issues
#[test]
fn hello_world_param_and_hello_receiver() {
    run_snippet_with_deps("hello_receiver", &[], &["hello_world_param"]);
}
