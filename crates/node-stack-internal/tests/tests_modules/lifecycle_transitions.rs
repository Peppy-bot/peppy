//! Tests focused on the `NodeStage` lifecycle transitions managed by
//! `NodeEntity`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use config::node::Name;
use node_stack::{
    BuildContext, InstanceState, NodeEntity, NodeStack, NodeStackError, NodeStage,
    TrackedNodeInstance,
};

use crate::helpers::config_common::core_node_config;

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
                start_cmd: ["sensor"]
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
    let guard = handle.read().expect("entity poisoned");

    match guard.stage() {
        NodeStage::Added { config_path: cp } => {
            assert_eq!(cp, &config_path);
        }
        other => panic!("expected Added stage, got {:?}", other),
    }
    assert_eq!(guard.config_path(), config_path.as_path());
    assert!(guard.artifact_path().is_none());
    assert!(guard.instances().is_empty());
}

/// Helper: append a `Running` instance directly into a `Ready` entity via
/// the `__test_stage_mut` backdoor. Replaces the old `start_instance(...)`
/// pattern that no longer exists on `NodeEntity`.
fn append_running_instance(handle: &node_stack::EntityHandle, id: Name, pid: Option<u32>) {
    let mut guard = handle.write().expect("entity poisoned");
    let NodeStage::Ready { instances, .. } = guard.__test_stage_mut() else {
        panic!("expected Ready stage to append running instance");
    };
    instances.push(TrackedNodeInstance::new(id, pid, InstanceState::Running));
}

#[test]
fn ready_with_running_instance_round_trip() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor");
    let artifact_path = PathBuf::from("/tmp/sensor.sif");

    stack
        .push_config(sensor_config(), false, &config_path)
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .__test_set_stage(NodeStage::Ready {
            config_path: config_path.clone(),
            artifact_path: artifact_path.clone(),
            instances: vec![],
        });

    append_running_instance(&handle, Name::new("inst-1").unwrap(), Some(42));

    let guard = handle.read().expect("entity poisoned");
    match guard.stage() {
        NodeStage::Ready {
            config_path: cp,
            artifact_path: sp,
            instances,
        } => {
            assert_eq!(cp, &config_path);
            assert_eq!(sp, &artifact_path);
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].instance_id().as_str(), "inst-1");
            assert_eq!(instances[0].pid(), Some(42));
            assert_eq!(instances[0].state(), InstanceState::Running);
        }
        other => panic!("expected Ready, got {:?}", other),
    }
}

#[test]
fn stop_instance_removes_last_instance_keeps_ready() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor");
    let artifact_path = PathBuf::from("/tmp/sensor.sif");

    stack
        .push_config(sensor_config(), false, &config_path)
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .__test_set_stage(NodeStage::Ready {
            config_path: config_path.clone(),
            artifact_path: artifact_path.clone(),
            instances: vec![],
        });

    let instance_id = Name::new("only-inst").unwrap();
    append_running_instance(&handle, instance_id.clone(), Some(42));

    // Remove the only instance: entity stays in Ready with an empty list.
    let removed = handle
        .write()
        .expect("entity poisoned")
        .stop_instance(&instance_id);
    assert!(removed, "stop_instance should report success");

    let guard = handle.read().expect("entity poisoned");
    match guard.stage() {
        NodeStage::Ready {
            config_path: cp,
            artifact_path: sp,
            instances,
        } => {
            assert_eq!(cp, &config_path);
            assert_eq!(sp, &artifact_path);
            assert!(
                instances.is_empty(),
                "instances list should be empty after removing the only instance"
            );
        }
        other => panic!("expected Ready, got {:?}", other),
    }
    assert_eq!(guard.artifact_path(), Some(artifact_path.as_path()));
    assert!(guard.instances().is_empty());
}

