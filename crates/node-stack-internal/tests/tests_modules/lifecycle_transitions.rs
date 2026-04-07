//! Tests focused on the `NodeStage` lifecycle transitions managed by
//! `NodeEntity`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use config::node::Name;
use node_stack::{
    BuildContext, NodeEntity, NodeStack, NodeStackError, NodeStage, TrackedNodeInstance,
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
    assert!(guard.sif_path().is_none());
    assert!(guard.instances().is_empty());
}

#[test]
fn restore_built_transitions_added_to_built() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor/peppy.json5");
    let sif_path = PathBuf::from("/tmp/peppy/added_nodes/sensor_1.0.0.sif");

    stack
        .push_config(sensor_config(), false, &config_path)
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .restore_built(sif_path.clone())
        .expect("Added → Built should succeed");

    let guard = handle.read().expect("entity poisoned");
    match guard.stage() {
        NodeStage::Built {
            config_path: cp,
            sif_path: sp,
        } => {
            assert_eq!(cp, &config_path, "config_path must be preserved");
            assert_eq!(sp, &sif_path);
        }
        other => panic!("expected Built stage, got {:?}", other),
    }
    assert_eq!(guard.config_path(), config_path.as_path());
    assert_eq!(guard.sif_path(), Some(sif_path.as_path()));
    assert!(guard.instances().is_empty());
}

#[test]
fn restore_built_rejects_when_not_in_added() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let sif_path = PathBuf::from("/tmp/sensor.sif");

    stack
        .push_config(sensor_config(), false, PathBuf::from("/tmp/sensor"))
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .restore_built(sif_path.clone())
        .expect("first transition should succeed");

    // Second call should fail — already Built.
    let err = handle
        .write()
        .expect("entity poisoned")
        .restore_built(sif_path)
        .expect_err("second restore_built should fail");

    match err {
        NodeStackError::InvalidStageTransition { from, to, .. } => {
            assert_eq!(from, "Built");
            assert_eq!(to, "Built");
        }
        other => panic!("expected InvalidStageTransition, got {:?}", other),
    }
}

#[test]
fn start_instance_rejects_when_only_added() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    stack
        .push_config(sensor_config(), false, PathBuf::from("/tmp/sensor"))
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    let instance = TrackedNodeInstance::new(Name::new("inst").unwrap(), Some(42));

    let err = handle
        .write()
        .expect("entity poisoned")
        .start_instance(instance)
        .expect_err("start_instance on Added should fail");

    match err {
        NodeStackError::InvalidStageTransition { from, to, .. } => {
            assert_eq!(from, "Added");
            assert_eq!(to, "Started");
        }
        other => panic!("expected InvalidStageTransition, got {:?}", other),
    }
}

#[test]
fn start_instance_transitions_built_to_started() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let sif_path = PathBuf::from("/tmp/sensor.sif");

    stack
        .push_config(sensor_config(), false, PathBuf::from("/tmp/sensor"))
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .restore_built(sif_path.clone())
        .expect("restore_built should succeed");

    let instance = TrackedNodeInstance::new(Name::new("inst-1").unwrap(), Some(42));
    handle
        .write()
        .expect("entity poisoned")
        .start_instance(instance)
        .expect("start_instance should succeed");

    let guard = handle.read().expect("entity poisoned");
    match guard.stage() {
        NodeStage::Started {
            config_path: cp,
            sif_path: sp,
            instances,
        } => {
            assert_eq!(cp, &PathBuf::from("/tmp/sensor"));
            assert_eq!(sp, &sif_path);
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].instance_id().as_str(), "inst-1");
            assert_eq!(instances[0].pid(), Some(42));
        }
        other => panic!("expected Started, got {:?}", other),
    }
}

#[test]
fn stop_instance_falls_back_to_built_when_last_instance_removed() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor");
    let sif_path = PathBuf::from("/tmp/sensor.sif");

    stack
        .push_config(sensor_config(), false, &config_path)
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .restore_built(sif_path.clone())
        .expect("restore_built should succeed");

    let instance_id = Name::new("only-inst").unwrap();
    handle
        .write()
        .expect("entity poisoned")
        .start_instance(TrackedNodeInstance::new(instance_id.clone(), Some(42)))
        .expect("start_instance should succeed");

    // Remove the only instance: entity should fall back to Built.
    let removed = handle
        .write()
        .expect("entity poisoned")
        .stop_instance(&instance_id);
    assert!(removed, "stop_instance should report success");

    let guard = handle.read().expect("entity poisoned");
    match guard.stage() {
        NodeStage::Built {
            config_path: cp,
            sif_path: sp,
        } => {
            assert_eq!(cp, &config_path);
            assert_eq!(sp, &sif_path);
        }
        other => panic!(
            "expected Built after removing last instance, got {:?}",
            other
        ),
    }
    assert_eq!(guard.sif_path(), Some(sif_path.as_path()));
    assert!(guard.instances().is_empty());
}

