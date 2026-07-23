//! Per-data-root daemon singleton: an exclusive advisory lock on
//! `<peppy root>/runtime/daemon.lock`, held for the whole daemon process
//! lifetime. flock-based via [`std::fs::File::try_lock`], so the kernel
//! releases the lock on any process exit, including SIGKILL, and acquisition
//! is atomic under racing boots. The lock file is deliberately never
//! unlinked: an unlink-and-reopen scheme would let two racing daemons lock
//! different inodes behind the same path. The file content is intentionally
//! empty; `daemon_state.json5` records the pid.

use std::fs::{File, TryLockError};

use daemon_config::consts::PeppyDirs;

use crate::error::{Error, Result};

const DAEMON_LOCK_FILENAME: &str = "daemon.lock";

/// Acquires the daemon singleton lock without blocking. The returned [`File`]
/// IS the lock: the caller must keep it alive for the whole daemon run,
/// spanning in-process restarts. A lock held by another process maps to
/// [`Error::AlreadyRunning`]; any other failure surfaces as an IO error.
pub(crate) fn acquire_daemon_singleton_lock(peppy_dirs: &PeppyDirs) -> Result<File> {
    let runtime_dir = peppy_dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime_dir)?;
    let lock_file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(runtime_dir.join(DAEMON_LOCK_FILENAME))?;
    match lock_file.try_lock() {
        Ok(()) => Ok(lock_file),
        Err(TryLockError::WouldBlock) => Err(Error::AlreadyRunning),
        Err(TryLockError::Error(e)) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_acquisition_reports_already_running_until_the_first_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let first = acquire_daemon_singleton_lock(&peppy_dirs).unwrap();
        let second = acquire_daemon_singleton_lock(&peppy_dirs);
        assert!(matches!(second, Err(Error::AlreadyRunning)));

        drop(first);
        acquire_daemon_singleton_lock(&peppy_dirs).unwrap();
    }

    #[test]
    fn distinct_data_roots_do_not_contend() {
        let first_tmp = tempfile::tempdir().unwrap();
        let second_tmp = tempfile::tempdir().unwrap();
        let first_dirs = PeppyDirs::new(first_tmp.path());
        let second_dirs = PeppyDirs::new(second_tmp.path());

        let _first = acquire_daemon_singleton_lock(&first_dirs).unwrap();
        let _second = acquire_daemon_singleton_lock(&second_dirs).unwrap();
    }
}