#[test]
fn stop_instance_keeps_other_instances_when_one_removed() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor");
    let artifact_path = PathBuf::from("/tmp/sensor.sif");

    stack
        .push_config(sensor_config(), false, &config_path)
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .__test_set_stage(NodeStage::Ready {
            config_path,
            artifact_path: artifact_path.clone(),
            instances: vec![],
        });

    let id_a = Name::new("inst-a").unwrap();
    let id_b = Name::new("inst-b").unwrap();
    append_running_instance(&handle, id_a.clone(), Some(1));
    append_running_instance(&handle, id_b.clone(), Some(2));

    let removed = handle
        .write()
        .expect("entity poisoned")
        .stop_instance(&id_a);
    assert!(removed);

    let guard = handle.read().expect("entity poisoned");
    match guard.stage() {
        NodeStage::Ready { instances, .. } => {
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].instance_id(), &id_b);
        }
        other => panic!("expected Ready with one instance, got {:?}", other),
    }
}

#[test]
fn stop_instance_returns_false_when_instance_not_tracked() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    stack
        .push_config(sensor_config(), false, PathBuf::from("/tmp/sensor"))
        .expect("push_config should succeed");
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .__test_set_stage(NodeStage::Ready {
            config_path: PathBuf::from("/tmp/sensor"),
            artifact_path: PathBuf::from("/tmp/sensor.sif"),
            instances: vec![],
        });
    append_running_instance(&handle, Name::new("only").unwrap(), Some(1));

    let removed = handle
        .write()
        .expect("entity poisoned")
        .stop_instance(&Name::new("nonexistent").unwrap());
    assert!(!removed);
}

#[test]
fn stop_instance_skips_starting_instances() {
    // stop_instance only acts on Running instances. A Starting instance is
    // an in-flight prepare_and_spawn that hasn't been committed yet — it
    // can't be stopped via the messenger because it hasn't subscribed.
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    stack
        .push_config(sensor_config(), false, PathBuf::from("/tmp/sensor"))
        .expect("push_config should succeed");
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    let id = Name::new("starting-inst").unwrap();
    handle
        .write()
        .expect("entity poisoned")
        .__test_set_stage(NodeStage::Ready {
            config_path: PathBuf::from("/tmp/sensor"),
            artifact_path: PathBuf::from("/tmp/sensor.sif"),
            instances: vec![TrackedNodeInstance::new(
                id.clone(),
                None,
                InstanceState::Starting,
            )],
        });

    let removed = handle.write().expect("entity poisoned").stop_instance(&id);
    assert!(
        !removed,
        "stop_instance must not remove a Starting instance"
    );
    // The Starting instance is still in the list.
    let guard = handle.read().expect("entity poisoned");
    assert_eq!(guard.instances().len(), 1);
    assert_eq!(guard.instances()[0].state(), InstanceState::Starting);
}

