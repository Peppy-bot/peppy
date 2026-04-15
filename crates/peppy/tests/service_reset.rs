use peppy::test_support::ServeCommandEmulation;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use config::consts::{NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH};
use core_node::encoding::StackListRequest;
use node_stack::SerializedNodeGraph;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::commands::service::{ServiceCommand, ServiceCommands};
use peppy::context::AppContext;

const CALLER_INSTANCE_ID: &str = "peppy-test";

fn write_node_config(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    git_hash: &str,
    run_cmd: &[&str],
) -> PathBuf {
    let node_dir = nodes_directory.join(node_name);
    fs::create_dir_all(&node_dir).expect("failed to create node directory");
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    let run_cmd_json5 = run_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("run_cmd arg should serialize"))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        &node_config_path,
        r#"{
                schema_version: 1,
                manifest: {
                    name: "{node_name}",
                    tag: "{node_tag}",
                },
                execution: {
                    language: "rust",
                    run_cmd: [{run_cmd_json5}]
                }
            }"#
        .replace("{node_name}", node_name)
        .replace("{node_tag}", node_tag)
        .replace("{run_cmd_json5}", &run_cmd_json5),
    )
    .expect("failed to write node config");
    config::fingerprint::create_codegen_fingerprint(
        &node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let peppy_output_dir = node_dir.join(PEPPY_OUTPUT_DIR);
    fs::create_dir_all(&peppy_output_dir).expect("failed to create peppy output directory");
    fs::write(peppy_output_dir.join("git.hash"), git_hash).expect("failed to write node git hash");
    node_dir
}

fn read_daemon_git_hash(daemon_state_path: &Path) -> String {
    let contents =
        fs::read_to_string(daemon_state_path).expect("daemon state file should be readable");
    let value: serde_json::Value =
        serde_json::from_str(&contents).expect("daemon state should parse as JSON");
    value
        .get("git_hash")
        .and_then(|v| v.as_str())
        .filter(|git_hash| !git_hash.is_empty())
        .expect("daemon state should include a non-empty git_hash")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_reset_command_resets_node_stack() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let node_name = "reset_test_node";
    let node_tag = "0.1.0";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());
    let node_path = write_node_config(
        nodes_dir.path(),
        node_name,
        node_tag,
        &git_hash,
        &["sh", "-c", "exit 0"],
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            variant: Vec::new(),
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&ctx)
    .expect("node add command should succeed");

    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = StackListRequest::new(false)
        .poll(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_name}:{node_tag}"))),
        "graph should contain added node before reset. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    ServiceCommand {
        command: ServiceCommands::Reset {},
    }
    .execute(&ctx)
    .expect("service reset command should succeed");

    let response = StackListRequest::new(false)
        .poll(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("stack_list request should complete after reset");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse after reset");

    assert_eq!(
        graph.nodes.len(),
        1,
        "graph should contain only the root node after reset. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    assert_eq!(
        graph.nodes[0].name,
        core_node_name,
        "root node name should match core node name. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}
