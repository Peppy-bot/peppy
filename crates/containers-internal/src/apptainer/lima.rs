use super::super::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) const LIMA_INSTANCE: &str = env!("LIMA_INSTANCE");
pub(crate) const LIMA_TEMPLATE: &str = env!("LIMA_TEMPLATE");
pub(crate) const MIN_LIMA_VERSION: (u32, u32, u32) = (2, 0, 0);

/// Single-quote a path for safe embedding in a shell command string.
fn shell_escape(path: &Path) -> String {
    // Replace any single quotes in the path with the '\'' idiom, then wrap in single quotes.
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Check that the bundled Lima version meets the minimum requirement.
pub(crate) fn check_lima_version(limactl: &Path) -> Result<()> {
    let output = Command::new(limactl).arg("--version").output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::LimaVersionCheckFailed(format!(
            "`{} --version` exited with {}{}",
            limactl.display(),
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        )));
    }

    let version_str = String::from_utf8_lossy(&output.stdout);
    let found = parse_lima_version(&version_str).unwrap_or_default();

    if found < MIN_LIMA_VERSION {
        return Err(Error::LimaVersionTooOld {
            found: format!("{}.{}.{}", found.0, found.1, found.2),
            minimum: format!(
                "{}.{}.{}",
                MIN_LIMA_VERSION.0, MIN_LIMA_VERSION.1, MIN_LIMA_VERSION.2
            ),
        });
    }

    Ok(())
}

/// Ensure the peppy Lima instance exists and is running.
///
/// * If the instance does not exist, create and start it with `template`.
/// * If it exists but is stopped, start it.
/// * If it is already running, this is a no-op.
pub(crate) fn ensure_lima_instance(limactl: &Path, lima_home: &Path, template: &str) -> Result<()> {
    std::fs::create_dir_all(lima_home).map_err(|e| {
        Error::LimaInstanceError(format!(
            "failed to create LIMA_HOME {}: {e}",
            lima_home.display()
        ))
    })?;

    let list_output = Command::new(limactl)
        .env("LIMA_HOME", lima_home)
        .args(["list", "--format", "{{.Status}}", LIMA_INSTANCE])
        .output();

    let instance_status = match &list_output {
        Ok(o) if o.status.success() => {
            let status = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if status.is_empty() {
                None
            } else {
                Some(status)
            }
        }
        _ => None,
    };

    match instance_status.as_deref() {
        Some("Running") => Ok(()),
        Some(_) => {
            tracing::info!("Starting Lima {} instance...", LIMA_INSTANCE);
            let start = Command::new(limactl)
                .env("LIMA_HOME", lima_home)
                .args(["start", LIMA_INSTANCE])
                .output()?;

            if start.status.success() {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&start.stderr);
                Err(Error::LimaInstanceError(format!(
                    "failed to start Lima {} instance: {stderr}",
                    LIMA_INSTANCE
                )))
            }
        }
        None => {
            tracing::info!(
                "Creating Lima {} instance with {} (first run, may take a few minutes)...",
                LIMA_INSTANCE,
                template
            );
            let name_flag = format!("--name={}", LIMA_INSTANCE);
            let create = Command::new(limactl)
                .env("LIMA_HOME", lima_home)
                .args(["start", &name_flag, "--tty=false", template])
                .output()?;

            if create.status.success() {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&create.stderr);
                Err(Error::LimaInstanceError(format!(
                    "failed to create Lima {} instance: {stderr}",
                    LIMA_INSTANCE
                )))
            }
        }
    }
}