#[test]
fn push_config_resets_existing_entity_to_added() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path_v1 = PathBuf::from("/tmp/sensor/v1/peppy.json5");
    let config_path_v2 = PathBuf::from("/tmp/sensor/v2/peppy.json5");
    let artifact_path = PathBuf::from("/tmp/sensor.sif");

    // Initial push + build.
    stack
        .push_config(sensor_config(), false, &config_path_v1)
        .expect("first push_config should succeed");
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .__test_set_stage(NodeStage::Ready {
            config_path: config_path_v1.clone(),
            artifact_path,
            instances: vec![],
        });

    assert!(handle.read().unwrap().artifact_path().is_some());

    // Re-push with the same config but a different config_path. The entity
    // should be reset to Added with the new config_path; artifact_path is gone.
    stack
        .push_config(sensor_config(), false, &config_path_v2)
        .expect("second push_config should succeed");

    let handle_after = stack.find("sensor", "1.0.0").expect("entity should exist");
    let guard = handle_after.read().expect("entity poisoned");
    match guard.stage() {
        NodeStage::Added { config_path: cp } => {
            assert_eq!(cp, &config_path_v2);
        }
        other => panic!("expected Added after re-push, got {:?}", other),
    }
    assert!(guard.artifact_path().is_none());
    assert!(guard.instances().is_empty());
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
    stack
        .push_config(sensor_config(), false, &config_path)
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

    // Exactly one Ok; exactly one InvalidStageTransition where the loser saw
    // Building (it might also see the eventual Built stage if the winner was
    // already in Phase 3). Both shapes are valid rejections.
    let (ok_count, transition_err_count) = [&r1, &r2].iter().fold((0, 0), |(o, e), r| match r {
        Ok(()) => (o + 1, e),
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

    // Entity ended up in Built.
    let guard = handle.read().expect("entity poisoned");
    assert!(matches!(guard.stage(), NodeStage::Ready { .. }));

    // The on-disk archive exists exactly once.
    let archive = peppy_dirs.added_nodes_dir().join("sensor_1.0.0.tar.zst");
    assert!(archive.is_file(), "expected archive at {:?}", archive);
}

// ===========================================================================
// Build with add_cmd: Added → Building → Built (or Building → Added on failure)
// ===========================================================================

/// Returns a sensor config whose `execution.add_cmd` runs the given shell snippet.
fn sensor_config_with_add_cmd(add_cmd_shell: &str) -> config::node::NodeConfig {
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
                add_cmd: ["sh", "-c", "{}"],
                start_cmd: ["sensor"]
            }}
        }}"#,
        add_cmd_shell
    );
    serde_json5::from_str::<config::node::NodeConfig>(&json).expect("valid sensor+add_cmd config")
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
            sensor_config_with_add_cmd("echo built > marker.txt"),
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
    .expect("build should succeed when add_cmd exits 0");

    // Entity is in Built.
    let guard = handle.read().expect("entity poisoned");
    assert!(matches!(guard.stage(), NodeStage::Ready { .. }));
    drop(guard);

    // The archive exists and contains the marker file produced by add_cmd.
    let archive = h.peppy_dirs.added_nodes_dir().join("sensor_1.0.0.tar.zst");
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
    assert!(found, "marker.txt produced by add_cmd should be in archive");

    // Keep harness alive (so tempdirs survive until the assertions above).
    drop(h.peppy_root);
    drop(h.working_dir);
}

#[tokio::test]
async fn build_rolls_back_to_added_when_add_cmd_fails() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor/peppy.json5");
    stack
        .push_config(sensor_config_with_add_cmd("exit 7"), false, &config_path)
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    let h = build_harness();

    let err = NodeEntity::build(
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
    .expect_err("build should fail when add_cmd exits non-zero");

    match &err {
        NodeStackError::BuildFailed { reason, .. } => {
            assert!(
                reason.contains("add_cmd failed"),
                "reason should mention 'add_cmd failed', got: {}",
                reason
            );
            assert!(
                reason.contains("7"),
                "reason should include exit status 7, got: {}",
                reason
            );
        }
        other => panic!("expected BuildFailed, got {:?}", other),
    }

    // Entity rolled back to Added (not stuck in Building).
    let guard = handle.read().expect("entity poisoned");
    assert!(
        matches!(guard.stage(), NodeStage::Added { .. }),
        "entity should be back in Added after add_cmd failure, got {:?}",
        guard.stage()
    );

    // No archive on disk.
    let archive = h.peppy_dirs.added_nodes_dir().join("sensor_1.0.0.tar.zst");
    assert!(
        !archive.exists(),
        "no archive should exist after failed add_cmd"
    );

    drop(h.peppy_root);
    drop(h.working_dir);
}

// ===========================================================================
// Start: prepare_and_spawn / commit_started / abort_started
// ===========================================================================

/// Returns a sensor config that uses a long-running shell loop as start_cmd.
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
                start_cmd: ["sh", "-c", "trap 'exit 0' TERM; while true; do sleep 0.1; done"]
            }
        }"#,
    )
    .expect("valid long-running sensor config")
}

