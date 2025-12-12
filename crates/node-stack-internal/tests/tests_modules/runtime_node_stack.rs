use config::node::Name;
use node_stack::{NodeStack, NodeStackError};

use crate::helpers::config_common::master_node_config;

// TODO: Make the same tests but for service/action
#[test]
fn topic_dependency_resolved_when_dependency_added_first() {
    let brain_dependent: config::node::NodeConfig = serde_json5::from_str(
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

    let lidar_dependency: config::node::NodeConfig = serde_json5::from_str(
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

    let stack = NodeStack::new(master_node_config(), None);

    // Add the lidar dependency first
    stack
        .push_config(&lidar_dependency, None)
        .expect("dependency node has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + dependency node");

    // Now add the dependent node - should succeed because dependency exists
    stack
        .push_config(&brain_dependent, None)
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
fn topic_dependency_fails_when_dependency_missing() {
    let brain_dependent: config::node::NodeConfig = serde_json5::from_str(
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

    let stack = NodeStack::new(master_node_config(), None);

    // Adding a node that depends on a non-existent node should fail
    let result = stack.push_config(&brain_dependent, None);
    assert!(
        result.is_err(),
        "should fail to add node when dependency doesn't exist"
    );
    assert_eq!(stack.len(), 1, "stack should only have master node");
}

#[test]
fn service_dependency_resolved_when_dependency_added_first() {
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

    let stack = NodeStack::new(master_node_config(), None);

    // Add the dependency first
    stack
        .push_config(&dependency, None)
        .expect("dependency node has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + dependency node");

    // Now add the dependent node
    stack
        .push_config(&dependent, None)
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
fn action_dependency_resolved_when_dependency_added_first() {
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
            }
        }"#,
    )
    .expect("valid dependency node config");

    let stack = NodeStack::new(master_node_config(), None);

    // Add the dependency first
    stack
        .push_config(&dependency, None)
        .expect("dependency node has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + dependency node");

    // Now add the dependent node
    stack
        .push_config(&dependent, None)
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
        vec![("controller".to_string(), "1.0.0".to_string())],
        "dependency edge should be wired for actions"
    );

    let dependants = stack
        .dependents_of("controller", "1.0.0")
        .into_iter()
        .map(|node| node.config().manifest.name.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        dependants,
        vec!["brain"],
        "inverse relationship should also be wired for actions"
    );
}

#[test]
fn topic_dependency_fails_when_node_name_mismatches() {
    // Dependent expects "uvc_camera" but we add "lidar" instead
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
                          node: "uvc_camera",
                          name: "push_lidar_object",
                          tag: "1.0.0"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependent node config");

    let wrong_dependency: config::node::NodeConfig = serde_json5::from_str(
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

    let stack = NodeStack::new(master_node_config(), None);

    // Add lidar (which is NOT the expected dependency "uvc_camera")
    stack
        .push_config(&wrong_dependency, None)
        .expect("lidar has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + lidar");

    // Adding brain should fail because it expects "uvc_camera", not "lidar"
    let result = stack.push_config(&dependent, None);
    let Err(NodeStackError::MissingDependency {
        dependency,
        dependency_tag,
        ..
    }) = result
    else {
        panic!("expected MissingDependency error, got {:?}", result);
    };
    assert_eq!(dependency, "uvc_camera");
    assert_eq!(dependency_tag, "1.0.0");
    assert_eq!(
        stack.len(),
        2,
        "stack should still only have master + lidar"
    );
}

#[test]
fn topic_dependency_fails_when_node_tag_mismatches() {
    // Dependent expects tag "1.0.0" but we add tag "2.0.0" instead
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

    let wrong_tag_dependency: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "lidar",
              tag: "2.0.0"
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

    let stack = NodeStack::new(master_node_config(), None);

    // Add lidar with tag "2.0.0" (NOT the expected tag "1.0.0")
    stack
        .push_config(&wrong_tag_dependency, None)
        .expect("lidar has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + lidar");

    // Adding brain should fail because it expects lidar with tag "1.0.0", not "2.0.0"
    let result = stack.push_config(&dependent, None);
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
    assert_eq!(
        stack.len(),
        2,
        "stack should still only have master + lidar"
    );
}

#[test]
fn topic_dependency_fails_when_topic_not_exposed() {
    // Test the scenario where a node subscribes to a topic from another node,
    // but the target node exists without exposing the requested topic
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

    // This node has the correct name but exposes a different topic
    let dependency_wrong_topic: config::node::NodeConfig = serde_json5::from_str(
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

    let stack = NodeStack::new(master_node_config(), None);

    // Add the node with the correct name but wrong topic
    stack
        .push_config(&dependency_wrong_topic, None)
        .expect("lidar has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + lidar");

    // Adding brain should fail because lidar doesn't expose "push_lidar_object"
    let result = stack.push_config(&dependent, None);
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
        "stack should still only have master + lidar"
    );
}

#[test]
fn add_instance_creates_new_entity() {
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

    let stack = NodeStack::new(master_node_config(), None);
    assert_eq!(stack.len(), 1, "stack should start with root node only");

    // Add instance without specifying instance_id (should generate one)
    let instance_id = stack
        .push_config(&config, None)
        .expect("should add instance");

    assert_eq!(stack.len(), 2, "stack should have master node + one entity");
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

    let stack = NodeStack::new(master_node_config(), None);

    // Add first instance
    let first_id = stack
        .push_config(&config, None)
        .expect("should add first instance");

    assert_eq!(stack.len(), 2, "stack should have master node + one entity");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        1,
        "entity should have one instance"
    );

    // Add second instance to same entity
    let second_id = stack
        .push_config(&config, None)
        .expect("should add second instance");

    assert_eq!(stack.len(), 2, "stack should still have root + one entity");

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

    let stack = NodeStack::new(master_node_config(), None);
    let custom_id = Name::new("my-custom-instance").expect("valid name");

    let returned_id = stack
        .push_config(&config, Some(&custom_id))
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

    let stack = NodeStack::new(master_node_config(), None);
    let first_id = Name::new("instance-1").expect("valid name");
    let second_id = Name::new("instance-2").expect("valid name");

    stack
        .push_config(&config, Some(&first_id))
        .expect("should add first instance");
    stack
        .push_config(&config, Some(&second_id))
        .expect("should add second instance");

    assert_eq!(stack.len(), 2, "stack should have master node + one entity");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        2,
        "entity should have two instances before removal"
    );

    // Remove first instance
    let removed = stack
        .remove_instance("sensor", "1.0.0", &first_id)
        .expect("should succeed");
    assert!(removed, "instance should be removed");

    // Entity should still exist with one instance
    assert_eq!(stack.len(), 2, "entity should still exist");
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

    let stack = NodeStack::new(master_node_config(), None);
    let instance_id = Name::new("only-instance").expect("valid name");

    stack
        .push_config(&config, Some(&instance_id))
        .expect("should add instance");
    assert_eq!(stack.len(), 2, "stack should have root + one entity");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        1,
        "entity should have one instance"
    );

    // Remove the only instance
    let removed = stack
        .remove_instance("sensor", "1.0.0", &instance_id)
        .expect("should succeed");
    assert!(removed, "instance should be removed");

    // Entity should be gone, but master node remains
    assert_eq!(stack.len(), 1, "stack should only have the master node");
    assert!(
        stack.find("sensor", "1.0.0").is_none(),
        "entity should not exist"
    );
}