/// Ensure the apptainer installation is available inside the Lima VM guest.
///
/// Syncs the host-side installation to `/tmp/peppy/apptainer/` in the guest.
/// This path lives on the guest's native writable filesystem, avoiding Lima's
/// read-only home directory mount. A version-stamped marker file avoids
/// redundant copies on subsequent invocations.
///
/// Returns the guest-side path to `bin/apptainer`.
pub(crate) fn ensure_guest_apptainer(
    host_dir: &Path,
    limactl: &Path,
    lima_home: &Path,
    instance: &str,
) -> Result<PathBuf> {
    let guest_dir = PathBuf::from("/tmp/peppy/apptainer");
    let guest_bin = guest_dir.join("bin/apptainer");

    let version = option_env!("APPTAINER_VERSION").unwrap_or("unknown");
    let marker_name = format!(".peppy-sync-{version}");
    let marker_path = guest_dir.join(&marker_name);

    // Fast path: check if the version marker exists (sub-second limactl call).
    let marker_exists = match Command::new(limactl)
        .env("LIMA_HOME", lima_home)
        .args(["shell", instance, "--", "test", "-f"])
        .arg(&marker_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => status.success(),
        Err(err) => {
            tracing::warn!(
                error = %err,
                limactl = %limactl.display(),
                instance,
                marker = %marker_path.display(),
                "Failed to check guest marker; forcing full sync"
            );
            false
        }
    };

    if marker_exists {
        return Ok(guest_bin);
    }

    tracing::info!("Syncing apptainer installation to Lima VM guest...");

    // Remove stale installation in guest.
    let _ = Command::new(limactl)
        .env("LIMA_HOME", lima_home)
        .args(["shell", instance, "--", "rm", "-rf"])
        .arg(&guest_dir)
        .status();

    // Create the target directory in the guest.
    let mkdir = Command::new(limactl)
        .env("LIMA_HOME", lima_home)
        .args(["shell", instance, "--", "mkdir", "-p"])
        .arg(&guest_dir)
        .output()
        .map_err(|e| Error::LimaSyncFailed(format!("failed to create guest directory: {e}")))?;

    if !mkdir.status.success() {
        let stderr = String::from_utf8_lossy(&mkdir.stderr);
        return Err(Error::LimaSyncFailed(format!(
            "mkdir in guest returned {}: {stderr}",
            mkdir.status
        )));
    }

    // Copy host installation to guest via tar pipe.
    // `limactl copy -r` is unreliable with long or special-character paths,
    // so we tar on the host and untar in the guest through a pipe.
    let limactl_str = limactl.to_string_lossy();
    let lima_home_str = lima_home.to_string_lossy();
    let tar_pipe = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "tar -cf - -C {} . | LIMA_HOME={} {} shell {} -- tar -xf - -C {}",
            shell_escape(host_dir),
            shell_escape(Path::new(&*lima_home_str)),
            shell_escape(Path::new(&*limactl_str)),
            instance,
            guest_dir.display(),
        ))
        .output()
        .map_err(|e| Error::LimaSyncFailed(format!("tar pipe to guest failed: {e}")))?;

    if !tar_pipe.status.success() {
        let stderr = String::from_utf8_lossy(&tar_pipe.stderr);
        return Err(Error::LimaSyncFailed(format!(
            "tar pipe to guest returned {}: {stderr}",
            tar_pipe.status
        )));
    }

    // Write the version marker so we skip the sync next time.
    let _ = Command::new(limactl)
        .env("LIMA_HOME", lima_home)
        .args(["shell", instance, "--", "touch"])
        .arg(guest_dir.join(&marker_name))
        .status();

    Ok(guest_bin)
}

/// Resolve the Lima installation directory (contains `bin/limactl`, `share/lima/`).
///
/// Resolution order:
/// 1. `PEPPY_LIMA_DIR` environment variable
/// 2. `../lima/` relative to the current executable (installed layout)
/// 3. Compile-time `LIMA_INSTALL_DIR` set by build.rs
pub(crate) fn resolve_lima_dir() -> Result<PathBuf> {
    // 1) Runtime override via environment variable
    if let Ok(dir) = std::env::var("PEPPY_LIMA_DIR") {
        let dir = dir.trim().to_string();
        if !dir.is_empty() {
            let path = PathBuf::from(&dir);
            if path.is_dir() {
                return Ok(path);
            }
            tracing::warn!(
                "PEPPY_LIMA_DIR={} does not exist or is not a directory",
                dir
            );
        }
    }

    // 2) Relative to the current executable: {exe_dir}/../lima/
    //    This is the installed layout created by install.sh ($PEPPY_HOME/lima/).
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let candidate = exe_dir.join("../lima");
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }

    // 3) Compile-time path injected by build.rs
    if let Some(dir) = option_env!("LIMA_INSTALL_DIR") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Ok(path);
        }
        tracing::debug!(
            "Compile-time LIMA_INSTALL_DIR={} does not exist at runtime",
            dir
        );
    }

    Err(Error::LimaRequired)
}

