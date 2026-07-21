use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::node::{
    ConsumedTopic, DependsOn, EmittedTopic, NodeConfigParser, NodeDependency, Toolchain,
    TopicInterfaces,
};
use config::runtime::{BoundProducers, Name as ConfigName, ProducerRef};
use peppy::commands::Command;
use peppy::commands::node::{
    NodeCommand, NodeCommands, NodeName, TimeoutConfig, run_instance_async,
};
use peppy::context::AppContext;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::{CoreNodePresenceMessenger, MessengerHandle};

use super::common::test_node_target;

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
    if !exposed.iter().any(|topic| topic.name() == topic_name) {
        exposed.push(EmittedTopic::Native(config::node::NativeEmittedTopic {
            name: topic_name.to_string(),
            ..Default::default()
        }));
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
            cardinality: config::node::Cardinality::One,
        }],
        contracts: vec![],
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

fn add_ready_node(ctx: &Arc<AppContext>, source: &Path) {
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(source.display().to_string()),
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
    .execute(ctx)
    .expect("node add should succeed");
}

/// Installs the ready/health listeners a running instance is expected to
/// serve. The returned task handles detach on drop, so discarding them keeps
/// the services listening for the rest of the test (the same pattern
/// `node_pair.rs` uses).
async fn install_node_services(
    messenger: &MessengerHandle,
    core_node: &str,
    node_name: &str,
    instance_id: &str,
) {
    listen_for_node_ready(
        messenger,
        core_node,
        instance_id,
        test_node_target(node_name),
    )
    .await
    .expect("node ready service should start");
    listen_for_node_health(
        messenger,
        core_node,
        instance_id,
        test_node_target(node_name),
    )
    .await
    .expect("node health service should start");
}

/// A node's row parsed from the rendered stack inventory table.
struct InventoryRow {
    stage: String,
    instances: String,
}

