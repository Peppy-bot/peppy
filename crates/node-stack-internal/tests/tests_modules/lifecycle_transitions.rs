//! Tests focused on the `NodeStage` lifecycle transitions managed by
//! `NodeEntity`.

use parking_lot::Mutex as StdMutex;
use std::path::PathBuf;
use std::sync::Arc;

use config::node::Name;
use node_stack::{
    BuildContext, EntitySnapshot, InstanceState, NodeEntity, NodeStack, NodeStackError, NodeStage,
    RestoreTarget,
};

use crate::helpers::config_common::core_node_config;
use crate::helpers::real_lifecycle;

fn sensor_config() -> config::node::NodeConfig {
    serde_json5::from_str::<config::node::NodeConfig>(
        r#"{
            schema_version: 1,
            manifest: {
                name: "sensor",
                tag: "1.0.0",
            },
            interfaces: {
                topics: {
                    emits: [
                        { name: "data_stream", qos_profile: "sensor_data" }
                    ]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sensor"]
            }
        }"#,
    )
    .expect("valid sensor config")
}

#[test]
fn push_config_creates_entity_in_added_stage() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor/peppy.json5");

    stack
        .push_config(sensor_config(), false, &config_path)
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    let guard = handle.read();

    if let NodeStage::Added { config_path: cp } = guard.stage() {
        assert_eq!(cp, &config_path);
    } else {
        panic!("expected Added stage, got {:?}", guard.stage());
    }
    assert_eq!(guard.config_path(), config_path.as_path());
    assert!(guard.artifact_path().is_none());
    assert!(guard.instances().is_empty());
}

#[tokio::test]
async fn ready_with_running_instance_round_trip() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let config_path = harness.peppy_root.path().join("sensor.json5");

    let handle = real_lifecycle::build_ready(&stack, &harness, sensor_config(), &config_path).await;
    let _guard = real_lifecycle::spawn_running_instance(
        Arc::clone(&handle),
        &harness,
        Name::new("inst-1").unwrap(),
    )
    .await;

    let guard = handle.read();
    match guard.stage() {
        NodeStage::Ready {
            config_path: cp,
            artifact_path: sp,
            instances,
        } => {
            assert_eq!(cp, &config_path);
            assert!(sp.is_file(), "archive should exist on disk");
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].instance_id().as_str(), "inst-1");
            assert!(instances[0].pid().is_some());
            assert_eq!(instances[0].state(), InstanceState::Running);
        }
        other => panic!("expected Ready, got {:?}", other),
    }
}

#[tokio::test]
async fn stop_instance_removes_last_instance_keeps_ready() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let config_path = harness.peppy_root.path().join("sensor.json5");

    let handle = real_lifecycle::build_ready(&stack, &harness, sensor_config(), &config_path).await;
    let instance_id = Name::new("only-inst").unwrap();
    let _running =
        real_lifecycle::spawn_running_instance(Arc::clone(&handle), &harness, instance_id.clone())
            .await;

    // Remove the only instance: entity stays in Ready with an empty list.
    let removed = handle.write().stop_instance(&instance_id);
    assert!(removed, "stop_instance should report success");

    let guard = handle.read();
    match guard.stage() {
        NodeStage::Ready {
            config_path: cp,
            artifact_path: sp,
            instances,
        } => {
            assert_eq!(cp, &config_path);
            assert!(sp.is_file(), "archive should exist on disk");
            assert!(
                instances.is_empty(),
                "instances list should be empty after removing the only instance"
            );
        }
        other => panic!("expected Ready, got {:?}", other),
    }
    assert!(guard.artifact_path().is_some());
    assert!(guard.instances().is_empty());
}

#[tokio::test]
async fn stop_instance_keeps_other_instances_when_one_removed() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let config_path = harness.peppy_root.path().join("sensor.json5");

    let handle = real_lifecycle::build_ready(&stack, &harness, sensor_config(), &config_path).await;

    let id_a = Name::new("inst-a").unwrap();
    let id_b = Name::new("inst-b").unwrap();
    let _a =
        real_lifecycle::spawn_running_instance(Arc::clone(&handle), &harness, id_a.clone()).await;
    let _b =
        real_lifecycle::spawn_running_instance(Arc::clone(&handle), &harness, id_b.clone()).await;

    let removed = handle.write().stop_instance(&id_a);
    assert!(removed);

    let guard = handle.read();
    match guard.stage() {
        NodeStage::Ready { instances, .. } => {
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].instance_id(), &id_b);
        }
        other => panic!("expected Ready with one instance, got {:?}", other),
    }
}

#[tokio::test]
async fn stop_instance_returns_false_when_instance_not_tracked() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let config_path = harness.peppy_root.path().join("sensor.json5");
    let handle = real_lifecycle::build_ready(&stack, &harness, sensor_config(), &config_path).await;
    let _only = real_lifecycle::spawn_running_instance(
        Arc::clone(&handle),
        &harness,
        Name::new("only").unwrap(),
    )
    .await;

    let removed = handle
        .write()
        .stop_instance(&Name::new("nonexistent").unwrap());
    assert!(!removed);
}

