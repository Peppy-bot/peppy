use std::path::PathBuf;

use node_stack::{NodeStack, NodeStackError};

use crate::helpers::config_common::daemon_node_config;

#[test]
fn service_dependency_resolved_when_dependency_added_first() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: {
              name: "brain",
              tag: "1.0.0",
              language: "rust",
            },
            build: {
              start_cmd: ["brain"]
            },
            interfaces: {
                subscribes_to: {
                    services: [
                        {
                          id: "reset_sensor_sub",
                          node: "lidar",
                          name: "reset_sensor",
                          tag: "1.0.0"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependent node config");

    let dependency: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: {
              name: "lidar",
              tag: "1.0.0",
              language: "rust",
            },
            build: {
              start_cmd: ["lidar"]
            },
            interfaces: {
                exposes: {
                    services: [
                        {
                          name: "reset_sensor",
                          request_message_format: {
                            force: "bool"
                          },
                          response_message_format: {
                            success: "bool",
                            error_message: {
                              $type: "string",
                              $optional: true
                            }
                          }
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependency node config");

    let stack = NodeStack::new(daemon_node_config(), None, PathBuf::from("/tmp"));

    // Add the dependency first
    stack
        .push_config(dependency, false, PathBuf::from("/tmp"))
        .expect("dependency node has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have daemon + dependency node");

    // Now add the dependent node
    stack
        .push_config(dependent, false, PathBuf::from("/tmp"))
        .expect("dependent node should be added when dependency exists");
    assert_eq!(stack.len(), 3, "stack should include the dependent node");

    let deps = stack
        .dependencies_of("brain", "1.0.0")
        .into_iter()
        .map(|node| {
            (
                node.config().manifest.name.as_str().to_owned(),
                node.config().manifest.tag.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        deps,
        vec![("lidar".to_string(), "1.0.0".to_string())],
        "dependency edge should be wired for services"
    );

    let dependants = stack
        .dependents_of("lidar", "1.0.0")
        .into_iter()
        .map(|node| node.config().manifest.name.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        dependants,
        vec!["brain"],
        "inverse relationship should also be wired for services"
    );
}

#[test]
fn service_dependency_fails_when_dependency_is_missing() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: {
              name: "brain",
              tag: "1.0.0",
              language: "rust",
            },
            build: {
              start_cmd: ["brain"]
            },
            interfaces: {
                subscribes_to: {
                    services: [
                        {
                          id: "reset_sensor_sub",
                          node: "lidar",
                          name: "reset_sensor",
                          tag: "1.0.0"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependent node config");

    let stack = NodeStack::new(daemon_node_config(), None, PathBuf::from("/tmp"));

    // Adding a node that depends on a non-existent service provider should fail
    let result = stack.push_config(dependent, false, PathBuf::from("/tmp"));
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
    assert_eq!(stack.len(), 1, "stack should only have daemon node");
}

#[test]
fn service_dependency_fails_when_service_not_exposed_by_dependency() {
    // Test the scenario where a node subscribes to a service from another node,
    // but the target node exists without exposing the requested service
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: {
              name: "brain",
              tag: "1.0.0",
              language: "rust",
            },
            build: {
              start_cmd: ["brain"]
            },
            interfaces: {
                subscribes_to: {
                    services: [
                        {
                          id: "reset_sensor_sub",
                          node: "lidar",
                          name: "reset_sensor",
                          tag: "1.0.0"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependent node config");

    // This node has the correct name but exposes a different service
    let dependency_wrong_service: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: {
              name: "lidar",
              tag: "1.0.0",
              language: "rust",
            },
            build: {
              start_cmd: ["lidar"]
            },
            interfaces: {
                exposes: {
                    services: [
                        {
                          name: "calibrate_sensor",
                          request_message_format: {
                            force: "bool"
                          },
                          response_message_format: {
                            success: "bool"
                          }
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependency node config with wrong service");

    let stack = NodeStack::new(daemon_node_config(), None, PathBuf::from("/tmp"));

    // Add the node with the correct name but wrong service
    stack
        .push_config(dependency_wrong_service, false, PathBuf::from("/tmp"))
        .expect("lidar has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have daemon + lidar");

    // Adding brain should fail because lidar doesn't expose "reset_sensor"
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
    assert_eq!(interface_kind, "Service");
    assert_eq!(interface_name, "reset_sensor");
    assert_eq!(
        stack.len(),
        2,
        "stack should still only have daemon + lidar"
    );
}
