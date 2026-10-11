use super::common::{
    node_add_command, read_daemon_git_hash, serve_app_context, stack_graph,
    write_minimal_node_config,
};
use peppy::test_support::ServeCommandEmulation;

use peppy::commands::Command;
use peppy::commands::stack::{StackCommand, StackCommands};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_reset_command_resets_node_stack() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let node_name = "reset_test_node";
    let node_tag = "v1";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());
    let node_path = write_minimal_node_config(
        nodes_dir.path(),
        node_name,
        node_tag,
        &git_hash,
        &["sh", "-c", "exit 0"],
    );

    let ctx = serve_app_context(&serve, nodes_dir.path());

    node_add_command(&node_path)
        .execute(&ctx)
        .expect("node add command should succeed");

    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let graph = stack_graph(messenger_handle, &core_node_name).await;

    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_name}:{node_tag}"))),
        "graph should contain added node before reset. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    StackCommand {
        command: StackCommands::Reset { federated: false },
    }
    .execute(&ctx)
    .expect("stack reset command should succeed");

    let graph = stack_graph(messenger_handle, &core_node_name).await;

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
