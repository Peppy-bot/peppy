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

pub fn spawn_output_reader<R: Read + Send + 'static>(
    reader: R,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    stream: FeedbackStream,
    log_file: Arc<StdMutex<File>>,
    stderr_tail: Option<Arc<StdMutex<VecDeque<String>>>>,
) -> JoinHandle<()> {
    let stream_prefix = match stream {
        FeedbackStream::Stdout => "stdout",
        FeedbackStream::Stderr => "stderr",
    };

    tokio::task::spawn_blocking(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines().map_while(|r| r.ok()) {
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

    for handle in reader_handles {
        let _ = handle.await;
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
