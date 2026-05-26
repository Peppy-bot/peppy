use core_node_api::NodeStage;
use core_node_api::encoding::{NodeInfoRequest, NodeInfoResponse};
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, TimeoutConfig};
use peppy::context::AppContext;
use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use peppylib::core_node::transport::poll_node_info;
const CALLER_INSTANCE_ID: &str = "peppy-test";
const DEFAULTS: TimeoutConfig = TimeoutConfig {
    idle_secs: 60,
    max_secs: 3600,
};

fn write_peppy_json5(dir: &std::path::Path, content: &str) {
    let peppy_json5_path = dir.join("peppy.json5");
    let mut file = std::fs::File::create(&peppy_json5_path).expect("create peppy.json5");
    file.write_all(content.as_bytes())
        .expect("write peppy.json5");
}

/// Spins up a serve emulation, adds the node described by `peppy_json5` to the
/// node stack (without building it), and returns the pieces each test needs
/// to query `node_info` afterwards. This mirrors what a user would do when
/// calling `peppy node info name:tag` in practice: add first, then inspect.
struct AddedNode {
    rt: tokio::runtime::Runtime,
    _serve: ServeCommandEmulation,
    core_node_name: String,
    node_ctx: Arc<AppContext>,
    _node_dir: tempfile::TempDir,
    _dep_dirs: Vec<tempfile::TempDir>,
    _work_dir: tempfile::TempDir,
    _log_guard: tracing::subscriber::DefaultGuard,
}

fn add_node_to_stack(peppy_json5: &str) -> AddedNode {
    add_nodes_to_stack(&[], peppy_json5)
}

/// Adds each `dependency` config first, then the main `peppy_json5`. Useful
/// for tests where the main node declares `depends_on` — the stack rejects
/// adds whose declared dependencies aren't already present.
fn add_nodes_to_stack(dependencies: &[&str], peppy_json5: &str) -> AddedNode {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    // A separate working directory for AppContext so it doesn't clobber any
    // node's `peppy.json5` — the CLI treats `.` as the working dir, not the
    // node source.
    let work_dir = tempfile::tempdir().expect("failed to create temp work dir");

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Capture tracing output so the `info!()` line from the Add command stays
    // out of the test output unless the test explicitly asserts on it.
    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let log_guard = tracing::subscriber::set_default(subscriber);

    // Keep the dependency node dirs alive for the lifetime of the test so the
    // daemon can resolve any lazy paths stashed in its NodeEntity copies.
    let mut dep_dirs: Vec<tempfile::TempDir> = Vec::with_capacity(dependencies.len());
    for dep_peppy_json5 in dependencies {
        let dep_dir = tempfile::tempdir().expect("failed to create temp dep dir");
        write_peppy_json5(dep_dir.path(), dep_peppy_json5);
        NodeCommand {
            command: NodeCommands::Add {
                source: Some(dep_dir.path().display().to_string()),
                git_ref: None,
                sync: false,
                build: false,
                run: false,
                args: Vec::new(),
                instance_id: None,
                binds: Vec::new(),
                idle_timeout: DEFAULTS.idle_secs,
                max_timeout: DEFAULTS.max_secs,
                force: true,
            },
        }
        .execute(&node_ctx)
        .expect("dependency node add should succeed");
        dep_dirs.push(dep_dir);
    }

    let node_dir = tempfile::tempdir().expect("failed to create temp node dir");
    write_peppy_json5(node_dir.path(), peppy_json5);

    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_dir.path().display().to_string()),
            git_ref: None,
            sync: false,
            build: false,
            run: false,
            args: Vec::new(),
            instance_id: None,
            binds: Vec::new(),
            idle_timeout: DEFAULTS.idle_secs,
            max_timeout: DEFAULTS.max_secs,
            force: true,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    AddedNode {
        rt,
        _serve: serve,
        core_node_name,
        node_ctx,
        _node_dir: node_dir,
        _dep_dirs: dep_dirs,
        _work_dir: work_dir,
        _log_guard: log_guard,
    }
}

