use node_stack::NodeStack;

#[test]
fn dynamically_add_node_to_node_stack_matching_topic() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { 
              name: "brain", 
              tag: "1.0.0" 
            },
            interfaces: {
                subscribes_to: {
                    topics: [
                        { 
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

    let dependency: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { 
              name: "lidar", 
              tag: "1.0.0" 
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

    let stack = NodeStack::from_configs(vec![dependent]);
    assert_eq!(stack.len(), 1, "stack should start with a single node");
    assert!(
        stack.dependencies_of("brain", "1.0.0").is_empty(),
        "dependency edge is deferred until the dependency is registered"
    );

    stack.push_config(dependency);
    assert_eq!(stack.len(), 2, "stack should include the newly added node");

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
        "adding a node should satisfy pending dependency edges"
    );

    let dependants = stack
        .dependents_of("lidar", "1.0.0")
        .into_iter()
        .map(|node| node.config().manifest.name.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        dependants,
        vec!["brain"],
        "dependency insertion should also update inverse relationships"
    );
}

#[test]
fn dynamically_add_node_to_node_stack_matching_topic_no_node() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { 
              name: "brain", 
              tag: "1.0.0" 
            },
            interfaces: {
                subscribes_to: {
                    topics: [
                        {
                          // No node is specified here, the message can come from any node
                          name: "push_frame", 
                          tag: "1.0.0" 
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependent node config");

    let dependency1: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { 
              name: "chest_camera", 
              tag: "1.0.0" 
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                          name: "push_frame",
                          qos_profile: "sensor_data",
                          message_format: {
                            header: {
                              $type: "object",
                              stamp: "time",
                              frame_id: "u32",
                            },
                            encoding: "string", // "rgb8", "bgr8", "yuyv", "mjpeg"
                            width: "u32",
                            height: "u32",
                            image: {
                              $type: "array",
                              $items: "u8",
                              $length: 3
                            },
                          }
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependency node config");

    let dependency2: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { 
              name: "wrist_camera", 
              tag: "1.0.0" 
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                          // Same topic name
                          name: "push_frame",
                          qos_profile: "sensor_data",
                          // But a different message format for this one
                          message_format: {
                            width: "u32",
                            height: "u32",
                            image: {
                              $type: "array",
                              $items: "u8",
                              $length: 3
                            },
                          },
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependency node config");

    let stack = NodeStack::from_configs(vec![dependent]);
    assert_eq!(stack.len(), 1, "stack should start with a single node");
    assert!(
        stack.dependencies_of("brain", "1.0.0").is_empty(),
        "dependency edge is deferred until the dependency is registered"
    );

    stack.push_config(dependency1);
    stack.push_config(dependency2);
    assert_eq!(stack.len(), 3, "stack should include the newly added node");

    // TODO: Also fix this in the `generator-internal`
    todo!(
        "There should be no dependency for the brain since it's unable to tell what is the message format of the incoming topic"
    )
}

#[test]
fn dynamically_add_node_to_node_stack_matching_service() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { 
              name: "brain", 
              tag: "1.0.0" 
            },
            interfaces: {
                subscribes_to: {
                    services: [
                        { 
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
            schema_version: 1,
            manifest: { 
              name: "lidar", 
              tag: "1.0.0" 
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

    let stack = NodeStack::from_configs(vec![dependent]);
    assert_eq!(stack.len(), 1, "stack should start with a single node");
    assert!(
        stack.dependencies_of("brain", "1.0.0").is_empty(),
        "dependency edge is deferred until the dependency is registered"
    );

    stack.push_config(dependency);
    assert_eq!(stack.len(), 2, "stack should include the newly added node");

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
        "adding a node should satisfy pending service dependency edges"
    );

    let dependants = stack
        .dependents_of("lidar", "1.0.0")
        .into_iter()
        .map(|node| node.config().manifest.name.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        dependants,
        vec!["brain"],
        "dependency insertion should also update inverse relationships for services"
    );
}

#[test]
fn dynamically_add_node_to_node_stack_matching_action() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { 
              name: "brain", 
              tag: "1.0.0" 
            },
            interfaces: {
                subscribes_to: {
                    actions: [
                        { 
                          node: "controller", 
                          name: "move_right_arm", 
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
            schema_version: 1,
            manifest: { 
              name: "controller", 
              tag: "1.0.0" 
            },
            interfaces: {
                exposes: {
                    actions: [
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
                            response_message_format: { accepted: "bool" }
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
            }
        }"#,
    )
    .expect("valid dependency node config");

    let stack = NodeStack::from_configs(vec![dependent]);
    assert_eq!(stack.len(), 1, "stack should start with a single node");
    assert!(
        stack.dependencies_of("brain", "1.0.0").is_empty(),
        "dependency edge is deferred until the dependency is registered"
    );

    stack.push_config(dependency);
    assert_eq!(stack.len(), 2, "stack should include the newly added node");

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
        vec![("controller".to_string(), "1.0.0".to_string())],
        "adding a node should satisfy pending action dependency edges"
    );

    let dependants = stack
        .dependents_of("controller", "1.0.0")
        .into_iter()
        .map(|node| node.config().manifest.name.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        dependants,
        vec!["brain"],
        "dependency insertion should also update inverse relationships for actions"
    );
}

#[test]
fn dynamically_add_node_to_node_stack_wrong_node_name() {
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { 
              name: "brain", 
              tag: "1.0.0" 
            },
            interfaces: {
                subscribes_to: {
                    topics: [
                        { 
                          node: "uvc_camera", // Wrong node name
                          name: "push_lidar_object", 
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
            schema_version: 1,
            manifest: { 
              name: "lidar", 
              tag: "1.0.0" 
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

    let stack = NodeStack::from_configs(vec![dependent]);
    assert_eq!(stack.len(), 1, "stack should start with a single node");
    assert!(
        stack.dependencies_of("brain", "1.0.0").is_empty(),
        "missing dependency cannot be fulfilled until a matching node is added"
    );

    stack.push_config(dependency);
    assert_eq!(stack.len(), 2, "stack should include the newly added node");
    assert!(
        stack.dependencies_of("brain", "1.0.0").is_empty(),
        "node names must match for dependency edges to be wired"
    );
    assert!(
        stack.dependents_of("lidar", "1.0.0").is_empty(),
        "node with mismatched name should not have dependents registered"
    );
}
