//! I/O primitives for streaming child-process output to a feedback channel and
//! a log file. These are used by [`crate::node_stack::NodeEntity::build`] (and
//! by `core-node-internal` services for non-build process spawns).

use chrono::Local;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Maximum number of stderr lines to retain for error diagnostics.
/// Used by both the build (apptainer/archive) path and the start (node run) path.
pub const STDERR_TAIL_LINES: usize = 20;

#[derive(Clone, Copy)]
pub enum FeedbackStream {
    Stdout,
    Stderr,
}

pub struct FeedbackLine {
    pub stream: FeedbackStream,
    pub line: String,
}

/// Pushes a line into a bounded ring buffer of stderr output.
/// When the buffer is full, the oldest line is dropped.
pub fn push_stderr_line(buffer: &Arc<StdMutex<VecDeque<String>>>, line: &str) {
    let mut guard = buffer.lock().expect("stderr buffer lock poisoned");
    if guard.len() == STDERR_TAIL_LINES {
        guard.pop_front();
    }
    guard.push_back(line.to_string());
}

pub(crate) fn spawn_output_reader<R: Read + Send + 'static>(
    reader: R,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    stream: FeedbackStream,
    log_file: Arc<StdMutex<File>>,
    stderr_tail: Option<Arc<StdMutex<VecDeque<String>>>>,
) -> JoinHandle<std::io::Result<()>> {
    let stream_prefix = match stream {
        FeedbackStream::Stdout => "stdout",
        FeedbackStream::Stderr => "stderr",
    };

    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            let line = line?;
            // Always write to log file
            if let Ok(mut file) = log_file.lock() {
                let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
                let _ = writeln!(file, "[{}] [{}] {}", timestamp, stream_prefix, line);
            }

            if let Some(ref buffer) = stderr_tail {
                push_stderr_line(buffer, &line);
            }

            let _ = feedback_tx.send(FeedbackLine {
                stream,
                line: line.to_string(),
            });
        }
        Ok(())
    })
}

/// Streams stdout/stderr from a spawned child process to both the feedback
/// publisher and the log file. Optionally collects the last [`STDERR_TAIL_LINES`]
/// lines of stderr for error diagnostics.
///
/// Returns the process exit status and (if `collect_stderr_tail` is true) the
/// collected stderr tail lines.
pub async fn stream_child_output(
    mut child: std::process::Child,
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

    if let Some(stdout) = child.stdout.take() {
        reader_handles.push(spawn_output_reader(
            stdout,
            feedback_tx.clone(),
            FeedbackStream::Stdout,
            Arc::clone(&log_file),
            None,
        ));
    }

    if let Some(stderr) = child.stderr.take() {
        reader_handles.push(spawn_output_reader(
            stderr,
            feedback_tx.clone(),
            FeedbackStream::Stderr,
            Arc::clone(&log_file),
            stderr_tail.clone(),
        ));
    }

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .map_err(|e| format!("failed to wait for process: {}", e))?
        .map_err(|e| format!("failed to wait for process: {}", e))?;

    // Join reader tasks and surface the first error so build diagnostics receive
    // failures instead of masked truncated output. Process wait already returned
    // above, so we are guaranteed not to leak the child here.
    let mut reader_error: Option<String> = None;
    for handle in reader_handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if reader_error.is_none() {
                    reader_error = Some(format!("output reader I/O error: {}", e));
                }
            }
            Err(e) => {
                if reader_error.is_none() {
                    reader_error = Some(format!("output reader task join error: {}", e));
                }
            }
        }
    }
    if let Some(err) = reader_error {
        return Err(err);
    }

    let tail_lines = match stderr_tail {
        Some(ref tail) => tail
            .lock()
            .map(|t| t.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default(),
        None => Vec::new(),
    };

    Ok((status, tail_lines))
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
