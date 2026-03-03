use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::path::Path;
use std::sync::Arc;

use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::node::{
    ExposedTopic, Name as ConfigName, NodeConfigParser, SubscribedTopic, SubscribesTo, Toolchain,
};
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::commands::stack::{StackCommand, StackCommands};
use peppy::context::AppContext;

fn make_consumer_depend_on_provider(
    provider_peppy_json5: &Path,
    consumer_peppy_json5: &Path,
    consumer_name: &str,
    provider_name: &str,
) {
    let topic_name = "stack_list_topic";

    let mut provider_cfg = NodeConfigParser::from_path(provider_peppy_json5)
        .expect("provider peppy.json5 should read");

    provider_cfg.process.as_mut().unwrap().add_cmd = None;

    let exposes = provider_cfg
        .interfaces
        .exposes
        .get_or_insert_with(Default::default);
    let topics = exposes.topics.get_or_insert_with(Vec::new);
    if !topics.iter().any(|topic| topic.name == topic_name) {
        topics.push(ExposedTopic {
            name: topic_name.to_string(),
            ..Default::default()
        });
    }

    let updated_provider_content =
        serde_json::to_string_pretty(&provider_cfg).expect("provider peppy.json5 should serialize");
    std::fs::write(provider_peppy_json5, updated_provider_content)
        .expect("provider peppy.json5 should update");
    config::fingerprint::create_codegen_fingerprint(
        provider_peppy_json5,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let mut consumer_cfg = NodeConfigParser::from_path(consumer_peppy_json5)
        .expect("consumer peppy.json5 should read");

    consumer_cfg.process.as_mut().unwrap().add_cmd = None;

    consumer_cfg.interfaces.subscribes_to = Some(SubscribesTo {
        topics: Some(vec![SubscribedTopic {
            id: ConfigName::new(format!("{consumer_name}_{topic_name}"))
                .expect("subscribed topic id should be valid"),
            node: provider_name.to_string(),
            name: topic_name.to_string(),
            tag: "0.1.0".to_string(),
        }]),
        ..Default::default()
    });

    // Write JSON (valid JSON5) back to disk.
    let updated_consumer_content =
        serde_json::to_string_pretty(&consumer_cfg).expect("consumer peppy.json5 should serialize");
    std::fs::write(consumer_peppy_json5, updated_consumer_content)
        .expect("consumer peppy.json5 should update");
    config::fingerprint::create_codegen_fingerprint(
        consumer_peppy_json5,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_list_command_succeeds() {
    // Mock messaging is sufficient for listing and dependency graph tests (no spawned node process).
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let daemon_node_name = serve.daemon_node_name().to_string();
    assert!(
        !daemon_node_name.is_empty(),
        "daemon_node_name should not be empty"
    );

    // Create a temp directory for the nodes
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for nodes");
    let provider_name = "test_list_provider";
    let consumer_name = "test_list_consumer";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Set up logging
    let log_capture = LogCapture::new();
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
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("provider node init command should succeed");

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(consumer_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
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

    // Make the consumer depend on the provider by subscribing to a topic exposed by the provider.
    make_consumer_depend_on_provider(
        &provider_peppy_json5,
        &consumer_peppy_json5,
        consumer_name,
        provider_name,
    );

    // Add the provider
    NodeCommand {
        command: NodeCommands::Add {
            source: provider_path.display().to_string(),
            git_ref: None,
            start: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("provider node add command should succeed");

    // Add the consumer, it depends on the provider
    NodeCommand {
        command: NodeCommands::Add {
            source: consumer_path.display().to_string(),
            git_ref: None,
            start: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("consumer node add command should succeed");

    // Now run the node list command and assert it prints nodes and dependencies
    StackCommand {
        command: StackCommands::List {
            dot_graph_path: None,
        },
    }
    .execute(&node_ctx)
    .expect("node list command should succeed");

    let logs = log_capture.logs();
    let provider_label = format!("{provider_name}:0.1.0");
    let consumer_label = format!("{consumer_name}:0.1.0");
    let provider_line = logs
        .lines()
        .find(|line| line.contains(&provider_label) && line.contains("instances:"))
        .expect("logs should contain the provider node");
    assert!(
        provider_line.contains("0 instances:"),
        "logs should contain the provider node and instance count. Logs:\n{}",
        logs
    );
    let consumer_line = logs
        .lines()
        .find(|line| line.contains(&consumer_label) && line.contains("instances:"))
        .expect("logs should contain the consumer node");
    assert!(
        consumer_line.contains("0 instances:"),
        "logs should contain the consumer node and instance count. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains(&format!("{consumer_label} -> {provider_label}")),
        "logs should contain the dependency edge consumer -> provider. Logs:\n{}",
        logs
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_list_command_with_dot_representation_succeeds() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let daemon_node_name = serve.daemon_node_name().to_string();
    assert!(
        !daemon_node_name.is_empty(),
        "daemon_node_name should not be empty"
    );

    // Create a temp directory for the nodes
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for nodes");
    let provider_name = "test_list_dot_provider";
    let consumer_name = "test_list_dot_consumer";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Set up logging
    let log_capture = LogCapture::new();
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
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("provider node init command should succeed");

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(consumer_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
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

    make_consumer_depend_on_provider(
        &provider_peppy_json5,
        &consumer_peppy_json5,
        consumer_name,
        provider_name,
    );

    NodeCommand {
        command: NodeCommands::Add {
            source: provider_path.display().to_string(),
            git_ref: None,
            start: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("provider node add command should succeed");

    NodeCommand {
        command: NodeCommands::Add {
            source: consumer_path.display().to_string(),
            git_ref: None,
            start: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("consumer node add command should succeed");

    let dot_graph_path = node_dir.path().join("node_stack.dot");

    StackCommand {
        command: StackCommands::List {
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
            .split_whitespace()
            .next()
            .unwrap_or("")
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        let dst = rhs
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