/// Parse a node's row from the rendered stack inventory table.
///
/// Inventory rows are drawn with box borders ('│'), which sets them apart from
/// dependency edges and lets us read each padded cell by column order: NODE,
/// STAGE, INSTANCES, PATH. Empty segments belong to the enclosing core-node
/// panel and nested table borders, so they are discarded.
fn node_inventory_row(output: &str, label: &str) -> InventoryRow {
    let row = output
        .lines()
        .find(|line| line.contains('│') && line.contains(label))
        .unwrap_or_else(|| panic!("inventory row for {label} should be present:\n{output}"));

    let cells: Vec<&str> = row
        .split('│')
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect();

    InventoryRow {
        stage: cells.get(1).unwrap_or(&"").to_string(),
        instances: cells.get(2).unwrap_or(&"").to_string(),
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
    let output = peppy::commands::stack::list_nodes_collecting(&node_ctx, false)
        .await
        .expect("node list command should succeed")
        .output;

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
async fn stack_list_renders_every_live_daemon_local_first_and_honors_override() {
    let local = ServeCommandEmulation::with_mock_named("z-local")
        .await
        .expect("local daemon should start");
    let shared_messenger = local.messenger();
    let remote = ServeCommandEmulation::with_shared_mock(Arc::clone(&shared_messenger), "a-remote")
        .await
        .expect("remote daemon should start");

    // Put a real bound consumer in the remote stack whose producer address
    // points at the local daemon. The list fan-out must preserve that
    // cross-daemon address as `instance@core_node` inside the remote section.
    let work_dir = tempfile::tempdir().expect("node work dir");
    let provider_name = "shared-provider";
    let consumer_name = "remote-consumer";
    let provider_instance = "provider-instance";
    let consumer_instance = "consumer-instance";
    let local_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(local.daemon_state_path()),
    );
    let remote_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(remote.daemon_state_path()),
    );

    for node_name in [provider_name, consumer_name] {
        NodeCommand {
            command: NodeCommands::Init {
                node_name: NodeName::new(node_name).expect("valid node name"),
                to_dir: None,
                toolchain: Toolchain::Cargo,
                with_container: false,
            },
        }
        .execute(&local_ctx)
        .expect("node init should succeed");
    }
    let provider_path = work_dir.path().join(provider_name);
    let consumer_path = work_dir.path().join(consumer_name);
    make_consumer_depend_on_provider(
        &provider_path.join("peppy.json5"),
        &consumer_path.join("peppy.json5"),
        provider_name,
    );
    peppy::test_support::override_run_cmd(&provider_path.join("peppy.json5"));
    peppy::test_support::override_run_cmd(&consumer_path.join("peppy.json5"));

    add_ready_node(&local_ctx, &provider_path);
    add_ready_node(&remote_ctx, &provider_path);
    add_ready_node(&remote_ctx, &consumer_path);

    let messenger_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    install_node_services(
        &messenger_handle,
        "z-local",
        provider_name,
        provider_instance,
    )
    .await;
    let timeouts = TimeoutConfig {
        idle_secs: 30,
        max_secs: 60,
    };
    run_instance_async(
        &messenger_handle,
        "z-local",
        provider_name,
        "v1",
        &[],
        Some(provider_instance.to_string()),
        BTreeMap::new(),
        BTreeMap::new(),
        Vec::new(),
        &timeouts,
    )
    .await
    .expect("local producer should start");

    install_node_services(
        &messenger_handle,
        "a-remote",
        consumer_name,
        consumer_instance,
    )
    .await;
    let mut slot_bindings = BTreeMap::new();
    slot_bindings.insert(
        provider_name.to_string(),
        BoundProducers::try_from(vec![ProducerRef::new("z-local", provider_instance)])
            .expect("one producer is a valid binding set"),
    );
    run_instance_async(
        &messenger_handle,
        "a-remote",
        consumer_name,
        "v1",
        &[],
        Some(consumer_instance.to_string()),
        slot_bindings,
        BTreeMap::new(),
        Vec::new(),
        &timeouts,
    )
    .await
    .expect("remote consumer should start");

    let ctx = Arc::new(
        AppContext::with_messenger(local.temp_dir(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(local.daemon_state_path()),
    );
    let report = peppy::commands::stack::list_nodes_collecting(&ctx, false)
        .await
        .expect("multi-daemon stack list should succeed");
    assert!(
        report.failed_names.is_empty(),
        "every daemon section must succeed:\n{}",
        report.output
    );
    let output = report.output;

    let local_header = output
        .find("Core node: z-local (host:")
        .expect("local header");
    let remote_header = output
        .find("Core node: a-remote (host:")
        .expect("remote header");
    assert!(
        local_header < remote_header,
        "local daemon must render before lexicographically earlier peers:\n{output}"
    );
    let local_section = &output[local_header..remote_header];
    let remote_section = &output[remote_header..];
    assert_eq!(
        output.matches("Core node:").count(),
        2,
        "one section per daemon:\n{output}"
    );
    assert!(
        local_section.contains("z-local:"),
        "local root row missing:\n{output}"
    );
    assert!(
        remote_section.contains("a-remote:"),
        "remote root row missing:\n{output}"
    );
    assert!(
        remote_section.contains(&format!("{provider_name} → {provider_instance}@z-local")),
        "cross-daemon binding missing from remote section:\n{output}"
    );
    assert!(
        !local_section.contains(&format!("{provider_name} → {provider_instance}@z-local")),
        "remote consumer binding leaked into the local section:\n{output}"
    );

    let targeted_ctx = Arc::new(
        AppContext::with_messenger(local.temp_dir(), shared_messenger)
            .with_daemon_state_file(local.daemon_state_path())
            .with_core_node_override(Some("a-remote".to_string())),
    );
    let targeted_report = peppy::commands::stack::list_nodes_collecting(&targeted_ctx, false)
        .await
        .expect("explicit remote stack list should succeed");
    assert!(
        targeted_report.failed_names.is_empty(),
        "the targeted section must succeed:\n{}",
        targeted_report.output
    );
    let targeted = targeted_report.output;
    assert!(targeted.contains("Core node: a-remote (host:"));
    assert!(!targeted.contains("Core node: z-local"));
    assert_eq!(targeted.matches("Core node:").count(), 1);

    drop(remote);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_list_warns_when_multiple_live_tokens_claim_one_name() {
    let daemon = ServeCommandEmulation::with_mock_named("claimed-core")
        .await
        .expect("daemon should start");
    let shared_messenger = daemon.messenger();
    let handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _foreign_claim =
        CoreNodePresenceMessenger::declare(&handle, "claimed-core", "foreign-instance")
            .await
            .expect("foreign presence should be declared");
    let ctx = Arc::new(
        AppContext::with_messenger(daemon.temp_dir(), shared_messenger)
            .with_daemon_state_file(daemon.daemon_state_path()),
    );

    let report = peppy::commands::stack::list_nodes_collecting(&ctx, false)
        .await
        .expect("duplicate tokens should not duplicate or fail the section");
    assert!(
        report.failed_names.is_empty(),
        "the duplicated name must still be answered by the live daemon:\n{}",
        report.output
    );
    let output = report.output;
    assert_eq!(output.matches("Core node: claimed-core").count(), 1);
    assert!(
        output.contains("warning: 2 live daemons currently claim this name"),
        "duplicate warning missing:\n{output}"
    );

    let targeted_ctx = Arc::new(
        AppContext::with_messenger(daemon.temp_dir(), daemon.messenger())
            .with_daemon_state_file(daemon.daemon_state_path())
            .with_core_node_override(Some("claimed-core".to_string())),
    );
    let targeted = peppy::commands::stack::list_nodes_collecting(&targeted_ctx, false)
        .await
        .expect("targeted duplicate-name query should succeed")
        .output;
    assert_eq!(targeted.matches("Core node: claimed-core").count(), 1);
    assert!(
        targeted
            .contains("warning: 2 live daemons currently claim this name; answered by instance "),
        "targeted collision should identify the answering daemon:\n{targeted}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_list_keeps_healthy_sections_when_an_enumerated_daemon_cannot_answer() {
    let daemon = ServeCommandEmulation::with_mock_named("healthy-core")
        .await
        .expect("daemon should start");
    let shared_messenger = daemon.messenger();
    let handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    // A retained presence token with no matching stack-list listener models
    // the narrow enumerate-to-query race where a daemon vanishes.
    let _stale_claim = CoreNodePresenceMessenger::declare(&handle, "gone-core", "gone-instance")
        .await
        .expect("stale presence should be declared");
    let ctx = Arc::new(
        AppContext::with_messenger(daemon.temp_dir(), shared_messenger)
            .with_daemon_state_file(daemon.daemon_state_path()),
    );

    let report = peppy::commands::stack::list_nodes_collecting(&ctx, false)
        .await
        .expect("collection succeeds even when one section fails");
    assert!(report.output.contains("Core node: healthy-core (host:"));
    assert!(
        report.output.contains("healthy-core:"),
        "healthy graph missing:\n{}",
        report.output
    );
    assert!(
        report
            .output
            .contains("Core node: gone-core (host: unknown)")
    );
    assert!(
        report.output.contains("error:"),
        "failed section missing:\n{}",
        report.output
    );
    assert_eq!(
        report.failed_names,
        vec!["gone-core".to_string()],
        "the failed daemon must drive the CLI's non-zero exit"
    );
}
