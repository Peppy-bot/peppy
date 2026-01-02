#[allow(dead_code)]
mod helpers;

use std::path::Path;
use std::sync::Arc;

use config::node::{Name as ConfigName, NodeConfigParser, SubscribedTopic, SubscribesTo};
use helpers::TestServeHandle;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::{AppContext, DaemonState};

fn make_consumer_depend_on_provider(
    consumer_peppy_json5: &Path,
    consumer_name: &str,
    provider_name: &str,
) {
    let mut consumer_cfg = NodeConfigParser::from_path(consumer_peppy_json5)
        .expect("consumer peppy.json5 should read");

    consumer_cfg.interfaces.subscribes_to = Some(SubscribesTo {
        topics: Some(vec![SubscribedTopic {
            id: ConfigName::new(format!("{consumer_name}_hello_world"))
                .expect("subscribed topic id should be valid"),
            node: provider_name.to_string(),
            name: "hello_world".to_string(),
            tag: "0.1.0".to_string(),
        }]),
        ..Default::default()
    });

    // Write JSON (valid JSON5) back to disk.
    let updated_consumer_content =
        serde_json::to_string_pretty(&consumer_cfg).expect("consumer peppy.json5 should serialize");
    std::fs::write(consumer_peppy_json5, updated_consumer_content)
        .expect("consumer peppy.json5 should update");
}

#[test]
fn node_list_command_succeeds() {
    let _serial_guard = helpers::serve_test_lock().lock().unwrap();
    // Mock messaging is sufficient for listing and dependency graph tests (no spawned node process).
    let serve = TestServeHandle::new();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    // Create a temp directory for the nodes
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for nodes");
    let provider_name = "test_list_provider";
    let consumer_name = "test_list_consumer";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
        serve.messenger(),
    ));

    // Set up logging
    let log_capture = serve.log_capture().clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Create both nodes using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(provider_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("provider node init command should succeed");

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(consumer_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("consumer node init command should succeed");

    let provider_path = node_dir.path().join(provider_name);
    let provider_peppy_json5 = provider_path.join("peppy.json5");
    assert!(
        provider_peppy_json5.exists(),
        "provider peppy.json5 should exist at {}",
        provider_peppy_json5.display()
    );

    let consumer_path = node_dir.path().join(consumer_name);
    let consumer_peppy_json5 = consumer_path.join("peppy.json5");
    assert!(
        consumer_peppy_json5.exists(),
        "consumer peppy.json5 should exist at {}",
        consumer_peppy_json5.display()
    );

    // Make the consumer depend on the provider by subscribing to its `hello_world` topic.
    make_consumer_depend_on_provider(&consumer_peppy_json5, consumer_name, provider_name);

    // Add the provider
    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: provider_peppy_json5,
            run: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("provider node add command should succeed");

    // Add the consumer, it depends on the provider
    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: consumer_peppy_json5,
            run: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("consumer node add command should succeed");

    // Now run the node list command and assert it prints nodes and dependencies
    NodeCommand {
        command: NodeCommands::List {
            dot_graph_path: None,
        },
    }
    .execute(&node_ctx)
    .expect("node list command should succeed");

    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("{provider_name}:0.1.0 (0 instances)")),
        "logs should contain the provider node and instance count. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains(&format!("{consumer_name}:0.1.0 (0 instances)")),
        "logs should contain the consumer node and instance count. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains(&format!(
            "{consumer_name}:0.1.0 (0 instances) -> {provider_name}:0.1.0 (0 instances)"
        )),
        "logs should contain the dependency edge consumer -> provider. Logs:\n{}",
        logs
    );
}

