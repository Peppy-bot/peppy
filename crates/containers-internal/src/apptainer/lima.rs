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
                .args([
                    "start",
                    &name_flag,
                    "--tty=false",
                    "--mount-writable",
                    template,
                ])
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

/// Disable AppArmor's user namespace restriction inside the Lima guest.
///
/// Ubuntu 24.04+ enables `kernel.apparmor_restrict_unprivileged_userns=1` by
/// default, which blocks Apptainer's unprivileged user namespace operations.
/// This applies the same workaround used by Lima's own `apptainer.yaml` template.
///
/// Note: `sudo` runs inside the Lima VM guest, which has passwordless sudo by default.
pub(crate) fn ensure_guest_userns(limactl: &Path, lima_home: &Path, instance: &str) -> Result<()> {
    let check = Command::new(limactl)
        .env("LIMA_HOME", lima_home)
        .args([
            "shell",
            instance,
            "--",
            "cat",
            "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
        ])
        .output();

    let needs_fix = match &check {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim() == "1",
        // File doesn't exist (non-AppArmor system) or other error — skip
        _ => false,
    };

    if !needs_fix {
        return Ok(());
    }

    tracing::info!("Disabling AppArmor user namespace restriction in Lima guest...");

    let apply = Command::new(limactl)
        .env("LIMA_HOME", lima_home)
        .args([
            "shell",
            instance,
            "--",
            "sudo",
            "sh",
            "-c",
            "echo 'kernel.apparmor_restrict_unprivileged_userns=0' > /etc/sysctl.d/99-userns.conf && sysctl --system",
        ])
        .output()
        .map_err(|e| Error::LimaInstanceError(format!("failed to apply userns sysctl: {e}")))?;

    if !apply.status.success() {
        let stderr = String::from_utf8_lossy(&apply.stderr);
        return Err(Error::LimaInstanceError(format!(
            "failed to disable AppArmor userns restriction in guest: {stderr}"
        )));
    }

    Ok(())
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

    let version = crate::APPTAINER_VERSION;
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
    match Command::new(limactl)
        .env("LIMA_HOME", lima_home)
        .args(["shell", instance, "--", "touch"])
        .arg(&marker_path)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            tracing::warn!(
                status = %status,
                limactl = %limactl.display(),
                instance,
                marker = %marker_path.display(),
                "Failed to write guest marker; next run may perform full sync"
            );
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                limactl = %limactl.display(),
                instance,
                marker = %marker_path.display(),
                "Failed to write guest marker; next run may perform full sync"
            );
        }
    }

    Ok(guest_bin)
}

/// Resolve the Lima installation directory (contains `bin/limactl`, `share/lima/`).
///
/// Resolution order:
/// 1. `PEPPY_LIMA_DIR` environment variable
/// 2. `lima/` relative to the current executable (installed layout)
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

    // 2) Relative to the current executable: {exe_dir}/lima/
    //    This is the installed layout created by install.sh ($PEPPY_BIN_DIR/lima/).
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let candidate = exe_dir.join("lima");
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

/// Stop a Lima instance.
///
/// Idempotent: returns `Ok(())` if the instance is already stopped or does not
/// exist, so callers do not need to guard against these cases.
pub(crate) fn stop_instance(limactl: &Path, lima_home: &Path, instance: &str) -> Result<()> {
    let list_output = Command::new(limactl)
        .env("LIMA_HOME", lima_home)
        .args(["list", "--format", "{{.Status}}", instance])
        .output();

    let status = match &list_output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        }
        _ => None,
    };

    match status.as_deref() {
        Some("Running") => {
            tracing::info!("Stopping Lima {} instance...", instance);
            let output = Command::new(limactl)
                .env("LIMA_HOME", lima_home)
                .args(["stop", instance])
                .output()?;

            if output.status.success() {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(Error::LimaInstanceError(format!(
                    "failed to stop Lima {} instance: {stderr}",
                    instance
                )))
            }
        }
        Some(other) => {
            tracing::info!(
                "Lima {} instance status is '{}', skipping stop",
                instance,
                other
            );
            Ok(())
        }
        None => {
            tracing::info!("Lima {} instance does not exist, skipping stop", instance);
            Ok(())
        }
    }
}

