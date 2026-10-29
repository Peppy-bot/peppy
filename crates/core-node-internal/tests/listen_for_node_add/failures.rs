use super::*;
use common::{TestPackagesCache, write_plain_peppy_json5};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_fails_when_packages_cache_missing() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();

    // No nodes.json5 written at all.
    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::ResolveRef {
            name: "ghost",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    assert_failure(
        res.failed(),
        PeppyFailureKind::PreconditionFailed,
        "peppy repo refresh",
    );
    assert_eq!(node_stack.len(), 1, "only root entity should exist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_fails_when_root_node_unknown() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();

    let tmp = TempDir::new().unwrap();
    let some_dir = tmp.path().join("other");
    write_plain_peppy_json5(&some_dir, &minimal_node_config("other", "v1", &[]));

    TestPackagesCache::new()
        .fs_entry("other", "v1", &some_dir)
        .write(&started.peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::ResolveRef {
            name: "ghost",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    let failure = res.failed();
    assert_failure(failure, PeppyFailureKind::Rejected, "ghost:v1");
    assert!(
        failure.message().contains("missing"),
        "the failure should list the missing root; got: {failure}"
    );
    assert_eq!(node_stack.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_fails_when_dep_missing_in_cache() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();

    let tmp = TempDir::new().unwrap();
    let target_dir = tmp.path().join("target");
    write_plain_peppy_json5(
        &target_dir,
        &minimal_node_config("target", "v1", &[("missing_dep", "v999")]),
    );

    // Target is in the cache, but missing_dep is not.
    TestPackagesCache::new()
        .fs_entry("target", "v1", &target_dir)
        .write(&started.peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::ResolveRef {
            name: "target",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    assert_failure(res.failed(), PeppyFailureKind::Rejected, "missing_dep:v999");
    assert_eq!(node_stack.len(), 1, "nothing should have been added");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_fails_on_cycle() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();

    let tmp = TempDir::new().unwrap();
    let a_dir = tmp.path().join("a");
    let b_dir = tmp.path().join("b");
    write_plain_peppy_json5(&a_dir, &minimal_node_config("a", "v1", &[("b", "v1")]));
    write_plain_peppy_json5(&b_dir, &minimal_node_config("b", "v1", &[("a", "v1")]));

    TestPackagesCache::new()
        .fs_entry("a", "v1", &a_dir)
        .fs_entry("b", "v1", &b_dir)
        .write(&started.peppy_dirs);

    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::ResolveRef {
            name: "a",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    assert_failure(
        res.failed(),
        PeppyFailureKind::Rejected,
        "Dependency resolution failed",
    );
    assert_eq!(node_stack.len(), 1, "nothing should have been added");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_node_add_rolls_back_on_mid_batch_failure() {
    let started = start_core_node_with_mock_messenger().await;
    let node_stack = started.node_stack.clone();

    let tmp = TempDir::new().unwrap();
    let b_dir = tmp.path().join("b");
    write_plain_peppy_json5(&b_dir, &minimal_node_config("b", "v1", &[]));

    // C declares a dep on `ghost_dep` that is NOT in the cache. The
    // resolver collects this as a "missing" dep and must fail cleanly
    // before ANY node is pushed onto the stack.
    let c_dir = tmp.path().join("c");
    write_plain_peppy_json5(
        &c_dir,
        &minimal_node_config("c", "v1", &[("ghost_dep", "v999")]),
    );
    let a_dir = tmp.path().join("a");
    write_plain_peppy_json5(
        &a_dir,
        &minimal_node_config("a", "v1", &[("b", "v1"), ("c", "v1")]),
    );

    TestPackagesCache::new()
        .fs_entry("a", "v1", &a_dir)
        .fs_entry("b", "v1", &b_dir)
        .fs_entry("c", "v1", &c_dir)
        .write(&started.peppy_dirs);

    let pre_len = node_stack.len();
    let res = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        NodeAddSource::ResolveRef {
            name: "a",
            tag: "v1",
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    assert_failure(res.failed(), PeppyFailureKind::Rejected, "ghost_dep:v999");
    // Stack must be unchanged; no partial add.
    assert_eq!(node_stack.len(), pre_len, "stack should be unchanged");
    assert!(!node_stack.contains("a", "v1"));
    assert!(!node_stack.contains("b", "v1"));
    assert!(!node_stack.contains("c", "v1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_config_found() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = minimal_node_config(TARGET_NODE_NAME, TARGET_NODE_TAG, &[]);
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

    assert_failure(
        add_result.failed(),
        PeppyFailureKind::AttemptFailed,
        NODE_CONFIG_FILE,
    );

    assert!(!node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 1, "root");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_git_hash_mismatch_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = minimal_node_config("git_hash_mismatch_node", "v1", &[]);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let peppy_dir = source_dir
        .path()
        .join(daemon_config::consts::PEPPY_OUTPUT_DIR);
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

    assert_failure(
        add_result.failed(),
        PeppyFailureKind::PreconditionFailed,
        "git hash mismatch",
    );
    assert!(!node_stack.contains("git_hash_mismatch_node", "v1"));
}
/// A consumer node that depends on a provider no stack holds.
const CONFIG_WITH_AN_UNRESOLVED_DEPENDENCY: &str = r#"{
    peppy_schema: "node/v1",
    manifest: {
        name: "consumer_node",
        tag: "v1",
        description: ["A test node."],
        depends_on: {
            nodes: [
                { name: "non_existent_node", tag: "v1", link_id: "non_existent_node" }
            ]
        },
    },
    execution: {
        language: "rust",
        run_cmd: ["sleep", "10"],
    },
}"#;

/// Two depends_on.nodes entries sharing the same link_id must be rejected at
/// parse time, before any dependency resolution runs.
const CONFIG_WITH_A_DUPLICATE_LINK_ID: &str = r#"{
    peppy_schema: "node/v1",
    manifest: {
        name: "dup_link_id_node",
        tag: "v1",
        description: ["A test node."],
        depends_on: {
            nodes: [
                { name: "alpha", tag: "v1", link_id: "dup" },
                { name: "beta",  tag: "v1", link_id: "dup" },
            ],
        },
    },
    execution: {
        language: "rust",
        run_cmd: ["sleep", "10"],
    },
}"#;

/// A node that consumes a topic of a dependency the stack does not hold. The
/// add must fail with a missing dependency BEFORE running build_cmd. This
/// mimics real nodes (e.g. fake_video_reconstruction) where `cargo build`
/// fails because peppygen interfaces are incomplete when dependencies are
/// missing.
const CONFIG_CONSUMING_AN_ABSENT_DEPENDENCY: &str = r#"{
    peppy_schema: "node/v1",
    manifest: {
      name: "add_cmd_node",
      tag: "v1",
      description: ["A test node."],
      depends_on: {
        nodes: [
          { name: "fake_uvc_camera", tag: "v1", link_id: "fake_uvc_camera" }
        ]
      },
    },
    interfaces: {
      topics: {
        consumes: [
          {
            link_id: "fake_uvc_camera",
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
}"#;

#[rstest::rstest]
#[case::invalid_json5(
    r#"{ manifest: [unclosed"#.to_string(),
    PeppyFailureKind::AttemptFailed,
    &["Failed to parse node config"]
)]
#[case::no_run_cmd(
    node_config_with_execution("no_run_cmd_node", "v1", r#"{ language: "rust" }"#),
    PeppyFailureKind::AttemptFailed,
    &["process"]
)]
#[case::unresolved_dependency(
    CONFIG_WITH_AN_UNRESOLVED_DEPENDENCY.to_string(),
    PeppyFailureKind::PreconditionFailed,
    &["Failed to add node", "does not exist in the stack"]
)]
#[case::duplicate_link_id(
    CONFIG_WITH_A_DUPLICATE_LINK_ID.to_string(),
    PeppyFailureKind::AttemptFailed,
    &["Duplicate link_id", "dup"]
)]
#[case::absent_dependency_of_a_consumed_topic(
    CONFIG_CONSUMING_AN_ABSENT_DEPENDENCY.to_string(),
    PeppyFailureKind::PreconditionFailed,
    &["does not exist in the stack"]
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_rejects_a_config_it_cannot_add(
    #[case] peppy_json5: String,
    #[case] expected_kind: PeppyFailureKind,
    #[case] expected_error_parts: &[&str],
) {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
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
    .expect("node_add request should complete");

    for part in expected_error_parts {
        assert_failure(add_result.failed(), expected_kind, part);
    }

    assert_eq!(node_stack.len(), 1, "only root should exist");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_fails_on_missing_interface_even_when_dependency_exists() {
    // The dependency node (fake_uvc_camera:v1) exists in the stack but emits a
    // DIFFERENT topic name than what the dependent node subscribes to. The node add should
    // fail with a MissingInterface error BEFORE running build_cmd. This mimics the real
    // scenario where `fake_uvc_camera` is added first, but `fake_video_reconstruction`
    // fails because the interface names don't match.
    const DEPENDENCY_NODE_NAME: &str = "fake_uvc_camera";
    const DEPENDENCY_NODE_TAG: &str = "v1";
    const TARGET_NODE_NAME: &str = "fake_video_reconstruction";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Step 1: Add the dependency node that emits a topic with a DIFFERENT name
    // than what the dependent node will subscribe to.
    let dep_source_dir = tempfile::tempdir().expect("failed to create temp dep source dir");
    let dep_peppy_json5 = minimal_node_config(DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG, &[]);
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

    dep_result.completed();
    assert!(
        node_stack.contains(DEPENDENCY_NODE_NAME, DEPENDENCY_NODE_TAG),
        "dependency node should be in the stack"
    );
    assert_eq!(node_stack.len(), 2, "root + dependency");

    // Step 2: Add the dependent node that subscribes to a topic name that the
    // dependency does NOT emit (node name+tag matches, but interface doesn't).
    let target_source_dir = tempfile::tempdir().expect("failed to create temp target source dir");

    let target_peppy_json5 = r#"{
        peppy_schema: "node/v1",
        manifest: {
          name: "TARGET_NODE_NAME",
          tag: "TARGET_NODE_TAG",
          description: ["A test node."],
          depends_on: {
            nodes: [
              { name: "DEPENDENCY_NODE_NAME", tag: "DEPENDENCY_NODE_TAG", link_id: "DEPENDENCY_NODE_NAME" }
            ]
          },
        },
        interfaces: {
          topics: {
            consumes: [
              {
                link_id: "DEPENDENCY_NODE_NAME",
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

    assert_failure(
        add_result.failed(),
        PeppyFailureKind::PreconditionFailed,
        "is not exposed",
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
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Create directories that should be excluded from the copy
    for dir_name in [".venv", "target", "node_modules", "__pycache__"] {
        std::fs::create_dir(source_dir.path().join(dir_name))
            .expect("failed to create excluded dir");
    }

    let peppy_json5 = minimal_node_config(TARGET_NODE_NAME, TARGET_NODE_TAG, &[]);
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

    add_result.completed();

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

    assert_failure(
        add_result.failed(),
        PeppyFailureKind::AttemptFailed,
        "Failed to parse node config",
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

    let failure = add_result.failed();
    assert_eq!(
        failure.kind().as_str(),
        PeppyFailureKind::AttemptFailed.name(),
        "the failure has another kind: {failure}"
    );
    // The shallow probe reports "not found in repository"; if the probe falls
    // back (e.g. local transport doesn't support shallow fetch), the full clone
    // path reports a filesystem read error instead.
    let err = failure.message();
    assert!(
        err.contains("not found in repository") || err.contains("Failed to parse node config"),
        "error should mention config not found or parse failure, got: {err}"
    );
}

/// A git source whose URL holds credentials, on a loopback port that refuses
/// the connection: the add fails, and the feedback, the error and the add log
/// name the repository without the credentials.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_failure_shows_the_url_without_its_credentials() {
    const SHOWN_URL: &str = "https://127.0.0.1:1/org/repo.git";

    let (started_core_node, mut exported) = common::start_core_node_with_log_export(None).await;
    let repo_url = GitUrl::try_from("https://user:secret@127.0.0.1:1/org/repo.git")
        .expect("the URL should parse");

    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let goal = NodeAddGoal::new_git(
        repo_url,
        "nodes/any_node",
        None,
        TEST_GIT_HASH,
        RESULT_TIMEOUT.as_secs(),
    );
    let add = send_logged_node_add(&started_core_node, &goal, &feedback_tx).await;

    let failure = add.end.failed();
    assert_eq!(
        failure.kind().as_str(),
        PeppyFailureKind::AttemptFailed.name(),
        "the clone cannot succeed: {failure}"
    );
    let error = failure.message().to_owned();
    let feedback: Vec<String> = std::iter::from_fn(|| feedback_rx.try_recv().ok())
        .map(|entry| entry.line)
        .collect();
    let log = std::fs::read_to_string(&add.log_path).expect("the add log should exist");

    assert!(
        error.contains(SHOWN_URL),
        "the error should name the repository, got: {error}"
    );
    assert!(
        feedback.iter().any(|line| line.contains(SHOWN_URL)),
        "the feedback should name the repository, got: {feedback:?}"
    );
    assert!(
        log.contains(SHOWN_URL),
        "the add log should name the repository, got: {log}"
    );
    let exported: Vec<String> = log_export::test_support::drain_records(&mut exported)
        .into_iter()
        .map(|record| record.body)
        .collect();
    assert!(
        exported.iter().any(|body| body.contains(SHOWN_URL)),
        "the exported records should name the repository, got: {exported:?}"
    );
    let feedback = feedback.join("\n");
    let exported = exported.join("\n");
    for (what, text) in [
        ("error", error.as_str()),
        ("feedback", feedback.as_str()),
        ("add log", log.as_str()),
        ("exported records", exported.as_str()),
    ] {
        assert!(
            !text.contains("secret") && !text.contains("user@"),
            "the {what} should hold no credentials, got: {text}"
        );
    }
}
