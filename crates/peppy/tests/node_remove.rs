use config::node::Toolchain;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::test_support::LogCapture;
use peppylib::MessengerHandle;
use peppylib::services::shutdown::listen_for_shutdown;
use std::sync::Arc;
use std::time::Duration;

use super::common::{
    emulate_startup_services, node_add_command, setup, stack_graph, test_node_target,
};

#[test]
fn node_remove_command_succeeds() {
    let (rt, serve, node_ctx, node_dir) = setup();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let node_name = "test_remove_node";
    let node_tag = "v1";

    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            description: config::description::Description::new("A test node.").unwrap(),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    peppy::test_support::disable_build_cmd(&peppy_json5_path);

    node_add_command(&node_path)
        .execute(&node_ctx)
        .expect("node add command should succeed");

    // Assert there is one node + the core node in the node stack after add
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let graph = rt.block_on(stack_graph(messenger_handle, &core_node_name));

    assert_eq!(
        graph.nodes.len(),
        2,
        "graph should only contain the core node + the added node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    NodeCommand {
        command: NodeCommands::Remove {
            node_ref: (node_name.to_string(), node_tag.to_string()),
            stop_instances: false,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node remove command should succeed");

    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Removed node '{node_name}:{node_tag}'")),
        "logs should contain success message for removing node. Logs:\n{}",
        logs
    );

    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let graph = rt.block_on(stack_graph(messenger_handle, &core_node_name));

    assert_eq!(
        graph.nodes.len(),
        1,
        "graph should only contain the core node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

/// What a test reads back after removing a node with a running instance: the
/// logs the commands wrote and the node graph the daemon reports afterwards.
struct RunningNodeRemoval {
    logs: String,
    graph: core_node_api::SerializedNodeGraph,
}

/// Adds `node_name` with a running instance `instance_id`, executes `remove`
/// on it, and waits until the daemon asks that instance to shut down.
fn remove_running_node(
    node_name: &str,
    instance_id: &str,
    remove: NodeCommands,
) -> RunningNodeRemoval {
    let (rt, serve, node_ctx, node_dir) = setup();
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            description: config::description::Description::new("A test node.").unwrap(),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    // Override the launch command to avoid spawning a real node process; the
    // ready, health and shutdown services run in-process on the mock
    // messenger. The instance must still be running when the remove reaches
    // it (the shutdown request awaited below is only ever sent for a live
    // instance), so its lifetime is tied to this function rather than to a
    // fixed 4s.
    let instances = peppy::test_support::InstanceLifetime::new();
    peppy::test_support::override_run_cmd_while(&peppy_json5_path, &instances.sentinel());

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    rt.block_on(emulate_startup_services(
        &node_messenger,
        &core_node_name,
        node_name,
        instance_id,
    ));
    let (_node_shutdown_handle, node_shutdown_rx) = rt
        .block_on(listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        ))
        .expect("node shutdown service should start");

    // A chained run (`run: true`) adds and starts the node in one command.
    NodeCommand {
        command: NodeCommands::Add {
            clock: None,
            publish_clock: None,
            restart: None,
            max_restarts: None,
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: true,
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            links: Vec::new(),
            vacant_links: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add+start command should succeed");

    NodeCommand { command: remove }
        .execute(&node_ctx)
        .expect("node remove command should succeed");

    rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), node_shutdown_rx).await })
        .expect("shutdown request should arrive")
        .expect("shutdown signal should be delivered");

    let logs = log_capture.logs();
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");
    let graph = rt.block_on(stack_graph(messenger_handle, &core_node_name));
    RunningNodeRemoval { logs, graph }
}

#[test]
fn node_remove_command_force_bypasses_prompt_and_stops_instances() {
    let node_name = "test_remove_running_node";
    let node_tag = "v1";
    let removal = remove_running_node(
        node_name,
        "test_remove_running_instance",
        NodeCommands::Remove {
            node_ref: (node_name.to_string(), node_tag.to_string()),
            stop_instances: false,
            force: true,
        },
    );

    // Node should be removed from the stack.
    let graph = removal.graph;
    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_name}:{node_tag}"))),
        "graph should not contain the removed node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

#[test]
fn node_remove_command_with_stop_instances_succeeds_and_stops_instances() {
    let node_name = "test_remove_stop_instances_node";
    let node_tag = "v1";
    let instance_id = "test_remove_stop_instances_instance";
    let removal = remove_running_node(
        node_name,
        instance_id,
        NodeCommands::Remove {
            node_ref: (node_name.to_string(), node_tag.to_string()),
            stop_instances: true,
            force: false,
        },
    );

    let logs = removal.logs;
    assert!(
        logs.contains(&format!("Removed node '{node_name}:{node_tag}'")),
        "logs should contain success message for removing node. Logs:\n{}",
        logs
    );
    // The emulated node answers its shutdown request and keeps running, so
    // the removal force-kills it and the CLI says so.
    assert!(
        logs.contains(&format!(
            "Node instance '{instance_id}' did not shut down gracefully within the grace period \
             and was force-killed"
        )),
        "logs should name the force-killed instance. Logs:\n{logs}"
    );

    let graph = removal.graph;
    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_name}:{node_tag}"))),
        "graph should not contain the removed node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}