#[tokio::test]
async fn stop_instance_skips_starting_instances() {
    // stop_instance only acts on Running instances. A Starting instance is
    // an in-flight prepare_and_spawn that hasn't been committed yet — it
    // can't be stopped via the messenger because it hasn't subscribed.
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let config_path = harness.peppy_root.path().join("sensor.json5");
    let handle = real_lifecycle::build_ready(&stack, &harness, sensor_config(), &config_path).await;

    // Drive prepare_and_spawn to produce a Starting instance, but do NOT
    // commit_started — the instance stays in Starting.
    let id = Name::new("starting-inst").unwrap();
    let (mut child, _started_ctx) = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &id,
            runtime_config_json5: "{}",
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &harness.peppy_dirs,
            output_sinks: harness.output_sinks(),
        },
    )
    .await
    .expect("prepare_and_spawn should succeed");

    let removed = handle.write().stop_instance(&id);
    assert!(
        !removed,
        "stop_instance must not remove a Starting instance"
    );
    // The Starting instance is still in the list.
    {
        let guard = handle.read();
        assert_eq!(guard.instances().len(), 1);
        assert_eq!(guard.instances()[0].state(), InstanceState::Starting);
    }

    // Cleanup: kill the still-starting child.
    let _ = child.kill().await;
}

#[tokio::test]
async fn push_config_resets_existing_entity_to_added() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let config_path_v1 = harness.peppy_root.path().join("sensor_v1.json5");
    let config_path_v2 = harness.peppy_root.path().join("sensor_v2.json5");

    // Initial push + real build.
    let handle =
        real_lifecycle::build_ready(&stack, &harness, sensor_config(), &config_path_v1).await;

    assert!(handle.read().artifact_path().is_some());

    // Re-push with the same config but a different config_path. The entity
    // should be reset to Added with the new config_path; artifact_path is gone.
    stack
        .push_config(sensor_config(), false, &config_path_v2)
        .expect("second push_config should succeed");

    let handle_after = stack.find("sensor", "1.0.0").expect("entity should exist");
    let guard = handle_after.read();
    match guard.stage() {
        NodeStage::Added { config_path: cp } => {
            assert_eq!(cp, &config_path_v2);
        }
        other => panic!("expected Added after re-push, got {:?}", other),
    }
    assert!(guard.artifact_path().is_none());
    assert!(guard.instances().is_empty());
}

#[test]
fn push_config_rewires_when_dependency_keys_change_with_unchanged_interfaces() {
    // This test checks a specific bug fix: when you re-push a node config that keeps the same interfaces
    // but changes which producer it depends on, the system must actually rewire the dependencies
    // (not skip the work thinking "nothing changed").
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    let producer_a: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { name: "producer_a", tag: "1.0.0" },
            interfaces: { services: { exposes: [ { name: "reset_sensor" } ] } },
            execution: { language: "rust", run_cmd: ["producer_a"] }
        }"#,
    )
    .expect("valid producer_a config");

    let producer_b: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { name: "producer_b", tag: "1.0.0" },
            interfaces: { services: { exposes: [ { name: "reset_sensor" } ] } },
            execution: { language: "rust", run_cmd: ["producer_b"] }
        }"#,
    )
    .expect("valid producer_b config");

    let consumer_pointing_at = |producer_name: &str| -> config::node::NodeConfig {
        let local_id = format!("p_{}", producer_name);
        serde_json5::from_str(
            &r#"{
                schema_version: 1,
                manifest: {
                    name: "consumer",
                    tag: "1.0.0",
                    depends_on: {
                        nodes: [
                            { name: "PRODUCER", tag: "1.0.0", local_id: "LOCAL_ID" }
                        ]
                    },
                },
                interfaces: {
                    services: {
                        consumes: [
                            { local_node_id: "LOCAL_ID", name: "reset_sensor" }
                        ]
                    }
                },
                execution: { language: "rust", run_cmd: ["consumer"] }
            }"#
            .replace("PRODUCER", producer_name)
            .replace("LOCAL_ID", &local_id),
        )
        .expect("valid consumer config")
    };

    stack
        .push_config(producer_a, false, PathBuf::from("/tmp/producer_a.json5"))
        .expect("push producer_a");
    stack
        .push_config(producer_b, false, PathBuf::from("/tmp/producer_b.json5"))
        .expect("push producer_b");
    stack
        .push_config(
            consumer_pointing_at("producer_a"),
            false,
            PathBuf::from("/tmp/consumer_v1.json5"),
        )
        .expect("first consumer push");

    // Initially the consumer depends on producer_a.
    let deps_a: Vec<String> = stack
        .dependencies_of("consumer", "1.0.0")
        .iter()
        .map(|h| h.read().config().manifest.name.as_str().to_owned())
        .collect();
    assert_eq!(deps_a, vec!["producer_a".to_string()]);

    // Re-push the consumer with the same interfaces, but pointed at
    // producer_b. With the fix, dependency_keys drift triggers rewire.
    stack
        .push_config(
            consumer_pointing_at("producer_b"),
            false,
            PathBuf::from("/tmp/consumer_v2.json5"),
        )
        .expect("second consumer push");

    let deps_b: Vec<String> = stack
        .dependencies_of("consumer", "1.0.0")
        .iter()
        .map(|h| h.read().config().manifest.name.as_str().to_owned())
        .collect();
    assert_eq!(
        deps_b,
        vec!["producer_b".to_string()],
        "consumer must be rewired to producer_b after dependency-spec drift"
    );
}

