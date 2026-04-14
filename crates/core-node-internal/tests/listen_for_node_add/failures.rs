use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_config_found() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            interfaces: {
                topics: {
                    emits: [{ name: "/example" }]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    std::fs::remove_file(source_dir.path().join(NODE_CONFIG_FILE))
        .expect("failed to remove peppy.json5 config file");

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should not succeed, the config file is missing",
    );

    assert!(!node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 1, "root");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_git_hash_mismatch_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "git_hash_mismatch_node",
            tag: "0.1.0",
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let peppy_dir = source_dir.path().join(config::consts::PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir).expect("failed to create .peppy dir");
    std::fs::write(peppy_dir.join("git.hash"), "wrong-hash\n")
        .expect("failed to write wrong git hash file");

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
        "node_add should fail when git hash mismatches"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("git hash mismatch"))
            .unwrap_or(false),
        "error message should indicate git hash mismatch, got: {:?}",
        add_result.error_message
    );
    assert!(!node_stack.contains("git_hash_mismatch_node", "0.1.0"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_invalid_config_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{ manifest: [unclosed"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

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
        "node_add should fail for invalid json5"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Failed to parse node config"))
            .unwrap_or(false),
        "error message should indicate parse failure, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_run_cmd_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "no_run_cmd_node",
            tag: "0.1.0",
        },
        execution: {
            language: "rust",
        },
    }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

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
        "node_add should fail when run_cmd is missing"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("process"))
            .unwrap_or(false),
        "error message should mention process, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_dependency_not_resolved() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Try to add a consumer node that depends on a non-existent provider
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "consumer_node",
            tag: "1.0.0",
            depends_on: {
                nodes: [
                    { name: "non_existent_node", tag: "1.0.0", local_id: "non_existent_node" }
                ]
            },
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"],
        },
    }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

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
        "node_add should fail when dependencies are missing"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Failed to add node"))
            .unwrap_or(false),
        "error message should indicate add failure, got: {:?}",
        add_result.error_message
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("does not exist in the stack"))
            .unwrap_or(false),
        "error message should indicate missing dependency, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_fails_runs_add_cmd_on_missing_node_dependency() {
    // If there is a missing dependency, the NODE_ADD_ACTION should fail with a MissingDependency error
    // BEFORE running build_cmd. This mimics real nodes (e.g. fake_video_reconstruction) where
    // `cargo build` fails because peppygen interfaces are incomplete when dependencies are missing.
    const TARGET_NODE_NAME: &str = "add_cmd_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
          name: "TARGET_NODE_NAME",
          tag: "TARGET_NODE_TAG",
          depends_on: {
            nodes: [
              { name: "fake_uvc_camera", tag: "0.1.0", local_id: "fake_uvc_camera" }
            ]
          },
        },
        interfaces: {
          topics: {
            consumes: [
              {
                local_node_id: "fake_uvc_camera",
                name: "video_stream"
              },
            ],
          },
        },
        execution: {
          language: "rust",
          build_cmd: ["true"],
          run_cmd: ["sleep", "10"]
        },
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should fail when dependency is missing"
    );

    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("does not exist in the stack"))
            .unwrap_or(false),
        "error message should indicate missing dependency, got: {:?}",
        add_result.error_message
    );

    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when dependency is missing"
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_fails_on_missing_interface_even_when_dependency_exists() {
    // The dependency node (fake_uvc_camera:0.1.0) exists in the stack but emits a
    // DIFFERENT topic name than what the dependent node subscribes to. The node add should
    // fail with a MissingInterface error BEFORE running build_cmd. This mimics the real
    // scenario where `fake_uvc_camera` is added first, but `fake_video_reconstruction`
    // fails because the interface names don't match.
    const DEPENDENCY_NODE_NAME: &str = "fake_uvc_camera";
    const DEPENDENCY_NODE_TAG: &str = "0.1.0";
    const TARGET_NODE_NAME: &str = "fake_video_reconstruction";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Step 1: Add the dependency node that emits a topic with a DIFFERENT name
    // than what the dependent node will subscribe to.
    let dep_source_dir = tempfile::tempdir().expect("failed to create temp dep source dir");
    let dep_peppy_json5 = r#"{
            schema_version: 1,
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
    write_peppy_json5(dep_source_dir.path(), &dep_peppy_json5);

    let dep_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        dep_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("dependency node_add request should complete");

    assert!(
        dep_result.success,
        "dependency node_add should succeed, got error: {:?}",
        dep_result.error_message
    );
    assert!(
        node_stack.contains(DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG),
        "dependency node should be in the stack"
    );
    assert_eq!(node_stack.len(), 2, "root + dependency");

    // Step 2: Add the dependent node that subscribes to a topic name that the
    // dependency does NOT emit (node name+tag matches, but interface doesn't).
    let target_source_dir = tempfile::tempdir().expect("failed to create temp target source dir");

    let target_peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
          name: "TARGET_NODE_NAME",
          tag: "TARGET_NODE_TAG",
          depends_on: {
            nodes: [
              { name: "DEPENDENCY_NODE_NAME", tag: "DEPENDENCY_NODE_TAG", local_id: "DEPENDENCY_NODE_NAME" }
            ]
          },
        },
        interfaces: {
          topics: {
            consumes: [
              {
                local_node_id: "DEPENDENCY_NODE_NAME",
                name: "video_stream"
              },
            ],
          },
        },
        execution: {
          language: "rust",
          build_cmd: ["true"],
          run_cmd: ["sleep", "10"]
        },
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG)
    .replace("DEPENDENCY_NODE_NAME", DEPENDENCY_NODE_NAME)
    .replace("DEPENDENCY_NODE_TAG", DEPENDENCY_NODE_TAG);
    write_peppy_json5(target_source_dir.path(), &target_peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        target_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when interface is not exposed by dependency"
    );

    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("is not exposed"))
            .unwrap_or(false),
        "error message should indicate missing interface, got: {:?}",
        add_result.error_message
    );

    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "dependent node should not be added when interface is missing"
    );
    assert_eq!(node_stack.len(), 2, "root + dependency only");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_reports_excluded_dirs_in_feedback() {
    const TARGET_NODE_NAME: &str = "excluded_dirs_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Create directories that should be excluded from the copy
    for dir_name in [".venv", "target", "node_modules", "__pycache__"] {
        std::fs::create_dir(source_dir.path().join(dir_name))
            .expect("failed to create excluded dir");
    }

    let peppy_json5 = r#"{
            schema_version: 1,
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
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Some(feedback_tx),
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }

    let excluded_feedback = feedback.iter().find(|entry| {
        entry.is_stdout() && entry.line.starts_with("Excluded directories from copy:")
    });
    assert!(
        excluded_feedback.is_some(),
        "feedback should include excluded directories message, got: {feedback:?}"
    );

    let line = &excluded_feedback.unwrap().line;
    for expected in [".venv", "__pycache__", "node_modules", "target"] {
        assert!(
            line.contains(expected),
            "excluded dirs feedback should mention '{expected}', got: {line}"
        );
    }

    // Archive entry assertions (verifying excluded dirs are absent from the
    // produced artifact) live in `listen_for_node_build.rs` since the artifact
    // is built by the `node build` action.
    let _ = add_result.log_path;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_invalid_config_fails_fast() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = create_git_repo_with_invalid_config(git_repo_temp_dir.path());

    let started_core_node = start_core_node_with_mock_messenger().await;

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: "nodes/bad_node",
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should fail for invalid config"
    );
    assert!(
        add_result
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("Failed to parse node config"),
        "error should mention config parse failure, got: {:?}",
        add_result.error_message
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_missing_config_fails() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = create_git_repo_with_invalid_config(git_repo_temp_dir.path());

    let started_core_node = start_core_node_with_mock_messenger().await;

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    // Point to a path that doesn't exist in the repo.
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: "nodes/nonexistent_node",
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should fail for missing config"
    );
    let err = add_result.error_message.as_deref().unwrap_or("");
    // The shallow probe reports "not found in repository"; if the probe falls
    // back (e.g. local transport doesn't support shallow fetch), the full clone
    // path reports a filesystem read error instead.
    assert!(
        err.contains("not found in repository") || err.contains("Failed to parse node config"),
        "error should mention config not found or parse failure, got: {:?}",
        add_result.error_message
    );
}
