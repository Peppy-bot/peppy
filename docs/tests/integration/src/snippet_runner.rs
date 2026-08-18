use crate::{peppy_binary, workspace_root};
use config::node::{NodeConfigParser, PeppygenLanguage};
use peppy::test_support::{ServeCommandEmulation, wait_for_log};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

struct NodeSetup {
    /// Root of the emulated daemon's directory tree, exported to every peppy
    /// subprocess as `PEPPY_HOME` so the CLI resolves the same state file,
    /// `conf/`, and cache the emulated daemon serves. Its `conf/` holds
    /// `repositories.json5`, so contract-backed snippets can register an fs
    /// repo the daemon will scan on refresh.
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

fn peppy_output(peppy: &Path, daemon_root: &Path, current_dir: &Path, args: &[&str]) -> Output {
    Command::new(peppy)
        .args(args)
        .env(config::consts::PEPPY_HOME_ENV, daemon_root)
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
    let daemon_root = serve.temp_dir().to_path_buf();

    // Create a node in a tempdir with `peppy node init`
    let temp_dir = tempfile::tempdir().expect("failed to create temp directory");
    let nodes_root = temp_dir.path();

    let init_output = peppy_output(
        peppy,
        &daemon_root,
        nodes_root,
        &["node", "init", node_name.as_str()],
    );
    assert_success(&init_output, "peppy node init");

    NodeSetup {
        daemon_root,
        node_ref,
        _temp_dir: temp_dir,
        _rt: rt,
        _serve: serve,
    }
}

/// The lock guarding one snippet directory, created on first use and kept for
/// the process's lifetime (hence the `&'static` through a leak).
///
/// `peppy node sync` generates bindings INTO the snippet directory it runs in,
/// and `node add`, `node build` and `node run` then read that same directory,
/// so two tests working on the same snippet must take turns for the whole
/// sync-through-run span: a re-sync under a concurrent build would rewrite the
/// generated sources mid-compile. Tests share a snippet whenever they share a
/// producer (several consumer snippets reading one producer's topic) or drive
/// one snippet through several deployments. The lock is per directory, so
/// unrelated snippets still run in parallel.
fn snippet_dir_lock(node_dir: &Path) -> &'static Mutex<()> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, &'static Mutex<()>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(PoisonError::into_inner);
    locks
        .entry(node_dir.to_path_buf())
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
}

/// Syncs and adds the node, returning the snippet directory's guard so the
/// caller can keep holding it through the build and run that read the synced
/// directory.
fn sync_and_add_node(
    peppy: &Path,
    daemon_root: &Path,
    node_dir: &Path,
    context: &str,
    extra_sync_args: &[&str],
) -> MutexGuard<'static, ()> {
    // A test that panics mid-span poisons this; the next test still has to run,
    // and it starts by rewriting the same generated directory anyway.
    let guard = snippet_dir_lock(node_dir)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);

    let mut sync_cmd = vec!["node", "sync"];
    sync_cmd.extend_from_slice(extra_sync_args);
    let sync_output = peppy_output(peppy, daemon_root, node_dir, &sync_cmd);
    assert_success(&sync_output, &format!("peppy node sync for {context}"));

    let add_output = peppy_output(
        peppy,
        daemon_root,
        node_dir,
        &["node", "add", ".", "--force"],
    );
    assert_success(&add_output, &format!("peppy node add . for {context}"));

    guard
}

/// Registers `contracts_root` (relative to the workspace) as a filesystem
/// repository the emulated daemon will scan, then refreshes so that
/// `depends_on.contracts` and `manifest.implements` references resolve from the
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
        &setup.daemon_root,
        &contracts_dir,
        &["repo", "refresh"],
    );
    assert_success(&refresh_output, "peppy repo refresh");
}

fn build_node(peppy: &Path, daemon_root: &Path, node_dir: &Path, node_ref: &str) {
    let build_output = peppy_output(peppy, daemon_root, node_dir, &["node", "build", node_ref]);
    assert_success(&build_output, &format!("peppy node build {node_ref}"));
}

pub fn run_snippet(snippets_root: &str, snippet_name: &str, start_args: &[&str]) {
    run_snippet_with_deps(snippets_root, snippet_name, start_args, &[]);
}