#[tokio::test]
async fn push_config_rejects_replacement_with_live_instances() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let config_path_v1 = harness.peppy_root.path().join("sensor_v1.json5");
    let config_path_v2 = harness.peppy_root.path().join("sensor_v2.json5");

    let handle =
        real_lifecycle::build_ready(&stack, &harness, sensor_config(), &config_path_v1).await;
    let _running = real_lifecycle::spawn_running_instance(
        Arc::clone(&handle),
        &harness,
        Name::new("inst-1").unwrap(),
    )
    .await;

    let err = stack
        .push_config(sensor_config(), false, &config_path_v2)
        .expect_err("re-push should be rejected when live instances exist");
    match err {
        NodeStackError::CannotOverwriteNodeWithLiveInstances {
            node_name,
            node_tag,
        } => {
            assert_eq!(node_name, "sensor");
            assert_eq!(node_tag, "1.0.0");
        }
        other => panic!(
            "expected CannotOverwriteNodeWithLiveInstances, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn concurrent_builds_are_rejected_immediately() {
    use std::time::Duration;
    use tokio::time::timeout;

    // Process node (no container) — `NodeEntity::build` runs the pure-Rust
    // archive path, no apptainer required.
    let stack = NodeStack::new(
        crate::helpers::config_common::core_node_config(),
        None,
        PathBuf::from("/tmp"),
    );
    let config_path = PathBuf::from("/tmp/sensor/peppy.json5");
    // Use an `build_cmd` that sleeps long enough to keep the winning task in
    // `Building` while the loser observes the stage. Without this, archiving
    // a tiny working dir is fast enough that the winner can reach `Ready`
    // before the loser even gets scheduled — that race produced the
    // ambiguous "from: Ready" rejection the assertion below now refuses.
    stack
        .push_config(
            sensor_config_with_build_cmd("sleep 0.5"),
            false,
            &config_path,
        )
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");

    // Working directory with some content to archive.
    let working_dir = tempfile::tempdir().expect("tempdir working_dir");
    std::fs::write(working_dir.path().join("hello.txt"), b"hi").unwrap();

    // Isolated peppy_dirs root for this test.
    let peppy_root = tempfile::tempdir().expect("tempdir peppy_root");
    let peppy_dirs = config::consts::PeppyDirs::new(peppy_root.path().to_path_buf());

    let log_path = peppy_root.path().join("build.log");
    let log_file = Arc::new(StdMutex::new(
        std::fs::File::create(&log_path).expect("create log"),
    ));

    let (feedback_tx, _feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<node_stack::build_io::FeedbackLine>();

    // Use a barrier so both tasks reach the call to `build` as close to
    // simultaneously as possible — maximizes the race window. With explicit
    // Building stage, the loser is rejected immediately (no queueing).
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let spawn_build =
        |handle: node_stack::EntityHandle,
         working_dir: PathBuf,
         peppy_dirs: config::consts::PeppyDirs,
         feedback_tx: tokio::sync::mpsc::UnboundedSender<node_stack::build_io::FeedbackLine>,
         log_file: Arc<StdMutex<std::fs::File>>,
         barrier: Arc<tokio::sync::Barrier>| {
            tokio::spawn(async move {
                barrier.wait().await;
                NodeEntity::build(
                    &handle,
                    BuildContext {
                        working_dir: &working_dir,
                        peppy_dirs: &peppy_dirs,
                        feedback_tx: &feedback_tx,
                        log_file,
                        env_vars: &[],
                    },
                )
                .await
            })
        };

    let t1 = spawn_build(
        Arc::clone(&handle),
        working_dir.path().to_path_buf(),
        peppy_dirs.clone(),
        feedback_tx.clone(),
        Arc::clone(&log_file),
        Arc::clone(&barrier),
    );
    let t2 = spawn_build(
        Arc::clone(&handle),
        working_dir.path().to_path_buf(),
        peppy_dirs.clone(),
        feedback_tx.clone(),
        Arc::clone(&log_file),
        Arc::clone(&barrier),
    );

    // Both must complete (no deadlock) within a generous timeout.
    let (r1, r2) = timeout(Duration::from_secs(30), async {
        let r1 = t1.await.expect("task 1 panicked");
        let r2 = t2.await.expect("task 2 panicked");
        (r1, r2)
    })
    .await
    .expect("concurrent builds should not deadlock");

    // Exactly one Ok; exactly one InvalidStageTransition. The loser typically
    // observes `Building` (winner is mid-Phase 2 `sleep 0.5`), but on a slow
    // or heavily-loaded runner the loser could be starved past the 500 ms
    // window and observe `Ready` (winner already completed Phase 3). Both
    // shapes are valid rejections — the point of the test is that the loser
    // is rejected immediately, regardless of which post-Added stage it sees.
    let (ok_count, transition_err_count) = [&r1, &r2].iter().fold((0, 0), |(o, e), r| match r {
        Ok(_) => (o + 1, e),
        Err(NodeStackError::InvalidStageTransition { from, to, .. })
            if (*from == "Building" || *from == "Ready") && *to == "Ready" =>
        {
            (o, e + 1)
        }
        Err(other) => panic!("unexpected build error: {:?}", other),
    });
    assert_eq!(ok_count, 1, "exactly one build should succeed");
    assert_eq!(
        transition_err_count, 1,
        "the loser should fail immediately with InvalidStageTransition (Building→Ready or Ready→Ready)"
    );

    // Entity ended up in Ready.
    let guard = handle.read();
    assert!(matches!(guard.stage(), NodeStage::Ready { .. }));

    // The on-disk archive exists exactly once.
    let archive = peppy_dirs.built_nodes_dir().join("sensor_1.0.0.tar.zst");
    assert!(archive.is_file(), "expected archive at {:?}", archive);
}

// ===========================================================================
// Build with build_cmd: Added → Building → Built (or Building → Added on failure)
// ===========================================================================

/// Returns a sensor config whose `execution.build_cmd` runs the given shell snippet.
///
/// Builds the config programmatically (rather than format!-ing JSON) so the
/// shell snippet can contain quotes, backslashes, and braces without breaking
/// the JSON5 parser.
fn sensor_config_with_build_cmd(build_cmd_shell: &str) -> config::node::NodeConfig {
    // Embed the snippet via serde_json so any special characters are escaped
    // correctly into a JSON string literal.
    let escaped_snippet = serde_json5::to_string(&build_cmd_shell.to_string())
        .expect("snippet should be JSON-encodable");
    let json = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "sensor",
                tag: "1.0.0",
            }},
            interfaces: {{
                topics: {{
                    emits: [
                        {{ name: "data_stream", qos_profile: "sensor_data" }}
                    ]
                }}
            }},
            execution: {{
                language: "rust",
                build_cmd: ["sh", "-c", {snippet}],
                run_cmd: ["sensor"]
            }}
        }}"#,
        snippet = escaped_snippet
    );
    serde_json5::from_str::<config::node::NodeConfig>(&json).expect("valid sensor+build_cmd config")
}

