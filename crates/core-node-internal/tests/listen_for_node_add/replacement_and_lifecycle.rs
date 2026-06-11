use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_same_tags_overwrites_when_no_dependents() {
    const NODE_NAME: &str = "overwrite_node";
    const NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");

    // First add: no interfaces
    let peppy_json5_v1 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
            },
            interfaces: {
                topics: {
                    emits: [{ name: "wrong_topic_name" }]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{NODE_NAME}", NODE_NAME)
    .replace("{NODE_TAG}", NODE_TAG);
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add v1 should complete");

    assert!(
        add_v1.success,
        "node_add v1 should succeed, got error: {:?}",
        add_v1.error_message
    );

    assert_eq!(node_stack.len(), 2, "root + v1");
    assert_eq!(entity_instance_count(&node_stack, NODE_NAME, NODE_TAG), 0);

    // Second add: same name+tag but different interfaces -> should overwrite.
    let peppy_json5_v2 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
            },
            interfaces: {
                topics: {
                    emits: [{ name: "/example" }]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{NODE_NAME}", NODE_NAME)
    .replace("{NODE_TAG}", NODE_TAG);
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    let add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add v2 should complete");

    assert!(
        add_v2.success,
        "node_add should overwrite when there are no dependents, got error: {:?}",
        add_v2.error_message
    );

    assert_eq!(node_stack.len(), 2, "stack should be unchanged");
    let entity = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should exist after v2 overwrite");
    let entity_guard = entity.read();
    assert_eq!(
        entity_guard.instances().len(),
        0,
        "should not have any instances"
    );
    // Artifact path/existence assertions belong to the build phase and live in
    // `listen_for_node_build.rs`. Here we only verify the add-side overwrite
    // semantics: stack shape, no instances, and that the live config reflects
    // the overwritten interfaces.
    let _ = add_v2.log_path;
    assert!(
        entity_guard
            .config()
            .interfaces
            .topics
            .as_ref()
            .and_then(|t| t.emits.as_ref())
            .is_some_and(|topics| topics.iter().any(|topic| topic.name == "/example")),
        "node should have updated interfaces from the overwritten config"
    );
    drop(entity_guard);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_same_tags_fails_when_node_has_dependents() {
    const DEPENDENCY_NODE_NAME: &str = "lidar";
    const DEPENDENCY_NODE_TAG: &str = "v1";
    const DEPENDENT_NODE_NAME: &str = "brain";
    const DEPENDENT_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let dependency_source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependency_source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependent_source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let dependency_peppy_json5_v1 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
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
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v1.path(), &dependency_peppy_json5_v1);

    let dependency_add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add v1 should complete");
    assert!(
        dependency_add_v1.success,
        "dependency node_add v1 should succeed, got error: {:?}",
        dependency_add_v1.error_message
    );

    let dependent_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{DEPENDENT_NODE_NAME}",
                tag: "{DEPENDENT_NODE_TAG}",
                depends_on: {
                    nodes: [
                        { name: "{DEPENDENCY_NODE_NAME}", tag: "{DEPENDENCY_NODE_TAG}", link_id: "{DEPENDENCY_NODE_NAME}" }
                    ]
                },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          link_id: "{DEPENDENCY_NODE_NAME}",
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENT_NODE_NAME}", DEPENDENT_NODE_NAME)
    .replace("{DEPENDENT_NODE_TAG}", DEPENDENT_NODE_TAG)
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependent_source_dir.path(), &dependent_peppy_json5);

    let dependent_add = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependent_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependent node_add should complete");
    assert!(
        dependent_add.success,
        "dependent node_add should succeed, got error: {:?}",
        dependent_add.error_message
    );

    assert_eq!(node_stack.len(), 3, "root + dependency + dependent");

    // Overwrite attempt: same name+tag but different interfaces should fail due to dependent nodes.
    let dependency_peppy_json5_v2 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
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
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v2.path(), &dependency_peppy_json5_v2);

    let dependency_add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add v2 should complete");

    assert!(
        !dependency_add_v2.success,
        "overwriting an existing node should fail when it has dependents"
    );
    assert!(
        dependency_add_v2
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Cannot overwrite node"))
            .unwrap_or(false),
        "error message should indicate overwrite is not allowed, got: {:?}",
        dependency_add_v2.error_message
    );

    assert_eq!(node_stack.len(), 3, "stack should be unchanged");
    assert!(
        node_stack.contains(DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG),
        "dependent node should still exist after failed overwrite"
    );

    // Path equality alone isn't enough — assert the live entity config still
    // exposes the v1-only interface (`reset_sensor`) and does NOT expose the
    // v2-only interface (`new_service`). This proves the failed overwrite
    // truly preserved the original revision rather than just the path.
    {
        let handle = node_stack
            .find(DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG)
            .expect("dependency entity should exist");
        let guard = handle.read();
        let services = guard
            .config()
            .interfaces
            .services
            .as_ref()
            .expect("services section should be present");
        let exposed: Vec<&str> = services
            .exposes
            .as_ref()
            .map(|v| v.iter().map(|s| s.name.as_str()).collect())
            .unwrap_or_default();
        assert!(
            exposed.contains(&"reset_sensor"),
            "v1-only service `reset_sensor` should still be exposed; got {:?}",
            exposed
        );
        assert!(
            !exposed.contains(&"new_service"),
            "v2-only service `new_service` should NOT be exposed; got {:?}",
            exposed
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_different_tags_create_two_entities() {
    const NODE_NAME: &str = "versioned_node";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5_v1 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{NODE_NAME}",
                tag: "v1",
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
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{NODE_NAME}", NODE_NAME);
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add v1 should complete");

    assert!(
        add_v1.success,
        "node_add v1 should succeed, got error: {:?}",
        add_v1.error_message
    );

    let peppy_json5_v2 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{NODE_NAME}",
                tag: "v2",
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
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{NODE_NAME}", NODE_NAME);
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    let add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add v2 should complete");

    assert!(
        add_v2.success,
        "node_add v2 should succeed, got error: {:?}",
        add_v2.error_message
    );

    assert_eq!(node_stack.len(), 3, "root + two versions");
    assert!(node_stack.contains(NODE_NAME, "v1"));
    assert!(node_stack.contains(NODE_NAME, "v2"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_fingerprint_mismatch() {
    const TARGET_NODE_NAME: &str = "fingerprint_mismatch_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    // Write the config file only (without fingerprint)
    let config_path = source_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&config_path, &peppy_json5).expect("failed to write peppy.json5");

    // Create a wrong fingerprint that won't match the actual peppy.json5 content
    config::fingerprint::create_wrong_codegen_fingerprint(
        &config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when fingerprint mismatches"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Codegen fingerprint verification failed"))
            .unwrap_or(false),
        "error message should indicate fingerprint verification failure, got: {:?}",
        add_result.error_message
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("fingerprint mismatch"))
            .unwrap_or(false),
        "error message should mention fingerprint mismatch, got: {:?}",
        add_result.error_message
    );

    // Node should not be in the stack
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when fingerprint mismatches"
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");
}
/// Tests that a new goal can be processed after a previous action was abandoned
/// (goal accepted but result never polled).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_abandoned_action_does_not_block_next_goal() {
    const FIRST_NODE_NAME: &str = "abandoned_node";
    const FIRST_NODE_TAG: &str = "v1";
    const SECOND_NODE_NAME: &str = "second_node";
    const SECOND_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create first node source directory
    let first_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let first_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{FIRST_NODE_NAME}",
                tag: "{FIRST_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{FIRST_NODE_NAME}", FIRST_NODE_NAME)
    .replace("{FIRST_NODE_TAG}", FIRST_NODE_TAG);
    write_peppy_json5(first_source_dir.path(), &first_peppy_json5);

    // Write git hash file for first node
    let first_peppy_dir = first_source_dir.path().join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&first_peppy_dir).expect("failed to create .peppy dir");
    std::fs::write(first_peppy_dir.join("git.hash"), TEST_GIT_HASH)
        .expect("failed to write git hash");

    // Send first goal but DON'T wait for result (simulating abandoned action)
    let first_goal = NodeAddGoal::new(
        first_source_dir.path(),
        TEST_GIT_HASH,
        RESULT_TIMEOUT.as_secs(),
    );
    let first_goal_payload = first_goal.encode().expect("failed to encode goal");

    let first_action_handle = ActionMessenger::send_goal(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started_core_node.core_node_name),
        names::NODE_ADD_ACTION,
        Some(&started_core_node.core_node_name),
        None,
        first_goal_payload,
        QoSProfile::default(),
        GOAL_TIMEOUT,
    )
    .await
    .expect("first goal should be sent");

    // Verify first goal was accepted
    let first_goal_response_payload = first_action_handle.goal_response().payload();
    let first_goal_response = NodeAddGoalResponse::decode(&first_goal_response_payload)
        .expect("failed to decode first goal response");
    assert!(
        first_goal_response.accepted,
        "first goal should be accepted"
    );

    // Wait for the first action to settle without polling its action
    // result (the whole point of this test is that the result is never
    // requested). With `node_add` and `node_build` now split into two
    // separate actions, settling means the entity reached `Added` (the
    // terminal stage of `node_add`); a follow-up `node_build` would be a
    // separate goal.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(handle) = node_stack.find(FIRST_NODE_NAME, FIRST_NODE_TAG) {
                let guard = handle.read();
                if matches!(guard.stage(), node_stack::NodeStage::Added { .. }) {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("first node never reached Added within 30s");

    // Now send second goal - this should succeed even though we never polled
    // for the first action's result
    let second_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let second_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{SECOND_NODE_NAME}",
                tag: "{SECOND_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{SECOND_NODE_NAME}", SECOND_NODE_NAME)
    .replace("{SECOND_NODE_TAG}", SECOND_NODE_TAG);
    write_peppy_json5(second_source_dir.path(), &second_peppy_json5);

    let second_add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        second_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("second node_add request should complete");

    assert!(
        second_add_result.success,
        "second node_add should succeed even after first action was abandoned, got error: {:?}",
        second_add_result.error_message
    );

    // Verify both nodes are in the stack
    assert!(
        node_stack.contains(FIRST_NODE_NAME, FIRST_NODE_TAG),
        "first node should be in stack (action completed even though result wasn't polled)"
    );
    assert!(
        node_stack.contains(SECOND_NODE_NAME, SECOND_NODE_TAG),
        "second node should be in stack"
    );
    assert_eq!(node_stack.len(), 3, "root + first + second nodes");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_same_node_shutdown_existing_instances() {
    use peppylib::messaging::{MessengerHandle, SHUTDOWN_SERVICE, ServiceMessenger};
    use std::sync::Arc;
    use tokio::sync::{Mutex, Notify, oneshot};

    const NODE_NAME: &str = "readd_node";
    const NODE_TAG: &str = "v1";
    const INSTANCE_1: &str = "readd_instance_1";
    const INSTANCE_2: &str = "readd_instance_2";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5_v1 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{NODE_NAME}", NODE_NAME)
    .replace("{NODE_TAG}", NODE_TAG);
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add v1 should complete");
    assert!(
        add_v1.success,
        "node_add v1 should succeed, got error: {:?}",
        add_v1.error_message
    );
    build_staged_node(&started_core_node, NODE_NAME, NODE_TAG).await;

    let instance_id_1 = config::node::Name::new(INSTANCE_1).expect("valid instance id 1");
    let instance_id_2 = config::node::Name::new(INSTANCE_2).expect("valid instance id 2");
    let _running_1 =
        spawn_real_running_instance(&started_core_node, NODE_NAME, NODE_TAG, &instance_id_1).await;
    let _running_2 =
        spawn_real_running_instance(&started_core_node, NODE_NAME, NODE_TAG, &instance_id_2).await;

    let instance_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));

    let (called_tx_1, called_rx_1) = oneshot::channel::<()>();
    let called_tx_1 = Arc::new(Mutex::new(Some(called_tx_1)));
    let allow_shutdown_1 = Arc::new(Notify::new());
    let allow_shutdown_1_clone = Arc::clone(&allow_shutdown_1);
    let mut shutdown_endpoint_1 = ServiceMessenger::listen(
        &instance_messenger,
        &started_core_node.core_node_name,
        INSTANCE_1,
        common::test_node_target(NODE_NAME),
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service for instance 1");
    let _shutdown_task_1 = AbortOnDrop(peppylib::runtime::spawn({
        let called_tx_1 = Arc::clone(&called_tx_1);
        async move {
            shutdown_endpoint_1
                .handle_requests(move |context| {
                    let called_tx_1 = Arc::clone(&called_tx_1);
                    let allow_shutdown_1_clone = Arc::clone(&allow_shutdown_1_clone);
                    async move {
                        let payload = context.message().payload().to_owned();
                        if let Some(tx) = called_tx_1.lock().await.take() {
                            let _ = tx.send(());
                        }
                        allow_shutdown_1_clone.notified().await;
                        Ok(payload)
                    }
                })
                .await
        }
    }));

    let (called_tx_2, called_rx_2) = oneshot::channel::<()>();
    let called_tx_2 = Arc::new(Mutex::new(Some(called_tx_2)));
    let allow_shutdown_2 = Arc::new(Notify::new());
    let allow_shutdown_2_clone = Arc::clone(&allow_shutdown_2);
    let mut shutdown_endpoint_2 = ServiceMessenger::listen(
        &instance_messenger,
        &started_core_node.core_node_name,
        INSTANCE_2,
        common::test_node_target(NODE_NAME),
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service for instance 2");
    let _shutdown_task_2 = AbortOnDrop(peppylib::runtime::spawn({
        let called_tx_2 = Arc::clone(&called_tx_2);
        async move {
            shutdown_endpoint_2
                .handle_requests(move |context| {
                    let called_tx_2 = Arc::clone(&called_tx_2);
                    let allow_shutdown_2_clone = Arc::clone(&allow_shutdown_2_clone);
                    async move {
                        let payload = context.message().payload().to_owned();
                        if let Some(tx) = called_tx_2.lock().await.take() {
                            let _ = tx.send(());
                        }
                        allow_shutdown_2_clone.notified().await;
                        Ok(payload)
                    }
                })
                .await
        }
    }));

    // Wait until both shutdown services are reachable through the broker.
    // Replaces a fixed sleep that races against messenger registration.
    super::wait_until_service_reachable(
        &instance_messenger,
        &started_core_node.core_node_name,
        NODE_NAME,
        SHUTDOWN_SERVICE,
        &started_core_node.core_node_name,
        INSTANCE_1,
        Duration::from_secs(5),
    )
    .await;
    super::wait_until_service_reachable(
        &instance_messenger,
        &started_core_node.core_node_name,
        NODE_NAME,
        SHUTDOWN_SERVICE,
        &started_core_node.core_node_name,
        INSTANCE_2,
        Duration::from_secs(5),
    )
    .await;

    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5_v2 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{NODE_NAME}", NODE_NAME)
    .replace("{NODE_TAG}", NODE_TAG);
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    // Drop a v2-only marker into the source directory so the rebuilt
    // archive bytes diverge from v1. Without this the deterministic
    // archive naming would produce identical artifact bytes and the
    // mid-overwrite assertions below couldn't distinguish "still v1" from
    // "already v2".
    std::fs::write(
        source_dir_v2.path().join("v2_marker.txt"),
        b"v2-only payload",
    )
    .expect("write v2 marker");

    // The server wildcards the caller-side positions of its feedback publish
    // keyexpr, so a concrete caller still receives feedback over a real
    // messenger (the mock adapter just doesn't deliver feedback).
    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();

    let caller_handle = started_core_node.caller_handle.clone();
    let core_node_name = started_core_node.core_node_name.clone();
    let source_path_v2 = source_dir_v2.path().to_path_buf();
    let add_task = tokio::spawn(async move {
        send_node_add_and_wait(
            &caller_handle,
            &core_node_name,
            &source_path_v2,
            GOAL_TIMEOUT,
            RESULT_TIMEOUT,
            Some(feedback_tx),
        )
        .await
    });

    // Wait for both shutdown requests in parallel — the overwrite shuts
    // both instances down concurrently, so awaiting them in sequence
    // (with a notify between) makes the assertion order-dependent and
    // racy.
    let (rx_1, rx_2) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(5), called_rx_1),
        tokio::time::timeout(Duration::from_secs(5), called_rx_2),
    );
    rx_1.expect("shutdown request for instance 1 should arrive within timeout")
        .expect("shutdown channel for instance 1 should not be dropped");
    rx_2.expect("shutdown request for instance 2 should arrive within timeout")
        .expect("shutdown channel for instance 2 should not be dropped");

    // Release both shutdown handlers so the overwrite can proceed.
    allow_shutdown_1.notify_one();
    allow_shutdown_2.notify_one();

    let add_v2 = add_task
        .await
        .expect("node_add overwrite task should join")
        .expect("node_add overwrite request should complete");

    assert!(
        add_v2.success,
        "node_add overwrite should succeed, got error: {:?}",
        add_v2.error_message
    );

    let _ = add_v2.log_path;
    assert_eq!(
        entity_instance_count(&node_stack, NODE_NAME, NODE_TAG),
        0,
        "instances should be stopped before overwrite completes"
    );

    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }
    let expected_instance_1 = format!("{INSTANCE_1} has been stopped");
    let expected_instance_2 = format!("{INSTANCE_2} has been stopped");
    let saw_instance_1 = feedback
        .iter()
        .any(|entry| entry.is_stdout() && entry.line.trim() == expected_instance_1.as_str());
    let saw_instance_2 = feedback
        .iter()
        .any(|entry| entry.is_stdout() && entry.line.trim() == expected_instance_2.as_str());
    assert!(saw_instance_1, "should emit stop feedback for instance 1");
    assert!(saw_instance_2, "should emit stop feedback for instance 2");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_same_node_with_running_instance_and_dependents_succeeds() {
    use peppylib::messaging::{MessengerHandle, SHUTDOWN_SERVICE, ServiceMessenger};
    use std::sync::Arc;
    use tokio::sync::{Mutex, Notify, oneshot};

    const DEPENDENCY_NODE_NAME: &str = "lidar_dep";
    const DEPENDENCY_NODE_TAG: &str = "v1";
    const DEPENDENT_NODE_NAME: &str = "brain_dep";
    const DEPENDENT_NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "lidar_dep_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let dependency_source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependency_source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependent_source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let dependency_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
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
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v1.path(), &dependency_peppy_json5);

    let dependency_add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add v1 should complete");
    assert!(
        dependency_add_v1.success,
        "dependency node_add v1 should succeed: {:?}",
        dependency_add_v1.error_message
    );

    let dependent_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{DEPENDENT_NODE_NAME}",
                tag: "{DEPENDENT_NODE_TAG}",
                depends_on: {
                    nodes: [
                        { name: "{DEPENDENCY_NODE_NAME}", tag: "{DEPENDENCY_NODE_TAG}", link_id: "{DEPENDENCY_NODE_NAME}" }
                    ]
                },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          link_id: "{DEPENDENCY_NODE_NAME}",
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENT_NODE_NAME}", DEPENDENT_NODE_NAME)
    .replace("{DEPENDENT_NODE_TAG}", DEPENDENT_NODE_TAG)
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependent_source_dir.path(), &dependent_peppy_json5);

    let dependent_add = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependent_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependent node_add should complete");
    assert!(
        dependent_add.success,
        "dependent node_add should succeed: {:?}",
        dependent_add.error_message
    );
    build_staged_node(
        &started_core_node,
        DEPENDENCY_NODE_NAME,
        DEPENDENCY_NODE_TAG,
    )
    .await;
    build_staged_node(&started_core_node, DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG).await;

    // Add a fake running instance to the dependency node
    let instance_id = config::node::Name::new(INSTANCE_ID).expect("valid instance id");
    let _running = spawn_real_running_instance(
        &started_core_node,
        DEPENDENCY_NODE_NAME,
        DEPENDENCY_NODE_TAG,
        &instance_id,
    )
    .await;

    // Mock the shutdown service for the running instance
    let instance_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let (called_tx, called_rx) = oneshot::channel::<()>();
    let called_tx = Arc::new(Mutex::new(Some(called_tx)));
    let allow_shutdown = Arc::new(Notify::new());
    let allow_shutdown_clone = Arc::clone(&allow_shutdown);
    let mut shutdown_endpoint = ServiceMessenger::listen(
        &instance_messenger,
        &started_core_node.core_node_name,
        INSTANCE_ID,
        common::test_node_target(DEPENDENCY_NODE_NAME),
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service");
    let _shutdown_task = AbortOnDrop(peppylib::runtime::spawn({
        let called_tx = Arc::clone(&called_tx);
        async move {
            shutdown_endpoint
                .handle_requests(move |context| {
                    let called_tx = Arc::clone(&called_tx);
                    let allow_shutdown_clone = Arc::clone(&allow_shutdown_clone);
                    async move {
                        let payload = context.message().payload().to_owned();
                        if let Some(tx) = called_tx.lock().await.take() {
                            let _ = tx.send(());
                        }
                        allow_shutdown_clone.notified().await;
                        Ok(payload)
                    }
                })
                .await
        }
    }));

    super::wait_until_service_reachable(
        &instance_messenger,
        &started_core_node.core_node_name,
        DEPENDENCY_NODE_NAME,
        SHUTDOWN_SERVICE,
        &started_core_node.core_node_name,
        INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;

    // Re-add the dependency with the same interface
    write_peppy_json5(dependency_source_dir_v2.path(), &dependency_peppy_json5);

    let caller_handle = started_core_node.caller_handle.clone();
    let core_node_name = started_core_node.core_node_name.clone();
    let source_path_v2 = dependency_source_dir_v2.path().to_path_buf();
    let add_task = tokio::spawn(async move {
        send_node_add_and_wait(
            &caller_handle,
            &core_node_name,
            &source_path_v2,
            GOAL_TIMEOUT,
            RESULT_TIMEOUT,
            None,
        )
        .await
    });

    // Wait for shutdown to be requested, then allow it to complete
    tokio::time::timeout(Duration::from_secs(5), called_rx)
        .await
        .expect("shutdown request should arrive within timeout")
        .expect("shutdown channel should not be dropped");
    allow_shutdown.notify_one();

    let add_v2 = add_task
        .await
        .expect("node_add re-add task should join")
        .expect("node_add re-add request should complete");

    assert!(
        add_v2.success,
        "re-adding a node with same interface should succeed even when dependents exist, got: {:?}",
        add_v2.error_message
    );

    assert_eq!(
        entity_instance_count(&node_stack, DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG),
        0,
        "running instance should have been stopped"
    );
    assert!(
        node_stack.contains(DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG),
        "dependent node should still be in the stack"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_same_node_changing_interface_with_running_instance_and_dependents_fails() {
    // The instance is stopped first (shutdown succeeds), then push_config fails because
    // the new interface breaks the dependent. The stack is preserved with the old config.
    use peppylib::messaging::{MessengerHandle, SHUTDOWN_SERVICE, ServiceMessenger};
    use std::sync::Arc;

    const DEPENDENCY_NODE_NAME: &str = "lidar_iface";
    const DEPENDENCY_NODE_TAG: &str = "v1";
    const DEPENDENT_NODE_NAME: &str = "brain_iface";
    const DEPENDENT_NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "lidar_iface_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let dependency_source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependency_source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependent_source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let dependency_peppy_json5_v1 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
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
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v1.path(), &dependency_peppy_json5_v1);

    let dependency_add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add v1 should complete");
    assert!(
        dependency_add_v1.success,
        "dependency node_add v1 should succeed: {:?}",
        dependency_add_v1.error_message
    );

    let dependent_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{DEPENDENT_NODE_NAME}",
                tag: "{DEPENDENT_NODE_TAG}",
                depends_on: {
                    nodes: [
                        { name: "{DEPENDENCY_NODE_NAME}", tag: "{DEPENDENCY_NODE_TAG}", link_id: "{DEPENDENCY_NODE_NAME}" }
                    ]
                },
            },
            interfaces: {
                services: {
                    consumes: [
                        {
                          link_id: "{DEPENDENCY_NODE_NAME}",
                          name: "reset_sensor"
                        }
                    ]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENT_NODE_NAME}", DEPENDENT_NODE_NAME)
    .replace("{DEPENDENT_NODE_TAG}", DEPENDENT_NODE_TAG)
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependent_source_dir.path(), &dependent_peppy_json5);

    let dependent_add = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependent_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependent node_add should complete");
    assert!(
        dependent_add.success,
        "dependent node_add should succeed: {:?}",
        dependent_add.error_message
    );
    build_staged_node(
        &started_core_node,
        DEPENDENCY_NODE_NAME,
        DEPENDENCY_NODE_TAG,
    )
    .await;
    build_staged_node(&started_core_node, DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG).await;

    // Add a fake running instance to the dependency node
    let instance_id = config::node::Name::new(INSTANCE_ID).expect("valid instance id");
    let _running = spawn_real_running_instance(
        &started_core_node,
        DEPENDENCY_NODE_NAME,
        DEPENDENCY_NODE_TAG,
        &instance_id,
    )
    .await;

    // Register a SHUTDOWN_SERVICE handler that responds immediately.
    // Shutdown succeeds; push_config then rejects the overwrite due to the interface change.
    let instance_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let mut shutdown_endpoint = ServiceMessenger::listen(
        &instance_messenger,
        &started_core_node.core_node_name,
        INSTANCE_ID,
        common::test_node_target(DEPENDENCY_NODE_NAME),
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service");
    let _shutdown_task = AbortOnDrop(peppylib::runtime::spawn(async move {
        shutdown_endpoint
            .handle_requests(|context| async move { Ok(context.message().payload().to_owned()) })
            .await
    }));

    super::wait_until_service_reachable(
        &instance_messenger,
        &started_core_node.core_node_name,
        DEPENDENCY_NODE_NAME,
        SHUTDOWN_SERVICE,
        &started_core_node.core_node_name,
        INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;

    // Try to overwrite with a different interface (new_service instead of reset_sensor).
    let dependency_peppy_json5_v2 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
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
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v2.path(), &dependency_peppy_json5_v2);

    let add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add overwrite request should complete");

    assert!(
        !add_v2.success,
        "overwriting with a changed interface should fail when dependents exist"
    );
    assert!(
        add_v2
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Cannot overwrite node"))
            .unwrap_or(false),
        "error should indicate the overwrite is blocked: {:?}",
        add_v2.error_message
    );

    // Shutdown succeeded, so the instance was stopped before push_config rejected the overwrite
    assert_eq!(
        entity_instance_count(&node_stack, DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG),
        0,
        "running instance should have been stopped before push_config rejected the overwrite"
    );
    assert!(
        node_stack.contains(DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG),
        "dependent node should still be in the stack after failed overwrite"
    );

    // The dependency's interface must remain the v1 shape: `reset_sensor`
    // is still exposed and the v2 `new_service` was never spliced in.
    {
        let handle = node_stack
            .find(DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG)
            .expect("dependency entity missing");
        let guard = handle.read();
        let exposes = guard
            .config()
            .interfaces
            .services
            .as_ref()
            .and_then(|s| s.exposes.as_ref())
            .expect("v1 services.exposes should be present");
        let names: Vec<&str> = exposes.iter().map(|svc| svc.name.as_str()).collect();
        assert!(
            names.contains(&"reset_sensor"),
            "v1 `reset_sensor` should still be exposed after failed overwrite, got: {:?}",
            names
        );
        assert!(
            !names.contains(&"new_service"),
            "v2 `new_service` must not have leaked through the failed overwrite, got: {:?}",
            names
        );
    }
}
/// When a running node instance does not respond to SHUTDOWN_SERVICE (e.g. the
/// process is frozen), the overwrite path must behave like the SIGINT teardown:
/// after the cooperative shutdown times out, `stop_instances` force-kills the
/// instance's process group, waits for it to die, and the add proceeds. The
/// stuck instance must be removed (not orphaned) and the dependent preserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_same_node_with_running_instance_and_dependents_force_kills_stuck_node() {
    use peppylib::messaging::{MessengerHandle, SHUTDOWN_SERVICE, ServiceMessenger};
    use std::sync::Arc;
    use tokio::sync::Notify;

    const DEPENDENCY_NODE_NAME: &str = "lidar_stuck";
    const DEPENDENCY_NODE_TAG: &str = "v1";
    const DEPENDENT_NODE_NAME: &str = "brain_stuck";
    const DEPENDENT_NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "lidar_stuck_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let dependency_source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependency_source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let dependent_source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let dependency_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{DEPENDENCY_NODE_NAME}",
                tag: "{DEPENDENCY_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependency_source_dir_v1.path(), &dependency_peppy_json5);

    let dependency_add_v1 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v1.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add v1 should complete");
    assert!(
        dependency_add_v1.success,
        "dependency node_add v1 should succeed: {:?}",
        dependency_add_v1.error_message
    );

    let dependent_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{DEPENDENT_NODE_NAME}",
                tag: "{DEPENDENT_NODE_TAG}",
                depends_on: {
                    nodes: [
                        { name: "{DEPENDENCY_NODE_NAME}", tag: "{DEPENDENCY_NODE_TAG}", link_id: "{DEPENDENCY_NODE_NAME}" }
                    ]
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            },
        }"#
    .replace("{DEPENDENT_NODE_NAME}", DEPENDENT_NODE_NAME)
    .replace("{DEPENDENT_NODE_TAG}", DEPENDENT_NODE_TAG)
    .replace("{DEPENDENCY_NODE_NAME}", DEPENDENCY_NODE_NAME)
    .replace("{DEPENDENCY_NODE_TAG}", DEPENDENCY_NODE_TAG);
    write_peppy_json5(dependent_source_dir.path(), &dependent_peppy_json5);

    let dependent_add = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependent_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependent node_add should complete");
    assert!(
        dependent_add.success,
        "dependent node_add should succeed: {:?}",
        dependent_add.error_message
    );
    build_staged_node(
        &started_core_node,
        DEPENDENCY_NODE_NAME,
        DEPENDENCY_NODE_TAG,
    )
    .await;
    build_staged_node(&started_core_node, DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG).await;

    // Spawn a real running instance WITHOUT the auto-shutdown listener so
    // the production shutdown path observes a stuck process that never
    // responds or terminates.
    let instance_id = config::node::Name::new(INSTANCE_ID).expect("valid instance id");
    let running = spawn_real_stuck_instance(
        &started_core_node,
        DEPENDENCY_NODE_NAME,
        DEPENDENCY_NODE_TAG,
        &instance_id,
    )
    .await;
    let stuck_pid = running.pid;

    // Register a SHUTDOWN_SERVICE handler that blocks forever — simulates a frozen/unresponsive node.
    // `notify_one` is never called, so the handler never returns, causing the poll to time out.
    let instance_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let never_unblock = Arc::new(Notify::new());
    let never_unblock_clone = Arc::clone(&never_unblock);
    let mut shutdown_endpoint = ServiceMessenger::listen(
        &instance_messenger,
        &started_core_node.core_node_name,
        INSTANCE_ID,
        common::test_node_target(DEPENDENCY_NODE_NAME),
        SHUTDOWN_SERVICE,
    )
    .await
    .expect("failed to expose shutdown service");
    let _shutdown_task = AbortOnDrop(peppylib::runtime::spawn(async move {
        shutdown_endpoint
            .handle_requests(move |context| {
                let never_unblock_clone = Arc::clone(&never_unblock_clone);
                async move {
                    let payload = context.message().payload().to_owned();
                    // Block forever — the node never acknowledges the shutdown
                    never_unblock_clone.notified().await;
                    Ok(payload)
                }
            })
            .await
    }));

    super::wait_until_service_reachable(
        &instance_messenger,
        &started_core_node.core_node_name,
        DEPENDENCY_NODE_NAME,
        SHUTDOWN_SERVICE,
        &started_core_node.core_node_name,
        INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;

    // Re-add with the same interface. The cooperative shutdown poll will time
    // out against the forever-blocking handler; the overwrite path must then
    // force-kill the stuck instance's process group and proceed.
    write_peppy_json5(dependency_source_dir_v2.path(), &dependency_peppy_json5);
    let add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dependency_source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    // The cooperative shutdown can NEVER succeed here (the handler blocks
    // forever and no listener kills the process), so a successful overwrite is
    // proof the stuck instance was force-killed.
    assert!(
        add_v2.success,
        "node_add should force-kill the stuck instance and succeed: {:?}",
        add_v2.error_message
    );

    // The stuck instance must be removed (force-killed, not orphaned).
    assert_eq!(
        entity_instance_count(&node_stack, DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG),
        0,
        "stuck instance should be removed after force-kill"
    );
    assert!(
        node_stack.contains(DEPENDENT_NODE_NAME, DEPENDENT_NODE_TAG),
        "dependent node should still be in the stack"
    );

    // The real process must be gone — no orphan. Poll: the reap is bounded and
    // best-effort, so the OS may finish teardown a beat after the add returns.
    poll_until(
        Duration::from_secs(5),
        &format!("stuck instance process {stuck_pid} should be force-killed, not orphaned"),
        || (!is_process_running(stuck_pid)).then_some(()),
    )
    .await;
}

/// Overwriting an entity with SEVERAL stuck running instances must stop them as
/// one batch — every cooperative shutdown sent concurrently, one shared grace
/// budget — exactly like the SIGINT teardown, not one full grace window per
/// instance. Locks in both halves of `stop_instances`: correctness (both
/// process groups force-killed and removed, no orphans) and the shared-budget
/// timing (a per-instance loop would burn at least 2 × grace in stuck-grace
/// alone before the second force-kill even fired).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_overwrite_with_two_stuck_instances_shares_one_grace_budget() {
    const NODE_NAME: &str = "lidar_two_stuck";
    const NODE_TAG: &str = "v1";
    // Wider than the 3s default so the timing window below has comfortable
    // margins on both sides even under parallel-test CI load: batched ≈ one
    // grace window (~6.3s), serial ≥ two (12s+), bound at 2 × grace (12s).
    const SHUTDOWN_GRACE_SECS: u64 = 6;

    let started_core_node = common::start_core_node_with_shutdown_grace(SHUTDOWN_GRACE_SECS).await;
    let node_stack = started_core_node.node_stack.clone();

    // v1: a real spawnable node (forks two grandchildren in its group).
    let _source_dir_v1 = add_and_build_forking_node(&started_core_node, NODE_NAME, NODE_TAG).await;

    // Two stuck instances of the same entity: neither installs a shutdown
    // listener, so the overwrite's cooperative phase can never succeed and the
    // full grace window is burned before the force phase.
    let id_a = config::node::Name::new("two_stuck_a").expect("valid instance id");
    let id_b = config::node::Name::new("two_stuck_b").expect("valid instance id");
    let inst_a = spawn_real_stuck_instance(&started_core_node, NODE_NAME, NODE_TAG, &id_a).await;
    let inst_b = spawn_real_stuck_instance(&started_core_node, NODE_NAME, NODE_TAG, &id_b).await;
    let pid_a = inst_a.pid;
    let pid_b = inst_b.pid;
    // Assert liveness BEFORE forgetting the guards, so a failure here still
    // reaps the children via the guards' drop instead of leaking them.
    assert!(
        is_process_running(pid_a) && is_process_running(pid_b),
        "both stuck instances should be running before the overwrite"
    );
    // Drop the guards' stop-on-drop so only the overwrite path kills them.
    std::mem::forget(inst_a);
    std::mem::forget(inst_b);

    // v2 with the same name/tag triggers the overwrite path.
    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5_v2 = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "{NAME}", tag: "{TAG}" },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{NAME}", NODE_NAME)
    .replace("{TAG}", NODE_TAG);
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    let grace = Duration::from_secs(SHUTDOWN_GRACE_SECS);
    let overwrite_started = std::time::Instant::now();
    let add_v2 = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_v2.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");
    let elapsed = overwrite_started.elapsed();

    assert!(
        add_v2.success,
        "node_add should force-kill both stuck instances and succeed: {:?}",
        add_v2.error_message
    );

    // Lower bound: the stuck instances really did sit out a full grace window
    // (neither answers SHUTDOWN_SERVICE). Pins the test's premise — if a future
    // change short-circuits the grace on an unreachable/failed cooperative
    // send, this fires instead of the upper bound passing vacuously.
    assert!(
        elapsed >= grace,
        "stuck instances should burn the full grace window, took only {elapsed:?}"
    );
    // Upper bound (shared budget): the whole batch burns ONE stuck-grace
    // window (plus the bounded reap and the add's own staging work —
    // comfortably under a second grace window). A per-instance loop burns
    // ≥ 2 × grace in stuck-grace alone, so it cannot finish under this bound.
    assert!(
        elapsed < grace * 2,
        "overwrite of two stuck instances should share one grace budget, \
         took {elapsed:?} (a per-instance loop would take at least {:?})",
        grace * 2
    );

    // Both stuck instances must be removed (force-killed, not orphaned).
    assert_eq!(
        entity_instance_count(&node_stack, NODE_NAME, NODE_TAG),
        0,
        "both stuck instances should be removed after the overwrite"
    );
    for pid in [pid_a, pid_b] {
        poll_until(
            Duration::from_secs(5),
            &format!("stuck instance process {pid} should be force-killed, not orphaned"),
            || (!is_process_running(pid)).then_some(()),
        )
        .await;
    }
}