#[test]
fn remove_nonexistent_instance_returns_false() {
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

    let stack = NodeStack::new(master_node_config(), None);
    let instance_id = Name::new("real-instance").expect("valid name");
    let fake_id = Name::new("fake-instance").expect("valid name");

    stack
        .push_config(&config, Some(&instance_id))
        .expect("should add instance");

    // Try to remove non-existent instance
    let removed = stack
        .remove_instance("sensor", "1.0.0", &fake_id)
        .expect("should succeed");
    assert!(!removed, "should return false for non-existent instance");

    // Try to remove from non-existent entity
    let removed = stack
        .remove_instance("nonexistent", "1.0.0", &instance_id)
        .expect("should succeed");
    assert!(!removed, "should return false for non-existent entity");

    // Original instance should still be there
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(entity.instances().len(), 1, "instance should still exist");
}

#[test]
fn reset_clears_all_except_master_node() {
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

    let stack = NodeStack::new(master_node_config(), None);
    stack
        .push_config(&config1, None)
        .expect("config1 has no dependencies");
    stack
        .push_config(&config2, None)
        .expect("config2 has no dependencies");
    assert_eq!(stack.len(), 3, "stack should have root + two entities");

    stack.reset();

    assert_eq!(stack.len(), 1, "stack should only have root after reset");
    assert!(
        stack.contains("master", "1.0.0"),
        "root should still exist after reset"
    );
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
fn adding_same_entity_adds_new_instance() {
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

    let stack = NodeStack::new(master_node_config(), None);
    stack
        .push_config(&config, None)
        .expect("config has no dependencies");
    assert_eq!(
        stack.len(),
        2,
        "stack should have the master node + one entity"
    );

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        1,
        "entity should have one instance after first push"
    );

    // Adding the same config again should add a new instance to the existing entity
    stack
        .push_config(&config, None)
        .expect("config has no dependencies");
    assert_eq!(
        stack.len(),
        2,
        "stack should still have the master node + one entity"
    );

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        2,
        "entity should have two instances after second push"
    );
}

