use std::path::PathBuf;

use config::node::Name;
use node_stack::{NodeStack, NodeStackError};

use crate::helpers::config_common::core_node_config;
use crate::helpers::fixtures;
use crate::helpers::real_lifecycle;

#[tokio::test]
async fn add_instance_creates_new_entity() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            interfaces: {
                topics: {
                    emits: [
                        {
                          name: "data_stream",
                          qos_profile: "sensor_data"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    assert_eq!(stack.len(), 1, "stack should start with root node only");

    // Push config + build (no I/O) first
    fixtures::push_built(&stack, &harness, config).await;

    assert_eq!(stack.len(), 2, "stack should have core node + one entity");
    assert!(
        stack.contains("sensor", "1.0.0"),
        "entity should be findable"
    );

    // Spawn an instance
    let _guard = fixtures::start_instance_in_stack(&stack, &harness, "sensor", "1.0.0", None).await;
    let instance_id = _guard.instance_id.clone();

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    let entity_guard = entity.read();
    assert_eq!(
        entity_guard.instances().len(),
        1,
        "entity should have one instance"
    );
    assert_eq!(
        entity_guard.instances()[0].instance_id(),
        &instance_id,
        "instance ID should match the returned one"
    );

    // Verify sensor is in the stack but is not the root (core node is the root/parent)
    let root = stack.root();
    let root_guard = root.read();
    assert_eq!(
        root_guard.config().manifest.name.as_str(),
        "core",
        "root should be core node, not sensor"
    );
    assert_ne!(
        entity_guard.config().manifest.name.as_str(),
        root_guard.config().manifest.name.as_str(),
        "sensor should not be the root node"
    );
    drop(root_guard);
    drop(entity_guard);

    // Verify sensor is in the stack's snapshot alongside the core node
    let snapshot = stack.snapshot();
    assert_eq!(
        snapshot.len(),
        2,
        "snapshot should contain core node and sensor"
    );
    let names: Vec<_> = snapshot
        .iter()
        .map(|e| e.read().config().manifest.name.as_str().to_owned())
        .collect();
    assert!(
        names.iter().any(|n| n == "core"),
        "snapshot should contain core"
    );
    assert!(
        names.iter().any(|n| n == "sensor"),
        "snapshot should contain sensor"
    );
}

#[tokio::test]
async fn add_instance_to_existing_entity() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            interfaces: {
                topics: {
                    emits: [
                        {
                          name: "data_stream",
                          qos_profile: "sensor_data"
                        }
                    ]
                },
                services: {
                    exposes: [
                        {
                          name: "calibrate"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();

    // Push config + build first
    fixtures::push_built(&stack, &harness, config).await;

    // Spawn first instance
    let _g1 = fixtures::start_instance_in_stack(
        &stack,
        &harness,
        "sensor",
        "1.0.0",
        Some(&Name::new("first").expect("valid name")),
    )
    .await;
    let first_id = _g1.instance_id.clone();

    assert_eq!(stack.len(), 2, "stack should have core node + one entity");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().instances().len(),
        1,
        "entity should have one instance"
    );

    // Spawn second instance to same entity
    let _g2 = fixtures::start_instance_in_stack(
        &stack,
        &harness,
        "sensor",
        "1.0.0",
        Some(&Name::new("second").expect("valid name")),
    )
    .await;
    let second_id = _g2.instance_id.clone();

    assert_eq!(stack.len(), 2, "stack should still have root + one entity");

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    let entity_guard = entity.read();
    assert_eq!(
        entity_guard.instances().len(),
        2,
        "entity should have two instances"
    );

    let instance_ids: Vec<_> = entity_guard
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

#[tokio::test]
async fn add_instance_with_specific_id() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            interfaces: {
                topics: {
                    emits: [
                        {
                          name: "data_stream",
                          qos_profile: "sensor_data"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let custom_id = Name::new("my-custom-instance").expect("valid name");

    // First push the config + build
    fixtures::push_built(&stack, &harness, config).await;

    // Then spawn an instance with the specific ID
    let _guard =
        fixtures::start_instance_in_stack(&stack, &harness, "sensor", "1.0.0", Some(&custom_id))
            .await;
    let returned_id = _guard.instance_id.clone();

    assert_eq!(
        returned_id, custom_id,
        "returned ID should match the provided one"
    );

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().instances()[0].instance_id(),
        &custom_id,
        "instance should have the custom ID"
    );
}

#[tokio::test]
async fn remove_instance_from_entity_with_multiple_instances() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            interfaces: {
                topics: {
                    emits: [
                        {
                          name: "data_stream",
                          qos_profile: "sensor_data"
                        }
                    ]
                },
                services: {
                    exposes: [
                        {
                          name: "calibrate"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let first_id = Name::new("instance-1").expect("valid name");
    let second_id = Name::new("instance-2").expect("valid name");

    // Push config + build first
    fixtures::push_built(&stack, &harness, config).await;

    // Spawn instances
    let _g1 =
        fixtures::start_instance_in_stack(&stack, &harness, "sensor", "1.0.0", Some(&first_id))
            .await;
    let _g2 =
        fixtures::start_instance_in_stack(&stack, &harness, "sensor", "1.0.0", Some(&second_id))
            .await;

    assert_eq!(stack.len(), 2, "stack should have core node + one entity");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().instances().len(),
        2,
        "entity should have two instances before removal"
    );

    // Remove first instance
    let removed = fixtures::stop_instance_in_stack(&stack, "sensor", "1.0.0", &first_id);
    assert!(removed, "instance should be removed");

    // Entity should still exist with one instance
    assert_eq!(stack.len(), 2, "entity should still exist");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    let entity_guard = entity.read();
    assert_eq!(
        entity_guard.instances().len(),
        1,
        "entity should have one instance after removal"
    );
    assert_eq!(
        entity_guard.instances()[0].instance_id(),
        &second_id,
        "remaining instance should be the second one"
    );
}

#[tokio::test]
async fn remove_last_instance_keeps_entity_in_graph() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "reset_sensor" }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let instance_id = Name::new("only-instance").expect("valid name");

    // Push config + build and spawn instance
    fixtures::push_built(&stack, &harness, config).await;
    let _guard =
        fixtures::start_instance_in_stack(&stack, &harness, "sensor", "1.0.0", Some(&instance_id))
            .await;
    assert_eq!(stack.len(), 2, "stack should have root + one entity");
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().instances().len(),
        1,
        "entity should have one instance"
    );

    // Remove the only instance
    let removed = fixtures::stop_instance_in_stack(&stack, "sensor", "1.0.0", &instance_id);
    assert!(removed, "instance should be removed");

    // Entity stays in the graph with 0 instances; dependency edges are preserved
    assert_eq!(stack.len(), 2, "stack should still have root + entity");
    let entity = stack
        .find("sensor", "1.0.0")
        .expect("entity should still exist");
    assert_eq!(
        entity.read().instances().len(),
        0,
        "entity should have no instances"
    );
}

#[tokio::test]
async fn remove_nonexistent_instance_returns_false() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        {
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let instance_id = Name::new("real-instance").expect("valid name");
    let fake_id = Name::new("fake-instance").expect("valid name");

    // Push config + build and spawn instance
    fixtures::push_built(&stack, &harness, config).await;
    let _guard =
        fixtures::start_instance_in_stack(&stack, &harness, "sensor", "1.0.0", Some(&instance_id))
            .await;

    // Try to remove non-existent instance
    let removed = fixtures::stop_instance_in_stack(&stack, "sensor", "1.0.0", &fake_id);
    assert!(!removed, "should return false for non-existent instance");

    // Try to remove from non-existent entity
    let removed = fixtures::stop_instance_in_stack(&stack, "nonexistent", "1.0.0", &instance_id);
    assert!(!removed, "should return false for non-existent entity");

    // Original instance should still be there
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().instances().len(),
        1,
        "instance should still exist"
    );
}

#[test]
fn reset_clears_all_except_core_node() {
    let config1: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor1",
              tag: "1.0.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        {
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor1"]
            }
        }"#,
    )
    .expect("valid node config");

    let config2: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor2",
              tag: "1.0.0",
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
              run_cmd: ["sensor2"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    stack
        .push_config(config1, false, PathBuf::from("/tmp"))
        .expect("config1 has no dependencies");
    stack
        .push_config(config2, false, PathBuf::from("/tmp"))
        .expect("config2 has no dependencies");
    assert_eq!(stack.len(), 3, "stack should have root + two entities");

    stack.reset();

    assert_eq!(stack.len(), 1, "stack should only have root after reset");
    assert!(
        stack.contains("core", "1.0.0"),
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

#[tokio::test]
async fn spawning_multiple_instances_on_same_entity() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        {
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    fixtures::push_built(&stack, &harness, config).await;
    assert_eq!(
        stack.len(),
        2,
        "stack should have the core node + one entity"
    );

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().instances().len(),
        0,
        "entity should have no instances after push_config"
    );

    // Spawn first instance
    let _g1 = fixtures::start_instance_in_stack(
        &stack,
        &harness,
        "sensor",
        "1.0.0",
        Some(&Name::new("first").expect("valid name")),
    )
    .await;

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().instances().len(),
        1,
        "entity should have one instance after first spawn"
    );

    // Spawn second instance on the same entity
    let _g2 = fixtures::start_instance_in_stack(
        &stack,
        &harness,
        "sensor",
        "1.0.0",
        Some(&Name::new("second").expect("valid name")),
    )
    .await;
    assert_eq!(
        stack.len(),
        2,
        "stack should still have the core node + one entity"
    );

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().instances().len(),
        2,
        "entity should have two instances after second spawn"
    );
}

#[test]
fn adding_same_entity_with_different_interfaces_overwrites_when_no_dependents() {
    // First config: emits a topic
    let config_with_topic: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            interfaces: {
                topics: {
                    emits: [
                        {
                          name: "data_stream",
                          qos_profile: "sensor_data"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            },
        }"#,
    )
    .expect("valid node config");

    // Second config: same name and tag but emits a topic AND exposes a service
    let config_with_topic_and_service: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            interfaces: {
                topics: {
                    emits: [
                        {
                          name: "data_stream",
                          qos_profile: "sensor_data"
                        }
                    ]
                },
                services: {
                    exposes: [
                        {
                          name: "calibrate"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            },
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Add the first config
    stack
        .push_config(config_with_topic, false, PathBuf::from("/tmp/sensor_v1"))
        .expect("first config has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have core node + sensor");

    // Re-adding the same entity should overwrite the previous snapshot when there are no dependents
    stack
        .push_config(
            config_with_topic_and_service,
            false,
            PathBuf::from("/tmp/sensor_v2"),
        )
        .expect("should overwrite existing entity without dependents");
    assert_eq!(stack.len(), 2, "stack should still have core node + sensor");

    // Entity should still exist (no instances since push_config doesn't create them)
    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    let entity_guard = entity.read();
    assert_eq!(
        entity_guard.instances().len(),
        0,
        "entity should have no instances (push_config doesn't create instances)"
    );
    assert_eq!(
        entity_guard.config_path(),
        PathBuf::from("/tmp/sensor_v2").as_path(),
        "entity should use the latest snapshot config path"
    );
    assert!(
        entity_guard
            .config()
            .interfaces
            .services
            .as_ref()
            .and_then(|services| services.exposes.as_ref())
            .is_some_and(|exposes: &Vec<config::node::ExposedService>| exposes
                .iter()
                .any(|s| s.name == "calibrate")),
        "entity should have updated interfaces from the overwritten config"
    );
}

#[test]
fn adding_same_name_with_different_tag_and_different_interfaces_succeeds() {
    // First config: version 1.0.0 exposes a service
    let config_v1: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        {
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            },
        }"#,
    )
    .expect("valid node config");

    // Second config: version 2.0.0 emits a topic AND exposes a service (different interfaces are allowed with different tag)
    let config_v2: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "2.0.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "new_service" }
                    ]
                }
            },
            execution: {
              language: "rust",
              run_cmd: ["sensor"]
            },
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Add version 1.0.0
    stack
        .push_config(config_v1, false, PathBuf::from("/tmp"))
        .expect("first config has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have core node + sensor v1");

    // Adding version 2.0.0 with different interfaces should succeed (different tag = different entity)
    stack
        .push_config(config_v2, false, PathBuf::from("/tmp"))
        .expect("different tag should create new entity even with different interfaces");
    assert_eq!(
        stack.len(),
        3,
        "stack should have core node + sensor v1 + sensor v2"
    );

    // Both entities should exist (with no instances since push_config doesn't create them)
    let entity_v1 = stack
        .find("sensor", "1.0.0")
        .expect("v1 entity should exist");
    let entity_v2 = stack
        .find("sensor", "2.0.0")
        .expect("v2 entity should exist");

    assert_eq!(
        entity_v1.read().instances().len(),
        0,
        "v1 entity should have no instances (push_config doesn't create instances)"
    );
    assert_eq!(
        entity_v2.read().instances().len(),
        0,
        "v2 entity should have no instances (push_config doesn't create instances)"
    );
}

#[test]
fn root_returns_the_core_node() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    let root = stack.root();
    let root_guard = root.read();
    assert_eq!(
        root_guard.config().manifest.name.as_str(),
        "core",
        "root should be core node"
    );
    assert_eq!(
        root_guard.config().manifest.tag,
        "1.0.0",
        "root should have correct tag"
    );
    assert_eq!(
        root_guard.instances().len(),
        1,
        "root should have exactly one instance"
    );
}

#[test]
fn cannot_modify_root_node() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let _root_instance_id = stack.root().read().instances()[0].instance_id().clone();

    // Try to remove the root's config — must error with CannotModifyRootNode.
    let result = stack.remove_config("core", "1.0.0");
    assert!(
        result.is_err(),
        "should not be able to remove root node via remove_config"
    );

    // Try to push a new config that would overwrite root
    let result = stack.push_config(core_node_config(), false, PathBuf::from("/tmp"));
    assert!(
        result.is_err(),
        "should not be able to overwrite root node via push_config"
    );

    // Root should still be intact
    let root = stack.root();
    assert_eq!(
        root.read().instances().len(),
        1,
        "root should still have exactly one instance"
    );
}

