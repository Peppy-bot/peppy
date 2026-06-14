use std::path::PathBuf;

use config::{ConfigError, ParsingError};
use node_stack::{NodeStack, NodeStackError};

use crate::helpers::config_common::core_node_config;
use crate::helpers::graph_query;

#[test]
fn service_dependency_resolved_when_dependency_added_first() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "v1",
              depends_on: {
                nodes: [
                  { name: "lidar", tag: "v1", link_id: "lidar" }
                ]
              },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          link_id: "lidar",
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["brain"]
            },
        }"#,
    )
    .expect("valid dependent node config");

    let dependency: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "lidar",
              tag: "v1",
            },
            interfaces: {
                services: {
                    exposes: [
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
            },
            execution: {
              language: "rust",
              run_cmd: ["lidar"]
            },
        }"#,
    )
    .expect("valid dependency node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Add the dependency first
    stack
        .push_config(dependency, false, PathBuf::from("/tmp"))
        .expect("dependency node has no dependencies");
    assert_eq!(
        stack.len(),
        2,
        "stack should have core node + dependency node"
    );

    // Now add the dependent node
    stack
        .push_config(dependent, false, PathBuf::from("/tmp"))
        .expect("dependent node should be added when dependency exists");
    assert_eq!(stack.len(), 3, "stack should include the dependent node");

    let deps = graph_query::dependency_name_tags(&stack, "brain", "v1");
    assert_eq!(
        deps,
        vec![("lidar".to_string(), "v1".to_string())],
        "dependency edge should be wired for services"
    );

    let dependants = graph_query::dependent_names(&stack, "lidar", "v1");
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
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "v1",
              depends_on: {
                nodes: [
                  { name: "lidar", tag: "v1", link_id: "lidar" }
                ]
              },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          link_id: "lidar",
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["brain"]
            },
        }"#,
    )
    .expect("valid dependent node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Adding a node that depends on a non-existent service provider should fail
    let result = stack.push_config(dependent, false, PathBuf::from("/tmp"));
    let Err(NodeStackError::Config(ConfigError::Parsing(ParsingError::MissingDependency {
        dependency,
        dependency_tag,
        ..
    }))) = result
    else {
        panic!("expected MissingDependency error, got {:?}", result);
    };
    assert_eq!(dependency, "lidar");
    assert_eq!(dependency_tag, "v1");
    assert_eq!(stack.len(), 1, "stack should only have core node");
}

#[test]
fn service_dependency_fails_when_service_not_exposed_by_dependency() {
    // Test the scenario where a node subscribes to a service from another node,
    // but the target node exists without exposing the requested service
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "v1",
              depends_on: {
                nodes: [
                  { name: "lidar", tag: "v1", link_id: "lidar" }
                ]
              },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          link_id: "lidar",
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["brain"]
            },
        }"#,
    )
    .expect("valid dependent node config");

    // This node has the correct name but exposes a different service
    let dependency_wrong_service: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "lidar",
              tag: "v1",
            },
            interfaces: {
                services: {
                    exposes: [
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
            },
            execution: {
              language: "rust",
              run_cmd: ["lidar"]
            },
        }"#,
    )
    .expect("valid dependency node config with wrong service");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Add the node with the correct name but wrong service
    stack
        .push_config(dependency_wrong_service, false, PathBuf::from("/tmp"))
        .expect("lidar has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have core node + lidar");

    // Adding brain should fail because lidar doesn't expose "reset_sensor"
    let result = stack.push_config(dependent, false, PathBuf::from("/tmp"));
    let Err(NodeStackError::Config(ConfigError::Parsing(ParsingError::MissingInterface(info)))) =
        result
    else {
        panic!("expected MissingInterface error, got {:?}", result);
    };
    assert_eq!(info.dependency, "lidar");
    assert_eq!(info.dependency_tag, "v1");
    assert_eq!(info.interface_kind, "Service");
    assert_eq!(info.interface_name, "reset_sensor");
    assert_eq!(
        stack.len(),
        2,
        "stack should still only have core node + lidar"
    );
}

#[test]
fn service_dependency_fails_when_link_id_is_undeclared() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "v1",
              depends_on: {
                nodes: []
              },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          link_id: "nonexistent",
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["brain"]
            },
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    let result = stack.push_config(dependent, false, PathBuf::from("/tmp"));
    let Err(NodeStackError::Config(ConfigError::Parsing(ParsingError::UndeclaredLinkId {
        link_id,
        ..
    }))) = result
    else {
        panic!("expected UndeclaredLinkId error, got {:?}", result);
    };
    assert_eq!(link_id, "nonexistent");
    assert_eq!(stack.len(), 1, "stack should only have core node");
}
