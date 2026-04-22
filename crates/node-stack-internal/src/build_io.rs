//! I/O primitives for streaming child-process output to a feedback channel and
//! a log file.
//!
//! Two reader variants exist because build and start use different child types:
//!
//! - [`spawn_output_reader`] is **synchronous**: it consumes a `std::io::Read`
//!   inside `tokio::task::spawn_blocking`. This is the right shape for the
//!   build path, where the child is launched via `std::process::Command` (see
//!   `node_stack::build_steps::build_container_image`) and we wait for it to
//!   exit before continuing — no concurrent monitoring of `child.try_wait()`
//!   is needed.
//!
//! - [`spawn_output_reader_async`] uses `tokio::io::AsyncBufReadExt` on a
//!   `tokio::process::ChildStdout`/`ChildStderr`. This is required by the
//!   start path, where the daemon must call `child.try_wait()` concurrently
//!   with reading stdout/stderr (so it can detect early child exit while
//!   polling the ready/health signals). The reader tasks also outlive the
//!   `prepare_and_spawn` call — they remain alive as long as the spawned
//!   node is running so its stdout/stderr keeps streaming.

use chrono::Local;
use parking_lot::Mutex as StdMutex;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
#[cfg(test)]
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Maximum number of stderr lines to retain for error diagnostics.
/// Used by both the build (apptainer/archive) path and the start (node run) path.
pub const STDERR_TAIL_LINES: usize = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FeedbackStream {
    Stdout,
    Stderr,
    /// Out-of-band warning emitted by the daemon itself (not by the spawned
    /// child). Routed by feedback forwarders to a high-visibility sink so it
    /// is not buried under per-step scrolling output.
    Warning,
}

impl FeedbackStream {
    pub fn as_str(&self) -> &'static str {
        match self {
            FeedbackStream::Stdout => "stdout",
            FeedbackStream::Stderr => "stderr",
            FeedbackStream::Warning => "warning",
        }
    }
}

/// Writes a single feedback line to the log file in the canonical
/// `[timestamp] [stream] line` format. Errors are swallowed — log writes are
/// best-effort.
pub fn write_feedback_log_line(log_file: &Arc<StdMutex<File>>, stream: FeedbackStream, line: &str) {
    let mut file = log_file.lock();
    let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
    let _ = writeln!(file, "[{}] [{}] {}", timestamp, stream.as_str(), line);
}

/// Writes the canonical "executing a command" header to a log file in the
/// `[timestamp] Executing {label}: {cmd} (working_dir: {dir}[, k: v...])`
/// format used by every spawn-and-stream step. `extras` is appended as
/// comma-separated `key: value` pairs inside the trailing parenthesis. Errors
/// are swallowed — log writes are best-effort.
pub fn log_cmd_header(
    log_file: &Arc<StdMutex<File>>,
    label: &str,
    cmd: &str,
    working_dir: &Path,
    extras: &[(&str, &str)],
) {
    let mut file = log_file.lock();
    let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
    let mut line = format!(
        "[{}] Executing {}: {} (working_dir: {}",
        timestamp,
        label,
        cmd,
        working_dir.display()
    );
    for (k, v) in extras {
        line.push_str(&format!(", {}: {}", k, v));
    }
    line.push(')');
    let _ = writeln!(file, "{}", line);
    let _ = file.flush();
}

pub struct FeedbackLine {
    pub stream: FeedbackStream,
    pub line: String,
}

/// Hooks called by [`spawn_output_reader_async`] at meaningful moments in the
/// reader loop. This trait exists so `node-stack-internal` doesn't have to know
/// about the daemon's `FeedbackSync` quiescence-detection primitive — the
/// daemon implements `OutputReaderHooks` for its `FeedbackSync` and threads it
/// through `StartContext`.
///
/// Both methods default to no-ops so tests can use `NoOpHooks` directly.
pub trait OutputReaderHooks: Send + Sync {
    /// Called once when the first stdout line of the run arrives. Idempotent
    /// — the implementation is responsible for swallowing repeat calls.
    fn on_first_stdout_line(&self) {}
    /// Called after each line is successfully forwarded to the internal
    /// feedback channel (the one the reader writes to). The daemon's
    /// `FeedbackSync` increments its `read_count` here so that
    /// `flush_with_timeout` knows how many lines need to be drained.
    fn on_line_read(&self) {}
}

/// No-op implementation of [`OutputReaderHooks`] used by tests and any caller
/// that doesn't need quiescence tracking.
pub struct NoOpHooks;

impl OutputReaderHooks for NoOpHooks {}

