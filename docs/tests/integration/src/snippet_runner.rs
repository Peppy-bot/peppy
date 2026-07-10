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
    /// `repositories.json5`). Captured so contract-backed snippets can
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

/// Registers `contracts_root` (relative to the workspace) as a filesystem
/// repository the emulated daemon will scan, then refreshes so that
/// `depends_on.contracts` and `conforms_to` references resolve from the
/// contract cache during `node sync -r`. The serve emulation seeds an empty
/// `repositories.json5` at startup; we overwrite it with the single fs entry.
fn register_contract_repo(peppy: &Path, setup: &NodeSetup, contracts_root: &str) {
    let contracts_dir = workspace_root().join(contracts_root);
    let conf_dir = setup.daemon_root.join("conf");
    fs::create_dir_all(&conf_dir).expect("failed to create daemon conf dir");
    // `{:?}` quotes and escapes the path into a valid JSON string literal.
    let repos_content = format!(
        r#"[{{ "id": 1, "type": "fs", "path": {:?} }}]"#,
        contracts_dir.to_string_lossy().as_ref(),
    );
    fs::write(conf_dir.join("repositories.json5"), repos_content)
        .expect("failed to write repositories.json5");

    let refresh_output = peppy_output(
        peppy,
        &setup.daemon_state_path,
        &contracts_dir,
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
/// Each `(dep, dep_run_args)` entry is added, built, and started under
/// the deterministic instance id `<dep>_1` (with `dep_run_args` appended,
/// e.g. required `key=value` parameters), so the main node's `--bind`
/// lines (passed via `start_args`) can name it — every declared
/// `depends_on` slot must be bound, so consumer snippets launch against
/// live producers.
pub fn run_snippet_with_deps(
    snippets_root: &str,
    snippet_name: &str,
    start_args: &[&str],
    deps: &[(&str, &[&str])],
) {
    let peppy = peppy_binary();
    let node_dir = snippet_dir(snippets_root, snippet_name);

    let setup = setup_env(peppy, &node_dir);

    // Add, build, and run dependencies first (must happen before syncing
    // the main node).
    for (dep, dep_run_args) in deps {
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
        let dep_instance_id = format!("{dep}_1");
        let mut dep_run_cmd = vec![
            "node",
            "run",
            dep_ref.as_str(),
            "--instance-id",
            dep_instance_id.as_str(),
        ];
        dep_run_cmd.extend_from_slice(dep_run_args);
        let dep_run = peppy_output(peppy, &setup.daemon_state_path, &dep_dir, &dep_run_cmd);
        assert_success(&dep_run, &format!("peppy node run {dep_ref}"));
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

/// Run a snippet whose `depends_on` contract references (contract docs,
/// pairing docs, `conforms_to`) are resolved from a document repository
/// rather than from other nodes in the stack. `contracts_root` is a
/// workspace-relative directory of `contract/v1` / `pairing/v1` documents;
/// it is registered as an fs repo and refreshed, then the snippet is synced
/// with `-r`, added, built, and launched with `start_args`. The pairing
/// snippets launch solo with `--defer-pair <slot>`: a required slot boots
/// unpaired when explicitly deferred, with no peer present.
pub fn run_snippet_with_contract_repo(
    snippets_root: &str,
    snippet_name: &str,
    start_args: &[&str],
    contracts_root: &str,
) {
    let peppy = peppy_binary();
    let node_dir = snippet_dir(snippets_root, snippet_name);

    let setup = setup_env(peppy, &node_dir);

    register_contract_repo(peppy, &setup, contracts_root);

    // `-r` lets the daemon resolve the contract deps from the repo cache
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