#[test]
fn node_list_command_with_dot_representation_succeeds() {
    let _serial_guard = helpers::serve_test_lock().lock().unwrap();
    let serve = TestServeHandle::new();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    // Create a temp directory for the nodes
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for nodes");
    let provider_name = "test_list_dot_provider";
    let consumer_name = "test_list_dot_consumer";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
        serve.messenger(),
    ));

    // Set up logging
    let log_capture = serve.log_capture().clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Create both nodes using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(provider_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("provider node init command should succeed");

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(consumer_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("consumer node init command should succeed");

    let provider_path = node_dir.path().join(provider_name);
    let provider_peppy_json5 = provider_path.join("peppy.json5");
    assert!(
        provider_peppy_json5.exists(),
        "provider peppy.json5 should exist at {}",
        provider_peppy_json5.display()
    );

    let consumer_path = node_dir.path().join(consumer_name);
    let consumer_peppy_json5 = consumer_path.join("peppy.json5");
    assert!(
        consumer_peppy_json5.exists(),
        "consumer peppy.json5 should exist at {}",
        consumer_peppy_json5.display()
    );

    make_consumer_depend_on_provider(&consumer_peppy_json5, consumer_name, provider_name);

    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: provider_peppy_json5,
            run: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("provider node add command should succeed");

    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: consumer_peppy_json5,
            run: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("consumer node add command should succeed");

    let dot_graph_path = node_dir.path().join("node_stack.dot");

    NodeCommand {
        command: NodeCommands::List {
            dot_graph_path: Some(dot_graph_path.clone()),
        },
    }
    .execute(&node_ctx)
    .expect("node list command should succeed");

    assert!(
        dot_graph_path.exists(),
        "DOT graph output should exist at {}",
        dot_graph_path.display()
    );

    let dot_graph =
        std::fs::read_to_string(&dot_graph_path).expect("DOT graph output should be readable");
    assert!(
        !dot_graph.trim().is_empty(),
        "DOT graph output should not be empty"
    );

    // Verify the DOT graph contains both nodes.
    let provider_label_fragment = format!("{provider_name}:0.1.0\\n(0 instances)");
    let consumer_label_fragment = format!("{consumer_name}:0.1.0\\n(0 instances)");
    assert!(
        dot_graph.contains(&provider_label_fragment),
        "DOT graph should contain provider label fragment '{}'. DOT:\n{}",
        provider_label_fragment,
        dot_graph
    );
    assert!(
        dot_graph.contains(&consumer_label_fragment),
        "DOT graph should contain consumer label fragment '{}'. DOT:\n{}",
        consumer_label_fragment,
        dot_graph
    );

    // Verify the DOT graph contains the dependency edge (consumer -> provider).
    let provider_id = dot_graph
        .lines()
        .find_map(|line| {
            let trimmed = line.trim();
            if !trimmed.contains(&provider_label_fragment) {
                return None;
            }
            let token = trimmed.split_whitespace().next()?;
            let id = token
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>();
            if id.is_empty() { None } else { Some(id) }
        })
        .expect("provider node id should be present in DOT graph");

    let consumer_id = dot_graph
        .lines()
        .find_map(|line| {
            let trimmed = line.trim();
            if !trimmed.contains(&consumer_label_fragment) {
                return None;
            }
            let token = trimmed.split_whitespace().next()?;
            let id = token
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>();
            if id.is_empty() { None } else { Some(id) }
        })
        .expect("consumer node id should be present in DOT graph");

    let has_edge = dot_graph.lines().any(|line| {
        let trimmed = line.trim();
        let Some((lhs, rhs)) = trimmed.split_once("->") else {
            return false;
        };
        let src = lhs
            .trim()
            .split_whitespace()
            .next()
            .unwrap_or("")
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        let dst = rhs
            .trim()
            .split_whitespace()
            .next()
            .unwrap_or("")
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        src == consumer_id && dst == provider_id
    });

    assert!(
        has_edge,
        "DOT graph should contain dependency edge {} -> {}. DOT:\n{}",
        consumer_id, provider_id, dot_graph
    );

    let logs = log_capture.logs();
    assert!(
        logs.contains("DOT graph saved to"),
        "logs should mention DOT graph output path. Logs:\n{}",
        logs
    );
}
