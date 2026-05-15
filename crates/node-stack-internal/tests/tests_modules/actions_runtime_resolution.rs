use std::path::PathBuf;

use node_stack::{NodeStack, NodeStackError};

use crate::helpers::config_common::core_node_config;

#[test]
fn action_dependency_resolved_when_dependency_added_first() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "v1",
              depends_on: {
                nodes: [
                  { name: "controller", tag: "v1", local_id: "controller" }
                ]
              },
            },
            interfaces: {
                actions: {
                    consumes: [
                        {
                          local_node_id: "controller",
                          name: "move_right_arm"
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
              name: "controller",
              tag: "v1",
            },
            interfaces: {
                actions: {
                    exposes: [
                        {
                          name: "move_right_arm",
                          goal_service: {
                            request_message_format: {
                              arm_id: "u16",
                              desired_position: {
                                $type: "array",
                                $items: "i32",
                                $length: 3
                              }
                            },
                            response_message_format: {
                              accepted: "bool"
                            }
                          },
                          feedback_topic: {
                            qos_profile: "reliable",
                            message_format: {
                              current_position: {
                                $type: "array",
                                $items: "i32",
                                $length: 3
                              }
                            }
                          },
                          result_service: {
                            response_message_format: {
                              final_position: {
                                $type: "array",
                                $items: "i32",
                                $length: 3
                              }
                            }
                          }
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["controller"]
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

    let deps = stack
        .dependencies_of("brain", "v1")
        .into_iter()
        .map(|node| {
            let guard = node.read();
            (
                guard.config().manifest.name.as_str().to_owned(),
                guard.config().manifest.tag.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        deps,
        vec![("controller".to_string(), "v1".to_string())],
        "dependency edge should be wired for actions"
    );

    let dependants = stack
        .dependents_of("controller", "v1")
        .into_iter()
        .map(|node| node.read().config().manifest.name.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        dependants,
        vec!["brain"],
        "inverse relationship should also be wired for actions"
    );
}

#[test]
fn action_dependency_fails_when_dependency_is_missing() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "v1",
              depends_on: {
                nodes: [
                  { name: "controller", tag: "v1", local_id: "controller" }
                ]
              },
            },
            interfaces: {
                actions: {
                    consumes: [
                        {
                          local_node_id: "controller",
                          name: "move_right_arm"
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

    // Adding a node that depends on a non-existent action provider should fail
    let result = stack.push_config(dependent, false, PathBuf::from("/tmp"));
    let Err(NodeStackError::MissingDependency {
        dependency,
        dependency_tag,
        ..
    }) = result
    else {
        panic!("expected MissingDependency error, got {:?}", result);
    };
    assert_eq!(dependency, "controller");
    assert_eq!(dependency_tag, "v1");
    assert_eq!(stack.len(), 1, "stack should only have core node");
}

#[test]
fn action_dependency_fails_when_action_not_exposed_by_dependency() {
    // Test the scenario where a node subscribes to an action from another node,
    // but the target node exists without exposing the requested action
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "v1",
              depends_on: {
                nodes: [
                  { name: "controller", tag: "v1", local_id: "controller" }
                ]
              },
            },
            interfaces: {
                actions: {
                    consumes: [
                        {
                          local_node_id: "controller",
                          name: "move_right_arm"
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

    // This node has the correct name but exposes a different action
    let dependency_wrong_action: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "controller",
              tag: "v1",
            },
            interfaces: {
                actions: {
                    exposes: [
                        {
                          name: "move_left_arm",
                          goal_service: {
                            request_message_format: {
                              arm_id: "u16",
                              desired_position: {
                                $type: "array",
                                $items: "i32",
                                $length: 3
                              }
                            },
                            response_message_format: {
                              accepted: "bool"
                            }
                          },
                          feedback_topic: {
                            qos_profile: "reliable",
                            message_format: {
                              current_position: {
                                $type: "array",
                                $items: "i32",
                                $length: 3
                              }
                            }
                          },
                          result_service: {
                            response_message_format: {
                              final_position: {
                                $type: "array",
                                $items: "i32",
                                $length: 3
                              }
                            }
                          }
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["controller"]
            },
        }"#,
    )
    .expect("valid dependency node config with wrong action");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Add the node with the correct name but wrong action
    stack
        .push_config(dependency_wrong_action, false, PathBuf::from("/tmp"))
        .expect("controller has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have core node + controller");

    // Adding brain should fail because controller doesn't expose "move_right_arm"
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
    assert_eq!(dependency, "controller");
    assert_eq!(dependency_tag, "v1");
    assert_eq!(interface_kind, "Action");
    assert_eq!(interface_name, "move_right_arm");
    assert_eq!(
        stack.len(),
        2,
        "stack should still only have core node + controller"
    );
}

#[test]
fn action_dependency_fails_when_local_node_id_is_undeclared() {
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
                actions: {
                    consumes: [
                        {
                          local_node_id: "nonexistent",
                          name: "move_right_arm"
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
    let Err(NodeStackError::UndeclaredLocalNodeId { local_node_id, .. }) = result else {
        panic!("expected UndeclaredLocalNodeId error, got {:?}", result);
    };
    assert_eq!(local_node_id, "nonexistent");
    assert_eq!(stack.len(), 1, "stack should only have core node");
}