/// Helper that constructs the BuildContext fields shared across tests.
struct BuildHarness {
    working_dir: tempfile::TempDir,
    peppy_root: tempfile::TempDir,
    peppy_dirs: config::consts::PeppyDirs,
    log_file: Arc<StdMutex<std::fs::File>>,
    feedback_tx: tokio::sync::mpsc::UnboundedSender<node_stack::build_io::FeedbackLine>,
    _feedback_rx: tokio::sync::mpsc::UnboundedReceiver<node_stack::build_io::FeedbackLine>,
}

fn build_harness() -> BuildHarness {
    let working_dir = tempfile::tempdir().expect("tempdir working_dir");
    let peppy_root = tempfile::tempdir().expect("tempdir peppy_root");
    let peppy_dirs = config::consts::PeppyDirs::new(peppy_root.path().to_path_buf());
    let log_path = peppy_root.path().join("build.log");
    let log_file = Arc::new(StdMutex::new(
        std::fs::File::create(&log_path).expect("create log"),
    ));
    let (feedback_tx, _feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<node_stack::build_io::FeedbackLine>();
    BuildHarness {
        working_dir,
        peppy_root,
        peppy_dirs,
        log_file,
        feedback_tx,
        _feedback_rx,
    }
}

#[tokio::test]
async fn build_runs_add_cmd_for_process_node() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor/peppy.json5");
    stack
        .push_config(
            sensor_config_with_build_cmd("echo built > marker.txt"),
            false,
            &config_path,
        )
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    let h = build_harness();

    NodeEntity::build(
        &handle,
        BuildContext {
            working_dir: h.working_dir.path(),
            peppy_dirs: &h.peppy_dirs,
            feedback_tx: &h.feedback_tx,
            log_file: Arc::clone(&h.log_file),
            env_vars: &[],
        },
    )
    .await
    .expect("build should succeed when build_cmd exits 0");

    // Entity is in Built.
    let guard = handle.read();
    assert!(matches!(guard.stage(), NodeStage::Ready { .. }));
    drop(guard);

    // The archive exists and contains the marker file produced by build_cmd.
    let archive = h.peppy_dirs.built_nodes_dir().join("sensor_1.0.0.tar.zst");
    assert!(archive.is_file(), "expected archive at {:?}", archive);

    // Decode the archive and look for the marker.
    let f = std::fs::File::open(&archive).expect("open archive");
    let dec = zstd::stream::read::Decoder::new(f).expect("zstd decoder");
    let mut tar = tar::Archive::new(dec);
    let mut found = false;
    for entry in tar.entries().expect("tar entries") {
        let entry = entry.expect("tar entry");
        let path = entry.path().expect("entry path").into_owned();
        if path.file_name() == Some(std::ffi::OsStr::new("marker.txt")) {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "marker.txt produced by build_cmd should be in archive"
    );

    // Keep harness alive (so tempdirs survive until the assertions above).
    drop(h.peppy_root);
    drop(h.working_dir);
}

// ===========================================================================
// Start: prepare_and_spawn / commit_started / abort_started
// ===========================================================================

/// Returns a sensor config that uses a long-running shell loop as run_cmd.
/// The loop traps SIGTERM so `child.kill()` cleanly terminates it.
fn long_running_sensor_config() -> config::node::NodeConfig {
    serde_json5::from_str::<config::node::NodeConfig>(
        r#"{
            schema_version: 1,
            manifest: {
                name: "sensor",
                tag: "1.0.0",
            },
            interfaces: {
                topics: {
                    emits: [
                        { name: "data_stream", qos_profile: "sensor_data" }
                    ]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sh", "-c", "trap 'exit 0' TERM; while true; do sleep 0.1; done"]
            }
        }"#,
    )
    .expect("valid long-running sensor config")
}

/// Pushes a long-running sensor into the stack and drives the real build path
/// to land it in `Ready`
async fn push_built_long_running_sensor(
    stack: &NodeStack,
    harness: &real_lifecycle::LifecycleHarness,
) -> node_stack::EntityHandle {
    let config_path = harness.peppy_root.path().join("long_running_sensor.json5");
    real_lifecycle::build_ready(stack, harness, long_running_sensor_config(), config_path).await
}

/// Sets up a start-test harness: a `real_lifecycle::LifecycleHarness` plus a
/// pre-baked instance id. The lifecycle harness provides peppy_dirs, log_file,
/// feedback_tx + drain task, publish_enabled, and output_sinks().
fn start_harness(instance_id_str: &str) -> (Name, real_lifecycle::LifecycleHarness) {
    (
        Name::new(instance_id_str).expect("valid instance id"),
        real_lifecycle::lifecycle_harness(),
    )
}

#[tokio::test]
async fn prepare_and_spawn_rejects_when_not_built() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    stack
        .push_config(sensor_config(), false, PathBuf::from("/tmp/sensor"))
        .expect("push_config should succeed");
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    let (instance_id, h) = start_harness("test-inst-1");

    let err = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &instance_id,
            runtime_config_json5: "{}",
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &h.peppy_dirs,
            output_sinks: h.output_sinks(),
        },
    )
    .await
    .expect_err("prepare_and_spawn should fail on Added entity");

    match err {
        NodeStackError::InvalidStageTransition { from, to, .. } => {
            assert_eq!(from, "Added");
            assert_eq!(to, "spawn instance");
        }
        other => panic!("expected InvalidStageTransition, got {:?}", other),
    }

    // Entity remained in Added.
    let guard = handle.read();
    assert!(matches!(guard.stage(), NodeStage::Added { .. }));
}

