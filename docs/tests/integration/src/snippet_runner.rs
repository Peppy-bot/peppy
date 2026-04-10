use crate::{peppy_binary, workspace_root};
use config::node::NodeConfigParser;
use peppy::test_support::ServeCommandEmulation;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DAEMON_STATE_ENV: &str = "PEPPY_DAEMON_STATE_FILE";

struct NodeSetup {
    daemon_state_path: PathBuf,
    node_ref: String,
    _temp_dir: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
    _serve: ServeCommandEmulation,
}

fn snippet_dir(snippets_root: &str, snippet_name: &str) -> PathBuf {
    workspace_root().join(snippets_root).join(snippet_name)
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
    let node_name = node_config.manifest().name.as_str().to_string();
    let node_ref = format!("{}:{}", node_name, node_config.manifest().tag);

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_zenoh())
        .expect("failed to start serve emulation");
    // Allow the Zenoh router and core-node service subscriptions to stabilize
    // before spawning peppy subprocesses that open new sessions.
    std::thread::sleep(std::time::Duration::from_millis(200));
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

fn build_node(peppy: &Path, daemon_state_path: &Path, node_dir: &Path, node_ref: &str) {
    let build_output = peppy_output(
        peppy,
        daemon_state_path,
        node_dir,
        &["node", "build", node_ref],
    );
    assert_success(&build_output, &format!("peppy node build {node_ref}"));
}

pub fn run_snippet(snippets_root: &str, snippet_name: &str, start_args: &[&str]) {
    run_snippet_with_deps(snippets_root, snippet_name, start_args, &[]);
}

/// Run a snippet that depends on other snippets being added first.
/// Dependencies are added to the node stack but not started.
pub fn run_snippet_with_deps(
    snippets_root: &str,
    snippet_name: &str,
    start_args: &[&str],
    deps: &[&str],
) {
    let peppy = peppy_binary();
    let node_dir = snippet_dir(snippets_root, snippet_name);

    let setup = setup_env(peppy, &node_dir);

    // Add and build dependencies first (must happen before syncing the main node)
    for dep in deps {
        let dep_dir = snippet_dir(snippets_root, dep);
        let dep_config_path = dep_dir.join("peppy.json5");
        let dep_config = NodeConfigParser::from_path(&dep_config_path)
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", dep_config_path.display()));
        let dep_ref = format!(
            "{}:{}",
            dep_config.manifest().name.as_str(),
            dep_config.manifest().tag
        );
        sync_and_add_node(peppy, &setup.daemon_state_path, &dep_dir, dep);
        build_node(peppy, &setup.daemon_state_path, &dep_dir, &dep_ref);
    }

    // Now sync, add, and build the main node
    sync_and_add_node(peppy, &setup.daemon_state_path, &node_dir, snippet_name);
    build_node(peppy, &setup.daemon_state_path, &node_dir, &setup.node_ref);

    let mut start_cmd = vec!["node", "start", setup.node_ref.as_str()];
    start_cmd.extend_from_slice(start_args);

    let start_output = peppy_output(peppy, &setup.daemon_state_path, &node_dir, &start_cmd);
    assert_success(
        &start_output,
        &format!("peppy node start {}", setup.node_ref),
    );
}