/// Pushes a line into a bounded ring buffer of stderr output.
/// When the buffer is full, the oldest line is dropped.
pub fn push_stderr_line(buffer: &Arc<StdMutex<VecDeque<String>>>, line: &str) {
    let mut guard = buffer.lock();
    if guard.len() == STDERR_TAIL_LINES {
        guard.pop_front();
    }
    guard.push_back(line.to_string());
}

pub use process_wrap::tokio::ChildWrapper;
use process_wrap::tokio::{CommandWrap, ProcessGroup};

/// Spawns `cmd` as a process-group leader so [`stream_child_output`]'s
/// `KillGuard` can signal the entire subprocess tree on cancellation.
///
/// A single-pid kill would only reach the immediate child; descendants — e.g.
/// `sleep` inside a `sh -c "..."` wrapper, or `cargo` under a `make` target —
/// would be orphaned and keep the daemon's stdio pipes open until they exit
/// naturally, stalling cancellation. Making the child its own process-group
/// leader (PGID == its PID) lets `start_kill()` signal the whole group.
pub fn spawn_in_process_group(
    cmd: tokio::process::Command,
) -> std::io::Result<Box<dyn ChildWrapper>> {
    let mut wrap = CommandWrap::from(cmd);
    wrap.wrap(ProcessGroup::leader());
    wrap.spawn()
}

/// Streams stdout/stderr from a spawned child process to both the feedback
/// publisher and the log file. Optionally collects the last [`STDERR_TAIL_LINES`]
/// lines of stderr for error diagnostics.
///
/// Returns the process exit status and (if `collect_stderr_tail` is true) the
/// collected stderr tail lines.
///
/// **Cancellation contract:** the child must have been spawned via
/// [`spawn_in_process_group`] so that if this future is dropped before the
/// child exits, the internal `KillGuard` can SIGKILL the entire subprocess
/// tree.
pub async fn stream_child_output(
    mut child: Box<dyn ChildWrapper>,
    feedback_tx: &mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    collect_stderr_tail: bool,
) -> std::result::Result<(std::process::ExitStatus, Vec<String>), String> {
    let stderr_tail: Option<Arc<StdMutex<VecDeque<String>>>> = if collect_stderr_tail {
        Some(Arc::new(StdMutex::new(VecDeque::with_capacity(
            STDERR_TAIL_LINES,
        ))))
    } else {
        None
    };

    let mut reader_handles = Vec::new();

    // Drain stdout/stderr using tokio's async line reader so the wait
    // below can run concurrently and the future stays cancellation-safe.
    // `publish_enabled` is held permanently true and hooks are a no-op: the
    // build path has no quiescence tracking and no publish gate.
    let publish_enabled = Arc::new(AtomicBool::new(true));
    let hooks: Arc<dyn OutputReaderHooks> = Arc::new(NoOpHooks);
    if let Some(stdout) = child.stdout().take() {
        reader_handles.push(spawn_output_reader_async(
            stdout,
            feedback_tx.clone(),
            Arc::clone(&publish_enabled),
            Arc::clone(&hooks),
            FeedbackStream::Stdout,
            None,
            Arc::clone(&log_file),
        ));
    }

    if let Some(stderr) = child.stderr().take() {
        reader_handles.push(spawn_output_reader_async(
            stderr,
            feedback_tx.clone(),
            Arc::clone(&publish_enabled),
            Arc::clone(&hooks),
            FeedbackStream::Stderr,
            stderr_tail.clone(),
            Arc::clone(&log_file),
        ));
    }

    // Cancellation safety: if this future is dropped before `child.wait()`
    // returns, `KillGuard::drop` calls `start_kill`, which targets the whole
    // process group (see `spawn_in_process_group`) so `sh -c "..."` wrappers
    // and their descendants all die together.
    struct KillGuard<'a> {
        child: &'a mut dyn ChildWrapper,
        completed: bool,
    }
    impl Drop for KillGuard<'_> {
        fn drop(&mut self) {
            if !self.completed {
                let _ = self.child.start_kill();
            }
        }
    }
    let mut guard = KillGuard {
        child: child.as_mut(),
        completed: false,
    };
    let status = guard
        .child
        .wait()
        .await
        .map_err(|e| format!("failed to wait for process: {}", e))?;
    guard.completed = true;

    // Join reader tasks and surface the first error so build diagnostics
    // receive failures instead of masked truncated output. The tasks were
    // already spawned, so awaiting them sequentially here does not stall
    // concurrency — each completes as soon as its reader drains. Process
    // wait already returned above, so we are guaranteed not to leak the
    // child here.
    let mut reader_error: Option<String> = None;
    for handle in reader_handles {
        let outcome = match handle.await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("output reader I/O error: {}", e)),
            Err(e) => Some(format!("output reader task join error: {}", e)),
        };
        if reader_error.is_none() {
            reader_error = outcome;
        }
    }
    if let Some(err) = reader_error {
        return Err(err);
    }

    let tail_lines = match stderr_tail {
        Some(ref tail) => tail.lock().iter().cloned().collect::<Vec<_>>(),
        None => Vec::new(),
    };

    Ok((status, tail_lines))
}