/// Ensure that the given host paths are listed as mounts in the Lima config.
///
/// Reads the Lima YAML config, checks existing mount locations, and appends
/// any missing paths as writable mounts. Returns `true` if the config was
/// modified (meaning the VM needs to be restarted to pick up the changes).
/// Top-level system directories that Lima 2.0+ rejects as guest mountPoints.
///
/// NOTE: This list is duplicated in `config-internal/src/node/types.rs`
/// (which this crate cannot depend on). Keep both in sync.
const BLOCKED_MOUNT_PATHS: &[&str] = &[
    "/", "/bin", "/dev", "/etc", "/home", "/opt", "/sbin", "/tmp", "/usr", "/var",
];

/// Check whether a path is a blocked top-level system mount.
///
/// Only exact top-level matches are blocked — subdirectories like `/tmp/my_app`
/// are allowed. Also handles macOS `/private/X` equivalents (e.g., `/private/tmp`
/// maps to `/tmp`).
pub(crate) fn is_blocked_system_mount(path: &str) -> bool {
    if BLOCKED_MOUNT_PATHS.contains(&path) {
        return true;
    }
    // macOS: /private/tmp -> /tmp, /private/var -> /var
    if let Some(stripped) = path.strip_prefix("/private") {
        return BLOCKED_MOUNT_PATHS.contains(&stripped);
    }
    false
}