/// Pushes a sensor with the given start_cmd into the stack and forces it into
/// `Built` via `__test_set_stage`. The artifact_path points at a fake `.tar.zst`
/// — for the start path, the entity reads `artifact_path` only when extracting
/// the archive (process node case). To avoid that, we make the entity behave
/// as if it had no archive to extract by populating an empty placeholder file.
fn push_built_long_running_sensor(stack: &NodeStack, peppy_dirs: &config::consts::PeppyDirs) {
    stack
        .push_config(
            long_running_sensor_config(),
            false,
            PathBuf::from("/tmp/sensor"),
        )
        .expect("push_config should succeed");
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");

    // Create a fake archive containing nothing — extract_node_archive will
    // unpack it into a fresh instance dir without complaining.
    let added_nodes_dir = peppy_dirs.added_nodes_dir();
    std::fs::create_dir_all(&added_nodes_dir).expect("create added_nodes_dir");
    let archive_path = added_nodes_dir.join("sensor_1.0.0.tar.zst");
    let f = std::fs::File::create(&archive_path).expect("create archive");
    let mut encoder = zstd::stream::write::Encoder::new(f, 1).expect("zstd encoder");
    {
        let mut tar = tar::Builder::new(&mut encoder);
        tar.finish().expect("empty tar");
    }
    encoder.finish().expect("finish zstd");

    handle
        .write()
        .expect("entity poisoned")
        .__test_set_stage(NodeStage::Ready {
            config_path: PathBuf::from("/tmp/sensor"),
            artifact_path: archive_path,
            instances: vec![],
        });
}

/// Hooks impl that does nothing — used for tests that don't care about
/// quiescence detection.
struct NoOpTestHooks;
impl node_stack::build_io::OutputReaderHooks for NoOpTestHooks {}

/// Sets up the StartContext fields shared across start tests, including a
/// draining feedback consumer that prevents the internal channel from filling.
struct StartHarness {
    instance_id: Name,
    peppy_root: tempfile::TempDir,
    peppy_dirs: config::consts::PeppyDirs,
    log_file: Arc<StdMutex<std::fs::File>>,
    feedback_tx: tokio::sync::mpsc::UnboundedSender<node_stack::build_io::FeedbackLine>,
    publish_enabled: Arc<std::sync::atomic::AtomicBool>,
    hooks: Arc<dyn node_stack::build_io::OutputReaderHooks>,
}

impl StartHarness {
    fn output_sinks(&self) -> node_stack::OutputSinks {
        node_stack::OutputSinks {
            feedback_tx: self.feedback_tx.clone(),
            log_file: Arc::clone(&self.log_file),
            publish_enabled: Arc::clone(&self.publish_enabled),
            hooks: Arc::clone(&self.hooks),
        }
    }
}

fn start_harness(instance_id_str: &str) -> StartHarness {
    let peppy_root = tempfile::tempdir().expect("tempdir peppy_root");
    let peppy_dirs = config::consts::PeppyDirs::new(peppy_root.path().to_path_buf());
    let log_path = peppy_root.path().join("start.log");
    let log_file = Arc::new(StdMutex::new(
        std::fs::File::create(&log_path).expect("create log"),
    ));

    // Drain feedback so the internal channel never fills up.
    let (feedback_tx, mut feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<node_stack::build_io::FeedbackLine>();
    tokio::spawn(async move { while feedback_rx.recv().await.is_some() {} });

    StartHarness {
        instance_id: Name::new(instance_id_str).expect("valid instance id"),
        peppy_root,
        peppy_dirs,
        log_file,
        feedback_tx,
        publish_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        hooks: Arc::new(NoOpTestHooks),
    }
}

#[tokio::test]
async fn prepare_and_spawn_rejects_when_not_built() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    stack
        .push_config(sensor_config(), false, PathBuf::from("/tmp/sensor"))
        .expect("push_config should succeed");
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    let h = start_harness("test-inst-1");

    let err = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &h.instance_id,
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
            assert_eq!(to, "Ready");
        }
        other => panic!("expected InvalidStageTransition, got {:?}", other),
    }

    // Entity remained in Added.
    let guard = handle.read().expect("entity poisoned");
    assert!(matches!(guard.stage(), NodeStage::Added { .. }));
    drop(h.peppy_root);
}

