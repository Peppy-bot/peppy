//! I/O primitives for streaming child-process output to a feedback channel and
//! a log file.
//!
//! Two reader variants exist because build and start use different child types:
//!
//! - [`spawn_output_reader`] is **synchronous**: it consumes a `std::io::Read`
//!   inside `tokio::task::spawn_blocking`. This is the right shape for the
//!   build path, where the child is launched via `std::process::Command` (see
//!   `node_stack::build_steps::build_container_image`) and we wait for it to
//!   exit before continuing; no concurrent monitoring of `child.try_wait()`
//!   is needed.
//!
//! - [`spawn_output_reader_async`] reads raw bytes from a
//!   `tokio::process::ChildStdout`/`ChildStderr` and splits them into lines on
//!   `\n`, `\r`, and `\r\n` (see [`LineSplitter`]), so TTY-style `\r` progress
//!   repaints surface as feedback instead of accumulating silently inside one
//!   never-terminated line. This variant is required by the
//!   start path, where the daemon must call `child.try_wait()` concurrently
//!   with reading stdout/stderr (so it can detect early child exit while
//!   polling the ready/health signals). The reader tasks also outlive the
//!   `prepare_and_spawn` call; they remain alive as long as the spawned
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
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Maximum number of stderr lines to retain for error diagnostics.
/// Used by both the build (apptainer/archive) path and the start (node run) path.
pub const STDERR_TAIL_LINES: usize = 20;

pub use core_node_api::encoding::FeedbackStream;

/// Writes a single feedback line to the log file in the canonical
/// `[timestamp] [stream] line` format. Errors are swallowed; log writes are
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
/// are swallowed; log writes are best-effort.
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

/// Wraps `tx` as a `&str` line sink that forwards each line as a stdout
/// [`FeedbackLine`], the adapter shape blocking checkout/materialize progress
/// callbacks expect. Send errors are ignored: a closed channel means the
/// consumer is gone and the lines have nowhere to go.
pub fn stdout_line_sender(
    tx: mpsc::UnboundedSender<FeedbackLine>,
) -> impl Fn(&str) + Send + Sync + 'static {
    move |line: &str| {
        let _ = tx.send(FeedbackLine {
            stream: FeedbackStream::Stdout,
            line: line.to_owned(),
        });
    }
}

