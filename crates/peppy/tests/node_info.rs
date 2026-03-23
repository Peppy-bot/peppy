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
                depends_on: {{
                    nodes: [
                        {{ name: "camera_node", tag: "0.1.0", local_id: "camera_node" }},
                        {{ name: "lidar_node", tag: "0.1.0", local_id: "lidar_node" }},
                        {{ name: "config_node", tag: "0.1.0", local_id: "config_node" }},
                        {{ name: "navigation_node", tag: "0.1.0", local_id: "navigation_node" }}
                    ]
                }}
            }},
            codegen: {{
                language: "rust",
            }},
            process: {{
                start_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                topics: {{
                    consumes: [
                        {{ local_node_id: "camera_node", name: "video_stream" }},
                        {{ local_node_id: "lidar_node", name: "point_cloud" }},
                        {{ local_node_id: "camera_node", name: "depth_stream" }}
                    ]
                }},
                services: {{
                    consumes: [
                        {{ local_node_id: "config_node", name: "get_config" }}
                    ]
                }},
                actions: {{
                    consumes: [
                        {{ local_node_id: "navigation_node", name: "go_to_pose" }}
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

    // Extract dependencies (unique local_node_id values) - this mirrors what print_node_info does
    let mut dependencies: BTreeSet<&str> = BTreeSet::new();
    for topic in topics {
        if let config::node::ConsumedTopic::Linked(linked) = topic {
            dependencies.insert(&linked.local_node_id);
        }
    }
    for service in services {
        dependencies.insert(&service.local_node_id);
    }
    for action in actions {
        if !action.local_node_id.is_empty() {
            dependencies.insert(&action.local_node_id);
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
            }},
            codegen: {{
                language: "rust",
            }},
            process: {{
                start_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                topics: {{
                    emits: [
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

    // Verify emits exists
    let emitted_topics = info_response
        .config
        .interfaces
        .topics
        .as_ref()
        .and_then(|t| t.emits.as_ref())
        .expect("emitted topics should exist");
    assert!(!emitted_topics.is_empty(), "should have emitted topics");

    // Verify no expects (no dependencies)
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
}
