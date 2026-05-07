use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_fs_add_success() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

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
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    // `add` only adds the node to the NodeStack but doesn't spawn any instance
    assert_eq!(
        entity_instance_count(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG),
        0
    );

    // Add staged the node into the stack but did not produce an artifact;
    // artifact-related assertions live in `listen_for_node_build.rs`.
    let _ = add_result.log_path;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_success() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = test_helpers::create_nodes_git_repo(&git_repo_temp_dir);

    const TARGET_NODE_NAME: &str = "uvc_camera";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_REPO_PATH: &str = "nodes/uvc_camera";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: TARGET_REPO_PATH,
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    assert_eq!(
        entity_instance_count(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG),
        0
    );

    // Artifact assertions live in `listen_for_node_build.rs`.
    let _ = add_result.log_path;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_with_ref_success() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = create_versioned_nodes_git_repo(&git_repo_temp_dir);

    const TARGET_NODE_NAME: &str = "uvc_camera";
    const TARGET_REPO_PATH: &str = "nodes/uvc_camera";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let add_result_head = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url: repo_url.clone(),
            repo_path: TARGET_REPO_PATH,
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result_head.success,
        "node_add should succeed, got error: {:?}",
        add_result_head.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, "0.2.0"));

    let add_result_ref = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: TARGET_REPO_PATH,
            repo_ref: Some("v0.1.0"),
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result_ref.success,
        "node_add should succeed, got error: {:?}",
        add_result_ref.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, "0.1.0"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_http_add_success() {
    const TARGET_NODE_NAME: &str = "http_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
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
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);

    let manifest_path = bundle_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&manifest_path, &peppy_json5).expect("failed to write manifest");

    let test_file_content = "hello from http";
    let test_file_path = bundle_dir.path().join("test_file.txt");
    std::fs::write(&test_file_path, test_file_content).expect("failed to write test file");

    let mut tar_data = Vec::new();
    {
        let mut tar_builder = tar::Builder::new(&mut tar_data);
        tar_builder
            .append_path_with_name(&manifest_path, NODE_CONFIG_FILE)
            .expect("failed to append manifest to tar");
        tar_builder
            .append_path_with_name(&test_file_path, "test_file.txt")
            .expect("failed to append test file to tar");
        tar_builder.finish().expect("failed to finish tar");
    }

    let bundle_path = bundle_dir.path().join("http_node.tar.zst");
    let bundle_file = std::fs::File::create(&bundle_path).expect("failed to create bundle file");
    let mut encoder = zstd::Encoder::new(bundle_file, 0).expect("failed to create zstd encoder");
    encoder
        .write_all(&tar_data)
        .expect("failed to write compressed bundle");
    encoder.finish().expect("failed to finish encoder");
    let bundle_bytes = std::fs::read(&bundle_path).expect("failed to read bundle");

    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/http_node.tar.zst"))
            .respond_with(status_code(200).body(bundle_bytes)),
    );
    let url = url::Url::parse(&server.url("/bundles/http_node.tar.zst").to_string())
        .expect("http bundle url should parse");

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Http { url, sha256: None },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    assert_eq!(
        entity_instance_count(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG),
        0
    );

    // Artifact assertions live in `listen_for_node_build.rs`.
    let _ = (add_result.log_path, test_file_content);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_http_add_rejects_wrong_sha256() {
    const TARGET_NODE_NAME: &str = "http_sha_bad";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let (_bundle_dir, bundle_bytes) = create_minimal_http_bundle(TARGET_NODE_NAME, TARGET_NODE_TAG);
    let (_server, url) = serve_bundle_over_http(bundle_bytes);

    let wrong_sha256 = "a".repeat(64);
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Http {
            url,
            sha256: Some(wrong_sha256),
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should fail with wrong sha256"
    );
    assert!(
        add_result
            .error_message
            .as_deref()
            .map(|msg| msg.contains("checksum mismatch"))
            .unwrap_or(false),
        "error should mention checksum mismatch, got: {:?}",
        add_result.error_message
    );
    assert!(!node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_http_add_accepts_correct_sha256() {
    const TARGET_NODE_NAME: &str = "http_sha_ok";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let (_bundle_dir, bundle_bytes) = create_minimal_http_bundle(TARGET_NODE_NAME, TARGET_NODE_TAG);

    use sha2::{Digest, Sha256};
    let correct_sha256: String = Sha256::digest(&bundle_bytes)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();

    let (_server, url) = serve_bundle_over_http(bundle_bytes);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Http {
            url,
            sha256: Some(correct_sha256),
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed with correct sha256, got error: {:?}",
        add_result.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
}
/// Adding a node from a git source whose config has a default variant with a
/// local deployment source must succeed — fingerprint verification must not
/// be triggered because git-cloned sources never have fingerprint files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_with_default_local_variant_success() {
    const ROOT_NODE_NAME: &str = "git_default_variant_robot";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Build a local git repo containing a node with a default local variant.
    let git_repo_temp = TempDir::new().expect("failed to create git repo temp dir");
    let git_repo_path = git_repo_temp.path().join("variant_repo.git");
    std::fs::create_dir_all(&git_repo_path).expect("create git repo dir");

    let repo = Repository::init(&git_repo_path).expect("init git repo");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("create signature");

    // Root config: declares a default variant with a local deployment source.
    let root_config_rel = Path::new("nodes/robot/peppy.json5");
    let variant_config_rel = Path::new("nodes/robot/variants/default/peppy.json5");

    let root_dir = git_repo_path.join("nodes/robot");
    let variant_dir = git_repo_path.join("nodes/robot/variants/default");
    std::fs::create_dir_all(&root_dir).expect("create root node dir");
    std::fs::create_dir_all(&variant_dir).expect("create variant dir");

    let root_config = format!(
        r#"{{
        peppy_schema: "node_v1",
        manifest: {{
            name: "{ROOT_NODE_NAME}",
            tag: "{ROOT_NODE_TAG}",
            variants: [
                {{ name: "default", source: {{ local: "./variants/default" }} }}
            ]
        }},
        interfaces: {{
            topics: {{
                emits: [
                    {{ name: "joint_positions", qos_profile: "sensor_data", message_format: {{ x: "f64", y: "f64" }} }}
                ]
            }}
        }}
    }}"#
    );
    std::fs::write(git_repo_path.join(root_config_rel), &root_config).expect("write root config");

    let variant_config = r#"{
        peppy_schema: "node_v1",
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    std::fs::write(git_repo_path.join(variant_config_rel), variant_config)
        .expect("write variant config");

    let mut index = repo.index().expect("open index");
    index.add_path(root_config_rel).expect("add root config");
    index
        .add_path(variant_config_rel)
        .expect("add variant config");
    index.write().expect("write index");

    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "initial commit",
        &tree,
        &[],
    )
    .expect("commit");

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    // No explicit variant — the "default" variant is auto-resolved.
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: "nodes/robot",
            repo_ref: None,
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "git add with default local variant should succeed (no fingerprint verification): {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.run_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the default variant"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_git_add_emits_clone_feedback() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = test_helpers::create_nodes_git_repo(&git_repo_temp_dir);

    const TARGET_REPO_PATH: &str = "nodes/uvc_camera";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Git {
            repo_url,
            repo_path: TARGET_REPO_PATH,
            repo_ref: None,
        },
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

    let has_config_check = feedback
        .iter()
        .any(|f| f.is_stdout() && f.line.contains("Checking node config"));
    assert!(
        has_config_check,
        "feedback should include 'Checking node config' message, got: {:?}",
        feedback.iter().map(|f| &f.line).collect::<Vec<_>>()
    );

    let has_clone_feedback = feedback
        .iter()
        .any(|f| f.is_stdout() && f.line.contains("Cloning repository"));
    assert!(
        has_clone_feedback,
        "feedback should include 'Cloning repository' message, got: {:?}",
        feedback.iter().map(|f| &f.line).collect::<Vec<_>>()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_http_add_emits_download_feedback() {
    const TARGET_NODE_NAME: &str = "http_dl_feedback_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let (_bundle_dir, bundle_bytes) = create_minimal_http_bundle(TARGET_NODE_NAME, TARGET_NODE_TAG);
    let (server, url) = serve_bundle_over_http(bundle_bytes);

    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        NodeAddSource::Http {
            url: url.clone(),
            sha256: None,
        },
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

    let has_download_feedback = feedback
        .iter()
        .any(|f| f.is_stdout() && f.line.contains("Downloading bundle from"));
    assert!(
        has_download_feedback,
        "feedback should include 'Downloading bundle from' message, got: {:?}",
        feedback.iter().map(|f| &f.line).collect::<Vec<_>>()
    );

    drop(server);
}
