//! Synchronous work over a node's staging directory, run off the async add.
//!
//! `node add` copies a node's sources into a staging directory under the
//! daemon's tmp root and generates the node's bindings into that copy. The
//! generation is synchronous and, for a node with many interfaces, deep: the
//! parser and the pretty-printer recurse per generated item. The add that
//! reaches it is polled inside the launch task, its per-machine group and
//! the phase wrappers, all on one worker thread's stack, so the work runs on
//! Tokio's blocking pool instead, on a thread with nothing beneath it.
//!
//! The staging directory is owned by an `Arc<WorkingDirGuard>` shared
//! between the add and the job: the last owner to let go removes it. A job
//! that has started cannot be stopped, so an add that is dropped while its
//! job runs (a launch phase timeout, a `--force` replacement) leaves the
//! directory to the job, which removes it when it returns. A job whose add
//! is gone before the job starts skips its work. The job never touches the
//! node stack: publishing the node is the add's own continuation, which a
//! dropped add never reaches.

use crate::services::node::common::panic_message;
use node_stack::WorkingDirGuard;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// What a job needs from the add that starts it, all owned: nothing in here
/// borrows the add's state or keeps its feedback channel open.
pub(super) struct StagedJob {
    /// `name:tag` of the node the staging directory holds, for the job's own
    /// diagnostics.
    pub(super) node_label: String,
    /// The add's action log, named in the job's diagnostics so a failure
    /// whose add is gone still points the operator at the log they have.
    pub(super) log_path: PathBuf,
    /// The staging directory. The job holds this clone until it returns.
    pub(super) staging: Arc<WorkingDirGuard>,
}

/// Runs `work` against the job's staging directory on the blocking pool and
/// returns its result.
///
/// A work error comes back as is; a panic inside `work` comes back as an
/// error naming the panic. Both are also logged from the job, with the node,
/// the staging directory and the add log, so a failure whose add has already
/// been dropped is still on record. The add reports the returned error
/// through its own action-log sink, once.
pub(super) async fn run_staged_job<W>(job: StagedJob, work: W) -> Result<(), String>
where
    W: FnOnce(&Path) -> Result<(), String> + Send + 'static,
{
    let cancel = CancellationToken::new();
    // Fires when this future is dropped: an add that timed out or was
    // superseded lets a job that has not started yet skip its work.
    let _cancel_on_drop = cancel.clone().drop_guard();
    let node_label = job.node_label.clone();
    tokio::task::spawn_blocking(move || job.run(work, cancel))
        .await
        .unwrap_or_else(|join_error| Err(describe_join_error(&node_label, join_error)))
}

impl StagedJob {
    fn run<W>(self, work: W, cancel: CancellationToken) -> Result<(), String>
    where
        W: FnOnce(&Path) -> Result<(), String>,
    {
        if cancel.is_cancelled() {
            debug!(
                node = %self.node_label,
                staging = %self.staging.path().display(),
                "staged work skipped: the add was dropped before the work started"
            );
            return Err(format!(
                "staged work for {} skipped: its add was dropped before the work started",
                self.node_label
            ));
        }

        let result = match catch_unwind(AssertUnwindSafe(|| work(self.staging.path()))) {
            Ok(result) => result,
            Err(payload) => Err(format!(
                "staged work for {} panicked: {}",
                self.node_label,
                panic_message(&*payload)
            )),
        };
        if let Err(message) = &result {
            tracing::error!(
                node = %self.node_label,
                staging = %self.staging.path().display(),
                add_log = %self.log_path.display(),
                "{message}"
            );
        }
        result
        // `self.staging` drops here: when the add is already gone, the
        // directory goes with it.
    }
}