#[tokio::test]
async fn prepare_and_spawn_marks_instance_starting_then_commit_marks_running() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let h = start_harness("test-inst-2");
    push_built_long_running_sensor(&stack, &h.peppy_dirs);
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");

    let (child, started_ctx) = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &h.instance_id,
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
        let guard = handle.read().expect("entity poisoned");
        assert!(
            matches!(guard.stage(), NodeStage::Ready { .. }),
            "entity should remain in Ready during prepare_and_spawn, got {:?}",
            guard.stage()
        );
        let inst = guard
            .instances()
            .iter()
            .find(|i| i.instance_id() == &h.instance_id)
            .expect("just-registered instance should be present");
        assert_eq!(
            inst.state(),
            InstanceState::Starting,
            "instance should be in Starting state until commit_started runs"
        );
    }

    let pid = child.id().expect("child has pid");
    assert!(pid > 0, "spawned child should have a valid pid");

    let returned_pid =
        NodeEntity::commit_started(&handle, child, started_ctx, h.instance_id.clone())
            .await
            .expect("commit_started should succeed");
    assert_eq!(returned_pid, pid);

    // Entity is still in Ready, but the instance is now Running.
    {
        let guard = handle.read().expect("entity poisoned");
        match guard.stage() {
            NodeStage::Ready { instances, .. } => {
                assert_eq!(instances.len(), 1);
                assert_eq!(instances[0].instance_id(), &h.instance_id);
                assert_eq!(instances[0].pid(), Some(pid));
                assert_eq!(instances[0].state(), InstanceState::Running);
            }
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    // Tear down: stop the instance and kill the child process.
    handle
        .write()
        .expect("entity poisoned")
        .stop_instance(&h.instance_id);
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
    drop(h.peppy_root);
}

#[tokio::test]
async fn abort_started_removes_starting_instance_and_kills_child() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let h = start_harness("test-inst-3");
    push_built_long_running_sensor(&stack, &h.peppy_dirs);
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");

    let (child, started_ctx) = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &h.instance_id,
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
        &h.instance_id,
    )
    .await;
    assert!(
        msg.contains("synthetic failure"),
        "abort message should include the original error, got: {}",
        msg
    );

    // Entity is still in Ready, but the Starting instance was removed.
    {
        let guard = handle.read().expect("entity poisoned");
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
    drop(h.peppy_root);
}

