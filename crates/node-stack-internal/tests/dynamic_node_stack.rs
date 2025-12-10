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
                          id: "move_right_arm_sub",
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
fn dynamically_add_node_to_node_stack_wrong_topic_node_name() {
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
                          id: "lidar_object_sub",
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

#[test]
fn add_instance_to_new_entity() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new();
    assert!(stack.is_empty(), "stack should start empty");

    // Add instance without specifying instance_id (should generate one)
    let instance_id = stack
        .add_instance(&config, None)
        .expect("should add instance");

    assert_eq!(stack.len(), 1, "stack should have one entity");
    assert!(
        stack.contains("sensor", "1.0.0"),
        "entity should be findable"
    );

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        1,
        "entity should have one instance"
    );
    assert_eq!(
        entity.instances()[0].instance_id(),
        &instance_id,
        "instance ID should match the returned one"
    );
}

#[test]
fn add_instance_to_existing_entity() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new();

    // Add first instance
    let first_id = stack
        .add_instance(&config, None)
        .expect("should add first instance");

    assert_eq!(stack.len(), 1, "stack should have one entity");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        1,
        "entity should have one instance"
    );

    // Add second instance to same entity
    let second_id = stack
        .add_instance(&config, None)
        .expect("should add second instance");

    assert_eq!(stack.len(), 1, "stack should still have one entity");

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        2,
        "entity should have two instances"
    );

    let instance_ids: Vec<_> = entity
        .instances()
        .iter()
        .map(|i| i.instance_id().clone())
        .collect();
    assert!(
        instance_ids.contains(&first_id),
        "first instance should be present"
    );
    assert!(
        instance_ids.contains(&second_id),
        "second instance should be present"
    );
}

#[test]
fn add_instance_with_specific_id() {
    use config::node::Name;

    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new();
    let custom_id = Name::new("my-custom-instance").expect("valid name");

    let returned_id = stack
        .add_instance(&config, Some(&custom_id))
        .expect("should add instance");

    assert_eq!(
        returned_id, custom_id,
        "returned ID should match the provided one"
    );

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances()[0].instance_id(),
        &custom_id,
        "instance should have the custom ID"
    );
}

#[test]
fn remove_instance_from_entity_with_multiple_instances() {
    use config::node::Name;

    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new();
    let first_id = Name::new("instance-1").expect("valid name");
    let second_id = Name::new("instance-2").expect("valid name");

    stack
        .add_instance(&config, Some(&first_id))
        .expect("should add first instance");
    stack
        .add_instance(&config, Some(&second_id))
        .expect("should add second instance");

    assert_eq!(stack.len(), 1, "stack should have one entity");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        2,
        "entity should have two instances before removal"
    );

    // Remove first instance
    let removed = stack.remove_instance("sensor", "1.0.0", &first_id);
    assert!(removed, "instance should be removed");

    // Entity should still exist with one instance
    assert_eq!(stack.len(), 1, "entity should still exist");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        1,
        "entity should have one instance after removal"
    );
    assert_eq!(
        entity.instances()[0].instance_id(),
        &second_id,
        "remaining instance should be the second one"
    );
}

#[test]
fn remove_last_instance_removes_entity() {
    use config::node::Name;

    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new();
    let instance_id = Name::new("only-instance").expect("valid name");

    stack
        .add_instance(&config, Some(&instance_id))
        .expect("should add instance");
    assert_eq!(stack.len(), 1, "stack should have one entity");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        1,
        "entity should have one instance"
    );

    // Remove the only instance
    let removed = stack.remove_instance("sensor", "1.0.0", &instance_id);
    assert!(removed, "instance should be removed");

    // Entity should be gone
    assert!(stack.is_empty(), "stack should be empty");
    assert!(
        stack.find("sensor", "1.0.0").is_none(),
        "entity should not exist"
    );
}

#[test]
fn remove_nonexistent_instance_returns_false() {
    use config::node::Name;

    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new();
    let instance_id = Name::new("real-instance").expect("valid name");
    let fake_id = Name::new("fake-instance").expect("valid name");

    stack
        .add_instance(&config, Some(&instance_id))
        .expect("should add instance");

    // Try to remove non-existent instance
    let removed = stack.remove_instance("sensor", "1.0.0", &fake_id);
    assert!(!removed, "should return false for non-existent instance");

    // Try to remove from non-existent entity
    let removed = stack.remove_instance("nonexistent", "1.0.0", &instance_id);
    assert!(!removed, "should return false for non-existent entity");

    // Original instance should still be there
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(entity.instances().len(), 1, "instance should still exist");
}

#[test]
fn reset_clears_entire_stack() {
    let config1: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor1",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let config2: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor2",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::from_configs(vec![config1, config2]);
    assert_eq!(stack.len(), 2, "stack should have two entities");

    stack.reset();

    assert!(stack.is_empty(), "stack should be empty after reset");
    assert!(
        stack.find("sensor1", "1.0.0").is_none(),
        "sensor1 should not exist"
    );
    assert!(
        stack.find("sensor2", "1.0.0").is_none(),
        "sensor2 should not exist"
    );
}

#[test]
fn reset_allows_adding_new_entities() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::from_configs(vec![config.clone()]);
    assert_eq!(stack.len(), 1, "stack should have one entity");

    stack.reset();
    assert!(stack.is_empty(), "stack should be empty after reset");

    // Should be able to add entities again
    stack.push_config(config);
    assert_eq!(
        stack.len(),
        1,
        "stack should have one entity after re-adding"
    );
}