/// Ensure that the given host paths are listed as mounts in the Lima config.
///
/// Reads the Lima YAML config, checks existing mount locations, and appends
/// any missing paths as writable mounts. Returns `true` if the config was
/// modified (meaning the VM needs to be restarted to pick up the changes).
///
/// Also performs cleanup: removes existing mount entries for paths that no longer
/// exist on the host or that are blocked system paths (which Lima would reject).
pub(crate) fn ensure_extra_mounts(config_path: &Path, paths: &[&str]) -> Result<bool> {
    let content = std::fs::read_to_string(config_path).map_err(|e| {
        Error::LimaInstanceError(format!(
            "failed to read Lima config {}: {e}",
            config_path.display()
        ))
    })?;

    let mut config: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| {
        Error::LimaInstanceError(format!(
            "failed to parse Lima config {}: {e}",
            config_path.display()
        ))
    })?;

    // Get or create the mounts array
    let mounts = config
        .as_mapping_mut()
        .ok_or_else(|| Error::LimaInstanceError("Lima config is not a YAML mapping".into()))?
        .entry(serde_yaml::Value::String("mounts".to_string()))
        .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));

    let mounts_seq = mounts
        .as_sequence_mut()
        .ok_or_else(|| Error::LimaInstanceError("Lima config 'mounts' is not a sequence".into()))?;

    let mut modified = false;

    // Phase 1: Clean up stale or invalid existing mounts.
    let original_len = mounts_seq.len();
    mounts_seq.retain(|entry| {
        let Some(location) = entry.get("location").and_then(|v| v.as_str()) else {
            return true; // Keep entries without a location field
        };
        // Always keep the home mount
        if location == "~" || location == "null" {
            return true;
        }
        if is_blocked_system_mount(location) {
            tracing::info!("Removing invalid Lima mount (system path): {}", location);
            return false;
        }
        if !Path::new(location).exists() {
            tracing::info!(
                "Removing stale Lima mount (path does not exist): {}",
                location
            );
            return false;
        }
        true
    });
    if mounts_seq.len() != original_len {
        modified = true;
    }

    // Phase 2: Add new mounts (skip blocked system paths).
    let existing: Vec<String> = mounts_seq
        .iter()
        .filter_map(|entry| {
            entry
                .get("location")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();

    for path in paths {
        if is_blocked_system_mount(path) {
            tracing::info!(
                "Skipping Lima mount for system path: {} (blocked by Lima)",
                path
            );
            continue;
        }
        if !existing.iter().any(|loc| loc == *path) {
            let mut mount_entry = serde_yaml::Mapping::new();
            mount_entry.insert(
                serde_yaml::Value::String("location".to_string()),
                serde_yaml::Value::String(path.to_string()),
            );
            mount_entry.insert(
                serde_yaml::Value::String("writable".to_string()),
                serde_yaml::Value::Bool(true),
            );
            mounts_seq.push(serde_yaml::Value::Mapping(mount_entry));
            tracing::info!("Adding Lima mount: {}", path);
            modified = true;
        }
    }

    if modified {
        let yaml_str = serde_yaml::to_string(&config).map_err(|e| {
            Error::LimaInstanceError(format!("failed to serialize Lima config: {e}"))
        })?;
        std::fs::write(config_path, yaml_str).map_err(|e| {
            Error::LimaInstanceError(format!(
                "failed to write Lima config {}: {e}",
                config_path.display()
            ))
        })?;
    }

    Ok(modified)
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

    #[test]
    fn test_ensure_extra_mounts_adds_new_mount() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");
        let initial = "mounts:\n  - location: \"~\"\n    writable: true\n";
        fs::write(&config_path, initial).expect("write initial config");

        let modified =
            ensure_extra_mounts(&config_path, &["/tmp/test_mount"]).expect("should succeed");
        assert!(modified, "config should have been modified");

        let content = fs::read_to_string(&config_path).expect("read config");
        assert!(
            content.contains("/tmp/test_mount"),
            "config should contain new mount path, got:\n{content}"
        );
    }

    #[test]
    fn test_ensure_extra_mounts_skips_existing() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");

        // Use a real directory so the cleanup phase doesn't remove it as stale.
        let existing_dir = dir.path().join("existing_mount");
        fs::create_dir_all(&existing_dir).expect("create existing mount dir");
        let existing_path = existing_dir.to_str().unwrap();

        let initial = format!(
            "mounts:\n  - location: \"~\"\n    writable: true\n  - location: {existing_path}\n    writable: true\n"
        );
        fs::write(&config_path, initial).expect("write initial config");

        let modified = ensure_extra_mounts(&config_path, &[existing_path]).expect("should succeed");
        assert!(!modified, "config should not have been modified");
    }

    #[test]
    fn test_ensure_extra_mounts_creates_mounts_section() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");
        let initial = "images: []\n";
        fs::write(&config_path, initial).expect("write initial config");

        let modified =
            ensure_extra_mounts(&config_path, &["/data/shared"]).expect("should succeed");
        assert!(modified, "config should have been modified");

        let content = fs::read_to_string(&config_path).expect("read config");
        assert!(
            content.contains("/data/shared"),
            "config should contain new mount, got:\n{content}"
        );
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

    #[cfg(unix)]
    #[test]
    fn test_stop_instance_is_idempotent_for_nonexistent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().expect("create temp dir");
        let limactl = dir.path().join("limactl");
        let lima_home = dir.path().join("lima_home");
        fs::create_dir_all(&lima_home).expect("create lima home");

        // Fake limactl: `list` returns empty status (instance doesn't exist),
        // `stop` would fail — but should never be called.
        fs::write(
            &limactl,
            "#!/bin/sh\nif [ \"$1\" = \"list\" ]; then echo ''; exit 0; else echo 'should not be called' >&2; exit 1; fi\n",
        )
        .expect("write fake limactl");

        let mut perms = fs::metadata(&limactl).expect("read metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&limactl, perms).expect("set executable bit");

        let result = stop_instance(&limactl, &lima_home, "nonexistent_instance");
        assert!(
            result.is_ok(),
            "stop_instance should succeed for non-existent instance, got: {:?}",
            result.unwrap_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_stop_instance_is_idempotent_for_stopped() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().expect("create temp dir");
        let limactl = dir.path().join("limactl");
        let lima_home = dir.path().join("lima_home");
        fs::create_dir_all(&lima_home).expect("create lima home");

        // Fake limactl: `list` returns "Stopped" status.
        // `stop` would fail — but should never be called.
        fs::write(
            &limactl,
            "#!/bin/sh\nif [ \"$1\" = \"list\" ]; then echo 'Stopped'; exit 0; else echo 'should not be called' >&2; exit 1; fi\n",
        )
        .expect("write fake limactl");

        let mut perms = fs::metadata(&limactl).expect("read metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&limactl, perms).expect("set executable bit");

        let result = stop_instance(&limactl, &lima_home, "stopped_instance");
        assert!(
            result.is_ok(),
            "stop_instance should succeed for already-stopped instance, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_is_blocked_system_mount_rejects_top_level() {
        assert!(is_blocked_system_mount("/"));
        assert!(is_blocked_system_mount("/tmp"));
        assert!(is_blocked_system_mount("/var"));
        assert!(is_blocked_system_mount("/etc"));
        assert!(is_blocked_system_mount("/bin"));
        assert!(is_blocked_system_mount("/dev"));
        assert!(is_blocked_system_mount("/home"));
        assert!(is_blocked_system_mount("/opt"));
        assert!(is_blocked_system_mount("/sbin"));
        assert!(is_blocked_system_mount("/usr"));
    }

    #[test]
    fn test_is_blocked_system_mount_rejects_private_equivalents() {
        assert!(is_blocked_system_mount("/private/tmp"));
        assert!(is_blocked_system_mount("/private/var"));
        assert!(is_blocked_system_mount("/private/etc"));
    }

    #[test]
    fn test_is_blocked_system_mount_allows_subdirectories() {
        assert!(!is_blocked_system_mount("/tmp/my_app"));
        assert!(!is_blocked_system_mount("/var/log/my_app"));
        assert!(!is_blocked_system_mount("/data/shared"));
        assert!(!is_blocked_system_mount("/mnt/external"));
        assert!(!is_blocked_system_mount("/private/tmp/foo"));
    }

    #[test]
    fn test_ensure_extra_mounts_removes_system_path_mounts() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");
        let initial = r#"mounts:
- location: "~"
  writable: true
- location: /tmp
  writable: true
- location: /private/tmp
  writable: true
"#;
        fs::write(&config_path, initial).expect("write initial config");

        let modified = ensure_extra_mounts(&config_path, &[]).expect("should succeed");
        assert!(
            modified,
            "config should be modified (system path mounts removed)"
        );

        let content = fs::read_to_string(&config_path).expect("read config");
        assert!(
            !content.contains("location: /tmp"),
            "system path /tmp should be removed, got:\n{content}"
        );
        assert!(
            !content.contains("/private/tmp"),
            "system path /private/tmp should be removed, got:\n{content}"
        );
        assert!(
            content.contains("~"),
            "home mount should be preserved, got:\n{content}"
        );
    }

    #[test]
    fn test_ensure_extra_mounts_removes_stale_mounts() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");
        let initial = r#"mounts:
- location: "~"
  writable: true
- location: /nonexistent/stale/path/from/old/test
  writable: true
"#;
        fs::write(&config_path, initial).expect("write initial config");

        let modified = ensure_extra_mounts(&config_path, &[]).expect("should succeed");
        assert!(modified, "config should be modified (stale mount removed)");

        let content = fs::read_to_string(&config_path).expect("read config");
        assert!(
            !content.contains("/nonexistent/stale/path"),
            "stale mount should be removed, got:\n{content}"
        );
        assert!(
            content.contains("~"),
            "home mount should be preserved, got:\n{content}"
        );
    }

    #[test]
    fn test_ensure_extra_mounts_skips_adding_system_paths() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");
        let initial = "mounts:\n- location: \"~\"\n  writable: true\n";
        fs::write(&config_path, initial).expect("write initial config");

        // Create a real directory so the valid path actually exists
        let valid_dir = dir.path().join("valid_mount");
        fs::create_dir_all(&valid_dir).expect("create valid mount dir");
        let valid_path = valid_dir.to_str().unwrap();

        let modified =
            ensure_extra_mounts(&config_path, &["/tmp", valid_path]).expect("should succeed");
        assert!(modified, "config should be modified (valid path added)");

        let content = fs::read_to_string(&config_path).expect("read config");
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&content).expect("parse result config");
        let has_tmp_mount = parsed["mounts"]
            .as_sequence()
            .unwrap()
            .iter()
            .any(|m| m["location"].as_str() == Some("/tmp"));
        assert!(
            !has_tmp_mount,
            "system path /tmp should not be added, got:\n{content}"
        );
        assert!(
            content.contains(valid_path),
            "valid path should be added, got:\n{content}"
        );
    }
}