#[tokio::test]
async fn prepare_and_spawn_starts_additional_instance_alongside_existing() {
    // This is the multi-instance launch case: a node is already running and
    // the daemon launches a *second* instance of the same node. The entity
    // stays in Ready throughout; the new instance is appended to the
    // existing instances list, in Starting state, and flipped to Running by
    // commit_started.
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let h = start_harness("test-inst-second");
    push_built_long_running_sensor(&stack, &h.peppy_dirs);
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");

    // Pre-populate the entity with one running instance via __test_stage_mut.
    let existing_id = Name::new("existing-inst").unwrap();
    let existing_pid = 99999u32; // fake PID — never actually killed
    {
        let mut guard = handle.write().expect("entity poisoned");
        let NodeStage::Ready { instances, .. } = guard.__test_stage_mut() else {
            panic!("expected Ready");
        };
        instances.push(TrackedNodeInstance::new(
            existing_id.clone(),
            Some(existing_pid),
            InstanceState::Running,
        ));
    }

    // Now spawn a second instance via the regular start API.
    let (child, started_ctx) = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &h.instance_id,
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
        let guard = handle.read().expect("entity poisoned");
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
            .find(|i| i.instance_id() == &h.instance_id)
            .expect("new instance present");
        assert_eq!(new_inst.state(), InstanceState::Starting);
    }

    let new_pid = child.id().expect("child has pid");
    let returned_pid =
        NodeEntity::commit_started(&handle, child, started_ctx, h.instance_id.clone())
            .await
            .expect("commit_started should succeed");
    assert_eq!(returned_pid, new_pid);

    // After commit, both instances are Running.
    {
        let guard = handle.read().expect("entity poisoned");
        match guard.stage() {
            NodeStage::Ready { instances, .. } => {
                assert_eq!(instances.len(), 2, "should have both instances");
                for inst in instances {
                    assert_eq!(inst.state(), InstanceState::Running);
                }
                let new_inst = instances
                    .iter()
                    .find(|i| i.instance_id() == &h.instance_id)
                    .unwrap();
                assert_eq!(new_inst.pid(), Some(new_pid));
            }
            other => panic!("expected Ready, got {:?}", other),
        }
    }

    // Tear down: stop the new instance and SIGTERM the real child.
    handle
        .write()
        .expect("entity poisoned")
        .stop_instance(&h.instance_id);
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(new_pid.to_string())
        .status();
    drop(h.peppy_root);
}

#[tokio::test]
async fn prepare_and_spawn_rejects_duplicate_instance_id_when_running_already_present() {
    // prepare_and_spawn must reject an instance_id that's already tracked,
    // *before* spawning anything. The check is atomic with the append under
    // the same write lock.
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let h = start_harness("dup-inst");
    push_built_long_running_sensor(&stack, &h.peppy_dirs);
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");

    // Append a Running instance whose id collides with the harness id.
    {
        let mut guard = handle.write().expect("entity poisoned");
        let NodeStage::Ready { instances, .. } = guard.__test_stage_mut() else {
            panic!("expected Ready");
        };
        instances.push(TrackedNodeInstance::new(
            h.instance_id.clone(),
            Some(1),
            InstanceState::Running,
        ));
    }

    let err = NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id: &h.instance_id,
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
        NodeStackError::DuplicateInstanceId { instance_id, .. } => {
            assert_eq!(instance_id, h.instance_id.as_str());
        }
        other => panic!("expected DuplicateInstanceId, got {:?}", other),
    }

    // The entity remained in Ready with exactly the original instance —
    // no Starting instance was registered.
    let guard = handle.read().expect("entity poisoned");
    match guard.stage() {
        NodeStage::Ready { instances, .. } => {
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].instance_id(), &h.instance_id);
            assert_eq!(instances[0].state(), InstanceState::Running);
        }
        other => panic!("expected Ready, got {:?}", other),
    }
    drop(h.peppy_root);
}

// ===========================================================================
// Backwards / sideways transition rejection (exhaustive)
// ===========================================================================
//
// These tests use `__test_set_stage` to inject the entity into each non-allowed
// source stage and verify that every lifecycle method rejects with the
// expected `InvalidStageTransition` error. They are exhaustive coverage of
// the rejection table in the entity API documentation.

mod backwards_transitions_are_rejected {
    use super::*;

    fn make_entity_in(stack: &NodeStack, stage: NodeStage) -> node_stack::EntityHandle {
        stack
            .push_config(sensor_config(), false, PathBuf::from("/tmp/sensor"))
            .expect("push_config");
        let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
        handle
            .write()
            .expect("entity poisoned")
            .__test_set_stage(stage);
        handle
    }

    async fn run_build(
        handle: &node_stack::EntityHandle,
    ) -> std::result::Result<(), NodeStackError> {
        let h = build_harness();
        let result = NodeEntity::build(
            handle,
            BuildContext {
                working_dir: h.working_dir.path(),
                peppy_dirs: &h.peppy_dirs,
                feedback_tx: &h.feedback_tx,
                log_file: Arc::clone(&h.log_file),
                env_vars: &[],
            },
        )
        .await;
        drop(h.peppy_root);
        drop(h.working_dir);
        result
    }