#[tokio::test]
async fn prepare_and_spawn_marks_instance_starting_then_commit_marks_running() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let (instance_id, h) = start_harness("test-inst-2");
    let handle = push_built_long_running_sensor(&stack, &h).await;

    let (child, started_ctx) = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &instance_id,
            runtime_config_json5: "{}",
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &h.peppy_dirs,
            output_sinks: h.output_sinks(),
        },
    )
    .await
    .expect("prepare_and_spawn should succeed on Ready entity");

    // Entity is in Ready with the new instance in Starting state.
    {
        let guard = handle.read();
        assert!(
            matches!(guard.stage(), NodeStage::Ready { .. }),
            "entity should remain in Ready during prepare_and_spawn, got {:?}",
            guard.stage()
        );
        let inst = guard
            .instances()
            .iter()
            .find(|i| i.instance_id() == &instance_id)
            .expect("just-registered instance should be present");
        assert_eq!(
            inst.state(),
            InstanceState::Starting,
            "instance should be in Starting state until commit_started runs"
        );
    }

    let pid = child.id().expect("child has pid");
    assert!(pid > 0, "spawned child should have a valid pid");

    let returned_pid = NodeEntity::commit_started(&handle, child, started_ctx, instance_id.clone())
        .await
        .expect("commit_started should succeed");
    assert_eq!(returned_pid, pid);

    // Entity is still in Ready, but the instance is now Running.
    {
        let guard = handle.read();
        match guard.stage() {
            NodeStage::Ready { instances, .. } => {
                assert_eq!(instances.len(), 1);
                assert_eq!(instances[0].instance_id(), &instance_id);
                assert_eq!(instances[0].pid(), Some(pid));
                assert_eq!(instances[0].state(), InstanceState::Running);
            }
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    // Tear down: stop the instance and kill the child process.
    handle.write().stop_instance(&instance_id);
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
}

#[tokio::test]
async fn abort_started_removes_starting_instance_and_kills_child() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let (instance_id, h) = start_harness("test-inst-3");
    let handle = push_built_long_running_sensor(&stack, &h).await;

    let (child, started_ctx) = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &instance_id,
            runtime_config_json5: "{}",
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &h.peppy_dirs,
            output_sinks: h.output_sinks(),
        },
    )
    .await
    .expect("prepare_and_spawn should succeed");

    let pid = child.id().expect("child has pid");

    let msg = NodeEntity::abort_started(
        &handle,
        child,
        started_ctx,
        "synthetic failure".to_string(),
        &instance_id,
    )
    .await;
    assert!(
        msg.contains("synthetic failure"),
        "abort message should include the original error, got: {}",
        msg
    );

    // Entity is still in Ready, but the Starting instance was removed.
    {
        let guard = handle.read();
        assert!(
            matches!(guard.stage(), NodeStage::Ready { .. }),
            "entity should remain in Ready after abort, got {:?}",
            guard.stage()
        );
        assert!(
            guard.instances().is_empty(),
            "the in-flight Starting instance should have been removed, got {:?}",
            guard.instances()
        );
    }

    // The child process is dead — kill_and_collect_error called child.kill() + child.wait().
    // Best-effort verification: the PID should no longer be a running process.
    // (Use `kill -0` which returns success only if the process exists.)
    let still_alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        !still_alive,
        "child PID {} should be dead after abort_started, but kill -0 reported alive",
        pid
    );
}