/// Run a snippet that depends on other snippets being added first.
/// Each `(dep, dep_run_args)` entry is added, built, and started under
/// the deterministic instance id `<dep>_1` (with `dep_run_args` appended,
/// e.g. required `key=value` parameters), so the main node's `--link`
/// lines (passed via `start_args`) can name it: every declared
/// `depends_on` slot must be bound, so consumer snippets launch against
/// live producers.
pub fn run_snippet_with_deps(
    snippets_root: &str,
    snippet_name: &str,
    start_args: &[&str],
    deps: &[(&str, &[&str])],
) {
    run_snippet_with_deps_asserting_output(snippets_root, snippet_name, start_args, deps, &[]);
}

/// [`run_snippet_with_deps`] plus an assertion on what the snippet printed.
///
/// The main node runs under the deterministic instance id `<snippet_name>_1`,
/// so its stdout is readable at `<PEPPY_HOME>/logs/run/<instance_id>.log` once
/// the run returns. Every entry in `expected_output` must appear there. A
/// snippet whose printed lines depend on what the runtime handed it (a scalar
/// slot's `Option`, say) is then checked against what the run actually wired,
/// not just against the exit status.
pub fn run_snippet_with_deps_asserting_output(
    snippets_root: &str,
    snippet_name: &str,
    start_args: &[&str],
    deps: &[(&str, &[&str])],
    expected_output: &[&str],
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
        let _dep_dir_guard = sync_and_add_node(peppy, &setup.daemon_root, &dep_dir, dep, &[]);
        build_node(peppy, &setup.daemon_root, &dep_dir, &dep_ref);
        let dep_instance_id = format!("{dep}_1");
        let mut dep_run_cmd = vec![
            "node",
            "run",
            dep_ref.as_str(),
            "--instance-id",
            dep_instance_id.as_str(),
        ];
        dep_run_cmd.extend_from_slice(dep_run_args);
        let dep_run = peppy_output(peppy, &setup.daemon_root, &dep_dir, &dep_run_cmd);
        assert_success(&dep_run, &format!("peppy node run {dep_ref}"));
    }

    // Now sync, add, and build the main node
    let _dir_guard = sync_and_add_node(peppy, &setup.daemon_root, &node_dir, snippet_name, &[]);
    build_node(peppy, &setup.daemon_root, &node_dir, &setup.node_ref);

    let instance_id = format!("{snippet_name}_1");
    let mut run_cmd = vec![
        "node",
        "run",
        setup.node_ref.as_str(),
        "--instance-id",
        instance_id.as_str(),
    ];
    run_cmd.extend_from_slice(start_args);

    let start_output = peppy_output(peppy, &setup.daemon_root, &node_dir, &run_cmd);
    assert_success(&start_output, &format!("peppy node run {}", setup.node_ref));

    if !expected_output.is_empty() {
        assert_run_log_contains(&setup.daemon_root, &instance_id, expected_output);
    }
}

/// Wait for the run log of `instance_id` to carry every substring in
/// `expected`, then return. The node prints from its setup, which completes
/// before it answers the health check `peppy node run` waits on, so the lines
/// are already produced by the time this is called; the wait covers only the
/// daemon draining the node's stdout pipe into the log file.
fn assert_run_log_contains(daemon_root: &Path, instance_id: &str, expected: &[&str]) {
    let log_path = daemon_root
        .join("logs")
        .join("run")
        .join(format!("{instance_id}.log"));
    for line in expected {
        wait_for_log(
            || fs::read_to_string(&log_path).unwrap_or_default(),
            line,
            std::time::Duration::from_secs(30),
        );
    }
}

/// Runs a snippet's own node-author test suite (its `tests/` directory,
/// written against the generated `peppygen::mock` / `peppygen.mock` and
/// `fixtures` surfaces): syncs the snippet (adding each entry of `deps` to
/// the stack first, so its `depends_on` slots resolve at sync time), then
/// runs `cargo test` (Rust snippets) or `uv run --group dev pytest` (Python
/// snippets) inside the snippet directory. The dependency nodes are never
/// built or run — the whole point of the generated harness is that the tests
/// boot the node in-process against mocks of them.
pub fn run_node_tests(snippets_root: &str, snippet_name: &str, deps: &[&str]) {
    let peppy = peppy_binary();
    let node_dir = snippet_dir(snippets_root, snippet_name);

    let setup = setup_env(peppy, &node_dir);

    // Add dependencies to the stack so the main node's sync resolves their
    // interfaces (a `depends_on.nodes` slot reads the producer's manifest at
    // sync time, whether or not an instance ever runs).
    for dep in deps {
        let dep_dir = snippet_dir(snippets_root, dep);
        let _dep_dir_guard = sync_and_add_node(peppy, &setup.daemon_root, &dep_dir, dep, &[]);
    }

    // Hold the snippet directory's guard through the test run: the suite
    // compiles against the sources this sync just generated.
    let _dir_guard = sync_and_add_node(peppy, &setup.daemon_root, &node_dir, snippet_name, &[]);

    let node_config = NodeConfigParser::from_path(node_dir.join("peppy.json5"))
        .expect("failed to parse peppy.json5");
    let output = match node_config.execution.language {
        PeppygenLanguage::Rust => run_cargo_node_tests(&node_dir, snippet_name),
        PeppygenLanguage::Python => run_pytest_node_tests(&node_dir),
    };
    assert_success(&output, &format!("node tests for {snippet_name}"));
}