#[test]
fn stop_instance_keeps_started_when_other_instances_remain() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let sif_path = PathBuf::from("/tmp/sensor.sif");

    stack
        .push_config(sensor_config(), false, PathBuf::from("/tmp/sensor"))
        .expect("push_config should succeed");

    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .restore_built(sif_path.clone())
        .expect("restore_built should succeed");

    let id_a = Name::new("inst-a").unwrap();
    let id_b = Name::new("inst-b").unwrap();
    {
        let mut guard = handle.write().expect("entity poisoned");
        guard
            .start_instance(TrackedNodeInstance::new(id_a.clone(), Some(1)))
            .expect("start instance a");
        guard
            .start_instance(TrackedNodeInstance::new(id_b.clone(), Some(2)))
            .expect("start instance b");
    }

    let removed = handle
        .write()
        .expect("entity poisoned")
        .stop_instance(&id_a);
    assert!(removed);

    let guard = handle.read().expect("entity poisoned");
    match guard.stage() {
        NodeStage::Started { instances, .. } => {
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].instance_id(), &id_b);
        }
        other => panic!("expected Started with one instance, got {:?}", other),
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
        .restore_built(PathBuf::from("/tmp/sensor.sif"))
        .expect("restore_built should succeed");
    handle
        .write()
        .expect("entity poisoned")
        .start_instance(TrackedNodeInstance::new(
            Name::new("only").unwrap(),
            Some(1),
        ))
        .expect("start instance");

    let removed = handle
        .write()
        .expect("entity poisoned")
        .stop_instance(&Name::new("nonexistent").unwrap());
    assert!(!removed);
}

#[test]
fn duplicate_instance_id_is_rejected() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    stack
        .push_config(sensor_config(), false, PathBuf::from("/tmp/sensor"))
        .expect("push_config should succeed");
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .restore_built(PathBuf::from("/tmp/sensor.sif"))
        .expect("restore_built should succeed");

    let id = Name::new("dup").unwrap();
    handle
        .write()
        .expect("entity poisoned")
        .start_instance(TrackedNodeInstance::new(id.clone(), Some(1)))
        .expect("first start should succeed");

    let err = handle
        .write()
        .expect("entity poisoned")
        .start_instance(TrackedNodeInstance::new(id, Some(2)))
        .expect_err("second start with same id should fail");

    match err {
        NodeStackError::DuplicateInstanceId { .. } => {}
        other => panic!("expected DuplicateInstanceId, got {:?}", other),
    }
}

#[test]
fn push_config_resets_existing_entity_to_added() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path_v1 = PathBuf::from("/tmp/sensor/v1/peppy.json5");
    let config_path_v2 = PathBuf::from("/tmp/sensor/v2/peppy.json5");
    let sif_path = PathBuf::from("/tmp/sensor.sif");

    // Initial push + build.
    stack
        .push_config(sensor_config(), false, &config_path_v1)
        .expect("first push_config should succeed");
    let handle = stack.find("sensor", "1.0.0").expect("entity should exist");
    handle
        .write()
        .expect("entity poisoned")
        .restore_built(sif_path)
        .expect("restore_built should succeed");

    assert!(handle.read().unwrap().sif_path().is_some());

    // Re-push with the same config but a different config_path. The entity
    // should be reset to Added with the new config_path; sif_path is gone.
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
    assert!(guard.sif_path().is_none());
    assert!(guard.instances().is_empty());
}

#[tokio::test]
async fn build_calls_are_serialized_per_entity() {
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

    // A throwaway log file (the archive path doesn't actually use it, but
    // BuildContext requires one).
    let log_path = peppy_root.path().join("build.log");
    let log_file = Arc::new(StdMutex::new(
        std::fs::File::create(&log_path).expect("create log"),
    ));

    let (feedback_tx, _feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<node_stack::build_io::FeedbackLine>();

    // Use a barrier so both tasks reach the call to `build` as close to
    // simultaneously as possible — maximizes the race window for serialization.
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

    // Exactly one Ok, exactly one InvalidStageTransition (Built -> Built).
    let (ok_count, transition_err_count) = [&r1, &r2].iter().fold((0, 0), |(o, e), r| match r {
        Ok(()) => (o + 1, e),
        Err(NodeStackError::InvalidStageTransition {
            from: "Built",
            to: "Built",
            ..
        }) => (o, e + 1),
        Err(other) => panic!("unexpected build error: {:?}", other),
    });
    assert_eq!(ok_count, 1, "exactly one build should succeed");
    assert_eq!(
        transition_err_count, 1,
        "the queued build should fail with InvalidStageTransition Built->Built"
    );

    // Entity ended up in Built.
    let guard = handle.read().expect("entity poisoned");
    assert!(matches!(guard.stage(), NodeStage::Built { .. }));

    // The on-disk archive exists exactly once.
    let archive = peppy_dirs.added_nodes_dir().join("sensor_1.0.0.tar.zst");
    assert!(archive.is_file(), "expected archive at {:?}", archive);
}