#[tokio::test]
async fn prepare_and_spawn_starts_additional_instance_alongside_existing() {
    // This is the multi-instance launch case: a node is already running and
    // the daemon launches a *second* instance of the same node. The entity
    // stays in Ready throughout; the new instance is appended to the
    // existing instances list, in Starting state, and flipped to Running by
    // commit_started.
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let (instance_id, h) = start_harness("test-inst-second");
    let handle = push_built_long_running_sensor(&stack, &h).await;

    // Pre-populate with one real Running instance via spawn_running_instance.
    let existing_id = Name::new("existing-inst").unwrap();
    let _existing_guard =
        real_lifecycle::spawn_running_instance(Arc::clone(&handle), &h, existing_id.clone()).await;

    // Now spawn a second instance via the regular start API.
    let (child, started_ctx) = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &instance_id,
            runtime_config_json5: "{}",
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &h.peppy_dirs,
            output_sinks: h.output_sinks(),
        },
    )
    .await
    .expect("prepare_and_spawn should succeed on Ready entity with existing instances");

    // Entity is still in Ready. Both instances visible: existing Running
    // and new Starting.
    {
        let guard = handle.read();
        assert!(
            matches!(guard.stage(), NodeStage::Ready { .. }),
            "expected Ready, got {:?}",
            guard.stage()
        );
        let visible = guard.instances();
        assert_eq!(visible.len(), 2, "both instances should be visible");
        let existing = visible
            .iter()
            .find(|i| i.instance_id() == &existing_id)
            .expect("existing instance still present");
        assert_eq!(existing.state(), InstanceState::Running);
        let new_inst = visible
            .iter()
            .find(|i| i.instance_id() == &instance_id)
            .expect("new instance present");
        assert_eq!(new_inst.state(), InstanceState::Starting);
    }

    let new_pid = child.id().expect("child has pid");
    let returned_pid = NodeEntity::commit_started(&handle, child, started_ctx, instance_id.clone())
        .await
        .expect("commit_started should succeed");
    assert_eq!(returned_pid, new_pid);

    // After commit, both instances are Running.
    {
        let guard = handle.read();
        match guard.stage() {
            NodeStage::Ready { instances, .. } => {
                assert_eq!(instances.len(), 2, "should have both instances");
                for inst in instances {
                    assert_eq!(inst.state(), InstanceState::Running);
                }
                let new_inst = instances
                    .iter()
                    .find(|i| i.instance_id() == &instance_id)
                    .unwrap();
                assert_eq!(new_inst.pid(), Some(new_pid));
            }
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    // Tear down: stop the new instance and SIGTERM the real child.
    handle.write().stop_instance(&instance_id);
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(new_pid.to_string())
        .status();
}

#[tokio::test]
async fn prepare_and_spawn_rejects_duplicate_instance_id_when_running_already_present() {
    // prepare_and_spawn must reject an instance_id that's already tracked,
    // *before* spawning anything. The check is atomic with the append under
    // the same write lock.
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let (instance_id, h) = start_harness("dup-inst");
    let handle = push_built_long_running_sensor(&stack, &h).await;

    // Spawn a real Running instance whose id will collide with the next
    // prepare_and_spawn attempt.
    let _existing =
        real_lifecycle::spawn_running_instance(Arc::clone(&handle), &h, instance_id.clone()).await;

    let err = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &instance_id,
            runtime_config_json5: "{}",
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &h.peppy_dirs,
            output_sinks: h.output_sinks(),
        },
    )
    .await
    .expect_err("prepare_and_spawn should reject duplicate instance id");

    match err {
        NodeStackError::DuplicateInstanceId {
            instance_id: id, ..
        } => {
            assert_eq!(id, instance_id.as_str());
        }
        other => panic!("expected DuplicateInstanceId, got {:?}", other),
    }

    // The entity remained in Ready with exactly the original instance —
    // no Starting instance was registered.
    let guard = handle.read();
    match guard.stage() {
        NodeStage::Ready { instances, .. } => {
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].instance_id(), &instance_id);
            assert_eq!(instances[0].state(), InstanceState::Running);
        }
        other => panic!("expected Ready, got {:?}", other),
    }
}

// ===========================================================================
// Backwards / sideways transition rejection (exhaustive)
// ===========================================================================
//
// These tests verify the rejection rules embedded in `NodeEntity::build` and
// `NodeEntity::prepare_and_spawn` by exercising the pure validators
// (`NodeStage::ensure_buildable` / `ensure_spawnable`) that production code
// dispatches through. They construct `NodeStage` values directly — no entity,
// no backdoor — which is why they cover stages like `Building` that have no
// non-racy real-lifecycle production path.
//
// The happy path (and the rejection-from-Added integration cases) are still
// covered against real entities elsewhere in this file:
// `prepare_and_spawn_rejects_when_not_built` exercises an `Added` entity
// through the real lifecycle method, which proves the validator is actually
// dispatched from production code.

mod backwards_transitions_are_rejected {
    use super::*;
    use node_stack::TrackedNodeInstance;