/// `cargo test` inside a synced Rust snippet, mirroring how the generator's
/// own node-test helper invokes cargo: offline (everything the snippet needs
/// is vendored or cached), against a stable shared target dir so dependency
/// artifacts survive across runs. The dir is per snippet because every
/// snippet's generated library is named `peppygen`: sharing one target dir
/// across snippets would let cargo link one snippet's cached `peppygen` rlib
/// into another.
fn run_cargo_node_tests(node_dir: &Path, snippet_name: &str) -> Output {
    let target_dir = workspace_root()
        .join("target")
        .join("docs-snippet-node-tests")
        .join(snippet_name);
    fs::create_dir_all(&target_dir).expect("failed to create shared snippet test target dir");

    let mut command = Command::new("cargo");
    command
        .arg("test")
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TARGET_DIR", &target_dir)
        .current_dir(node_dir)
        .stdin(Stdio::null());
    forward_resolved_zenohd(&mut command);
    command.output().expect("failed to invoke cargo test on snippet node")
}

/// `uv run --group dev pytest` inside a synced Python snippet: the snippet's
/// `dev` dependency group carries pytest + pytest-asyncio, and `uv run` syncs
/// the project environment (including the vendored `.peppy/libs` path
/// dependencies the sync just generated) before running. The uv cache is
/// already shared: the workspace `.cargo/config.toml` points `UV_CACHE_DIR`
/// under `target/` for every process cargo runs, and this child inherits it.
fn run_pytest_node_tests(node_dir: &Path) -> Output {
    let mut command = Command::new("uv");
    command
        .args(["run", "--group", "dev", "pytest"])
        .current_dir(node_dir)
        .stdin(Stdio::null());
    forward_resolved_zenohd(&mut command);
    command.output().expect("failed to invoke uv run pytest on snippet node")
}

/// Forwards the zenohd binary this test toolchain resolved to the node test
/// suite via `PEPPY_ZENOHD_PATH`: the generated harness starts an ephemeral
/// router per test, and the snippet's vendored pmi is built without
/// `build_zenoh`, so inside the test environment (no `peppy` on PATH) it has
/// nothing else to resolve.
fn forward_resolved_zenohd(command: &mut Command) {
    if let Some(zenohd) = pmi::ZenohdFacade::resolved_zenohd_binary() {
        command.env(pmi::ZENOHD_PATH_VAR, zenohd);
    }
}

/// Run a snippet whose `depends_on` contract references (contract docs,
/// pairing docs, `manifest.implements`) are resolved from a document repository
/// rather than from other nodes in the stack. `contracts_root` is a
/// workspace-relative directory of `contract/v1` / `pairing/v1` documents;
/// it is registered as an fs repo and refreshed, then the snippet is synced
/// with `-r`, added, built, and launched with `start_args`. The pairing
/// snippets launch solo with `--vacant-link <slot>=<why>`: a slot the manifest
/// declares `optional: true` boots unpaired when the run declares it vacant,
/// with no peer present.
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
    let _dir_guard = sync_and_add_node(peppy, &setup.daemon_root, &node_dir, snippet_name, &["-r"]);
    build_node(peppy, &setup.daemon_root, &node_dir, &setup.node_ref);

    let mut run_cmd = vec!["node", "run", setup.node_ref.as_str()];
    run_cmd.extend_from_slice(start_args);

    let start_output = peppy_output(peppy, &setup.daemon_root, &node_dir, &run_cmd);
    assert_success(&start_output, &format!("peppy node run {}", setup.node_ref));
}