#[test]
fn node_stack_wires_dependencies_for_dependants() {
    let dependency: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "lidar",
              tag: "1.0.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "reset_sensor" }
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

    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "1.0.0",
              depends_on: {
                nodes: [
                  { name: "lidar", tag: "1.0.0", local_id: "lidar" }
                ]
              },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          local_node_id: "lidar",
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
    stack
        .push_config(dependency, false, PathBuf::from("/tmp"))
        .expect("dependency has no dependencies");
    stack
        .push_config(dependent, false, PathBuf::from("/tmp"))
        .expect("dependent dependency is present");

    let deps = stack.dependencies_of("brain", "1.0.0");
    assert!(
        deps.iter()
            .any(|entity| { entity.read().config().manifest.name.as_str() == "lidar" }),
        "brain should depend on lidar in the stack"
    );
}

#[test]
fn dependency_fails_when_node_name_mismatches() {
    // Dependent expects "uvc_camera" but we add "lidar" instead
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "1.0.0",
              depends_on: {
                nodes: [
                  { name: "uvc_camera", tag: "1.0.0", local_id: "uvc_camera" }
                ]
              },
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "reset_sensor" }
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

    let wrong_dependency: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "lidar",
              tag: "1.0.0",
            },
            execution: {
              language: "rust",
              run_cmd: ["lidar"]
            },
        }"#,
    )
    .expect("valid dependency node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Add lidar (which is NOT the expected dependency "uvc_camera")
    stack
        .push_config(wrong_dependency, false, PathBuf::from("/tmp"))
        .expect("lidar has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have core node + lidar");

    // Adding brain should fail because it expects "uvc_camera", not "lidar"
    let result = stack.push_config(dependent, false, PathBuf::from("/tmp"));
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
        "stack should still only have core node + lidar"
    );
}