    fn assert_rejected_to(
        result: std::result::Result<(), NodeStackError>,
        expected_from: &str,
        expected_to: &str,
    ) {
        match result {
            Err(NodeStackError::InvalidStageTransition { from, to, .. }) => {
                assert_eq!(from, expected_from, "unexpected `from` stage");
                assert_eq!(to, expected_to, "unexpected `to` stage");
            }
            Err(other) => panic!("expected InvalidStageTransition, got {:?}", other),
            Ok(()) => panic!("expected rejection, got Ok"),
        }
    }

    #[tokio::test]
    async fn build_rejected_from_building() {
        let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
        let handle = make_entity_in(
            &stack,
            NodeStage::Building {
                config_path: PathBuf::from("/tmp/sensor"),
            },
        );
        assert_rejected_to(run_build(&handle).await, "Building", "Ready");
    }

    #[tokio::test]
    async fn build_rejected_from_ready_empty() {
        // Ready with no instances — equivalent to the old `Built` case.
        let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
        let handle = make_entity_in(
            &stack,
            NodeStage::Ready {
                config_path: PathBuf::from("/tmp/sensor"),
                artifact_path: PathBuf::from("/tmp/sensor.sif"),
                instances: vec![],
            },
        );
        assert_rejected_to(run_build(&handle).await, "Ready", "Ready");
    }

    #[tokio::test]
    async fn build_rejected_from_ready_with_instances() {
        // Ready with one Running instance — equivalent to the old `Started` case.
        let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
        let handle = make_entity_in(
            &stack,
            NodeStage::Ready {
                config_path: PathBuf::from("/tmp/sensor"),
                artifact_path: PathBuf::from("/tmp/sensor.sif"),
                instances: vec![TrackedNodeInstance::new(
                    Name::new("inst").unwrap(),
                    Some(1),
                    InstanceState::Running,
                )],
            },
        );
        assert_rejected_to(run_build(&handle).await, "Ready", "Ready");
    }

    async fn run_prepare_and_spawn(
        handle: &node_stack::EntityHandle,
    ) -> std::result::Result<(tokio::process::Child, node_stack::StartedInstanceCtx), NodeStackError>
    {
        let h = start_harness("rejection-test");
        let result = NodeEntity::prepare_and_spawn(
            handle,
            node_stack::StartContext {
                instance_id: &h.instance_id,
                runtime_config_json5: "{}",
                env_vars: &[],
                mount_paths_resolved: &[],
                peppy_dirs: &h.peppy_dirs,
                output_sinks: h.output_sinks(),
            },
        )
        .await;
        // Hold the harness alive until the result is returned.
        drop(h.peppy_root);
        result
    }

    #[tokio::test]
    async fn prepare_and_spawn_rejected_from_building() {
        let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
        let handle = make_entity_in(
            &stack,
            NodeStage::Building {
                config_path: PathBuf::from("/tmp/sensor"),
            },
        );
        match run_prepare_and_spawn(&handle).await {
            Err(NodeStackError::InvalidStageTransition { from, to, .. }) => {
                assert_eq!(from, "Building");
                assert_eq!(to, "Ready");
            }
            other => panic!("expected InvalidStageTransition, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn prepare_and_spawn_rejected_from_added() {
        let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
        let handle = make_entity_in(
            &stack,
            NodeStage::Added {
                config_path: PathBuf::from("/tmp/sensor"),
            },
        );
        match run_prepare_and_spawn(&handle).await {
            Err(NodeStackError::InvalidStageTransition { from, to, .. }) => {
                assert_eq!(from, "Added");
                assert_eq!(to, "Ready");
            }
            other => panic!("expected InvalidStageTransition, got {:?}", other),
        }
    }
}
