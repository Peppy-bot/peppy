use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use config::node::{EmittedTopic, MessageFormat, NodeConfigParser, QoSProfile, Toolchain};
use core_node::nodes_repo_cache_path;
use daemon_config::consts::PeppyDirs;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeInitBuilder, NodeName};
use peppy::context::AppContext;

fn add_emitted_topic(peppy_json5: &Path) {
    let mut cfg = NodeConfigParser::from_path(peppy_json5).expect("peppy.json5 should read");

    let topic_ifaces = cfg.interfaces.topics.get_or_insert_with(Default::default);
    let topics = topic_ifaces.emits.get_or_insert_with(Vec::new);
    let message_format: MessageFormat = serde_json::from_value(serde_json::json!({
        "timestamp": "time",
        "message": "string",
    }))
    .expect("message format should deserialize");
    topics.push(EmittedTopic::Native(config::node::NativeEmittedTopic {
        name: "goodbye_world".to_string(),
        qos_profile: QoSProfile::Standard,
        message_format: Some(message_format),
    }));

    // Write JSON (valid JSON5) back to disk.
    let updated_content = serde_json::to_string_pretty(&cfg).expect("peppy.json5 should serialize");
    std::fs::write(peppy_json5, updated_content).expect("peppy.json5 should update");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_rust_command_succeeds() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    assert!(
        !serve.core_node_name().is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_sync_node";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Set up logging
    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    let old_fingerprint = config::fingerprint::read_codegen_fingerprint(
        &peppy_json5_path,
        config::consts::PEPPYGEN_OUTPUT_PATH,
    )
    .expect("fingerprint should be readable");
    assert!(
        !old_fingerprint.is_empty(),
        "fingerprint should not be empty"
    );

    // Change peppy.json5 to force a new fingerprint.
    // This simulates a developer changing their node interface definitions and needing to re-sync.
    add_emitted_topic(&peppy_json5_path);

    let expected_fingerprint =
        config::runtime::RuntimeConfig::generate_peppy_config_fingerprint(&peppy_json5_path)
            .expect("peppy.json5 fingerprint should generate");
    assert_ne!(
        expected_fingerprint, old_fingerprint,
        "fingerprint should change after modifying peppy.json5"
    );

    // Run sync from inside the node directory (ctx.root_dir must contain peppy.json5)
    let sync_ctx = Arc::new(
        AppContext::with_messenger(&node_path, Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            include_repositories: false,
        },
    }
    .execute(&sync_ctx)
    .expect("node sync command should succeed");

    let new_fingerprint = config::fingerprint::read_codegen_fingerprint(
        &peppy_json5_path,
        config::consts::PEPPYGEN_OUTPUT_PATH,
    )
    .expect("updated fingerprint should be readable");
    assert_eq!(
        new_fingerprint, expected_fingerprint,
        "fingerprint file should be updated by sync"
    );

    let logs = log_capture.logs();
    assert!(
        logs.contains("Synced node interfaces at"),
        "logs should contain sync success message. Logs:\n{}",
        logs
    );

    // Verify that the `goodbye_world` topic code was generated
    let goodbye_world_topic_path = node_path
        .join(config::consts::PEPPYGEN_OUTPUT_PATH)
        .join("src/emitted_topics/goodbye_world.rs");
    assert!(
        goodbye_world_topic_path.exists(),
        "goodbye_world topic should be generated at {}",
        goodbye_world_topic_path.display()
    );

    // Verify the generated file has expected content
    let goodbye_world_contents = std::fs::read_to_string(&goodbye_world_topic_path)
        .expect("goodbye_world.rs should be readable");
    assert!(
        goodbye_world_contents.contains("goodbye_world"),
        "goodbye_world.rs should contain topic name"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_python_command_succeeds() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    assert!(
        !serve.core_node_name().is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_sync_python_node";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Set up logging
    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Create a Python node using NodeInitBuilder with no timeout to avoid CI flakiness
    NodeInitBuilder::new(
        &node_ctx,
        NodeName::new(node_name).expect("valid node name"),
        Toolchain::Uv,
        false,
    )
    .with_timeout(None::<Duration>)
    .build()
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    let old_fingerprint = config::fingerprint::read_codegen_fingerprint(
        &peppy_json5_path,
        config::consts::PEPPYGEN_OUTPUT_PATH,
    )
    .expect("fingerprint should be readable");
    assert!(
        !old_fingerprint.is_empty(),
        "fingerprint should not be empty"
    );

    // Change peppy.json5 to force a new fingerprint.
    add_emitted_topic(&peppy_json5_path);

    let expected_fingerprint =
        config::runtime::RuntimeConfig::generate_peppy_config_fingerprint(&peppy_json5_path)
            .expect("peppy.json5 fingerprint should generate");
    assert_ne!(
        expected_fingerprint, old_fingerprint,
        "fingerprint should change after modifying peppy.json5"
    );

    // Run sync from inside the node directory
    let sync_ctx = Arc::new(
        AppContext::with_messenger(&node_path, Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            include_repositories: false,
        },
    }
    .execute(&sync_ctx)
    .expect("node sync command should succeed");

    let new_fingerprint = config::fingerprint::read_codegen_fingerprint(
        &peppy_json5_path,
        config::consts::PEPPYGEN_OUTPUT_PATH,
    )
    .expect("updated fingerprint should be readable");
    assert_eq!(
        new_fingerprint, expected_fingerprint,
        "fingerprint file should be updated by sync"
    );

    let logs = log_capture.logs();
    assert!(
        logs.contains("Synced node interfaces at"),
        "logs should contain sync success message. Logs:\n{}",
        logs
    );

    // Verify that the `goodbye_world` topic Python code was generated
    let goodbye_world_topic_path = node_path
        .join(config::consts::PEPPYGEN_OUTPUT_PATH)
        .join("peppygen/emitted_topics/goodbye_world.py");
    assert!(
        goodbye_world_topic_path.exists(),
        "goodbye_world topic should be generated at {}",
        goodbye_world_topic_path.display()
    );

    // Verify the generated file has expected content
    let goodbye_world_contents = std::fs::read_to_string(&goodbye_world_topic_path)
        .expect("goodbye_world.py should be readable");
    assert!(
        goodbye_world_contents.contains("goodbye_world"),
        "goodbye_world.py should contain topic name"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_with_path_succeeds() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_sync_path_node";

    // Create AppContext pointing to the temp directory (parent of the node)
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Set up logging
    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    let old_fingerprint = config::fingerprint::read_codegen_fingerprint(
        &peppy_json5_path,
        config::consts::PEPPYGEN_OUTPUT_PATH,
    )
    .expect("fingerprint should be readable");

    // Change peppy.json5 to force a new fingerprint.
    add_emitted_topic(&peppy_json5_path);

    let expected_fingerprint =
        config::runtime::RuntimeConfig::generate_peppy_config_fingerprint(&peppy_json5_path)
            .expect("peppy.json5 fingerprint should generate");
    assert_ne!(
        expected_fingerprint, old_fingerprint,
        "fingerprint should change after modifying peppy.json5"
    );

    // Run sync from the PARENT directory, passing the node subdirectory as path argument.
    // This is the key difference: sync_ctx points to the parent, not the node directory.
    let sync_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    NodeCommand {
        command: NodeCommands::Sync {
            path: Some(PathBuf::from(node_name)),
            include_repositories: false,
        },
    }
    .execute(&sync_ctx)
    .expect("node sync with path should succeed");

    let new_fingerprint = config::fingerprint::read_codegen_fingerprint(
        &peppy_json5_path,
        config::consts::PEPPYGEN_OUTPUT_PATH,
    )
    .expect("updated fingerprint should be readable");
    assert_eq!(
        new_fingerprint, expected_fingerprint,
        "fingerprint file should be updated by sync with path"
    );

    let logs = log_capture.logs();
    assert!(
        logs.contains("Synced node interfaces at"),
        "logs should contain sync success message. Logs:\n{}",
        logs
    );

    // Verify that the `goodbye_world` topic code was generated
    let goodbye_world_topic_path = node_path
        .join(config::consts::PEPPYGEN_OUTPUT_PATH)
        .join("src/emitted_topics/goodbye_world.rs");
    assert!(
        goodbye_world_topic_path.exists(),
        "goodbye_world topic should be generated at {}",
        goodbye_world_topic_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_with_include_repositories_prints_provenance() {
    // Verifies the CLI accepts `peppy node sync -r` (the new include-
    // repositories flag) and that the verbose two-section output reaches
    // the captured logs when a dep is repository-resolved.
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Camera node: written into a temp dir and registered as an `fs`
    // entry in the daemon's packages cache.
    let camera_dir = tempfile::tempdir().expect("camera tempdir");
    std::fs::write(
        camera_dir.path().join("peppy.json5"),
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "uvc_camera", tag: "v1" },
            interfaces: {
                topics: {
                    emits: [
                        {
                            name: "video_stream",
                            qos_profile: "sensor_data",
                            message_format: { encoding: "string" }
                        }
                    ],
                    consumes: [],
                },
                services: { exposes: [] },
                actions: { exposes: [] },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] }
        }"#,
    )
    .expect("write camera config");

    // Seed the daemon's packages cache so the repo tier can find it.
    let peppy_dirs = PeppyDirs::new(serve.temp_dir());
    std::fs::create_dir_all(peppy_dirs.cache_dir()).expect("create cache dir");
    let packages_json = serde_json::json!([{
        "node_name": "uvc_camera",
        "node_tag": "v1",
        "source_type": "fs",
        // `path` now points at the manifest file itself; the daemon's
        // materialize step derives the directory via `.parent()`.
        "path": camera_dir.path().join("peppy.json5").to_string_lossy(),
    }]);
    std::fs::write(
        nodes_repo_cache_path(&peppy_dirs),
        serde_json::to_string_pretty(&packages_json).unwrap(),
    )
    .expect("write nodes.json5");

    // Brain node consumes the camera's topic. Lives in its own temp dir
    // so we can run `node sync` against it directly.
    let brain_dir = tempfile::tempdir().expect("brain tempdir");
    std::fs::write(
        brain_dir.path().join("peppy.json5"),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "my_robot_brain",
                tag: "v1",
                depends_on: { nodes: [{ name: "uvc_camera", tag: "v1", link_id: "uvc_camera" }] }
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [{ link_id: "uvc_camera", name: "video_stream" }],
                },
                services: { exposes: [] },
                actions: { exposes: [] },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] }
        }"#,
    )
    .expect("write brain config");

    let sync_ctx = Arc::new(
        AppContext::with_messenger(brain_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            include_repositories: true,
        },
    }
    .execute(&sync_ctx)
    .expect("node sync -r should succeed");

    let logs = log_capture.logs();
    assert!(
        logs.contains("Synchronized from repositories:"),
        "verbose output should announce the repo section. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains("uvc_camera:v1 (fs)"),
        "verbose output should list the repo-resolved dep with its source kind. Logs:\n{}",
        logs
    );
}