    fn ready_empty() -> NodeStage {
        NodeStage::Ready {
            config_path: PathBuf::from("/tmp/sensor"),
            artifact_path: PathBuf::from("/tmp/sensor.sif"),
            instances: vec![],
        }
    }

    fn ready_with_running_instance() -> NodeStage {
        NodeStage::Ready {
            config_path: PathBuf::from("/tmp/sensor"),
            artifact_path: PathBuf::from("/tmp/sensor.sif"),
            instances: vec![TrackedNodeInstance::new(
                Name::new("inst").unwrap(),
                Some(1),
                InstanceState::Running,
            )],
        }
    }

    fn building() -> NodeStage {
        NodeStage::Building {
            config_path: PathBuf::from("/tmp/sensor"),
        }
    }

    fn added() -> NodeStage {
        NodeStage::Added {
            config_path: PathBuf::from("/tmp/sensor"),
        }
    }

    #[test]
    fn build_rejected_from_building() {
        assert_eq!(building().ensure_buildable(), Err("Building"));
    }

    #[test]
    fn build_rejected_from_ready_empty() {
        assert_eq!(ready_empty().ensure_buildable(), Err("Ready"));
    }

    #[test]
    fn build_rejected_from_ready_with_instances() {
        assert_eq!(
            ready_with_running_instance().ensure_buildable(),
            Err("Ready")
        );
    }

    #[test]
    fn build_accepted_from_added() {
        assert_eq!(added().ensure_buildable(), Ok(()));
    }

    #[test]
    fn prepare_and_spawn_rejected_from_added() {
        assert_eq!(added().ensure_spawnable(), Err("Added"));
    }

    #[test]
    fn prepare_and_spawn_rejected_from_building() {
        assert_eq!(building().ensure_spawnable(), Err("Building"));
    }

    #[test]
    fn prepare_and_spawn_accepted_from_ready_empty() {
        assert_eq!(ready_empty().ensure_spawnable(), Ok(()));
    }

    #[test]
    fn prepare_and_spawn_accepted_from_ready_with_running_instance() {
        assert_eq!(ready_with_running_instance().ensure_spawnable(), Ok(()));
    }
}

