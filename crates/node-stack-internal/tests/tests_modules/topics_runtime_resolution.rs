use std::path::PathBuf;

use node_stack::{NodeStack, NodeStackError};

use crate::helpers::config_common::core_node_config;

#[test]
fn topic_dependency_resolved_when_dependency_added_first() {
    let brain_dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "brain",
              tag: "1.0.0",
              language: "rust",
            },
            process: {
              start_cmd: ["brain"]
            },
            interfaces: {
                consumes: {
                    topics: [
                        {
                          id: "lidar_object_sub",
                          node: "lidar",
                          name: "push_lidar_object",
                          tag: "1.0.0"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependent node config");

    let lidar_dependency: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "lidar",
              tag: "1.0.0",
              language: "rust",
            },
            process: {
              start_cmd: ["lidar"]
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                          name: "push_lidar_object",
                          qos_profile: "sensor_data",
                          message_format: {
                            header: {
                              $type: "object",
                              stamp: "time",
                              frame_id: "u32",
                            },
                            x: "f32",
                            y: "f32",
                            z: "f32",
                            intensity: "f32",
                            return_type: "u8",
                            classification: "u8",
                          },
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependency node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Add the lidar dependency first
    stack
        .push_config(lidar_dependency, false, PathBuf::from("/tmp"))
        .expect("dependency node has no dependencies");
    assert_eq!(
        stack.len(),
        2,
        "stack should have core node + dependency node"
    );

    // Now add the dependent node - should succeed because dependency exists
    stack
        .push_config(brain_dependent, false, PathBuf::from("/tmp"))
        .expect("dependent node should be added when dependency exists");
    assert_eq!(stack.len(), 3, "stack should include the dependent node");

    let dependencies = stack.dependencies_of("brain", "1.0.0");
    let dependency_names: Vec<_> = dependencies
        .iter()
        .map(|node| node.config().manifest.name.as_str())
        .collect();
    assert_eq!(
        dependency_names,
        vec!["lidar"],
        "dependency edge should be wired"
    );

    let dependants = stack
        .dependents_of("lidar", "1.0.0")
        .into_iter()
        .map(|node| node.config().manifest.name.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        dependants,
        vec!["brain"],
        "inverse relationship should also be wired"
    );
}

#[test]
fn topic_dependency_fails_when_dependency_is_missing() {
    let brain_dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "brain",
              tag: "1.0.0",
              language: "rust",
            },
            process: {
              start_cmd: ["brain"]
            },
            interfaces: {
                consumes: {
                    topics: [
                        {
                          id: "lidar_object_sub",
                          node: "lidar",
                          name: "push_lidar_object",
                          tag: "1.0.0"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependent node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Adding a node that depends on a non-existent node should fail
    let result = stack.push_config(brain_dependent, false, PathBuf::from("/tmp"));
    let Err(NodeStackError::MissingDependency {
        dependency,
        dependency_tag,
        ..
    }) = result
    else {
        panic!("expected MissingDependency error, got {:?}", result);
    };
    assert_eq!(dependency, "lidar");
    assert_eq!(dependency_tag, "1.0.0");
    assert_eq!(stack.len(), 1, "stack should only have core node");
}

#[test]
fn topic_dependency_fails_when_topic_not_exposed_by_dependency() {
    // Test the scenario where a node subscribes to a topic from another node,
    // but the target node exists without exposing the requested topic
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "brain",
              tag: "1.0.0",
              language: "rust",
            },
            process: {
              start_cmd: ["brain"]
            },
            interfaces: {
                consumes: {
                    topics: [
                        {
                          id: "lidar_object_sub",
                          node: "lidar",
                          name: "push_lidar_object",
                          tag: "1.0.0"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependent node config");

    // This node has the correct name but exposes a different topic
    let dependency_wrong_topic: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "lidar",
              tag: "1.0.0",
              language: "rust",
            },
            process: {
              start_cmd: ["lidar"]
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                          name: "push_camera_frame",
                          qos_profile: "sensor_data",
                          message_format: {
                            width: "u32",
                            height: "u32"
                          }
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependency node config with wrong topic");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Add the node with the correct name but wrong topic
    stack
        .push_config(dependency_wrong_topic, false, PathBuf::from("/tmp"))
        .expect("lidar has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have core node + lidar");

    // Adding brain should fail because lidar doesn't expose "push_lidar_object"
    let result = stack.push_config(dependent, false, PathBuf::from("/tmp"));
    let Err(NodeStackError::MissingInterface {
        dependency,
        dependency_tag,
        interface_kind,
        interface_name,
        ..
    }) = result
    else {
        panic!("expected MissingInterface error, got {:?}", result);
    };
    assert_eq!(dependency, "lidar");
    assert_eq!(dependency_tag, "1.0.0");
    assert_eq!(interface_kind, "Topic");
    assert_eq!(interface_name, "push_lidar_object");
    assert_eq!(
        stack.len(),
        2,
        "stack should still only have core node + lidar"
    );
}
