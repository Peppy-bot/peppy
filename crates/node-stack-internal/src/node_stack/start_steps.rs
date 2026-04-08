//! Concrete start I/O steps invoked from
//! [`super::entity::NodeEntity::prepare_and_spawn`].
//!
//! These helpers were originally part of `core-node-internal::services::node::start`
//! and were moved here so that the lifecycle transition `Built → Starting` (and
//! the eventual `Starting → Started` commit) can run without crossing the crate
//! boundary back into core-node-internal. Messenger-bound logic (ready check,
//! health check) stays in core-node and is run between
//! [`super::entity::NodeEntity::prepare_and_spawn`] and
//! [`super::entity::NodeEntity::commit_started`].

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use chrono::Local;
use config::consts::{PeppyDirs, RUNTIME_CONFIG_VAR_NAME};
use config::node::{NodeConfig, PeppygenLanguage};
use tar::Archive;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tracing::debug;
use zstd::stream::read::Decoder;

/// Per-process counter used to name temporary runtime config files uniquely.
///
/// Each spawned node instance writes its `runtime_config.json5` to a unique
/// file under `peppy_dirs.runtime_config_dir()`. Using a shared path can cause
/// cross-test and cross-instance races where a node reads the wrong config
/// (instance_id/port), leading to hangs waiting for ready/health responses.
pub(super) static RUNTIME_CONFIG_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes a runtime config to a unique temp file under
/// `peppy_dirs.runtime_config_dir()` and returns the path.
///
/// Used by both `spawn_process_node` and `spawn_container_node` to materialize
/// the runtime config that the spawned child reads via the
/// `PEPPY_RUNTIME_CONFIG` environment variable.
pub(super) fn write_runtime_config_temp(
    peppy_dirs: &PeppyDirs,
    json5: &str,
) -> std::io::Result<PathBuf> {
    let runtime_dir = peppy_dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime_dir)?;
    let counter = RUNTIME_CONFIG_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let runtime_config_path = runtime_dir.join(format!("runtime_config_{pid}_{counter}.json5"));
    std::fs::write(&runtime_config_path, json5)?;
    Ok(runtime_config_path)
}

/// Extracts a `.tar.zst` archive into `destination` with path safety checks.
/// Rejects entries containing `..`, root, or prefix path components.
/// Directories are applied last to avoid permission interference during extraction.
///
/// This is `pub` (not `pub(super)`) because `core-node-internal` re-exports it
/// via `node-stack`'s lib.rs for use in node-add source resolution
/// (`resolve_local_archive_source`), which is unrelated to the start lifecycle
/// but uses the same archive format.
pub fn extract_tar_zst(archive_path: &Path, destination: &Path) -> std::result::Result<(), String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("Failed to open archive {}: {}", archive_path.display(), e))?;

    let decoder = Decoder::new(file).map_err(|e| {
        format!(
            "Failed to decode zstd archive {}: {}",
            archive_path.display(),
            e
        )
    })?;
    let mut archive = Archive::new(decoder);

    let entries = archive.entries().map_err(|e| {
        format!(
            "Failed to read archive entries from {}: {}",
            archive_path.display(),
            e
        )
    })?;

    let mut directories = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|e| {
            format!(
                "Failed to read archive entry from {}: {}",
                archive_path.display(),
                e
            )
        })?;

        let entry_path = entry
            .path()
            .map_err(|e| {
                format!(
                    "Failed to read entry path from {}: {}",
                    archive_path.display(),
                    e
                )
            })?
            .into_owned();

        if entry_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(..)
            )
        }) {
            return Err(format!(
                "Archive {} contains unsafe path: {}",
                archive_path.display(),
                entry_path.display()
            ));
        }

        if entry.header().entry_type().is_dir() {
            directories.push(entry);
        } else {
            let unpacked = entry.unpack_in(destination).map_err(|e| {
                format!(
                    "Failed to unpack entry {} from {}: {}",
                    entry_path.display(),
                    archive_path.display(),
                    e
                )
            })?;
            if !unpacked {
                return Err(format!(
                    "Archive {} contains unsafe path: {}",
                    archive_path.display(),
                    entry_path.display()
                ));
            }
        }
    }

    // Apply directory entries at the end, matching tar::Archive::unpack behavior (avoids
    // directory permissions interfering with descendant extraction).
    directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut dir in directories {
        let entry_path = dir
            .path()
            .map_err(|e| {
                format!(
                    "Failed to read entry path from {}: {}",
                    archive_path.display(),
                    e
                )
            })?
            .into_owned();
        let unpacked = dir.unpack_in(destination).map_err(|e| {
            format!(
                "Failed to unpack entry {} from {}: {}",
                entry_path.display(),
                archive_path.display(),
                e
            )
        })?;
        if !unpacked {
            return Err(format!(
                "Archive {} contains unsafe path: {}",
                archive_path.display(),
                entry_path.display()
            ));
        }
    }

    Ok(())
}