#[test]
fn dependency_fails_when_node_tag_mismatches() {
    // Dependent expects tag "1.0.0" but we add tag "2.0.0" instead
    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "1.0.0",
              depends_on: {
                nodes: [
                  { name: "lidar", tag: "1.0.0", local_id: "lidar" }
                ]
              },
            },
            execution: {
              language: "rust",
              run_cmd: ["brain"]
            },
        }"#,
    )
    .expect("valid dependent node config");

    let wrong_tag_dependency: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "lidar",
              tag: "2.0.0",
            },
            execution: {
              language: "rust",
              run_cmd: ["lidar"]
            },
        }"#,
    )
    .expect("valid dependency node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    // Add lidar with tag "2.0.0" (NOT the expected tag "1.0.0")
    stack
        .push_config(wrong_tag_dependency, false, PathBuf::from("/tmp"))
        .expect("lidar has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have core node + lidar");

    // Adding brain should fail because it expects lidar with tag "1.0.0", not "2.0.0"
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
    assert_eq!(
        stack.len(),
        2,
        "stack should still only have core node + lidar"
    );
}

#[test]
fn overwriting_existing_node_fails_if_node_has_dependencies() {
    let dependency_v1: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "lidar",
              tag: "1.0.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "reset_sensor" }
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

    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "1.0.0",
              depends_on: {
                nodes: [
                  { name: "lidar", tag: "1.0.0", local_id: "lidar" }
                ]
              },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          local_node_id: "lidar",
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

    let dependency_v2: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "lidar",
              tag: "1.0.0",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "new_service" }
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

    stack
        .push_config(dependency_v1, false, PathBuf::from("/tmp/lidar_v1"))
        .expect("dependency has no dependencies");
    stack
        .push_config(dependent, false, PathBuf::from("/tmp/brain"))
        .expect("dependent dependency is present");

    let result = stack.push_config(dependency_v2, false, PathBuf::from("/tmp/lidar_v2"));
    let Err(NodeStackError::CannotOverwriteNodeWithDependents {
        node_name,
        node_tag,
    }) = result
    else {
        panic!(
            "expected CannotOverwriteNodeWithDependents error, got {:?}",
            result
        );
    };
    assert_eq!(node_name, "lidar");
    assert_eq!(node_tag, "1.0.0");

    assert_eq!(
        stack.len(),
        3,
        "stack should still have core node + lidar + brain"
    );

    let entity = stack.find("lidar", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().config_path(),
        PathBuf::from("/tmp/lidar_v1").as_path(),
        "entity should still point to the original snapshot config path"
    );

    let dependents = stack.dependents_of("lidar", "1.0.0");
    assert!(
        dependents
            .iter()
            .any(|entity| { entity.read().config().manifest.name.as_str() == "brain" }),
        "lidar should still have brain as a dependent"
    );
}

