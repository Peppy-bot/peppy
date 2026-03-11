use core_node::encoding::{NodeInfoRequest, NodeInfoResponse, NodeSource};
use peppy::test_support::ServeCommandEmulation;
use peppylib::ServiceMessenger;
use peppylib::messaging::MessengerHandle;
use std::collections::BTreeSet;
use std::io::Write;
use std::time::Duration;

const CALLER_INSTANCE_ID: &str = "peppy-test";

fn write_peppy_json5(dir: &std::path::Path, content: &str) {
    let peppy_json5_path = dir.join("peppy.json5");
    let mut file = std::fs::File::create(&peppy_json5_path).expect("create peppy.json5");
    file.write_all(content.as_bytes())
        .expect("write peppy.json5");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_info_shows_dependencies_from_consumed_interfaces() {
    const NODE_NAME: &str = "consumer_node";
    const NODE_TAG: &str = "0.1.0";

    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    // Create a node directory with dependencies (consumes interfaces)
    let node_dir = tempfile::tempdir().expect("failed to create temp node dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                language: "rust"
            }},
            process: {{
                start_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                consumes: {{
                    topics: [
                        {{ id: "camera_feed", node: "camera_node", name: "video_stream" }},
                        {{ id: "lidar_data", node: "lidar_node", name: "point_cloud" }},
                        {{ id: "other_camera", node: "camera_node", name: "depth_stream" }}
                    ],
                    services: [
                        {{ id: "config_service", node: "config_node", name: "get_config" }}
                    ],
                    actions: [
                        {{ id: "navigate", node: "navigation_node", name: "go_to_pose" }}
                    ]
                }}
            }}
        }}"#
    );
    write_peppy_json5(node_dir.path(), &peppy_json5);

    // Send NodeInfoRequest to the core node
    let caller_handle = MessengerHandle::from_shared(serve.messenger());

    let request = NodeInfoRequest::new(NodeSource::Fs(node_dir.path().to_path_buf()));
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &caller_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        core_node::names::NODE_INFO,
        Some(&core_node_name),
        None,
        request_payload,
        Duration::from_secs(10),
    )
    .await
    .expect("node_info request should succeed");

    let info_response =
        NodeInfoResponse::decode(&response.payload()).expect("decode should succeed");

    // Verify basic info
    assert_eq!(info_response.config.manifest.name.as_str(), NODE_NAME);
    assert_eq!(info_response.config.manifest.tag, NODE_TAG);

    // Verify consumed interfaces exist
    let subscribes = info_response
        .config
        .interfaces
        .consumes
        .as_ref()
        .expect("consumes should exist");

    // Verify topics
    let topics = subscribes.topics.as_ref().expect("topics should exist");
    assert_eq!(topics.len(), 3, "should have 3 consumed topics");

    // Verify services
    let services = subscribes.services.as_ref().expect("services should exist");
    assert_eq!(services.len(), 1, "should have 1 consumed service");

    // Verify actions
    let actions = subscribes.actions.as_ref().expect("actions should exist");
    assert_eq!(actions.len(), 1, "should have 1 consumed action");

    // Extract dependencies (unique node names) - this mirrors what print_node_info does
    let mut dependencies: BTreeSet<&str> = BTreeSet::new();
    for topic in topics {
        dependencies.insert(&topic.node);
    }
    for service in services {
        dependencies.insert(&service.node);
    }
    for action in actions {
        if !action.node.is_empty() {
            dependencies.insert(&action.node);
        }
    }

    // Verify we have the expected unique dependencies
    assert_eq!(dependencies.len(), 4, "should have 4 unique dependencies");
    assert!(
        dependencies.contains("camera_node"),
        "should depend on camera_node"
    );
    assert!(
        dependencies.contains("lidar_node"),
        "should depend on lidar_node"
    );
    assert!(
        dependencies.contains("config_node"),
        "should depend on config_node"
    );
    assert!(
        dependencies.contains("navigation_node"),
        "should depend on navigation_node"
    );

    // Verify the dependencies are in sorted order (BTreeSet guarantees this)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_info_no_dependencies_when_no_consumes() {
    const NODE_NAME: &str = "standalone_node";
    const NODE_TAG: &str = "0.1.0";

    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    // Create a node with no dependencies (only exposes interfaces, doesn't subscribe)
    let node_dir = tempfile::tempdir().expect("failed to create temp node dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                language: "rust"
            }},
            process: {{
                start_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                exposes: {{
                    topics: [
                        {{ name: "output_data", qos_profile: "standard" }}
                    ]
                }}
            }}
        }}"#
    );
    write_peppy_json5(node_dir.path(), &peppy_json5);

    // Send NodeInfoRequest
    let caller_handle = MessengerHandle::from_shared(serve.messenger());

    let request = NodeInfoRequest::new(NodeSource::Fs(node_dir.path().to_path_buf()));
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &caller_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        core_node::names::NODE_INFO,
        Some(&core_node_name),
        None,
        request_payload,
        Duration::from_secs(10),
    )
    .await
    .expect("node_info request should succeed");

    let info_response =
        NodeInfoResponse::decode(&response.payload()).expect("decode should succeed");

    // Verify basic info
    assert_eq!(info_response.config.manifest.name.as_str(), NODE_NAME);

    // Verify exposes exists
    let exposes = info_response
        .config
        .interfaces
        .exposes
        .as_ref()
        .expect("exposes should exist");
    assert!(
        exposes.topics.as_ref().is_some_and(|t| !t.is_empty()),
        "should have exposed topics"
    );

    // Verify no consumes (no dependencies)
    assert!(
        info_response.config.interfaces.consumes.is_none()
            || info_response
                .config
                .interfaces
                .consumes
                .as_ref()
                .is_some_and(|s| {
                    s.topics.as_ref().is_none_or(|t| t.is_empty())
                        && s.services.as_ref().is_none_or(|sv| sv.is_empty())
                        && s.actions.as_ref().is_none_or(|a| a.is_empty())
                }),
        "standalone node should have no dependencies"
    );
}