/// Test-only blocking line reader retained so the existing
/// `spawn_output_reader_propagates_io_error` regression test can continue
/// exercising `BufRead::lines` error semantics.
#[cfg(test)]
fn spawn_output_reader<R: Read + Send + 'static>(
    reader: R,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    stream: FeedbackStream,
    log_file: Arc<StdMutex<File>>,
    stderr_tail: Option<Arc<StdMutex<VecDeque<String>>>>,
) -> JoinHandle<std::io::Result<()>> {
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            let line = line?;
            write_feedback_log_line(&log_file, stream, &line);
            if let Some(ref buffer) = stderr_tail {
                push_stderr_line(buffer, &line);
            }
            let _ = feedback_tx.send(FeedbackLine { stream, line });
        }
        Ok(())
    })
}

/// Async sibling of [`spawn_output_reader`], used by both the start path
/// (via `start_steps`) and the build path (via [`stream_child_output`]).
///
/// Reads lines from a `tokio::io::AsyncRead` (typically a
/// `tokio::process::ChildStdout`/`ChildStderr`), writes each line to the log
/// file, captures stderr lines into the optional `stderr_buffer`, and forwards
/// each line over `feedback_tx` (gated by `publish_enabled`).
///
/// The reader task is spawned via `tokio::spawn` and remains alive as long as
/// the underlying reader yields data. On the start path, this means the reader
/// continues running past the return of `prepare_and_spawn`/`commit_started`
/// for as long as the spawned node is alive — which is the desired behavior so
/// the daemon keeps streaming the running node's stdout/stderr.
pub fn spawn_output_reader_async<R>(
    reader: R,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    publish_enabled: Arc<AtomicBool>,
    hooks: Arc<dyn OutputReaderHooks>,
    stream: FeedbackStream,
    stderr_buffer: Option<Arc<StdMutex<VecDeque<String>>>>,
    log_file: Arc<StdMutex<File>>,
) -> JoinHandle<std::io::Result<()>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(reader).lines();

        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(e) => return Err(e),
            };

            write_feedback_log_line(&log_file, stream, &line);

            // Signal when the first stdout line arrives so container quiescence
            // detection can wait for the runscript to actually produce output.
            if matches!(stream, FeedbackStream::Stdout) {
                hooks.on_first_stdout_line();
            }

            // Always capture stderr for error diagnostics, regardless of publish state
            if matches!(stream, FeedbackStream::Stderr)
                && let Some(buffer) = &stderr_buffer
            {
                push_stderr_line(buffer, &line);
            }

            if !publish_enabled.load(Ordering::Acquire) {
                continue;
            }

            if feedback_tx.send(FeedbackLine { stream, line }).is_ok() {
                hooks.on_line_read();
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use tempfile::NamedTempFile;

    /// Reader that yields `prefix` then errors. Used to verify that
    /// `spawn_output_reader` propagates I/O errors instead of swallowing them.
    struct ErroringReader {
        prefix: Vec<u8>,
        pos: usize,
        errored: bool,
    }

    impl ErroringReader {
        fn new(prefix: &[u8]) -> Self {
            Self {
                prefix: prefix.to_vec(),
                pos: 0,
                errored: false,
            }
        }
    }

    impl io::Read for ErroringReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos < self.prefix.len() {
                let n = (self.prefix.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.prefix[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if !self.errored {
                self.errored = true;
                return Err(io::Error::other("synthetic read error"));
            }
            Ok(0)
        }
    }

    #[tokio::test]
    async fn spawn_output_reader_propagates_io_error() {
        let log_file = Arc::new(StdMutex::new(
            NamedTempFile::new()
                .expect("temp log")
                .reopen()
                .expect("reopen"),
        ));
        let (tx, _rx) = mpsc::unbounded_channel();
        // No newline at the end of the prefix → BufRead::lines yields the
        // partial line *and then* surfaces the next read error.
        let reader = ErroringReader::new(b"first line\npartial");
        let handle = spawn_output_reader(reader, tx, FeedbackStream::Stdout, log_file, None);
        let result = handle.await.expect("join should succeed");
        assert!(
            result.is_err(),
            "spawn_output_reader should propagate the synthetic read error"
        );
    }
}