/// Renders byte counts in the compact `KB`/`MB`/`GB` form shared by peppy's
/// progress lines (clone/fetch transfer reports, build disk-growth feedback).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Hooks called by [`spawn_output_reader_async`] at meaningful moments in the
/// reader loop. This trait exists so `node-stack-internal` does not have to
/// know about the daemon's `FeedbackSync` drain primitive: the daemon
/// implements `OutputReaderHooks` for its `FeedbackSync` and threads it through
/// `StartContext`.
///
/// All methods default to no-ops so tests and the build path can use
/// `NoOpHooks` directly.
pub trait OutputReaderHooks: Send + Sync {
    /// Called once when the first stdout line of the run arrives. Idempotent:
    /// the implementation is responsible for swallowing repeat calls.
    fn on_first_stdout_line(&self) {}
    /// Called after each line is successfully forwarded to the internal
    /// feedback channel (the one the reader writes to). The daemon's
    /// `FeedbackSync` counts these so its drain primitive knows how many lines
    /// still need to reach the external feedback stream.
    fn on_line_read(&self) {}
    /// Called synchronously by the spawner, once per reader, before the reader
    /// task is launched. Lets the daemon count the readers it must wait on
    /// without racing their startup.
    fn on_reader_registered(&self) {}
    /// Called when the reader has consumed every complete line currently
    /// buffered and its next read would block. This is a positive signal that
    /// the pipe is drained as of now, which the daemon's drain primitive relies
    /// on instead of inferring quiescence from the absence of reads (a starved
    /// reader task could otherwise look quiescent while data waits unread).
    fn on_reader_idle(&self) {}
    /// Called when the reader obtains a fresh line after having been idle, i.e.
    /// new output arrived. Pairs with [`Self::on_reader_idle`].
    fn on_reader_active(&self) {}
    /// Called once when the reader task exits (EOF or read error). The argument
    /// reports whether the reader was idle at exit so the daemon can keep its
    /// live and idle reader counts consistent.
    fn on_reader_exit(&self, _was_idle: bool) {}
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
/// A single-pid kill would only reach the immediate child; descendants (e.g.
/// `sleep` inside a `sh -c "..."` wrapper, or `cargo` under a `make` target)
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
/// [`spawn_in_process_group`] so that the entire subprocess tree can be
/// signaled. When `cancel_token` fires before the child exits, the child's
/// process group is SIGKILL'd *and reaped* (awaited) before this returns
/// `Err("build cancelled")`, so no zombie lingers and the working dir is free
/// for a superseding build. If this future is instead dropped outright, the
/// `KillGuard` still SIGKILLs the group as a fallback (without reaping).
pub async fn stream_child_output(
    mut child: Box<dyn ChildWrapper>,
    feedback_tx: &mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    collect_stderr_tail: bool,
    cancel_token: &CancellationToken,
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
    let status = tokio::select! {
        biased;
        wait_result = guard.child.wait() => {
            wait_result.map_err(|e| format!("failed to wait for process: {}", e))?
        }
        _ = cancel_token.cancelled() => {
            // SIGKILL the whole process group, then *await* the child so it is
            // reaped before we return; the superseding build must not race a
            // dying subprocess over the same working dir.
            let _ = guard.child.start_kill();
            let _ = guard.child.wait().await;
            guard.completed = true;
            return Err("build cancelled".to_string());
        }
    };
    guard.completed = true;

    // Join reader tasks and surface the first error so build diagnostics
    // receive failures instead of masked truncated output. The tasks were
    // already spawned, so awaiting them sequentially here does not stall
    // concurrency; each completes as soon as its reader drains. Process
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

/// How a fragment produced by [`LineSplitter`] was terminated, which decides
/// whether it forwards unconditionally or is subject to repaint coalescing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineTerminator {
    /// `\n` or `\r\n`: a real line. Always forwarded — this is the
    /// compatibility guarantee that keeps output containing no bare `\r`
    /// byte-identical to the previous `BufRead::lines` behavior.
    Newline,
    /// A bare `\r`: a terminal repaint (progress bars). Forwarded at most
    /// once per coalescing interval.
    CarriageReturn,
}

/// Splits a byte stream into fragments on `\n`, `\r`, and `\r\n` (one break),
/// decoding each fragment as lossy UTF-8.
///
/// `apptainer` (and the tools it drives) repaint progress with bare `\r`, so a
/// `\n`-only splitter buffers the whole progress stream as one giant
/// never-terminated line and the feedback channel sees nothing. Splitting on
/// `\r` as well surfaces those repaints as fragments.
///
/// A fragment ending in `\r` is held until the next byte arrives: only then is
/// it known whether the terminator was a bare `\r` (repaint) or the first half
/// of `\r\n` (a real line ending, which must forward unconditionally).
#[derive(Default)]
pub(crate) struct LineSplitter {
    /// Bytes of the fragment currently being accumulated.
    partial: Vec<u8>,
    /// A `\r` was seen and the fragment is complete, but its classification
    /// (bare `\r` vs `\r\n`) waits on the next byte.
    pending_cr: bool,
}

impl LineSplitter {
    /// Feeds `bytes` through the splitter, invoking `emit` for each completed
    /// fragment in order.
    pub(crate) fn push(&mut self, bytes: &[u8], mut emit: impl FnMut(String, LineTerminator)) {
        for &byte in bytes {
            if self.pending_cr {
                self.pending_cr = false;
                let line = self.take_partial();
                if byte == b'\n' {
                    emit(line, LineTerminator::Newline);
                    continue;
                }
                emit(line, LineTerminator::CarriageReturn);
            }
            match byte {
                b'\n' => {
                    let line = self.take_partial();
                    emit(line, LineTerminator::Newline);
                }
                b'\r' => self.pending_cr = true,
                _ => self.partial.push(byte),
            }
        }
    }

    /// The trailing fragment at EOF, if any: an unterminated partial line, or
    /// a fragment whose `\r` classification never resolved. Callers forward it
    /// unconditionally so the last repaint (the "100%" that matters) lands.
    pub(crate) fn finish(&mut self) -> Option<String> {
        if self.pending_cr || !self.partial.is_empty() {
            self.pending_cr = false;
            Some(self.take_partial())
        } else {
            None
        }
    }

