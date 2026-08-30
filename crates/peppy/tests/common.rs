use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;
use peppylib::MessengerHandle;
use peppylib::messaging::SenderTarget;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const TEST_NODE_TAG: &str = "v1";

/// SIGKILLs the daemon on drop so a failing/panicking test never leaks it.
pub struct DaemonGuard(pub Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `peppy service serve --messaging-engine mock` in an isolated home,
/// capturing stdout and stderr so tests can inspect the daemon's output.
pub fn spawn_daemon(home: &std::path::Path) -> (DaemonGuard, Arc<Mutex<String>>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_peppy"))
        .args(["service", "serve", "--messaging-engine", "mock"])
        // Pin the child's data root to this per-test home explicitly, so it stays
        // isolated even when the CI job exports its own per-run PEPPY_HOME. The
        // state file needs no extra pinning: it lives in the data root.
        .env(config::consts::PEPPY_HOME_ENV, home)
        .env("TMPDIR", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn peppy service serve");

    // Drain both streams into a shared buffer so tests can wait for output and
    // the child never blocks on a full pipe. The serve daemon logs to stdout.
    let logs = Arc::new(Mutex::new(String::new()));
    for stream in [
        Box::new(child.stdout.take().expect("piped stdout")) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().expect("piped stderr")) as Box<dyn std::io::Read + Send>,
    ] {
        let logs_writer = Arc::clone(&logs);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                logs_writer.lock().unwrap().push_str(&line);
                line.clear();
            }
        });
    }

    (DaemonGuard(child), logs)
}

/// Wait for the child to exit, returning its status, or panic after `timeout`.
pub fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not exit within {timeout:?}");
}

/// Builds a node-shaped [`SenderTarget`] with the standard test tag. Panics on
/// invalid names; tests use known-good values only.
pub fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, TEST_NODE_TAG).expect("test node target")
}

pub fn read_daemon_git_hash(daemon_state_path: &std::path::Path) -> String {
    let contents =
        std::fs::read_to_string(daemon_state_path).expect("daemon state file should be readable");
    let value: serde_json::Value =
        serde_json5::from_str(&contents).expect("daemon state should parse as JSON5");
    value
        .get("git_hash")
        .and_then(|v| v.as_str())
        .filter(|git_hash| !git_hash.is_empty())
        .expect("daemon state should include a non-empty git_hash")
        .to_string()
}

/// The on-disk layout the daemon expects of a staged node, written once: the
/// manifest, its codegen fingerprint, and the `git.hash` under the peppy output
/// dir. Every staging helper builds on this and differs only in how it
/// produces `manifest`.
pub fn install_node_manifest(
    nodes_directory: &std::path::Path,
    node_name: &str,
    git_hash: &str,
    manifest: &str,
) -> std::path::PathBuf {
    let node_dir = nodes_directory.join(node_name);
    std::fs::create_dir_all(&node_dir).expect("failed to create node directory");
    let node_config_path = node_dir.join(config::consts::NODE_CONFIG_FILE);
    std::fs::write(&node_config_path, manifest).expect("failed to write node config");
    config::fingerprint::create_codegen_fingerprint(
        &node_config_path,
        std::path::Path::new(config::consts::PEPPYGEN_OUTPUT_PATH),
    );

    let peppy_output_dir = node_dir.join(daemon_config::consts::PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_output_dir).expect("failed to create peppy output directory");
    std::fs::write(peppy_output_dir.join("git.hash"), git_hash)
        .expect("failed to write node git hash");
    node_dir
}

/// The `run_cmd` array body of a manifest, one JSON5 string per argument.
pub fn run_cmd_json5(run_cmd: &[impl AsRef<str>]) -> String {
    run_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg.as_ref()).expect("run_cmd arg should serialize"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Writes a minimal peppy.json5 with an explicit `run_cmd` (so the daemon's
/// build phase is skipped), optional `depends_on` / `implements` manifest
/// blocks, and an optional top-level `interfaces` block, staged via
/// [`install_node_manifest`].
#[allow(clippy::too_many_arguments)]
pub fn write_node_config_for_helper(
    nodes_directory: &std::path::Path,
    node_name: &str,
    node_tag: &str,
    git_hash: &str,
    run_cmd: &[String],
    depends_on_json5: Option<&str>,
    implements_json5: Option<&str>,
    interfaces_json5: Option<&str>,
) -> std::path::PathBuf {
    let run_cmd_json5 = run_cmd_json5(run_cmd);
    let manifest_extra = implements_json5
        .map(|implements| format!(",\n            implements: {implements}"))
        .unwrap_or_default()
        + &depends_on_json5
            .map(|deps| format!(",\n            depends_on: {deps}"))
            .unwrap_or_default();
    let interfaces_extra = interfaces_json5
        .map(|ifaces| format!(",\n            interfaces: {ifaces}"))
        .unwrap_or_default();
    let body = format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "{node_name}",
                tag: "{node_tag}"{manifest_extra}
            }}{interfaces_extra},
            execution: {{
                language: "rust",
                run_cmd: [{run_cmd_json5}]
            }}
        }}"#
    );
    install_node_manifest(nodes_directory, node_name, git_hash, &body)
}