/// Resolve the LIMA_HOME directory for VM instance data.
///
/// Resolution order:
/// 1. `{exe_dir}/../lima-data/` — installed layout (`~/.peppy/lima-data/`).
/// 2. Compile-time `LIMA_BUILD_HOME` — reuses the build-time VM during development.
/// 3. `~/.peppy/lima-data/` — fallback for unusual layouts.
pub(crate) fn resolve_lima_home() -> Result<PathBuf> {
    // 1) Relative to the current executable: {exe_dir}/../lima-data/
    //    In the installed layout this is ~/.peppy/lima-data/.
    //    Only use this if the directory already exists (i.e. was set up by install.sh).
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let candidate = exe_dir.join("../lima-data");
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }

    // 2) Compile-time build home — during `cargo test` / `cargo run` in the
    //    source tree the exe-relative path won't exist, so fall back to the
    //    LIMA_HOME used at build time (typically ~/.peppy/lima-build/).
    if let Some(dir) = option_env!("LIMA_BUILD_HOME") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Ok(path);
        }
    }

    // 3) Fallback: well-known location in the user's home
    let home = std::env::var("HOME")
        .map_err(|_| Error::ConfigurationError("HOME environment variable not set".into()))?;
    Ok(PathBuf::from(home).join(".peppy/lima-data"))
}

/// Parse a Lima version string like `"limactl version 1.1.0"` into `(major, minor, patch)`.
///
/// Returns `None` if the string cannot be parsed.
pub(crate) fn parse_lima_version(version_output: &str) -> Option<(u32, u32, u32)> {
    // Format: "limactl version X.Y.Z" or just "X.Y.Z"
    let version_str = version_output.trim().rsplit(' ').next()?;

    let mut parts = version_str.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_parse_lima_version_full_string() {
        assert_eq!(parse_lima_version("limactl version 1.1.0"), Some((1, 1, 0)));
    }

    #[test]
    fn test_parse_lima_version_bare_version() {
        assert_eq!(parse_lima_version("1.0.2"), Some((1, 0, 2)));
    }

    #[test]
    fn test_parse_lima_version_with_whitespace() {
        assert_eq!(
            parse_lima_version("  limactl version 0.19.1  \n"),
            Some((0, 19, 1))
        );
    }

    #[test]
    fn test_parse_lima_version_invalid() {
        assert_eq!(parse_lima_version("not a version"), None);
        assert_eq!(parse_lima_version(""), None);
        assert_eq!(parse_lima_version("1.2"), None);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_lima_version_returns_version_check_error_on_nonzero_exit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().expect("create temp dir");
        let limactl = dir.path().join("limactl");

        fs::write(&limactl, "#!/bin/sh\n>&2 echo 'bad lima'\nexit 42\n")
            .expect("write fake limactl");

        let mut perms = fs::metadata(&limactl).expect("read metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&limactl, perms).expect("set executable bit");

        let err = check_lima_version(&limactl).expect_err("expected version check failure");
        match err {
            Error::LimaVersionCheckFailed(msg) => {
                assert!(msg.contains("--version"), "unexpected message: {msg}");
                assert!(msg.contains("42"), "unexpected message: {msg}");
                assert!(msg.contains("bad lima"), "unexpected message: {msg}");
            }
            other => panic!("expected LimaVersionCheckFailed, got {other:?}"),
        }
    }
}
