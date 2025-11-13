use node_stack::NodeStack;

#[test]
fn dynamically_add_node_to_node_stack_happy_path() {
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
    todo!("Finish")
}