/// Creates (or recreates) a clean instance directory under `peppy_dirs.instances_dir()`.
/// Returns the path to the newly created directory.
///
/// Used directly for container nodes (whose instance dir is empty) and as the
/// first step of [`extract_node_archive`] for process nodes.
pub(super) fn create_instance_dir(
    instance_id: &str,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<PathBuf, String> {
    let instances_dir = peppy_dirs.instances_dir();
    std::fs::create_dir_all(&instances_dir).map_err(|e| {
        format!(
            "Failed to create instances directory {}: {}",
            instances_dir.display(),
            e
        )
    })?;

    let instance_dir = instances_dir.join(instance_id);

    // Clean up any leftover instance directory from a previous failed attempt,
    // since the instance ID is deterministic and may be retried.
    if instance_dir.exists() {
        std::fs::remove_dir_all(&instance_dir).map_err(|e| {
            format!(
                "Failed to clean up existing instance directory {}: {}",
                instance_dir.display(),
                e
            )
        })?;
    }

    std::fs::create_dir(&instance_dir).map_err(|e| {
        format!(
            "Failed to create instance directory {}: {}",
            instance_dir.display(),
            e
        )
    })?;

    Ok(instance_dir)
}

/// Extracts a `.tar.zst` node archive to a new instance directory.
/// Returns the path to the extracted instance directory.
///
/// Used for process nodes whose build artifact is a `.tar.zst` of the working
/// directory; container nodes use [`create_instance_dir`] instead since their
/// SIF image is self-contained.
pub(super) fn extract_node_archive(
    archive_path: &Path,
    instance_id: &str,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<PathBuf, String> {
    let instance_dir = create_instance_dir(instance_id, peppy_dirs)?;
    extract_tar_zst(archive_path, &instance_dir)?;
    Ok(instance_dir)
}

/// Runs a process node using its `start_cmd` and passes the
/// `PEPPY_RUNTIME_CONFIG` as an env var. Returns the spawned child on success.
///
/// Used by [`super::entity::NodeEntity::prepare_and_spawn`] for process nodes.
/// The runtime config is written to a unique temp file under
/// `peppy_dirs.runtime_config_dir()` (see [`write_runtime_config_temp`]).
pub(super) fn spawn_process_node(
    config: &NodeConfig,
    working_dir: &Path,
    runtime_config_json5: &str,
    env_vars: &[(String, String)],
    log_file: &Arc<StdMutex<File>>,
    peppy_dirs: &PeppyDirs,
) -> std::io::Result<Child> {
    let manifest = &config.manifest;
    let start_cmd = config
        .execution
        .start_cmd
        .as_ref()
        .ok_or_else(|| std::io::Error::other("node has no execution.start_cmd"))?;

    let Some((program, args)) = start_cmd.split_first() else {
        return Err(std::io::Error::other("start_cmd is empty"));
    };

    debug!(
        "Running node '{}:{}' with command: {} {:?} in dir {:?}",
        manifest.name.as_str(),
        manifest.tag,
        program,
        args,
        working_dir
    );

    // Log the command being executed to the log file before attempting to spawn
    {
        let full_cmd = start_cmd.join(" ");
        if let Ok(mut file) = log_file.lock() {
            let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
            let _ = writeln!(
                file,
                "[{}] Executing start_cmd: {} (working_dir: {})",
                timestamp,
                full_cmd,
                working_dir.display()
            );
            let _ = file.flush();
        }
    }

    let runtime_config_path = write_runtime_config_temp(peppy_dirs, runtime_config_json5)?;

    let mut command = Command::new(program);
    command.current_dir(working_dir);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env_vars {
        command.env(key, value);
    }
    // Set PWD to match the actual working directory so tools that read this
    // variable (e.g. capnproto's KJ) see a consistent value. The caller's
    // PWD is stripped by caller_env_overrides() since it refers to the
    // caller's directory, not the node's instance dir.
    command.env("PWD", working_dir);
    command.env(RUNTIME_CONFIG_VAR_NAME, &runtime_config_path);

    // Force unbuffered stdout/stderr for Python nodes. Without this, Python
    // defaults to full buffering when stdout is a pipe, delaying log capture.
    if config.execution.language == PeppygenLanguage::Python {
        command.env("PYTHONUNBUFFERED", "1");
    }

    command.spawn().map_err(|e| {
        let full_cmd = start_cmd.join(" ");
        std::io::Error::other(format!("failed to execute start_cmd `{}`: {}", full_cmd, e))
    })
}

/// Describes a bind mount for a container node.
pub(super) struct ContainerBind {
    pub(super) src: String,
    pub(super) dest: Option<String>,
    pub(super) opts: Option<String>,
}

/// Collect all bind mounts needed for a container node.
///
/// Always includes the runtime config file as the first entry so it is
/// accessible inside the container regardless of Apptainer's `$HOME` auto-bind
/// behavior (which may not cover `~/.peppy/` when running inside a Lima VM).
pub(super) fn collect_container_binds(
    runtime_config_path: &Path,
    mount_paths: &[String],
) -> Vec<ContainerBind> {
    let mut binds = Vec::with_capacity(1 + mount_paths.len());

    // Runtime config must always be bound into the container.
    binds.push(ContainerBind {
        src: runtime_config_path.to_string_lossy().into_owned(),
        dest: None,
        opts: None,
    });

    // User-specified mount paths (format: "host:container[:opts]")
    for m in mount_paths {
        let parts: Vec<&str> = m.splitn(3, ':').collect();
        binds.push(match parts.len() {
            1 => ContainerBind {
                src: parts[0].into(),
                dest: None,
                opts: None,
            },
            2 => ContainerBind {
                src: parts[0].into(),
                dest: Some(parts[1].into()),
                opts: None,
            },
            _ => ContainerBind {
                src: parts[0].into(),
                dest: Some(parts[1].into()),
                opts: Some(parts[2].into()),
            },
        });
    }

    binds
}

/// Starts a container node using the Apptainer runtime.
///
/// Builds an `apptainer run <sif_path>` command with environment variables
/// passed into the container via `--env` flags and optional bind mounts from
/// `mount_paths`. Returns a tokio [`Child`] with piped stdout/stderr for
/// async output capture. The Apptainer instance is constructed via
/// `tokio::task::spawn_blocking` inside this function.
#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn_container_node(
    sif_path: &Path,
    working_dir: &Path,
    runtime_config_json5: &str,
    env_vars: &[(String, String)],
    mount_paths: &[String],
    apptainer_run_extra_args: &[String],
    lima_shell_extra_args: &[String],
    log_file: &Arc<StdMutex<File>>,
    peppy_dirs: &PeppyDirs,
) -> std::io::Result<Child> {
    // Apptainer initialization is expensive (it may bootstrap a Lima VM on
    // macOS). Run it on a blocking pool so the tokio runtime isn't stalled.
    let mut apptainer = tokio::task::spawn_blocking(containers::Apptainer::new)
        .await
        .map_err(|e| std::io::Error::other(format!("Apptainer init task failed: {}", e)))?
        .map_err(|e| {
            std::io::Error::other(format!("Failed to initialize Apptainer runtime: {}", e))
        })?;

    let runtime_config_path = write_runtime_config_temp(peppy_dirs, runtime_config_json5)?;

    let sif_str = sif_path
        .to_str()
        .ok_or_else(|| std::io::Error::other("SIF path is not valid UTF-8"))?;

    // Collect all bind mounts (runtime config + user-specified mount_paths).
    let binds = collect_container_binds(&runtime_config_path, mount_paths);

    // Ensure host-side source directories exist for user-specified bind mounts.
    // Skip binds[0] (runtime config file) — its parent dir is already created above.
    for bind in &binds[1..] {
        let src = Path::new(&bind.src);
        if !src.exists() {
            std::fs::create_dir_all(src)?;
        }
    }

    // Ensure host paths outside $HOME are accessible in the Lima VM.
    // Skip binds[0] (runtime config) — it's always under $HOME.
    if binds.len() > 1 {
        let src_paths: Vec<&str> = binds[1..].iter().map(|b| b.src.as_str()).collect();
        apptainer
            .ensure_host_mounts(&src_paths)
            .map_err(|e| std::io::Error::other(format!("Failed to ensure host mounts: {}", e)))?;
    }

    // Build apptainer run command. Environment variables are passed into the
    // container via --env flags (not host-side process env) so they are
    // visible inside the container.
    let mut apptainer_cmd = apptainer.run(sif_str);
    for arg in apptainer_run_extra_args {
        apptainer_cmd = apptainer_cmd.raw_flag(arg);
    }
    apptainer_cmd = apptainer_cmd.lima_shell_extra_args(lima_shell_extra_args);
    for (key, value) in env_vars {
        // Apptainer manages HOME itself; passing it via --env triggers a warning.
        if key.eq_ignore_ascii_case("HOME") {
            continue;
        }
        apptainer_cmd = apptainer_cmd.env(key, value);
    }
    apptainer_cmd = apptainer_cmd.env(
        RUNTIME_CONFIG_VAR_NAME,
        runtime_config_path.to_str().unwrap_or_default(),
    );

    // Add all bind mounts (runtime config + user-specified).
    // Device passthrough mounts (src under /dev/ with no dest or same dest)
    // are skipped: Apptainer applies `nodev` to --bind mounts
    // which blocks device-node access. Host devices are already available
    // inside the container via `mount dev = yes` in apptainer.conf.
    // Remapped device mounts (e.g. /dev/video0:/dev/my_video0) still need
    // an explicit --bind.
    for bind in &binds {
        if bind.src.starts_with("/dev/") {
            let is_passthrough = match bind.dest.as_deref() {
                None => true,
                Some(dest) => dest == bind.src,
            };
            if is_passthrough {
                continue;
            }
        }
        apptainer_cmd = apptainer_cmd.bind(&bind.src, bind.dest.as_deref(), bind.opts.as_deref());
    }

    // Log the command being executed
    {
        if let Ok(mut file) = log_file.lock() {
            let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
            let _ = writeln!(
                file,
                "[{}] Executing apptainer run: {} (working_dir: {}, bind_mounts: [{}])",
                timestamp,
                sif_path.display(),
                working_dir.display(),
                mount_paths.join(", ")
            );
            let _ = file.flush();
        }
    }

    // Get the fully-built std::process::Command from the Apptainer facade,
    // then convert to tokio::process::Command for async stdio piping.
    let std_cmd = apptainer_cmd
        .into_std_command()
        .map_err(|e| std::io::Error::other(format!("Failed to build apptainer command: {}", e)))?;

    let mut command = Command::from(std_cmd);
    command
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    command.spawn().map_err(|e| {
        std::io::Error::other(format!(
            "failed to execute apptainer run for `{}`: {}",
            sif_path.display(),
            e
        ))
    })
}

/// Kills a child process, drains its output readers (so the stderr buffer
/// flushes), and returns a formatted error string with a stderr tail.
///
/// Used by [`super::entity::NodeEntity::abort_started`]. Joining the reader
/// handles is critical for stable error reporting — without it, the stderr
/// buffer can be empty due to async scheduling timing even though the lines
/// were already written to the log file.
pub(super) async fn kill_and_collect_error(
    mut child: Child,
    instance_id_str: &str,
    error: &str,
    stderr_buffer: Arc<StdMutex<VecDeque<String>>>,
    output_reader_handles: Vec<JoinHandle<()>>,
    log_file: Arc<StdMutex<File>>,
) -> String {
    if let Err(kill_err) = child.kill().await {
        debug!(
            "Failed to kill process for node instance '{}': {}",
            instance_id_str, kill_err
        );
    }

    let _ = child.wait().await;

    // Drain any remaining output that was already in-flight so error reporting is stable.
    // We intentionally ignore join errors so we don't mask the actual node start failure.
    for handle in output_reader_handles {
        let _ = handle.await;
    }

    let stderr_output = {
        let guard = stderr_buffer.lock().expect("stderr buffer lock poisoned");
        let buffer_lines: Vec<String> = guard.iter().cloned().collect();
        if !buffer_lines.is_empty() {
            buffer_lines.join("\n")
        } else {
            // Fall back to the log file for stderr lines — the log write is unconditional
            // and may have captured output that the stderr_buffer missed due to timing
            // (e.g. the async reader hadn't processed the line before we read the buffer).
            extract_stderr_from_log(&log_file)
        }
    };

    if !stderr_output.is_empty() {
        debug!(
            "Node instance '{}' stderr (tail): {}",
            instance_id_str, stderr_output
        );
    }

    if stderr_output.is_empty() {
        error.to_string()
    } else {
        format!(
            "{}\n\n--- stderr (last lines) ---\n{}",
            error, stderr_output
        )
    }
}

/// Extracts stderr lines from the log file.
///
/// The log file captures all output unconditionally (before any async processing),
/// so it serves as a reliable fallback when the stderr_buffer is empty due to
/// async scheduling timing (e.g., the reader task hadn't processed the line before
/// the buffer was read).
pub(super) fn extract_stderr_from_log(log_file: &Arc<StdMutex<File>>) -> String {
    use std::io::{Read, Seek};

    let content = match log_file.lock() {
        Ok(mut f) => {
            if f.seek(std::io::SeekFrom::Start(0)).is_err() {
                return String::new();
            }
            let mut buf = String::new();
            if f.read_to_string(&mut buf).is_err() {
                return String::new();
            }
            buf
        }
        Err(_) => return String::new(),
    };

    content
        .lines()
        .filter(|l| l.contains("[stderr]"))
        .filter_map(|l| l.split_once("[stderr] ").map(|(_, rest)| rest))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_container_binds_always_includes_runtime_config() {
        let rc = PathBuf::from("/home/user/.peppy/runtime/runtime_config_99_0.json5");
        let binds = collect_container_binds(&rc, &[]);

        assert_eq!(binds.len(), 1);
        assert_eq!(
            binds[0].src,
            "/home/user/.peppy/runtime/runtime_config_99_0.json5"
        );
        assert!(binds[0].dest.is_none());
        assert!(binds[0].opts.is_none());
    }

    #[test]
    fn collect_container_binds_includes_user_mounts() {
        let rc = PathBuf::from("/home/user/.peppy/runtime/rc.json5");
        let user_mounts = vec![
            "/data/input:/container/input:ro".to_string(),
            "/dev/ttyUSB0".to_string(),
        ];

        let binds = collect_container_binds(&rc, &user_mounts);

        assert_eq!(binds.len(), 3);
        // First entry is always the runtime config
        assert_eq!(binds[0].src, "/home/user/.peppy/runtime/rc.json5");
        assert!(binds[0].dest.is_none());
        assert!(binds[0].opts.is_none());
        // User mounts follow
        assert_eq!(binds[1].src, "/data/input");
        assert_eq!(binds[1].dest.as_deref(), Some("/container/input"));
        assert_eq!(binds[1].opts.as_deref(), Some("ro"));
        assert_eq!(binds[2].src, "/dev/ttyUSB0");
        assert!(binds[2].dest.is_none());
        assert!(binds[2].opts.is_none());
    }
}