fn fetch_info(
    added: &AddedNode,
    node_name: &str,
    node_tag: &str,
) -> core_node_api::encoding::NodeInfo {
    let messenger_handle = added
        .node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");
    let response = added
        .rt
        .block_on(poll_node_info(
            &NodeInfoRequest::new(node_name, node_tag),
            messenger_handle,
            &added.core_node_name,
            CALLER_INSTANCE_ID,
            &added.core_node_name,
            Duration::from_secs(10),
        ))
        .expect("node_info request should succeed");
    match response {
        core_node_api::encoding::NodeInfoResponse::Found(info) => *info,
        core_node_api::encoding::NodeInfoResponse::NotInStack => panic!(
            "node_info unexpectedly reported `{}:{}` as not in the stack",
            node_name, node_tag
        ),
    }
}

#[test]
fn node_info_shows_dependencies_from_consumed_interfaces() {
    const NODE_NAME: &str = "consumer_node";
    const NODE_TAG: &str = "v1";

    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                depends_on: {
                    nodes: [
                        { name: "camera_node", tag: "v1", link_id: "camera_node" },
                        { name: "lidar_node", tag: "v1", link_id: "lidar_node" },
                        { name: "config_node", tag: "v1", link_id: "config_node" },
                        { name: "navigation_node", tag: "v1", link_id: "navigation_node" }
                    ]
                }
            },
            interfaces: {
                topics: {
                    consumes: [
                        { link_id: "camera_node", name: "video_stream" },
                        { link_id: "lidar_node", name: "point_cloud" },
                        { link_id: "camera_node", name: "depth_stream" }
                    ]
                },
                services: {
                    consumes: [
                        { link_id: "config_node", name: "get_config" }
                    ]
                },
                actions: {
                    consumes: [
                        { link_id: "navigation_node", name: "go_to_pose" }
                    ]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{NODE_NAME}", NODE_NAME)
    .replace("{NODE_TAG}", NODE_TAG);

    // The stack rejects adds whose declared dependencies are missing — spin
    // up minimal publisher nodes that expose exactly the interfaces
    // `consumer_node` consumes, so the add resolves cleanly.
    let camera_node = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "camera_node", tag: "v1" },
            interfaces: {
                topics: {
                    emits: [
                        { name: "video_stream" },
                        { name: "depth_stream" }
                    ]
                }
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] }
        }"#;
    let lidar_node = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "lidar_node", tag: "v1" },
            interfaces: {
                topics: { emits: [{ name: "point_cloud" }] }
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] }
        }"#;
    let config_node = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "config_node", tag: "v1" },
            interfaces: {
                services: { exposes: [{ name: "get_config" }] }
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] }
        }"#;
    let navigation_node = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "navigation_node", tag: "v1" },
            interfaces: {
                actions: { exposes: [{ name: "go_to_pose" }] }
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] }
        }"#;

    let added = add_nodes_to_stack(
        &[camera_node, lidar_node, config_node, navigation_node],
        &peppy_json5,
    );
    let info_response = fetch_info(&added, NODE_NAME, NODE_TAG);

    // Verify basic info
    assert_eq!(info_response.config.manifest.name.as_str(), NODE_NAME);
    assert_eq!(info_response.config.manifest.tag, NODE_TAG);
    assert_eq!(
        info_response.stage,
        NodeStage::Added,
        "node added with build=false should be in Added stage, got {:?}",
        info_response.stage
    );

    // Verify consumed interfaces exist
    let topics = info_response
        .config
        .interfaces
        .topics
        .as_ref()
        .and_then(|t| t.consumes.as_ref())
        .expect("consumed topics should exist");
    assert_eq!(topics.len(), 3, "should have 3 consumed topics");

    let services = info_response
        .config
        .interfaces
        .services
        .as_ref()
        .and_then(|s| s.consumes.as_ref())
        .expect("consumed services should exist");
    assert_eq!(services.len(), 1, "should have 1 consumed service");

    let actions = info_response
        .config
        .interfaces
        .actions
        .as_ref()
        .and_then(|a| a.consumes.as_ref())
        .expect("consumed actions should exist");
    assert_eq!(actions.len(), 1, "should have 1 consumed action");

    // Extract dependencies (unique link_id values) - mirrors what
    // `format_node_info` does when rendering the "Dependencies" section.
    let mut dependencies: BTreeSet<&str> = BTreeSet::new();
    for topic in topics {
        if let config::node::ConsumedTopic::Linked(linked) = topic {
            dependencies.insert(&linked.link_id);
        }
    }
    for service in services {
        dependencies.insert(&service.link_id);
    }
    for action in actions {
        if !action.link_id.is_empty() {
            dependencies.insert(&action.link_id);
        }
    }

    let deps_vec: Vec<&str> = dependencies.iter().copied().collect();
    assert_eq!(
        deps_vec,
        vec![
            "camera_node",
            "config_node",
            "lidar_node",
            "navigation_node"
        ],
        "dependencies should be sorted alphabetically"
    );
}

