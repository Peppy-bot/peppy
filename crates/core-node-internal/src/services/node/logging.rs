//! Shared logging helpers for node command handlers: per-action log
//! files, the structured error-line writer used from goal handlers, and
//! the stack-operations append log.

use chrono::Local;
use daemon_config::consts::PeppyDirs;
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
///
/// Rejects `log_filename` values that aren't a single path component so
/// that a caller mistakenly splicing client-controlled input (e.g. a
/// `RepoNode` name) into the filename cannot escape `log_dir`.
pub(crate) fn create_action_log_file(
    log_dir: &Path,
    log_filename: &str,
) -> std::result::Result<(Arc<StdMutex<File>>, PathBuf), String> {
    validate_log_filename(log_filename)?;

    std::fs::create_dir_all(log_dir)
        .map_err(|e| format!("Failed to create logs directory: {}", e))?;

    let log_path = log_dir.join(log_filename);
    let file = File::create(&log_path).map_err(|e| format!("Failed to create log file: {}", e))?;

    Ok((Arc::new(StdMutex::new(file)), log_path))
}

fn validate_log_filename(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("invalid log filename: {:?}", name));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(format!("invalid log filename: {:?}", name));
    }
    // Belt-and-suspenders: `file_name()` on the parsed path should yield
    // the same bytes for any safe, single-component filename.
    if Path::new(name).file_name().and_then(|n| n.to_str()) != Some(name) {
        return Err(format!("invalid log filename: {:?}", name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_log_filename_accepts_safe_names() {
        for name in [
            "foo.log",
            "camera_0.1.0_20260101_000000_000.log",
            "a-b_c.log",
        ] {
            assert!(
                validate_log_filename(name).is_ok(),
                "should accept `{name}`"
            );
        }
    }

    #[test]
    fn validate_log_filename_rejects_traversal_and_separators() {
        for name in [
            "",
            ".",
            "..",
            "../evil.log",
            "a/b.log",
            "a\\b.log",
            "a\0b.log",
        ] {
            assert!(
                validate_log_filename(name).is_err(),
                "should reject `{name}`"
            );
        }
    }
}