/// A node declaring `depends_on.pairings` syncs into `paired_topics/<link_id>/<topic>`
/// modules once the pairing doc is in the daemon's pairing cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_generates_peer_modules_for_pairing_slots() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();

    let work_dir = tempfile::tempdir().expect("temp work dir");
    let ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    super::common::seed_pairing_repo(&serve, &ctx, repo_dir.path());

    // The arm side of arm_link/v1: one slot, link_id `controller`.
    let node_dir = tempfile::tempdir().expect("node dir");
    std::fs::write(
        node_dir.path().join("peppy.json5"),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "robot_arm",
                tag: "v1",
                depends_on: {
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }
                    ]
                }
            },
            interfaces: {
                topics: {
                    emits: [{ link_id: "controller", name: "joint_states" }],
                    consumes: [{ link_id: "controller", name: "joint_commands" }]
                }
            },
            execution: { language: "rust", run_cmd: ["sleep", "1"] }
        }"#,
    )
    .expect("write node config");

    let sync_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            include_repositories: false,
        },
    }
    .execute(&sync_ctx)
    .expect("node sync with a cached pairing doc should succeed");

    // Both directions of the slot: the arm emits joint_states and consumes
    // joint_commands, all under paired_topics/<link_id>/.
    let paired_topics_dir = node_dir
        .path()
        .join(config::consts::PEPPYGEN_OUTPUT_PATH)
        .join("src/paired_topics/controller");
    for module in ["joint_states.rs", "joint_commands.rs"] {
        assert!(
            paired_topics_dir.join(module).exists(),
            "expected generated module at {}",
            paired_topics_dir.join(module).display()
        );
    }
}

/// Without `repo refresh`, syncing a node with a pairing slot fails loudly
/// and points at the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_pairing_cache_miss_suggests_repo_refresh() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();

    let node_dir = tempfile::tempdir().expect("node dir");
    std::fs::write(
        node_dir.path().join("peppy.json5"),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "robot_arm",
                tag: "v1",
                depends_on: {
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }
                    ]
                }
            },
            interfaces: {
                topics: {
                    emits: [{ link_id: "controller", name: "joint_states" }],
                    consumes: [{ link_id: "controller", name: "joint_commands" }]
                }
            },
            execution: { language: "rust", run_cmd: ["sleep", "1"] }
        }"#,
    )
    .expect("write node config");

    let sync_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    let err = NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            include_repositories: false,
        },
    }
    .execute(&sync_ctx)
    .expect_err("sync must fail when the pairing doc is not cached");
    let msg = err.to_string();
    assert!(
        msg.contains("arm_link") && msg.contains("repo refresh"),
        "cache-miss error should name the pairing and suggest `peppy repo refresh`: {msg}"
    );
}