#[test]
fn node_info_no_dependencies_when_no_consumes() {
    const NODE_NAME: &str = "standalone_node";
    const NODE_TAG: &str = "v1";

    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
            },
            interfaces: {
                topics: {
                    emits: [
                        { name: "output_data", qos_profile: "standard" }
                    ]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{NODE_NAME}", NODE_NAME)
    .replace("{NODE_TAG}", NODE_TAG);

    let added = add_node_to_stack(&peppy_json5);
    let info_response = fetch_info(&added, NODE_NAME, NODE_TAG);

    assert_eq!(info_response.config.manifest.name.as_str(), NODE_NAME);

    // Verify emits exists
    let emitted_topics = info_response
        .config
        .interfaces
        .topics
        .as_ref()
        .and_then(|t| t.emits.as_ref())
        .expect("emitted topics should exist");
    assert!(!emitted_topics.is_empty(), "should have emitted topics");

    // No consumed interfaces anywhere.
    let no_consumed_topics = info_response
        .config
        .interfaces
        .topics
        .as_ref()
        .and_then(|t| t.consumes.as_ref())
        .is_none_or(|t| t.is_empty());
    let no_consumed_services = info_response
        .config
        .interfaces
        .services
        .as_ref()
        .and_then(|s| s.consumes.as_ref())
        .is_none_or(|s| s.is_empty());
    let no_consumed_actions = info_response
        .config
        .interfaces
        .actions
        .as_ref()
        .and_then(|a| a.consumes.as_ref())
        .is_none_or(|a| a.is_empty());
    assert!(
        no_consumed_topics && no_consumed_services && no_consumed_actions,
        "standalone node should have no dependencies"
    );
    let _ = added;
}

/// Asking for a node that was never added returns a *successful*
/// `NodeInfoResponse::NotInStack`. Prior to the response-shape change,
/// the daemon answered with `InvalidServiceRequest` — a protocol error
/// that produced a spurious ERROR log on every first-time `peppy node
/// add` because the preflight check passes through this same path.
#[test]
fn node_info_returns_not_in_stack_when_node_not_in_stack() {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let caller_handle = peppylib::MessengerHandle::from_shared(shared_messenger);

    // Also capture daemon-side logs so we can assert that no
    // service-handler ERROR is emitted for the negative lookup — the
    // original regression pollution came from this path.
    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let response = rt
        .block_on(poll_node_info(
            &NodeInfoRequest::new("ghost_node", "v999"),
            &caller_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_info should return a successful response for an unknown node");

    assert!(
        matches!(response, NodeInfoResponse::NotInStack),
        "expected NotInStack for ghost_node:v999, got: {response:?}"
    );

    let logs = log_capture.logs();
    assert!(
        !logs.contains("service handler returned error"),
        "negative node_info lookup must not emit a service-handler error log. Logs:\n{}",
        logs
    );
}
