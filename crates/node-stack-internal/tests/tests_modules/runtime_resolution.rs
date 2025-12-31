use std::path::PathBuf;

use config::node::Name;
use node_stack::{NodeStack, NodeStackError};

use crate::helpers::config_common::master_node_config;

#[test]
fn add_instance_creates_new_entity() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0",
              launch_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));
    assert_eq!(stack.len(), 1, "stack should start with root node only");

    // Push config first
    stack
        .push_config(&config, false, PathBuf::from("/tmp"))
        .expect("should push config");

    assert_eq!(stack.len(), 2, "stack should have master node + one entity");
    assert!(
        stack.contains("sensor", "1.0.0"),
        "entity should be findable"
    );

    // Spawn an instance
    let instance_id = stack
        .spawn_instance("sensor", "1.0.0", None)
        .expect("should spawn instance");

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

    // Verify sensor is in the stack but is not the root (master node is the root/parent)
    let root = stack.root();
    assert_eq!(
        root.config().manifest.name.as_str(),
        "master",
        "root should be master node, not sensor"
    );
    assert_ne!(
        entity.config().manifest.name.as_str(),
        root.config().manifest.name.as_str(),
        "sensor should not be the root node"
    );

    // Verify sensor is in the stack's snapshot alongside master
    let snapshot = stack.snapshot();
    assert_eq!(
        snapshot.len(),
        2,
        "snapshot should contain master and sensor"
    );
    let names: Vec<_> = snapshot
        .iter()
        .map(|e| e.config().manifest.name.as_str())
        .collect();
    assert!(names.contains(&"master"), "snapshot should contain master");
    assert!(names.contains(&"sensor"), "snapshot should contain sensor");
}

#[test]
fn add_instance_to_existing_entity() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0",
              launch_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));

    // Push config first
    stack
        .push_config(&config, false, PathBuf::from("/tmp"))
        .expect("should push config");

    // Spawn first instance
    let first_id = stack
        .spawn_instance("sensor", "1.0.0", None)
        .expect("should spawn first instance");

    assert_eq!(stack.len(), 2, "stack should have master node + one entity");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        1,
        "entity should have one instance"
    );

    // Spawn second instance to same entity
    let second_id = stack
        .spawn_instance("sensor", "1.0.0", None)
        .expect("should spawn second instance");

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
              tag: "1.0.0",
              launch_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));
    let custom_id = Name::new("my-custom-instance").expect("valid name");

    // First push the config
    stack
        .push_config(&config, false, PathBuf::from("/tmp"))
        .expect("should push config");

    // Then spawn an instance with the specific ID
    let returned_id = stack
        .spawn_instance("sensor", "1.0.0", Some(&custom_id))
        .expect("should spawn instance");

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
              tag: "1.0.0",
              launch_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));
    let first_id = Name::new("instance-1").expect("valid name");
    let second_id = Name::new("instance-2").expect("valid name");

    // Push config first
    stack
        .push_config(&config, false, PathBuf::from("/tmp"))
        .expect("should push config");

    // Spawn instances
    stack
        .spawn_instance("sensor", "1.0.0", Some(&first_id))
        .expect("should spawn first instance");
    stack
        .spawn_instance("sensor", "1.0.0", Some(&second_id))
        .expect("should spawn second instance");

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
              tag: "1.0.0",
              launch_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));
    let instance_id = Name::new("only-instance").expect("valid name");

    // Push config and spawn instance
    stack
        .push_config(&config, false, PathBuf::from("/tmp"))
        .expect("should push config");
    stack
        .spawn_instance("sensor", "1.0.0", Some(&instance_id))
        .expect("should spawn instance");
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
              tag: "1.0.0",
              launch_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));
    let instance_id = Name::new("real-instance").expect("valid name");
    let fake_id = Name::new("fake-instance").expect("valid name");

    // Push config and spawn instance
    stack
        .push_config(&config, false, PathBuf::from("/tmp"))
        .expect("should push config");
    stack
        .spawn_instance("sensor", "1.0.0", Some(&instance_id))
        .expect("should spawn instance");

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
              tag: "1.0.0",
              launch_cmd: ["sensor1"]
            }
        }"#,
    )
    .expect("valid node config");

    let config2: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor2",
              tag: "1.0.0",
              launch_cmd: ["sensor2"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));
    stack
        .push_config(&config1, false, PathBuf::from("/tmp"))
        .expect("config1 has no dependencies");
    stack
        .push_config(&config2, false, PathBuf::from("/tmp"))
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
fn spawning_multiple_instances_on_same_entity() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "sensor",
              tag: "1.0.0",
              launch_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));
    stack
        .push_config(&config, false, PathBuf::from("/tmp"))
        .expect("config has no dependencies");
    assert_eq!(
        stack.len(),
        2,
        "stack should have the master node + one entity"
    );

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        0,
        "entity should have no instances after push_config"
    );

    // Spawn first instance
    stack
        .spawn_instance("sensor", "1.0.0", None)
        .expect("should spawn instance");

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        1,
        "entity should have one instance after first spawn"
    );

    // Spawn second instance on the same entity
    stack
        .spawn_instance("sensor", "1.0.0", None)
        .expect("should spawn instance");
    assert_eq!(
        stack.len(),
        2,
        "stack should still have the master node + one entity"
    );

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        2,
        "entity should have two instances after second spawn"
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
              tag: "1.0.0",
              launch_cmd: ["sensor"]
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
              tag: "1.0.0",
              launch_cmd: ["sensor"]
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

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));

    // Add the first config
    stack
        .push_config(&config_with_topic, false, PathBuf::from("/tmp"))
        .expect("first config has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + sensor");

    // Adding the same entity with different interfaces should fail
    let result = stack.push_config(&config_with_topic_and_service, false, PathBuf::from("/tmp"));
    assert!(
        result.is_err(),
        "should fail to add same entity with different interfaces"
    );
    assert!(
        matches!(result.unwrap_err(), NodeStackError::ConfigMismatch { .. }),
        "error should be ConfigMismatch"
    );

    // Entity should still exist with original config (no instances since push_config doesn't create them)
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.instances().len(),
        0,
        "entity should have no instances (push_config doesn't create instances)"
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
              tag: "1.0.0",
              launch_cmd: ["sensor"]
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
              tag: "2.0.0",
              launch_cmd: ["sensor"]
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

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));

    // Add version 1.0.0
    stack
        .push_config(&config_v1, false, PathBuf::from("/tmp"))
        .expect("first config has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + sensor v1");

    // Adding version 2.0.0 with different interfaces should succeed (different tag = different entity)
    stack
        .push_config(&config_v2, false, PathBuf::from("/tmp"))
        .expect("different tag should create new entity even with different interfaces");
    assert_eq!(
        stack.len(),
        3,
        "stack should have master + sensor v1 + sensor v2"
    );

    // Both entities should exist (with no instances since push_config doesn't create them)
    let entity_v1 = stack
        .find("sensor", "1.0.0")
        .expect("v1 entity should exist");
    let entity_v2 = stack
        .find("sensor", "2.0.0")
        .expect("v2 entity should exist");

    assert_eq!(
        entity_v1.instances().len(),
        0,
        "v1 entity should have no instances (push_config doesn't create instances)"
    );
    assert_eq!(
        entity_v2.instances().len(),
        0,
        "v2 entity should have no instances (push_config doesn't create instances)"
    );
}