#[test]
fn updating_run_cmd_without_changing_interfaces_applies_new_config() {
    let original_config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            execution: {
              language: "rust",
              run_cmd: ["./old_binary"]
            }
        }"#,
    )
    .expect("valid node config");

    let updated_config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "sensor",
              tag: "1.0.0",
            },
            execution: {
              language: "rust",
              run_cmd: ["./new_binary"]
            }
        }"#,
    )
    .expect("valid node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    stack
        .push_config(original_config, false, PathBuf::from("/tmp/sensor"))
        .expect("first config has no dependencies");
    assert_eq!(stack.len(), 2, "stack should have core node + sensor");

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().config().execution.run_cmd.as_ref().unwrap(),
        &vec!["./old_binary"],
        "entity should have the original run_cmd"
    );

    // Re-push same name:tag:interfaces:root_path with different run_cmd
    stack
        .push_config(updated_config, false, PathBuf::from("/tmp/sensor"))
        .expect("should update config without error");
    assert_eq!(stack.len(), 2, "stack should still have core node + sensor");

    let entity = stack.find("sensor", "1.0.0").expect("entity should exist");
    let entity_guard = entity.read();
    assert_eq!(
        entity_guard.config().execution.run_cmd.as_ref().unwrap(),
        &vec!["./new_binary"],
        "entity should have the updated run_cmd after re-push"
    );
    assert_eq!(
        entity_guard.config_path(),
        PathBuf::from("/tmp/sensor").as_path(),
        "config_path should remain unchanged"
    );
}