/// A join error is the blocking pool's own refusal to run the job: a panic
/// that escaped the job's own catch, or a cancellation because the runtime
/// is shutting down before the job started.
fn describe_join_error(node_label: &str, error: tokio::task::JoinError) -> String {
    if error.is_panic() {
        return format!(
            "staged work for {node_label} panicked: {}",
            panic_message(&*error.into_panic())
        );
    }
    format!("staged work for {node_label} was cancelled by the runtime before it ran")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Weak;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::runtime::Runtime;
    use tokio::sync::{mpsc, oneshot};

    /// A staging directory under a scratch root, owned by a guard. The root
    /// outlives the guard so the test can observe the directory's absence.
    struct Staging {
        _root: tempfile::TempDir,
        path: PathBuf,
        guard: Arc<WorkingDirGuard>,
        weak: Weak<WorkingDirGuard>,
    }

    fn staging() -> Staging {
        let root = tempfile::tempdir().expect("scratch root");
        let path = root.path().join("stage");
        std::fs::create_dir_all(&path).expect("create staging dir");
        let guard = Arc::new(WorkingDirGuard::new(path.clone()));
        let weak = Arc::downgrade(&guard);
        Staging {
            _root: root,
            path,
            guard,
            weak,
        }
    }

    fn job(staging: &Staging) -> StagedJob {
        StagedJob {
            node_label: "waldo:v1".to_string(),
            log_path: PathBuf::from("/var/log/peppy/add/waldo_v1.log"),
            staging: Arc::clone(&staging.guard),
        }
    }

    /// A runtime whose blocking pool has exactly one thread, so a test can
    /// hold that thread and decide when a queued job starts, and can await a
    /// sentinel job to know a running one has returned.
    fn single_blocking_thread_runtime() -> Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("test runtime")
    }

    /// Completes once every blocking job submitted before it has returned:
    /// the pool's only thread takes jobs in submission order.
    async fn wait_for_blocking_pool_to_drain() {
        tokio::task::spawn_blocking(|| ())
            .await
            .expect("sentinel job runs");
    }

    /// Holds the pool's only thread until `release` is signalled, and
    /// resolves `occupied` once it holds it.
    fn occupy_blocking_thread() -> (std::sync::mpsc::Sender<()>, oneshot::Receiver<()>) {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (occupied_tx, occupied_rx) = oneshot::channel::<()>();
        tokio::task::spawn_blocking(move || {
            let _ = occupied_tx.send(());
            let _ = release_rx.recv();
        });
        (release_tx, occupied_rx)
    }

    #[tokio::test]
    async fn a_successful_job_leaves_the_staging_directory_to_the_add() {
        let staging = staging();
        let result = run_staged_job(job(&staging), |dir| {
            std::fs::write(dir.join("generated"), b"bindings").map_err(|e| e.to_string())
        })
        .await;

        assert_eq!(result, Ok(()));
        assert!(
            staging.path.join("generated").is_file(),
            "the job wrote into the staging directory"
        );
        assert_eq!(
            Arc::strong_count(&staging.guard),
            1,
            "a returned job holds no reference to the staging directory"
        );

        drop(staging.guard);
        assert!(staging.weak.upgrade().is_none());
        assert!(
            !staging.path.exists(),
            "the last owner removes the staging directory"
        );
    }

    #[tokio::test]
    async fn a_failing_job_returns_its_error_and_the_add_discards_the_staging() {
        let staging = staging();
        let result = run_staged_job(job(&staging), |_| {
            Err("Failed to generate peppygen library: boom".to_string())
        })
        .await;

        assert_eq!(
            result,
            Err("Failed to generate peppygen library: boom".to_string()),
            "a work error comes back verbatim"
        );
        assert_eq!(Arc::strong_count(&staging.guard), 1);

        drop(staging.guard);
        assert!(!staging.path.exists());
    }

    #[tokio::test]
    async fn a_panicking_job_returns_the_panic_and_releases_the_staging() {
        let staging = staging();
        let result = run_staged_job(job(&staging), |_| -> Result<(), String> {
            panic!("kaboom in the generator")
        })
        .await;

        let message = result.expect_err("a panic is reported as an error");
        assert!(
            message.contains("staged work for waldo:v1 panicked")
                && message.contains("kaboom in the generator"),
            "the error names the node and the panic: {message}"
        );
        assert_eq!(Arc::strong_count(&staging.guard), 1);

        drop(staging.guard);
        assert!(!staging.path.exists());
    }

    #[test]
    fn a_job_dropped_before_it_starts_skips_its_work_and_releases_the_staging() {
        let runtime = single_blocking_thread_runtime();
        runtime.block_on(async {
            let (release_blocker, occupied) = occupy_blocking_thread();
            occupied.await.expect("the blocker holds the pool's thread");

            let staging = staging();
            let ran = Arc::new(AtomicBool::new(false));
            let ran_in_job = Arc::clone(&ran);
            let mut pending = Box::pin(run_staged_job(job(&staging), move |_| {
                ran_in_job.store(true, Ordering::SeqCst);
                Ok(())
            }));
            // One poll submits the job; the pool's thread is held, so it queues.
            assert!(
                futures::poll!(pending.as_mut()).is_pending(),
                "a queued job cannot complete"
            );

            // The add times out: its future goes, and its share of the
            // staging directory with it.
            drop(pending);
            drop(staging.guard);
            assert!(
                staging.path.exists(),
                "the queued job still owns the staging directory"
            );

            release_blocker.send(()).expect("release the blocker");
            wait_for_blocking_pool_to_drain().await;

            assert!(
                !ran.load(Ordering::SeqCst),
                "a job whose add is gone skips its work"
            );
            assert!(staging.weak.upgrade().is_none());
            assert!(
                !staging.path.exists(),
                "the skipped job removes the staging directory it was left"
            );
        });
    }

    #[test]
    fn a_job_dropped_after_it_starts_keeps_its_staging_until_it_returns() {
        let runtime = single_blocking_thread_runtime();
        runtime.block_on(async {
            let staging = staging();
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let intact_while_running = Arc::new(AtomicBool::new(false));
            let intact_in_job = Arc::clone(&intact_while_running);
            let mut pending = Box::pin(run_staged_job(job(&staging), move |dir| {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                intact_in_job.store(dir.is_dir(), Ordering::SeqCst);
                Ok(())
            }));

            tokio::select! {
                _ = pending.as_mut() => panic!("the job cannot return before it is released"),
                started = started_rx => started.expect("the job signals its start"),
            }

            // The add is superseded: its future and its ownership go while
            // the job is mid-flight.
            drop(pending);
            drop(staging.guard);
            assert!(
                staging.weak.upgrade().is_some(),
                "the running job still owns the staging directory"
            );
            assert!(staging.path.exists());

            release_tx.send(()).expect("release the job");
            wait_for_blocking_pool_to_drain().await;

            assert!(
                intact_while_running.load(Ordering::SeqCst),
                "the staging directory was intact for the whole job"
            );
            assert!(staging.weak.upgrade().is_none());
            assert!(
                !staging.path.exists(),
                "the abandoned job removes the staging directory when it returns"
            );
        });
    }

    #[test]
    fn dropping_the_add_closes_its_feedback_channel_while_the_job_still_runs() {
        let runtime = single_blocking_thread_runtime();
        runtime.block_on(async {
            let staging = staging();
            let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<String>();
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let job = job(&staging);
            // The add owns the feedback sender, as `dispatch_node_add` does;
            // the job receives none of it.
            let mut add = Box::pin(async move {
                let _feedback_tx = feedback_tx;
                run_staged_job(job, move |_| {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                    Ok(())
                })
                .await
            });

            tokio::select! {
                _ = add.as_mut() => panic!("the job cannot return before it is released"),
                started = started_rx => started.expect("the job signals its start"),
            }

            drop(add);
            assert!(
                feedback_rx.recv().await.is_none(),
                "the feedback channel closes with the add, not with the job"
            );
            assert!(
                staging.weak.upgrade().is_some(),
                "the job is still running when the channel closes"
            );

            release_tx.send(()).expect("release the job");
            drop(staging.guard);
            wait_for_blocking_pool_to_drain().await;
            assert!(!staging.path.exists());
        });
    }
}