/// Registers node directories in the daemon's node cache (`cache/nodes.json5`)
/// so a `name:tag` reference (a launcher deployment source, or a dependency
/// absent from the stack) resolves without a full `repo refresh`.
/// `peppy_root` is the serve emulation's peppy dir root (`serve.temp_dir()`).
/// Call this AFTER the node configs' final bytes are on disk (a pinned add
/// verifies the manifest bytes against the entry's fingerprint) and after any
/// `repo refresh` in the test, which rewrites the cache file.
pub fn register_repo_caches(
    peppy_root: impl AsRef<std::path::Path>,
    nodes: &[(&str, &str, &std::path::Path)],
) {
    let peppy_dirs = daemon_config::consts::PeppyDirs::new(peppy_root.as_ref());
    std::fs::create_dir_all(peppy_dirs.cache_dir()).expect("failed to create cache dir");

    let node_entries: Vec<serde_json::Value> = nodes
        .iter()
        .map(|(name, tag, dir)| {
            let manifest_path = dir.join(config::consts::NODE_CONFIG_FILE);
            let content =
                std::fs::read_to_string(&manifest_path).expect("node manifest should exist");
            let parsed = config::node::NodeConfigParser::from_content(&content)
                .expect("node manifest should parse");
            serde_json::json!({
                "node_name": name,
                "node_tag": tag,
                "sha256": config::fingerprint::fingerprint_for_bytes(content.as_bytes()),
                // The origin and the links are serialized from the types
                // `repo refresh` writes, so a change to their shape reaches
                // this fixture.
                "origin": daemon_config::repository::EntryOrigin::Fs { path: manifest_path },
                "links": core_node::DeclaredLinks::from(&parsed.manifest),
            })
        })
        .collect();
    std::fs::write(
        core_node::nodes_repo_cache_path(&peppy_dirs),
        serde_json::to_string_pretty(&node_entries).expect("serialize nodes cache"),
    )
    .expect("failed to write nodes.json5");
}

