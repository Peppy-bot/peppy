use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use config::consts::{app_env, AppEnv};

pub const PID_FILE_ENV: &str = "PEPPY_SERVE_PID_FILE";

#[derive(Debug)]
pub struct PidLock {
    path: PathBuf,
    pid: u32,
}

#[derive(Debug)]
pub enum PidLockError {
    Io(io::Error),
    AlreadyRunning(u32),
}

impl From<io::Error> for PidLockError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl PidLock {
    pub fn acquire() -> Result<Self, PidLockError> {
        let path = Self::lock_file_path();
        Self::acquire_at(path)
    }

    fn acquire_at(path: PathBuf) -> Result<Self, PidLockError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let pid = std::process::id();

        loop {
            match Self::try_create_lock(&path, pid) {
                Ok(()) => return Ok(Self { path, pid }),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    match Self::read_pid(&path) {
                        Ok(Some(existing_pid)) => {
                            if process_is_running(existing_pid) {
                                return Err(PidLockError::AlreadyRunning(existing_pid));
                            }
                            let _ = fs::remove_file(&path);
                            continue;
                        }
                        Ok(None) => {
                            let _ = fs::remove_file(&path);
                            continue;
                        }
                        Err(read_err) => {
                            return Err(read_err);
                        }
                    }
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn try_create_lock(path: &Path, pid: u32) -> Result<(), io::Error> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?;
        write!(file, "{}", pid)?;
        file.sync_all()?;
        Ok(())
    }

    fn read_pid(path: &Path) -> Result<Option<u32>, PidLockError> {
        match fs::read_to_string(path) {
            Ok(contents) => match contents.trim().parse::<u32>() {
                Ok(pid) => Ok(Some(pid)),
                Err(_) => Ok(None),
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn lock_file_path() -> PathBuf {
        if let Some(value) = std::env::var_os(PID_FILE_ENV) {
            return PathBuf::from(value);
        }

        match app_env() {
            AppEnv::Prod => PathBuf::from("/var/run/peppy/peppy.pid"),
            AppEnv::Dev => std::env::temp_dir().join("peppy").join("peppy.pid"),
        }
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        if let Ok(contents) = fs::read_to_string(&self.path) {
            if contents.trim() != self.pid.to_string() {
                return;
            }
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let pid = pid as libc::pid_t;
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        true
    } else {
        matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        )
    }
}

#[cfg(not(unix))]
fn process_is_running(_pid: u32) -> bool {
    true
}