#[test]
fn adding_same_entity_with_different_interfaces_fails() {
    // First config: exposes a topic
    let config_with_topic: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0"
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                          name: "data_stream",
                          qos_profile: "sensor_data"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid node config");

    // Second config: same name and tag but exposes a topic AND a service
    let config_with_topic_and_service: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0"
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                          name: "data_stream",
                          qos_profile: "sensor_data"
                        }
                    ],
                    services: [
                        {
                          name: "calibrate"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(master_node_config(), None);

    // Add the first config
    stack
        .push_config(&config_with_topic, None)
        .expect("first config has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + sensor");

    // Adding the same entity with different interfaces should fail
    let result = stack.push_config(&config_with_topic_and_service, None);
    assert!(
        result.is_err(),
        "should fail to add same entity with different interfaces"
    );
    assert!(
        matches!(result.unwrap_err(), NodeStackError::ConfigMismatch { .. }),
        "error should be ConfigMismatch"
    );

    // Entity should still exist with original config and one instance
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        1,
        "entity should still have only one instance"
    );
}

#[test]
fn adding_same_name_with_different_tag_and_different_interfaces_succeeds() {
    // First config: version 1.0.0 exposes a topic
    let config_v1: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0"
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                          name: "data_stream",
                          qos_profile: "sensor_data"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid node config");

    // Second config: version 2.0.0 exposes a topic AND a service (different interfaces are allowed with different tag)
    let config_v2: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "2.0.0"
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                          name: "data_stream",
                          qos_profile: "sensor_data"
                        }
                    ],
                    services: [
                        {
                          name: "calibrate"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(master_node_config(), None);

    // Add version 1.0.0
    stack
        .push_config(&config_v1, None)
        .expect("first config has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + sensor v1");

    // Adding version 2.0.0 with different interfaces should succeed (different tag = different entity)
    stack
        .push_config(&config_v2, None)
        .expect("different tag should create new entity even with different interfaces");
    assert_eq!(
        stack.len(),
        3,
        "stack should have master + sensor v1 + sensor v2"
    );

    // Both entities should exist
    let entity_v1 = stack
        .find("sensor", "1.0.0")
        .expect("v1 entity should exist");
    let entity_v2 = stack
        .find("sensor", "2.0.0")
        .expect("v2 entity should exist");

    assert_eq!(
        entity_v1.instances().len(),
        1,
        "v1 entity should have one instance"
    );
    assert_eq!(
        entity_v2.instances().len(),
        1,
        "v2 entity should have one instance"
    );
}

#[test]
fn root_returns_the_master_node() {
    let stack = NodeStack::new(master_node_config(), None);

    let root = stack.root();
    assert_eq!(
        root.config().manifest.name.as_str(),
        "master",
        "root should be master node"
    );
    assert_eq!(
        root.config().manifest.tag,
        "1.0.0",
        "root should have correct tag"
    );
    assert_eq!(
        root.instances().len(),
        1,
        "root should have exactly one instance"
    );
}

#[test]
fn cannot_modify_root_node() {
    let stack = NodeStack::new(master_node_config(), None);
    let root_instance_id = stack.root().instances()[0].instance_id().clone();

    // Try to remove the root's instance
    let result = stack.remove_instance("master", "1.0.0", &root_instance_id);
    assert!(
        result.is_err(),
        "should not be able to remove root node instance"
    );

    // Try to add another instance to root
    let result = stack.push_config(&master_node_config(), None);
    assert!(
        result.is_err(),
        "should not be able to add instance to root node"
    );

    // Root should still be intact
    let root = stack.root();
    assert_eq!(
        root.instances().len(),
        1,
        "root should still have exactly one instance"
    );
}

#[test]
fn from_configs_with_empty_list_returns_error() {
    let result = NodeStack::from_configs(Vec::new());
    assert!(result.is_err(), "from_configs with empty list should fail");
}

#[test]
fn from_configs_uses_first_entity_as_root() {
    let config1: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "first",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let config2: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "second",
              tag: "1.0.0"
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::from_configs(vec![config1, config2]).expect("should create stack");
    assert_eq!(stack.len(), 2, "stack should have two nodes");

    let root = stack.root();
    assert_eq!(
        root.config().manifest.name.as_str(),
        "first",
        "first config should be root"
    );
}