/// The standard local-path `node add` (no sync, build, no run), for tests
/// that assert on the outcome themselves; [`add_built_node`] wraps it for the
/// expect-success case.
pub fn node_add_command(source: &std::path::Path) -> peppy::commands::node::NodeCommand {
    peppy::commands::node::NodeCommand {
        command: peppy::commands::node::NodeCommands::Add {
            source: Some(source.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            links: Vec::new(),
            vacant_links: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
}

/// Writes a node config and adds/builds it without running an instance.
pub fn add_built_node(ctx: &Arc<AppContext>, dir: &std::path::Path, config: &str) {
    use peppy::commands::Command;
    std::fs::write(dir.join("peppy.json5"), config).expect("write node config");
    peppy::commands::node::NodeCommand {
        command: peppy::commands::node::NodeCommands::Add {
            source: Some(dir.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            links: Vec::new(),
            vacant_links: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(ctx)
    .expect("node add should succeed");
}

/// Builds the standard `node run` command used by pairing/observer e2e tests.
pub fn node_run_command(
    instance_id: &str,
    node: &str,
    links: Vec<(String, String)>,
    vacant_links: Vec<(String, String)>,
) -> peppy::commands::node::NodeCommand {
    let vacant_links = vacant_links
        .into_iter()
        .map(|(link_id, reason)| {
            let reason = daemon_config::launcher::VacantReason::new(&reason)
                .expect("test reasons say something");
            (link_id, reason)
        })
        .collect();
    peppy::commands::node::NodeCommand {
        command: peppy::commands::node::NodeCommands::Run {
            node_ref: None,
            node_name: Some(node.to_string()),
            tag: Some(TEST_NODE_TAG.to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            links,
            vacant_links,
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
}

/// Starts the ready and health services every emulated test instance exposes.
pub async fn emulate_startup_services(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    instance_id: &str,
) {
    peppylib::services::ready::listen_for_node_ready(
        messenger,
        core_node_name,
        instance_id,
        test_node_target(node_name),
    )
    .await
    .expect("ready service should start");
    peppylib::services::health::listen_for_node_health(
        messenger,
        core_node_name,
        instance_id,
        test_node_target(node_name),
    )
    .await
    .expect("health service should start");
}

/// Installs the shutdown service an emulated instance owes, wired to actually
/// end the process — the daemon's cooperative request is answered *and* the
/// `run_cmd` is killed, so the stop completes at once.
///
/// Without this a stopped instance answers nothing and the daemon falls back to
/// waiting out its deadline (~12s per stop, 5s on a run-failure unwind), which
/// is pure dead time in tests that stop instances.
///
/// `pidfile` is where the node's own `run_cmd` recorded `$$`. It is read at
/// shutdown time, not at install time, so it names the live process even for a
/// run that failed before the CLI could log a pid. A node whose instances run
/// sequentially (the usual emulated shape) therefore always kills the right
/// one; the file is rewritten by each spawn.
pub async fn emulate_cooperative_shutdown(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    instance_id: &str,
    pidfile: std::path::PathBuf,
) {
    let (handle, shutdown_rx) = peppylib::services::shutdown::listen_for_shutdown(
        messenger,
        core_node_name,
        instance_id,
        test_node_target(node_name),
    )
    .await
    .expect("shutdown service should start");
    tokio::spawn(async move {
        // Held for the task's life so the service outlives the run it guards.
        let _handle = handle;
        if shutdown_rx.await.is_err() {
            return;
        }
        let Ok(pid) = std::fs::read_to_string(&pidfile) else {
            return;
        };
        let pid = pid.trim();
        if !pid.is_empty() {
            let _ = Command::new("kill").args(["-KILL", pid]).status();
        }
    });
}

/// A minimal two-role pairing document, shared by the pairing e2e tests
/// (`node_pair`, `repo_refresh`, `node_sync`).
pub const ARM_LINK_PAIRING: &str = r#"{
    peppy_schema: "pairing/v1",
    manifest: { name: "arm_link", tag: "v1" },
    roles: ["controller", "arm"],
    topics: [
        {
            emitted_by: "controller",
            name: "joint_commands",
            qos_profile: "reliable",
            message_format: { target_positions: { $type: "array", $items: "f64", $length: 3 } }
        },
        {
            emitted_by: "arm",
            name: "joint_states",
            qos_profile: "sensor_data",
            message_format: { positions: { $type: "array", $items: "f64", $length: 3 } }
        }
    ]
}"#;

/// Publishes `root`'s `peppy_repository.json5`, which is what a repository
/// offers a daemon. Call it once the repository holds every item the test
/// expects refresh to find.
pub fn publish_repo_index(root: &std::path::Path) {
    core_node::publish_repository_index(root)
        .expect("a well-formed test repository can be published");
}

/// Seeds the daemon's `repositories.json5` with one fs repo containing the
/// `arm_link` pairing doc and refreshes, so the doc lands in the daemon's
/// pairing cache.
pub fn seed_pairing_repo(
    serve: &ServeCommandEmulation,
    ctx: &Arc<AppContext>,
    repo_dir: &std::path::Path,
) {
    std::fs::write(repo_dir.join("arm_link.json5"), ARM_LINK_PAIRING).expect("write pairing doc");
    seed_docs_repo(serve, ctx, repo_dir);
}

/// The general form of [`seed_pairing_repo`]: publishes whatever pairing and
/// contract documents the caller already wrote into `repo_dir`, points the
/// daemon at it as its single fs repo, and refreshes so every doc lands in the
/// daemon's caches.
pub fn seed_docs_repo(
    serve: &ServeCommandEmulation,
    ctx: &Arc<AppContext>,
    repo_dir: &std::path::Path,
) {
    use peppy::commands::Command;
    publish_repo_index(repo_dir);
    let conf_dir = serve.temp_dir().join("conf");
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    let repos_content = serde_json::to_string_pretty(&serde_json::json!([
        { "id": 1, "type": "fs", "path": repo_dir.to_string_lossy() }
    ]))
    .expect("serialize repos");
    std::fs::write(conf_dir.join("repositories.json5"), repos_content).expect("write repos");

    peppy::commands::repo::RepoCommand {
        command: peppy::commands::repo::RepoCommands::Refresh,
    }
    .execute(ctx)
    .expect("repo refresh should discover the seeded documents");
}

pub fn setup() -> (
    tokio::runtime::Runtime,
    ServeCommandEmulation,
    Arc<AppContext>,
    tempfile::TempDir,
) {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let work_dir = tempfile::tempdir().expect("failed to create temp work dir");
    let ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), serve.messenger())
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    (rt, serve, ctx, work_dir)
}
