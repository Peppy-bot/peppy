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
            defer_links: Vec::new(),
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
    defer_links: Vec<String>,
) -> peppy::commands::node::NodeCommand {
    peppy::commands::node::NodeCommand {
        command: peppy::commands::node::NodeCommands::Run {
            node_ref: None,
            node_name: Some(node.to_string()),
            tag: Some(TEST_NODE_TAG.to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            links,
            defer_links,
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

/// Seeds the daemon's `repositories.json5` with one fs repo containing the
/// `arm_link` pairing doc and refreshes, so the doc lands in the daemon's
/// pairing cache.
pub fn seed_pairing_repo(
    serve: &ServeCommandEmulation,
    ctx: &Arc<AppContext>,
    repo_dir: &std::path::Path,
) {
    use peppy::commands::Command;
    std::fs::write(repo_dir.join("arm_link.json5"), ARM_LINK_PAIRING).expect("write pairing doc");
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
    .expect("repo refresh should discover the pairing doc");
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