    fn take_partial(&mut self) -> String {
        // Reuse the buffer's allocation on the (overwhelmingly common) valid
        // UTF-8 path; only invalid bytes pay for a lossy re-encode.
        match String::from_utf8(std::mem::take(&mut self.partial)) {
            Ok(line) => line,
            Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
        }
    }
}

/// Minimum spacing between forwarded bare-`\r` repaint fragments.
///
/// A TTY-style progress bar can repaint hundreds of times per second; at this
/// cap the scrolling feedback still animates while the channel, log file, and
/// idle-clock notifications see at most two lines per second from repaints.
pub(crate) const REPAINT_FORWARD_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// Decides which fragments forward: `\n`-terminated lines always do, bare-`\r`
/// repaints only when at least [`REPAINT_FORWARD_MIN_INTERVAL`] has passed
/// since the last forwarded line. Suppressed fragments are dropped entirely
/// (no log write, no stderr tail), matching their pre-`\r`-splitting
/// invisibility.
#[derive(Default)]
pub(crate) struct RepaintCoalescer {
    last_forward: Option<Instant>,
}

impl RepaintCoalescer {
    /// Whether a fragment with `terminator` observed at `now` forwards.
    pub(crate) fn should_forward(&mut self, terminator: LineTerminator, now: Instant) -> bool {
        let forward = match terminator {
            LineTerminator::Newline => true,
            LineTerminator::CarriageReturn => self
                .last_forward
                .is_none_or(|last| now.duration_since(last) >= REPAINT_FORWARD_MIN_INTERVAL),
        };
        if forward {
            self.last_forward = Some(now);
        }
        forward
    }
}

/// Async sibling of [`spawn_output_reader`], used by both the start path
/// (via `start_steps`) and the build path (via [`stream_child_output`]).
///
/// Reads output from a `tokio::io::AsyncRead` (typically a
/// `tokio::process::ChildStdout`/`ChildStderr`), splits it into lines on `\n`,
/// `\r`, and `\r\n` (see [`LineSplitter`]), writes each forwarded line to the
/// log file, captures forwarded stderr lines into the optional
/// `stderr_buffer`, and forwards each over `feedback_tx` (gated by
/// `publish_enabled`). `\n`-terminated lines always forward; bare-`\r` repaint
/// fragments are coalesced (see [`RepaintCoalescer`]) and suppressed ones skip
/// the log file and stderr tail too. The final fragment before EOF always
/// forwards so a progress stream's last repaint lands.
///
/// The reader task is spawned via `tokio::spawn` and remains alive as long as
/// the underlying reader yields data. On the start path, this means the reader
/// continues running past the return of `prepare_and_spawn`/`commit_started`
/// for as long as the spawned node is alive, which is the desired behavior so
/// the daemon keeps streaming the running node's stdout/stderr.
pub fn spawn_output_reader_async<R>(
    mut reader: R,
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
    use tokio::io::AsyncReadExt;

    tokio::spawn(async move {
        let mut splitter = LineSplitter::default();
        let mut coalescer = RepaintCoalescer::default();
        let mut buf = vec![0u8; 8192];
        // Tracks whether we have already signalled idle for the current quiet
        // stretch, so `on_reader_idle` fires once per active-to-idle transition.
        let mut idle = false;

        // Forwards one line: log file, first-stdout signal, stderr tail,
        // publish gate, feedback channel. Suppressed repaints never reach it.
        let forward_line = |line: String| {
            write_feedback_log_line(&log_file, stream, &line);

            // Signal when the first stdout line arrives so container drains can
            // wait for the runscript to actually produce output.
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
                return;
            }

            if feedback_tx.send(FeedbackLine { stream, line }).is_ok() {
                hooks.on_line_read();
            }
        };

        let outcome = loop {
            // Poll the read future once without committing to awaiting it.
            // A `Pending` result means every byte currently buffered has been
            // consumed, so the reader is caught up: signal idle before
            // blocking for more. A reader that is slow to be scheduled never
            // reaches this point, so it never falsely reports being drained.
            let next = reader.read(&mut buf);
            tokio::pin!(next);
            let read = match std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await {
                Poll::Ready(read) => read,
                Poll::Pending => {
                    if !idle {
                        idle = true;
                        hooks.on_reader_idle();
                    }
                    next.await
                }
            };

            let n = match read {
                Ok(0) => {
                    // EOF: the trailing fragment always forwards, so the last
                    // repaint of a progress stream is never lost.
                    if let Some(line) = splitter.finish() {
                        forward_line(line);
                    }
                    break Ok(());
                }
                Ok(n) => n,
                Err(e) => break Err(e),
            };

            // Data arrived: the reader is active again until it next blocks.
            if idle {
                idle = false;
                hooks.on_reader_active();
            }

            // One timestamp per chunk: every fragment in it arrived together.
            let now = Instant::now();
            splitter.push(&buf[..n], |line, terminator| {
                if coalescer.should_forward(terminator, now) {
                    forward_line(line);
                }
            });
        };

        // Report exit so the daemon stops counting this reader as live.
        hooks.on_reader_exit(idle);
        outcome
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

    // -- LineSplitter --

    fn split_all(chunks: &[&[u8]]) -> Vec<(String, Option<LineTerminator>)> {
        let mut splitter = LineSplitter::default();
        let mut out = Vec::new();
        for chunk in chunks {
            splitter.push(chunk, |line, term| out.push((line, Some(term))));
        }
        if let Some(line) = splitter.finish() {
            out.push((line, None));
        }
        out
    }

    #[test]
    fn splitter_breaks_on_cr_lf_and_crlf() {
        let got = split_all(&[b"a\rb\r\nc\nd"]);
        assert_eq!(
            got,
            vec![
                ("a".to_string(), Some(LineTerminator::CarriageReturn)),
                ("b".to_string(), Some(LineTerminator::Newline)),
                ("c".to_string(), Some(LineTerminator::Newline)),
                ("d".to_string(), None),
            ]
        );
    }

    #[test]
    fn splitter_resolves_crlf_split_across_chunks() {
        // The `\r` classification must wait for the `\n` in the next chunk:
        // classifying eagerly would demote a real `\r\n` line ending to a
        // throttleable repaint.
        let got = split_all(&[b"x\r", b"\ny"]);
        assert_eq!(
            got,
            vec![
                ("x".to_string(), Some(LineTerminator::Newline)),
                ("y".to_string(), None),
            ]
        );
    }

    #[test]
    fn splitter_yields_a_trailing_cr_fragment_at_eof() {
        let got = split_all(&[b"50%\r"]);
        assert_eq!(got, vec![("50%".to_string(), None)]);
    }

    #[test]
    fn splitter_preserves_newline_only_streams_verbatim() {
        // The compatibility guarantee for the run path: without bare `\r`,
        // fragments match `BufRead::lines` exactly (including empty lines and
        // a final unterminated line).
        let got = split_all(&[b"a\n", b"\nb"]);
        assert_eq!(
            got,
            vec![
                ("a".to_string(), Some(LineTerminator::Newline)),
                ("".to_string(), Some(LineTerminator::Newline)),
                ("b".to_string(), None),
            ]
        );
    }

    // -- RepaintCoalescer --

    #[test]
    fn coalescer_throttles_repaints_but_never_newlines() {
        let t0 = std::time::Instant::now();
        let mut coalescer = RepaintCoalescer::default();

        // First repaint forwards; a rapid follow-up is suppressed.
        assert!(coalescer.should_forward(LineTerminator::CarriageReturn, t0));
        assert!(!coalescer.should_forward(
            LineTerminator::CarriageReturn,
            t0 + Duration::from_millis(10)
        ));
        // A newline always forwards, and resets the spacing clock…
        assert!(coalescer.should_forward(LineTerminator::Newline, t0 + Duration::from_millis(20)));
        assert!(!coalescer.should_forward(
            LineTerminator::CarriageReturn,
            t0 + Duration::from_millis(30)
        ));
        // …and a repaint after the interval forwards again.
        assert!(coalescer.should_forward(
            LineTerminator::CarriageReturn,
            t0 + Duration::from_millis(20) + REPAINT_FORWARD_MIN_INTERVAL
        ));
    }

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024 / 2), "1.5 MB");
        assert_eq!(format_bytes(19 * 1024 * 1024 * 1024 / 10), "1.9 GB");
    }

    // -- spawn_output_reader_async end-to-end --

    async fn read_all_lines(input: Vec<u8>) -> Vec<String> {
        let log_file = Arc::new(StdMutex::new(
            NamedTempFile::new()
                .expect("temp log")
                .reopen()
                .expect("reopen"),
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_output_reader_async(
            std::io::Cursor::new(input),
            tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(NoOpHooks) as Arc<dyn OutputReaderHooks>,
            FeedbackStream::Stdout,
            None,
            log_file,
        );
        handle
            .await
            .expect("join should succeed")
            .expect("read should succeed");
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line.line);
        }
        lines
    }

    #[tokio::test]
    async fn async_reader_coalesces_repaints_and_delivers_the_final_one() {
        // All repaints arrive within one coalescing interval: the first one
        // forwards, the middle is suppressed, and the trailing fragment at
        // EOF always lands so the last "100%" is never lost.
        let lines = read_all_lines(b"10%\r20%\r100%".to_vec()).await;
        assert_eq!(lines, vec!["10%".to_string(), "100%".to_string()]);
    }

    #[tokio::test]
    async fn async_reader_forwards_every_newline_terminated_line() {
        let lines = read_all_lines(b"line1\nline2\r\n\nline3".to_vec()).await;
        assert_eq!(
            lines,
            vec![
                "line1".to_string(),
                "line2".to_string(),
                "".to_string(),
                "line3".to_string(),
            ]
        );
    }
}