#[test]
fn root_returns_the_master_node() {
    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));

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
    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));
    let root_instance_id = stack.root().instances()[0].instance_id().clone();

    // Try to remove the root's instance
    let result = stack.remove_instance("master", "1.0.0", &root_instance_id);
    assert!(
        result.is_err(),
        "should not be able to remove root node instance"
    );

    // Try to add another instance to root
    let result = stack.push_config(&master_node_config(), false, PathBuf::from("/tmp"));
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
fn node_stack_wires_dependencies_for_dependants() {
    let dependency: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "lidar",
              tag: "1.0.0",
              launch_cmd: ["lidar"]
            },
            interfaces: {
                exposes: {
                    services: [
                        { name: "reset_sensor" }
                    ]
                }
            }
        }"#,
    )
    .expect("valid dependency node config");

    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "brain",
              tag: "1.0.0",
              launch_cmd: ["brain"]
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

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));
    stack
        .push_config(&dependency, false, PathBuf::from("/tmp"))
        .expect("dependency has no dependencies");
    stack
        .push_config(&dependent, false, PathBuf::from("/tmp"))
        .expect("dependent dependency is present");

    let deps = stack.dependencies_of("brain", "1.0.0");
    assert!(
        deps.iter()
            .any(|entity| entity.config().manifest.name.as_str() == "lidar"),
        "brain should depend on lidar in the stack"
    );
}

#[test]
fn dependency_fails_when_node_name_mismatches() {
    // Dependent expects "uvc_camera" but we add "lidar" instead
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "brain",
              tag: "1.0.0",
              launch_cmd: ["brain"]
            },
            interfaces: {
                subscribes_to: {
                    services: [
                        {
                          id: "reset_sensor_sub",
                          node: "uvc_camera",
                          name: "reset_sensor",
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
              tag: "1.0.0",
              launch_cmd: ["lidar"]
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

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));

    // Add lidar (which is NOT the expected dependency "uvc_camera")
    stack
        .push_config(&wrong_dependency, false, PathBuf::from("/tmp"))
        .expect("lidar has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + lidar");

    // Adding brain should fail because it expects "uvc_camera", not "lidar"
    let result = stack.push_config(&dependent, false, PathBuf::from("/tmp"));
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
fn dependency_fails_when_node_tag_mismatches() {
    // Dependent expects tag "1.0.0" but we add tag "2.0.0" instead
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "brain",
              tag: "1.0.0",
              launch_cmd: ["brain"]
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

    let wrong_tag_dependency: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
              name: "lidar",
              tag: "2.0.0",
              launch_cmd: ["lidar"]
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

    let stack = NodeStack::new(master_node_config(), None, PathBuf::from("/tmp"));

    // Add lidar with tag "2.0.0" (NOT the expected tag "1.0.0")
    stack
        .push_config(&wrong_tag_dependency, false, PathBuf::from("/tmp"))
        .expect("lidar has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have master + lidar");

    // Adding brain should fail because it expects lidar with tag "1.0.0", not "2.0.0"
    let result = stack.push_config(&dependent, false, PathBuf::from("/tmp"));
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
