use crate::{peppy_binary, workspace_root};
use config::node::NodeConfigParser;
use peppy::test_support::ServeCommandEmulation;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DAEMON_STATE_ENV: &str = "PEPPY_DAEMON_STATE_FILE";

struct NodeSetup {
    daemon_state_path: PathBuf,
    /// Root of the emulated daemon's directory tree (its `conf/` holds
    /// `repositories.json5`). Captured so interface-backed snippets can
    /// register an fs repo the daemon will scan on refresh.
    daemon_root: PathBuf,
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
    let node_name = node_config.manifest.name.as_str().to_string();
    let node_ref = format!("{}:{}", node_name, node_config.manifest.tag);

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_zenoh())
        .expect("failed to start serve emulation");
    // Allow the Zenoh router and core-node service subscriptions to stabilize
    // before spawning peppy subprocesses that open new sessions.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let daemon_state_path = serve.daemon_state_path().to_path_buf();
    let daemon_root = serve.temp_dir().to_path_buf();

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
        daemon_root,
        node_ref,
        _temp_dir: temp_dir,
        _rt: rt,
        _serve: serve,
    }
}

fn sync_and_add_node(
    peppy: &Path,
    daemon_state_path: &Path,
    node_dir: &Path,
    context: &str,
    extra_sync_args: &[&str],
) {
    let mut sync_cmd = vec!["node", "sync"];
    sync_cmd.extend_from_slice(extra_sync_args);
    let sync_output = peppy_output(peppy, daemon_state_path, node_dir, &sync_cmd);
    assert_success(&sync_output, &format!("peppy node sync for {context}"));

    let add_output = peppy_output(
        peppy,
        daemon_state_path,
        node_dir,
        &["node", "add", ".", "--force"],
    );
    assert_success(&add_output, &format!("peppy node add . for {context}"));
}

/// Registers `interfaces_root` (relative to the workspace) as a filesystem
/// repository the emulated daemon will scan, then refreshes so that
/// `depends_on.interfaces` and `conforms_to` references resolve from the
/// interface cache during `node sync -r`. The serve emulation seeds an empty
/// `repositories.json5` at startup; we overwrite it with the single fs entry.
fn register_interface_repo(peppy: &Path, setup: &NodeSetup, interfaces_root: &str) {
    let interfaces_dir = workspace_root().join(interfaces_root);
    let conf_dir = setup.daemon_root.join("conf");
    fs::create_dir_all(&conf_dir).expect("failed to create daemon conf dir");
    // `{:?}` quotes and escapes the path into a valid JSON string literal.
    let repos_content = format!(
        r#"[{{ "id": 1, "type": "fs", "path": {:?} }}]"#,
        interfaces_dir.to_string_lossy().as_ref(),
    );
    fs::write(conf_dir.join("repositories.json5"), repos_content)
        .expect("failed to write repositories.json5");

    let refresh_output = peppy_output(
        peppy,
        &setup.daemon_state_path,
        &interfaces_dir,
        &["repo", "refresh"],
    );
    assert_success(&refresh_output, "peppy repo refresh");
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
            dep_config.manifest.name.as_str(),
            dep_config.manifest.tag
        );
        sync_and_add_node(peppy, &setup.daemon_state_path, &dep_dir, dep, &[]);
        build_node(peppy, &setup.daemon_state_path, &dep_dir, &dep_ref);
    }

    // Now sync, add, and build the main node
    sync_and_add_node(
        peppy,
        &setup.daemon_state_path,
        &node_dir,
        snippet_name,
        &[],
    );
    build_node(peppy, &setup.daemon_state_path, &node_dir, &setup.node_ref);

    let mut run_cmd = vec!["node", "run", setup.node_ref.as_str()];
    run_cmd.extend_from_slice(start_args);

    let start_output = peppy_output(peppy, &setup.daemon_state_path, &node_dir, &run_cmd);
    assert_success(&start_output, &format!("peppy node run {}", setup.node_ref));
}

/// Run a snippet whose `depends_on.interfaces` / `conforms_to` references are
/// resolved from an interface repository rather than from other nodes in the
/// stack. `interfaces_root` is a workspace-relative directory of
/// `interface/v1` documents; it is registered as an fs repo and refreshed,
/// then the snippet is synced with `-r`, added, built, and launched with NO
/// `--bind`. This is the launch path for an optional, `from_any` interface
/// consumer: it must come up with zero producers present.
pub fn run_snippet_with_interface_repo(
    snippets_root: &str,
    snippet_name: &str,
    start_args: &[&str],
    interfaces_root: &str,
) {
    let peppy = peppy_binary();
    let node_dir = snippet_dir(snippets_root, snippet_name);

    let setup = setup_env(peppy, &node_dir);

    register_interface_repo(peppy, &setup, interfaces_root);

    // `-r` lets the daemon resolve the interface deps from the repo cache
    // populated by the refresh above (no producer node is in the stack).
    sync_and_add_node(
        peppy,
        &setup.daemon_state_path,
        &node_dir,
        snippet_name,
        &["-r"],
    );
    build_node(peppy, &setup.daemon_state_path, &node_dir, &setup.node_ref);

    let mut run_cmd = vec!["node", "run", setup.node_ref.as_str()];
    run_cmd.extend_from_slice(start_args);

    let start_output = peppy_output(peppy, &setup.daemon_state_path, &node_dir, &run_cmd);
    assert_success(&start_output, &format!("peppy node run {}", setup.node_ref));
}