#[test]
fn updating_run_cmd_succeeds_even_when_node_has_dependents() {
    let dependency_v1: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "lidar",
              tag: "1.0.0",
            },
            execution: {
              language: "rust",
              run_cmd: ["./old_lidar"]
            },
        }"#,
    )
    .expect("valid dependency node config");

    let dependent: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "brain",
              tag: "1.0.0",
              depends_on: {
                nodes: [
                  { name: "lidar", tag: "1.0.0", local_id: "lidar" }
                ]
              },
            },
            execution: {
              language: "rust",
              run_cmd: ["brain"]
            },
        }"#,
    )
    .expect("valid dependent node config");

    // Same interfaces as v1, but different run_cmd
    let dependency_v1_updated: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            peppy_schema: "node_v1",
            manifest: {
              name: "lidar",
              tag: "1.0.0",
            },
            execution: {
              language: "rust",
              run_cmd: ["./new_lidar"]
            },
        }"#,
    )
    .expect("valid dependency node config");

    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    stack
        .push_config(dependency_v1, false, PathBuf::from("/tmp/lidar"))
        .expect("dependency has no dependencies");
    stack
        .push_config(dependent, false, PathBuf::from("/tmp/brain"))
        .expect("dependent dependency is present");

    // Updating run_cmd without changing interfaces should succeed even with dependents
    stack
        .push_config(dependency_v1_updated, false, PathBuf::from("/tmp/lidar"))
        .expect("non-breaking config update should succeed even with dependents");

    let entity = stack.find("lidar", "1.0.0").expect("entity should exist");
    assert_eq!(
        entity.read().config().execution.run_cmd.as_ref().unwrap(),
        &vec!["./new_lidar"],
        "lidar should have the updated run_cmd"
    );

    // Dependency wiring should still be intact
    let dependents = stack.dependents_of("lidar", "1.0.0");
    assert!(
        dependents
            .iter()
            .any(|entity| { entity.read().config().manifest.name.as_str() == "brain" }),
        "lidar should still have brain as a dependent after non-breaking update"
    );
}
