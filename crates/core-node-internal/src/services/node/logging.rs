//! Shared logging helpers for node command handlers: per-action log
//! files, the structured error-line writer used from goal handlers, and
//! the stack-operations append log.

use chrono::Local;
use config::consts::PeppyDirs;
use parking_lot::Mutex as StdMutex;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Write an error message to the node's log file with a timestamp.
///
/// Best-effort: silently ignores lock/write failures since the error is also
/// returned in the result encoding.
pub(crate) fn write_error_to_log(log_file: &Arc<StdMutex<File>>, error_msg: &str) {
    let mut file = log_file.lock();
    let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
    let _ = writeln!(file, "[{}] [error] {}", timestamp, error_msg);
    let _ = file.flush();
}

/// Appends a timestamped entry to the stack operations log.
///
/// Best-effort: silently ignores I/O failures since the operation it
/// describes has already completed.
pub(crate) fn append_stack_log(peppy_dirs: &PeppyDirs, message: &str) {
    let path = peppy_dirs.stack_log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
    let _ = writeln!(file, "[{}] {}", timestamp, message);
}

/// Creates a log file inside `log_dir` with the given filename.
///
/// Creates the directory tree if it doesn't exist. Returns the log file
/// handle (wrapped for concurrent access) and its path.
pub(crate) fn create_action_log_file(
    log_dir: &Path,
    log_filename: &str,
) -> std::result::Result<(Arc<StdMutex<File>>, PathBuf), String> {
    std::fs::create_dir_all(log_dir)
        .map_err(|e| format!("Failed to create logs directory: {}", e))?;

    let log_path = log_dir.join(log_filename);
    let file = File::create(&log_path).map_err(|e| format!("Failed to create log file: {}", e))?;

    Ok((Arc::new(StdMutex::new(file)), log_path))
}