/// Regression for the failed-rebuild rollback bug in the node-add path.
///
/// Scenario: a node is already `Ready` (built, with an artifact_path) when a
/// rebuild is started. `push_config` replaces the entity in-place and the
/// caller then drives `NodeEntity::build`. If build fails, the add path must
/// restore the previously-ready entity — otherwise the stack is left with a
/// dangling `Building` entity (or nothing at all) and the previously-ready
/// artifact is silently lost. This exercises
/// `NodeStack::restore_snapshot_if_matches`, which is the node-stack API the
/// add path calls on the build-failure branch.
#[tokio::test]
async fn restore_snapshot_if_matches_rolls_back_failed_rebuild() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let config_path_v1 = harness.peppy_root.path().join("sensor_v1.json5");
    let config_path_v2 = harness.peppy_root.path().join("sensor_v2.json5");

    // Build v1 Ready for real — this is what must survive a failed rebuild.
    let handle_v1 =
        real_lifecycle::build_ready(&stack, &harness, sensor_config(), &config_path_v1).await;
    let v1_config = handle_v1.read().config().clone();
    let artifact_v1 = handle_v1
        .read()
        .artifact_path()
        .expect("v1 artifact should exist")
        .to_path_buf();

    // Simulate the rebuild: re-push with a blocking build_cmd so the next
    // `NodeEntity::build` call sits in `Building` while we run the restore
    // assertions. The marker/proceed files give us a real-lifecycle way to
    // observe + control the `Building` stage without any backdoor.
    let control_dir = tempfile::tempdir().expect("control tempdir");
    let proceed_file = control_dir.path().join("proceed");
    let proceed_file_disp = proceed_file.display().to_string();
    let blocking_config = sensor_config_with_build_cmd(&format!(
        "while [ ! -f '{}' ]; do sleep 0.02; done; exit 0",
        proceed_file_disp
    ));
    stack
        .push_config(blocking_config, false, &config_path_v2)
        .expect("rebuild push_config should succeed");
    let handle_after_push = stack.find("sensor", "1.0.0").expect("entity should exist");
    let captured_generation = handle_after_push.read().generation();

    // Kick off the rebuild in a task. It will sit in `Building` until we
    // touch the proceed file — and by the time we do, the restore below will
    // have replaced the entity, so the build task's Phase 3 commit will
    // observe the generation drift and return InvalidStageTransition instead
    // of clobbering our restore.
    let working_dir = tempfile::tempdir().expect("working_dir tempdir");
    let working_path = working_dir.path().to_path_buf();
    let peppy_dirs_clone = harness.peppy_dirs.clone();
    let feedback_tx_clone = harness.feedback_tx.clone();
    let log_file_clone = Arc::clone(&harness.log_file);
    let build_handle_clone = Arc::clone(&handle_after_push);
    let build_task = tokio::spawn(async move {
        NodeEntity::build(
            &build_handle_clone,
            BuildContext {
                working_dir: &working_path,
                peppy_dirs: &peppy_dirs_clone,
                feedback_tx: &feedback_tx_clone,
                log_file: log_file_clone,
                env_vars: &[],
            },
        )
        .await
    });

    // Wait until the entity is observably in `Building`.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        {
            let guard = handle_after_push.read();
            if matches!(guard.stage(), NodeStage::Building { .. }) {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("entity did not enter Building within 10s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Build "fails" — the caller rolls back by restoring the v1 snapshot it
    // captured before calling push_config.
    let restored = stack.restore_snapshot_if_matches(
        RestoreTarget {
            name: "sensor",
            tag: "1.0.0",
            expected_handle: &handle_after_push,
            expected_generation: captured_generation,
        },
        EntitySnapshot {
            config: v1_config,
            config_path: config_path_v1.clone(),
            artifact_path: Some(artifact_v1.clone()),
        },
    );
    assert!(
        restored,
        "restore should succeed when handle+generation match"
    );

    // Unblock the build task and wait for it to finish. The task's Phase 3
    // check (generation + stage == Building) will fail because the restore
    // replaced the entity, so the task returns Err and our restore stays.
    std::fs::write(&proceed_file, b"").expect("touch proceed file");
    let build_result = build_task.await.expect("build task should not panic");
    assert!(
        build_result.is_err(),
        "build task should fail its Phase 3 commit after the restore drifted the generation, got Ok"
    );

    let handle_final = stack.find("sensor", "1.0.0").expect("entity should exist");
    let guard = handle_final.read();
    match guard.stage() {
        NodeStage::Ready {
            config_path,
            artifact_path,
            instances,
        } => {
            assert_eq!(config_path, &config_path_v1);
            assert_eq!(artifact_path, &artifact_v1);
            assert!(instances.is_empty());
        }
        other => panic!("expected Ready after rollback, got {:?}", other),
    }
    assert_eq!(
        guard.artifact_path(),
        Some(artifact_v1.as_path()),
        "previous artifact must be restored after a failed rebuild",
    );
}

/// `restore_snapshot_if_matches` must not clobber a concurrent replacement:
/// if the handle+generation drift (because another `push_config` landed
/// between the caller's capture and the build failure), the rollback should
/// be a no-op and the newer entity must stay in place.
#[tokio::test]
async fn restore_snapshot_if_matches_no_op_on_generation_drift() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let config_path_v1 = harness.peppy_root.path().join("sensor_v1.json5");
    let config_path_v2 = harness.peppy_root.path().join("sensor_v2.json5");

    // Push a blocking config so the next build call sits in `Building`
    // while the "concurrent push" bumps the entity's generation under it.
    let control_dir = tempfile::tempdir().expect("control tempdir");
    let proceed_file = control_dir.path().join("proceed");
    let proceed_file_disp = proceed_file.display().to_string();
    let blocking_config = sensor_config_with_build_cmd(&format!(
        "while [ ! -f '{}' ]; do sleep 0.02; done; exit 0",
        proceed_file_disp
    ));
    stack
        .push_config(blocking_config, false, &config_path_v1)
        .expect("initial push_config should succeed");
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    let stale_generation = handle.read().generation();
    let stale_config = handle.read().config().clone();

    // Drive a real build in the background so the entity transitions through
    // `Building`. We keep the task alive for the rest of the test.
    let working_dir = tempfile::tempdir().expect("working_dir tempdir");
    let working_path = working_dir.path().to_path_buf();
    let peppy_dirs_clone = harness.peppy_dirs.clone();
    let feedback_tx_clone = harness.feedback_tx.clone();
    let log_file_clone = Arc::clone(&harness.log_file);
    let build_handle_clone = Arc::clone(&handle);
    let build_task = tokio::spawn(async move {
        NodeEntity::build(
            &build_handle_clone,
            BuildContext {
                working_dir: &working_path,
                peppy_dirs: &peppy_dirs_clone,
                feedback_tx: &feedback_tx_clone,
                log_file: log_file_clone,
                env_vars: &[],
            },
        )
        .await
    });

    // Wait until we observe `Building` on the entity.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        {
            let guard = handle.read();
            if matches!(guard.stage(), NodeStage::Building { .. }) {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("entity did not enter Building within 10s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // A concurrent push replaces the entity under the same handle, bumping
    // its generation. push_config accepts the replacement even while the
    // entity is in Building — the in-flight build task will then fail its
    // Phase 3 check via the generation token.
    stack
        .push_config(sensor_config(), false, &config_path_v2)
        .expect("concurrent push_config should succeed");

    let restored = stack.restore_snapshot_if_matches(
        RestoreTarget {
            name: "sensor",
            tag: "1.0.0",
            expected_handle: &handle,
            expected_generation: stale_generation,
        },
        EntitySnapshot {
            config: stale_config,
            config_path: config_path_v1.clone(),
            artifact_path: Some(PathBuf::from("/tmp/sensor-v1.sif")),
        },
    );
    assert!(
        !restored,
        "restore must be a no-op when the generation has drifted"
    );

    {
        let guard = handle.read();
        match guard.stage() {
            NodeStage::Added { config_path } => {
                assert_eq!(
                    config_path, &config_path_v2,
                    "concurrent replacement must stay in place"
                );
            }
            other => panic!("expected Added (from the concurrent push), got {:?}", other),
        }
    }

    // Unblock + drain the build task to avoid leaking the sh child.
    std::fs::write(&proceed_file, b"").expect("touch proceed file");
    let _ = build_task.await;
}
