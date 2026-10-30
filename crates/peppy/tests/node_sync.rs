use super::common::{assert_daemon_failure, serve_app_context};
use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::path::{Path, PathBuf};
use std::time::Duration;

use config::failure_kind::PeppyFailureKind;
use config::node::{
    EmittedTopic, MessageFormat, NodeConfigParser, QoSProfile, Toolchain, TopicRetention,
};
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeInitBuilder, NodeName};

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
        description: config::description::Description::new("A test topic.").unwrap(),
        qos_profile: QoSProfile::Standard,
        retention: TopicRetention::LiveOnly,
        message_format: Some(message_format),
    }));

    // Write JSON (valid JSON5) back to disk.
    let updated_content = serde_json::to_string_pretty(&cfg).expect("peppy.json5 should serialize");
    std::fs::write(peppy_json5, updated_content).expect("peppy.json5 should update");
}

/// The codegen fingerprint recorded for the node whose manifest is
/// `peppy_json5`.
fn recorded_fingerprint(peppy_json5: &Path) -> String {
    config::fingerprint::read_codegen_fingerprint(peppy_json5, config::consts::PEPPYGEN_OUTPUT_PATH)
        .expect("fingerprint should be readable")
}

/// Changes the node's interfaces, as a developer does before a re-sync, and
/// returns the fingerprint a sync must record for the changed manifest, which
/// differs from the recorded one.
fn change_interfaces(peppy_json5: &Path) -> String {
    let old_fingerprint = recorded_fingerprint(peppy_json5);
    assert!(
        !old_fingerprint.is_empty(),
        "fingerprint should not be empty"
    );

    add_emitted_topic(peppy_json5);

    let expected_fingerprint =
        config::runtime::RuntimeConfig::generate_peppy_config_fingerprint(peppy_json5)
            .expect("peppy.json5 fingerprint should generate");
    assert_ne!(
        expected_fingerprint, old_fingerprint,
        "fingerprint should change after modifying peppy.json5"
    );
    expected_fingerprint
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_rust_command_succeeds() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    assert!(
        !serve.core_node_name().is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_sync_node";

    // Create AppContext pointing to the temp directory
    let node_ctx = serve_app_context(&serve, node_dir.path());

    // Set up logging
    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    // Create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            description: config::description::Description::new("A test node.").unwrap(),
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

    let expected_fingerprint = change_interfaces(&peppy_json5_path);

    // Run sync from inside the node directory (ctx.root_dir must contain peppy.json5)
    let sync_ctx = serve_app_context(&serve, &node_path);
    NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            include_repositories: false,
        },
    }
    .execute(&sync_ctx)
    .expect("node sync command should succeed");

    assert_eq!(
        recorded_fingerprint(&peppy_json5_path),
        expected_fingerprint,
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

    // The test surfaces: fixtures (harness + emitted observation) always
    // generate, the crate gates them behind the `testing` feature, and the
    // node's manifest gains the dev-dependency that enables it.
    let peppygen_dir = node_path.join(config::consts::PEPPYGEN_OUTPUT_PATH);
    let harness_path = peppygen_dir.join("src/fixtures/harness.rs");
    assert!(
        harness_path.exists(),
        "the fixtures harness should be generated at {}",
        harness_path.display()
    );
    assert!(
        peppygen_dir
            .join("src/fixtures/emitted_topics/goodbye_world.rs")
            .exists(),
        "an emitted-topic observation client should be generated"
    );
    assert!(
        peppygen_dir.join("src/mock.rs").exists(),
        "the mock category module should be generated (empty for a dep-less node)"
    );
    let generated_lib = std::fs::read_to_string(peppygen_dir.join("src/lib.rs"))
        .expect("generated lib.rs should be readable");
    assert!(
        generated_lib.contains("#[cfg(feature = \"testing\")]"),
        "generated lib.rs should gate the test modules behind the testing feature:\n{generated_lib}"
    );
    let generated_manifest = std::fs::read_to_string(peppygen_dir.join("Cargo.toml"))
        .expect("generated Cargo.toml should be readable");
    assert!(
        generated_manifest.contains("testing = [\"peppylib/testing\"]"),
        "generated Cargo.toml should declare the testing feature:\n{generated_manifest}"
    );
    let node_manifest = std::fs::read_to_string(node_path.join("Cargo.toml"))
        .expect("node Cargo.toml should be readable");
    assert!(
        node_manifest.contains("[dev-dependencies]")
            && node_manifest.contains(r#"features = ["testing"]"#),
        "sync should write the testing-featured peppygen dev-dependency:\n{node_manifest}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_python_command_succeeds() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    assert!(
        !serve.core_node_name().is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_sync_python_node";

    // Create AppContext pointing to the temp directory
    let node_ctx = serve_app_context(&serve, node_dir.path());

    // Set up logging
    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    // Create a Python node using NodeInitBuilder with no timeout to avoid CI flakiness
    NodeInitBuilder::new(
        &node_ctx,
        NodeName::new(node_name).expect("valid node name"),
        config::description::Description::new("A test node.").unwrap(),
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

    let expected_fingerprint = change_interfaces(&peppy_json5_path);

    // Run sync from inside the node directory
    let sync_ctx = serve_app_context(&serve, &node_path);
    NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            include_repositories: false,
        },
    }
    .execute(&sync_ctx)
    .expect("node sync command should succeed");

    assert_eq!(
        recorded_fingerprint(&peppy_json5_path),
        expected_fingerprint,
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

    // The test surfaces generate for Python too, inert (never imported by
    // peppygen/__init__.py) but present for the node's own test suite.
    let peppygen_pkg = node_path
        .join(config::consts::PEPPYGEN_OUTPUT_PATH)
        .join("peppygen");
    let harness_path = peppygen_pkg.join("fixtures/harness.py");
    assert!(
        harness_path.exists(),
        "the fixtures harness should be generated at {}",
        harness_path.display()
    );
    assert!(
        peppygen_pkg
            .join("fixtures/emitted_topics/goodbye_world.py")
            .exists(),
        "an emitted-topic observation client should be generated"
    );
    assert!(
        peppygen_pkg.join("mock/__init__.py").exists(),
        "the mock package should be generated (empty for a dep-less node)"
    );
    let init = std::fs::read_to_string(peppygen_pkg.join("__init__.py"))
        .expect("peppygen __init__.py should be readable");
    assert!(
        !init.contains("mock") && !init.contains("fixtures"),
        "peppygen/__init__.py must not import the test surfaces:\n{init}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_with_path_succeeds() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_sync_path_node";

    // Create AppContext pointing to the temp directory (parent of the node)
    let node_ctx = serve_app_context(&serve, node_dir.path());

    // Set up logging
    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    // Create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            description: config::description::Description::new("A test node.").unwrap(),
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

    let expected_fingerprint = change_interfaces(&peppy_json5_path);

    // Run sync from the PARENT directory, passing the node subdirectory as path argument.
    // This is the key difference: sync_ctx points to the parent, not the node directory.
    let sync_ctx = serve_app_context(&serve, node_dir.path());
    NodeCommand {
        command: NodeCommands::Sync {
            path: Some(PathBuf::from(node_name)),
            include_repositories: false,
        },
    }
    .execute(&sync_ctx)
    .expect("node sync with path should succeed");

    assert_eq!(
        recorded_fingerprint(&peppy_json5_path),
        expected_fingerprint,
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
    // Verifies the CLI accepts `peppy node sync -r` (include repositories)
    // and that the verbose two-section output reaches the captured logs when
    // a dep is repository-resolved.
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");

    let log_capture = LogCapture::new();
    let _guard = log_capture.install();

    // Camera node: written into a temp dir and registered as an `fs`
    // entry in the daemon's node cache.
    let camera_dir = tempfile::tempdir().expect("camera tempdir");
    std::fs::write(
        camera_dir.path().join("peppy.json5"),
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "uvc_camera", tag: "v1", description: ["A test node."] },
            interfaces: {
                topics: {
                    emits: [
                        {
                            name: "video_stream",
                            description: "A test topic.",
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

    // Seed the daemon's node cache so the repo tier can find it.
    super::common::register_repo_caches(
        serve.temp_dir(),
        &[("uvc_camera", "v1", camera_dir.path())],
    );

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
                description: ["A test node."],
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

    let sync_ctx = serve_app_context(&serve, brain_dir.path());
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

/// The arm side of `arm_link/v1`: one pairing slot, link_id `controller`, with
/// the topics the role emits and consumes.
const ROBOT_ARM_ON_ARM_LINK: &str = r#"{
    peppy_schema: "node/v1",
    manifest: {
        name: "robot_arm",
        tag: "v1",
        description: ["A test node."],
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
}"#;

/// A node declaring `depends_on.pairings` syncs into `paired_topics/<link_id>/<topic>`
/// modules once the pairing doc is in the daemon's pairing cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_generates_peer_modules_for_pairing_slots() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");

    let work_dir = tempfile::tempdir().expect("temp work dir");
    let ctx = serve_app_context(&serve, work_dir.path());
    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    super::common::seed_pairing_repo(&serve, &ctx, repo_dir.path());

    let node_dir = tempfile::tempdir().expect("node dir");
    std::fs::write(node_dir.path().join("peppy.json5"), ROBOT_ARM_ON_ARM_LINK)
        .expect("write node config");

    let sync_ctx = serve_app_context(&serve, node_dir.path());
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

    let node_dir = tempfile::tempdir().expect("node dir");
    std::fs::write(node_dir.path().join("peppy.json5"), ROBOT_ARM_ON_ARM_LINK)
        .expect("write node config");

    let sync_ctx = serve_app_context(&serve, node_dir.path());
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
    assert_daemon_failure(&err, PeppyFailureKind::AttemptFailed, "arm_link");
}
