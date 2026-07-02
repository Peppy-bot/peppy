use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::path::Path;
use std::sync::Arc;

use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::node::{
    ConsumedTopic, DependsOn, EmittedTopic, NodeConfigParser, NodeDependency, Toolchain,
    TopicInterfaces,
};
use config::runtime::Name as ConfigName;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::AppContext;

fn make_consumer_depend_on_provider(
    provider_peppy_json5: &Path,
    consumer_peppy_json5: &Path,
    provider_name: &str,
) {
    let topic_name = "stack_list_topic";

    let mut provider_cfg = NodeConfigParser::from_path(provider_peppy_json5)
        .expect("provider peppy.json5 should read");

    provider_cfg.execution.build_cmd = None;

    let topic_ifaces = provider_cfg
        .interfaces
        .topics
        .get_or_insert_with(Default::default);
    let exposed = topic_ifaces.emits.get_or_insert_with(Vec::new);
    if !exposed.iter().any(|topic| topic.name == topic_name) {
        exposed.push(EmittedTopic {
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

    consumer_cfg.execution.build_cmd = None;

    consumer_cfg.manifest.depends_on = Some(DependsOn {
        nodes: vec![NodeDependency {
            name: ConfigName::new(provider_name).expect("valid provider name"),
            tag: "v1".to_string(),
            link_id: provider_name.to_string(),
            from_any: false,
        }],
        interfaces: vec![],
        pairings: vec![],
    });

    consumer_cfg.interfaces.topics = Some(TopicInterfaces {
        consumes: Some(vec![ConsumedTopic {
            link_id: provider_name.to_string(),
            name: topic_name.to_string(),
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

/// A node's row parsed from the rendered stack inventory table.
struct InventoryRow {
    stage: String,
    instances: String,
}

/// Parse a node's row from the rendered stack inventory table.
///
/// Inventory rows are drawn with box borders ('│'), which sets them apart from
/// the plain-text dependency edges and lets us read each padded cell by column
/// order: NODE, STAGE, INSTANCES, PATH. Reading the cells directly keeps the
/// assertions independent of comfy-table's column widths and padding, which
/// shift with the detected terminal width.
fn node_inventory_row(output: &str, label: &str) -> InventoryRow {
    let row = output
        .lines()
        .find(|line| line.contains('│') && line.contains(label))
        .unwrap_or_else(|| panic!("inventory row for {label} should be present:\n{output}"));

    // split('│') yields an empty leading cell before the first border, then the
    // padded NODE, STAGE, INSTANCES, PATH cells, then an empty trailing cell.
    let cells: Vec<&str> = row.split('│').map(str::trim).collect();

    InventoryRow {
        stage: cells.get(2).unwrap_or(&"").to_string(),
        instances: cells.get(3).unwrap_or(&"").to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_list_command_succeeds() {
    // Mock messaging is sufficient for listing and dependency graph tests (no spawned node process).
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
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
    make_consumer_depend_on_provider(&provider_peppy_json5, &consumer_peppy_json5, provider_name);

    // Add the provider
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(provider_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
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
            source: Some(consumer_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("consumer node add command should succeed");

    // Run the list command via the testable collecting variant so we can
    // assert on the exact text the CLI would print without capturing stdout.
    // `false`: render without ANSI color so the assertions match plain table
    // text regardless of whether the test runs attached to a terminal.
    let output = peppy::commands::stack::list_nodes_collecting(&node_ctx, None, false)
        .await
        .expect("node list command should succeed");

    let provider_label = format!("{provider_name}:v1");
    let consumer_label = format!("{consumer_name}:v1");

    // [INFO] prefixes are a side-effect of the tracing formatter; the table
    // output must not include them.
    assert!(
        !output.contains("[INFO]"),
        "stack list output should not include [INFO] prefixes:\n{output}"
    );
    assert!(
        output.contains("NODE")
            && output.contains("STAGE")
            && output.contains("INSTANCES")
            && output.contains("PATH"),
        "table headers missing:\n{output}"
    );

    let provider_row = node_inventory_row(&output, &provider_label);
    assert_eq!(
        provider_row.stage, "Ready",
        "provider row should be in Ready stage:\n{output}"
    );
    // No instances were started, so the INSTANCES column must render as "0".
    assert_eq!(
        provider_row.instances, "0",
        "provider row should report zero instances:\n{output}"
    );

    let consumer_row = node_inventory_row(&output, &consumer_label);
    assert_eq!(
        consumer_row.stage, "Ready",
        "consumer row should be in Ready stage:\n{output}"
    );

    assert!(
        output.contains(&format!("{consumer_label} ➔ {provider_label}")),
        "output should contain the dependency edge consumer ➔ provider:\n{output}"
    );

    // Silence unused variable when LogCapture is only constructed for
    // log-side effects in sibling tests.
    let _ = log_capture;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_list_command_with_dot_representation_succeeds() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
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

    make_consumer_depend_on_provider(&provider_peppy_json5, &consumer_peppy_json5, provider_name);

    NodeCommand {
        command: NodeCommands::Add {
            source: Some(provider_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("provider node add command should succeed");

    NodeCommand {
        command: NodeCommands::Add {
            source: Some(consumer_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("consumer node add command should succeed");

    let dot_graph_path = node_dir.path().join("node_stack.dot");

    let output = peppy::commands::stack::list_nodes_collecting(
        &node_ctx,
        Some(dot_graph_path.clone()),
        false,
    )
    .await
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
    let provider_label_fragment = format!("{provider_name}:v1\\n[Ready] (0 instances)");
    let consumer_label_fragment = format!("{consumer_name}:v1\\n[Ready] (0 instances)");
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

    assert!(
        output.contains("DOT graph saved to"),
        "output should mention DOT graph output path:\n{output}"
    );

    // LogCapture is still wired up for other tests in this file; keep it
    // referenced so the compiler doesn't complain about unused vars.
    let _ = log_capture;
}
