//! Real-lifecycle test helpers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use config::consts::PeppyDirs;
use config::node::{Name, NodeConfig};
use node_stack::{
    BuildContext, EntityHandle, NodeEntity, NodeStack, OutputSinks, StartContext,
    build_io::{FeedbackLine, OutputReaderHooks},
};
use tokio::sync::mpsc;

/// Portable long-running start command used by fixture spawns. Traps SIGTERM
/// so `stop_instance` + `kill -TERM` cleanly shuts it down.
pub const LONG_RUNNING_START_CMD: &[&str] = &[
    "sh",
    "-c",
    "trap 'exit 0' TERM; while true; do sleep 0.1; done",
];

static FIXTURE_INSTANCE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Returns a process-wide unique fixture instance name. Used as the default
/// when a caller doesn't supply an explicit instance id.
pub fn fixture_instance_name() -> Name {
    let n = FIXTURE_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    Name::new(format!("fixture-inst-{}", n)).expect("fixture instance name")
}

struct NoOpHooks;
impl OutputReaderHooks for NoOpHooks {}

/// Tempdir-backed plumbing for real-lifecycle fixture calls. Callers keep
/// this alive for the duration of the test; dropping it cleans up the
/// tempdirs and aborts the feedback-drain task.
pub struct LifecycleHarness {
    pub peppy_root: tempfile::TempDir,
    pub peppy_dirs: PeppyDirs,
    pub working_dir: tempfile::TempDir,
    pub log_file: Arc<StdMutex<std::fs::File>>,
    pub feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    pub publish_enabled: Arc<AtomicBool>,
    pub hooks: Arc<dyn OutputReaderHooks>,
    feedback_drain: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for LifecycleHarness {
    fn drop(&mut self) {
        if let Some(h) = self.feedback_drain.take() {
            h.abort();
        }
    }
}

impl LifecycleHarness {
    pub fn output_sinks(&self) -> OutputSinks {
        OutputSinks {
            feedback_tx: self.feedback_tx.clone(),
            log_file: Arc::clone(&self.log_file),
            publish_enabled: Arc::clone(&self.publish_enabled),
            hooks: Arc::clone(&self.hooks),
        }
    }
}

/// Constructs a fresh [`LifecycleHarness`] with isolated tempdirs and a
/// background feedback-drain task so the internal channel does not fill up.
pub fn lifecycle_harness() -> LifecycleHarness {
    let peppy_root = tempfile::tempdir().expect("peppy_root tempdir");
    let peppy_dirs = PeppyDirs::new(peppy_root.path().to_path_buf());
    let working_dir = tempfile::tempdir().expect("working_dir tempdir");
    let log_path = peppy_root.path().join("test.log");
    let log_file = Arc::new(StdMutex::new(
        std::fs::File::create(&log_path).expect("create log"),
    ));
    let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let feedback_drain = tokio::spawn(async move { while feedback_rx.recv().await.is_some() {} });
    LifecycleHarness {
        peppy_root,
        peppy_dirs,
        working_dir,
        log_file,
        feedback_tx,
        publish_enabled: Arc::new(AtomicBool::new(true)),
        hooks: Arc::new(NoOpHooks),
        feedback_drain: Some(feedback_drain),
    }
}

/// Rewrites `config.execution` so that the build path runs the trivial
/// `archive_dir_to_storage` branch (no apptainer, no user add_cmd) and the
/// later spawn uses the portable long-running shell loop. Fixture callers
/// that pass arbitrary configs are opting into this override.
pub fn override_execution_for_fixture(mut config: NodeConfig) -> NodeConfig {
    config.execution.container = None;
    config.execution.add_cmd = None;
    config.execution.start_cmd = Some(
        LONG_RUNNING_START_CMD
            .iter()
            .map(|s| s.to_string())
            .collect(),
    );
    config
}

/// Pushes `config` into `stack`, rewrites its execution fields for fixture
/// use, then drives the real `NodeEntity::build` to produce a `Ready` entity
/// with an on-disk archive in `harness.peppy_dirs.added_nodes_dir()`.
///
/// Returns the `Ready` entity handle.
pub async fn build_ready(
    stack: &NodeStack,
    harness: &LifecycleHarness,
    config: NodeConfig,
    config_path: impl Into<PathBuf>,
) -> EntityHandle {
    let config = override_execution_for_fixture(config);
    let config_path = config_path.into();
    let name = config.manifest.name.as_str().to_owned();
    let tag = config.manifest.tag.clone();

    stack
        .push_config(config, false, &config_path)
        .expect("test fixture: push_config should succeed");
    let handle = stack
        .find(&name, &tag)
        .expect("test fixture: just-pushed entity should exist");

    NodeEntity::build(
        &handle,
        BuildContext {
            working_dir: harness.working_dir.path(),
            peppy_dirs: &harness.peppy_dirs,
            feedback_tx: &harness.feedback_tx,
            log_file: Arc::clone(&harness.log_file),
            env_vars: &[],
        },
    )
    .await
    .expect("test fixture: build should succeed on process node");

    handle
}

/// RAII guard for a fixture-spawned `Running` instance. On drop, calls
/// `stop_instance` on the entity and sends SIGTERM to the child process.
/// Tests that want to observe `stop_instance` side-effects (e.g. that it
/// returns `true` on the first call and `false` afterwards) can drop the
/// guard mid-test.
pub struct RunningInstanceGuard {
    pub handle: EntityHandle,
    pub instance_id: Name,
    pub pid: u32,
}

impl Drop for RunningInstanceGuard {
    fn drop(&mut self) {
        self.handle.write().stop_instance(&self.instance_id);
        // Best-effort termination. The `trap 'exit 0' TERM` loop in
        // `LONG_RUNNING_START_CMD` exits cleanly on SIGTERM.
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(self.pid.to_string())
            .status();
    }
}

/// Drives the real start lifecycle for `handle`: calls `prepare_and_spawn`
/// to register a `Starting` instance + spawn the child, then `commit_started`
/// to flip it to `Running`. Returns a guard that cleans up the child on drop.
///
/// The entity must be in `Ready` (typically produced by [`build_ready`]) and
/// its `start_cmd` must be spawnable — [`build_ready`] sets this up for you.
pub async fn spawn_running_instance(
    handle: EntityHandle,
    harness: &LifecycleHarness,
    instance_id: Name,
) -> RunningInstanceGuard {
    let (child, started_ctx) = NodeEntity::prepare_and_spawn(
        &handle,
        StartContext {
            instance_id: &instance_id,
            runtime_config_json5: "{}",
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &harness.peppy_dirs,
            output_sinks: harness.output_sinks(),
        },
    )
    .await
    .expect("test fixture: prepare_and_spawn should succeed on Ready entity");

    let pid = child.id().expect("fixture: spawned child should have pid");
    NodeEntity::commit_started(&handle, child, started_ctx, instance_id.clone())
        .await
        .expect("test fixture: commit_started should succeed");

    RunningInstanceGuard {
        handle,
        instance_id,
        pid,
    }
}
