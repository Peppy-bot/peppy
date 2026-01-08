#![allow(dead_code)]

use config::consts::{DEFAULT_ZENOH_HOST, NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::peppy_config::BuildSystem;
use master_node::{MasterNode, MasterNodeArguments};
use node_stack::NodeStack;
use peppylib::messaging::MessengerHandle;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter, start_zenohd_process};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

pub const CALLER_INSTANCE_ID: &str = "caller_instance";

/// Creates a fresh test node in a new temp directory.
/// Each call creates a completely new node with its own peppygen generation
/// and cargo build, ensuring isolation between tests.
pub fn create_test_node() -> PathBuf {
    init_test_node_project("example_node", "0.1.0")
}

/// Creates a fresh test node in a new temp directory.
/// Each call creates a completely new node with its own peppygen generation
/// and cargo build, ensuring isolation between tests.
pub fn create_test_node_with_name(node_name: &str, node_tag: &str) -> PathBuf {
    init_test_node_project(node_name, node_tag)
}

fn init_test_node_project(node_name: &str, node_tag: &str) -> PathBuf {
    let node_dir = tempfile::Builder::new()
        .prefix("peppy_test_node_")
        .tempdir()
        .expect("failed to create temp directory for test node")
        .keep();

    init_cargo_project(&node_dir, node_name);
    write_test_node_files(&node_dir, node_name, node_tag);

    generator::generate_lib_for_build_system(BuildSystem::Rust, &node_dir)
        .expect("failed to generate peppygen for test node");

    build_cargo_project(&node_dir);

    node_dir
}

fn init_cargo_project(node_dir: &Path, crate_name: &str) {
    let output = Command::new("cargo")
        .arg("init")
        .arg("--bin")
        .arg("--vcs")
        .arg("none")
        .arg("--name")
        .arg(crate_name)
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(node_dir)
        .output()
        .expect("failed to invoke `cargo init` for test node");

    assert!(
        output.status.success(),
        "`cargo init` failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_test_node_files(node_dir: &Path, crate_name: &str, node_tag: &str) {
    std::fs::write(
        node_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"

[dependencies]
peppygen = {{ path = "{PEPPYGEN_OUTPUT_PATH}" }}
"#
        ),
    )
    .expect("failed to write test node Cargo.toml");

    std::fs::write(
        node_dir.join("src/main.rs"),
        r#"use peppygen::{run, Result};

fn main() -> Result<()> {
    run(|args, node_runner| async {
        let _ = args;
        let _ = node_runner;
        Ok(())
    })
}
"#,
    )
    .expect("failed to write test node src/main.rs");

    std::fs::write(
        node_dir.join(NODE_CONFIG_FILE),
        format!(
            r#"{{
  schema_version: 1,
  manifest: {{
    name: "{crate_name}",
    tag: "{node_tag}",
    launch_cmd: [
      "cargo",
      "run",
      "--release"
    ]
  }},
  interfaces: {{
    exposes: {{
      topics: [
        {{
          name: "hello_world",
          qos_profile: "sensor_data",
          message_format: {{
            timestamp: "time",
            message: "string"
          }}
        }}
      ],
    }}
  }},
  logging: {{
    min_level: "info",
    format: "text"
  }}
}}"#
        ),
    )
    .expect("failed to write test node peppy.json5");
}

fn build_cargo_project(dir: &Path) {
    let output = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(dir)
        .output()
        .expect("failed to invoke `cargo build` for test node");

    assert!(
        output.status.success(),
        "`cargo build` failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub async fn create_mock_messenger() -> Arc<Mutex<Messenger>> {
    let adapter = MockAdapter::default();
    let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
    messenger
        .start_session()
        .await
        .expect("failed to start mock session");
    Arc::new(Mutex::new(messenger))
}

#[allow(dead_code)]
pub struct StartedMasterNode {
    pub shared_messenger: Arc<Mutex<Messenger>>,
    pub caller_handle: MessengerHandle,
    pub master_node_name: String,
    pub node_stack: NodeStack,
    pub task: JoinHandle<master_node::Result<()>>,
    pub _zenohd_temp_dir: Option<TempDir>,
}

pub async fn start_master_node() -> StartedMasterNode {
    let shared_messenger = create_mock_messenger().await;
    let node_startup_timeout = Duration::from_secs(10);
    let node_start_health_timeout = Duration::from_secs(30);
    start_master_node_with_messenger(
        shared_messenger,
        None,
        node_startup_timeout,
        node_start_health_timeout,
    )
    .await
}

pub async fn start_master_node_with_health_timeout(
    node_start_health_timeout: Duration,
) -> StartedMasterNode {
    let shared_messenger = create_mock_messenger().await;
    let node_startup_timeout = Duration::from_secs(10);
    start_master_node_with_messenger(
        shared_messenger,
        None,
        node_startup_timeout,
        node_start_health_timeout,
    )
    .await
}

pub async fn start_master_node_with_zenoh_messenger() -> StartedMasterNode {
    let (shared_messenger, temp_dir) = create_zenoh_messenger().await;
    // When launching real nodes we often spawn `cargo run`, which may take a while due to
    // compilation or cargo's global package-cache lock.
    let node_startup_timeout = Duration::from_secs(30);
    let node_start_health_timeout = Duration::from_secs(30);
    start_master_node_with_messenger(
        shared_messenger,
        Some(temp_dir),
        node_startup_timeout,
        node_start_health_timeout,
    )
    .await
}

async fn start_master_node_with_messenger(
    shared_messenger: Arc<Mutex<Messenger>>,
    zenohd_temp_dir: Option<TempDir>,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> StartedMasterNode {
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let node_arguments = MasterNodeArguments {
        node_startup_timeout,
        node_start_health_timeout,
    };
    let root_dir = std::env::current_dir().expect("failed to get current directory");
    let master_node = MasterNode::new(
        Arc::clone(&shared_messenger),
        Some("test_master_node"),
        node_arguments,
        root_dir,
    );
    let master_node_name = master_node.node_name().to_string();
    let node_stack = master_node.node_stack().clone();

    let task = tokio::spawn(async move { master_node.start().await });

    // Allow the MasterNode services to fully establish their listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    StartedMasterNode {
        shared_messenger,
        caller_handle,
        master_node_name,
        node_stack,
        task,
        _zenohd_temp_dir: zenohd_temp_dir,
    }
}

pub async fn create_zenoh_messenger() -> (Arc<Mutex<Messenger>>, TempDir) {
    // Use a real router so spawned nodes can connect over zenoh.
    let (mut messenger, temp_dir, _host, _port) = start_zenohd_process(DEFAULT_ZENOH_HOST, None)
        .await
        .expect("failed to start zenoh router for tests");
    messenger
        .start_session()
        .await
        .expect("failed to start zenoh session for tests");
    (Arc::new(Mutex::new(messenger)), temp_dir)
}
