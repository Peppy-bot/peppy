//! Concrete start I/O steps invoked from
//! [`super::entity::NodeEntity::prepare_and_spawn`]. Messenger-bound logic
//! (ready check, health check) stays in core-node and runs between
//! [`super::entity::NodeEntity::prepare_and_spawn`] and
//! [`super::entity::NodeEntity::commit_started`].

use parking_lot::Mutex as StdMutex;
use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use config::consts::{PeppyDirs, RUNTIME_CONFIG_VAR_NAME};
use config::node::{NodeConfig, PeppygenLanguage};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use tokio::sync::mpsc;

use crate::archive::extract_tar_zst;
use crate::build_io::{FeedbackLine, FeedbackStream, write_feedback_log_line};

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
    match std::fs::remove_dir_all(&instance_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "Failed to clean up existing instance directory {}: {}",
                instance_dir.display(),
                e
            ));
        }
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
    if let Err(e) = extract_tar_zst(archive_path, &instance_dir) {
        // Best-effort cleanup of the partially-extracted instance dir so a
        // failed extraction doesn't leave orphaned data on disk.
        let _ = std::fs::remove_dir_all(&instance_dir);
        return Err(e);
    }
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
) -> std::io::Result<(Child, PathBuf)> {
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

    crate::build_io::log_cmd_header(
        log_file,
        "start_cmd",
        &start_cmd.join(" "),
        working_dir,
        &[],
    );

    let runtime_config_path = write_runtime_config_temp(peppy_dirs, runtime_config_json5)?;

    let mut command = Command::new(program);
    command.current_dir(working_dir);
    command
        .args(args)
        .stdin(Stdio::null())
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

    let child = command.spawn().map_err(|e| {
        // Spawn failed: the temp runtime config file we just wrote will
        // never be owned by `StartedInstanceCtx`, so clean it up here to
        // avoid orphaning `runtime_config_*.json5` under the peppy tmp dir.
        let _ = std::fs::remove_file(&runtime_config_path);
        let full_cmd = start_cmd.join(" ");
        std::io::Error::other(format!("failed to execute start_cmd `{}`: {}", full_cmd, e))
    })?;
    Ok((child, runtime_config_path))
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

/// Inputs for [`spawn_container_node`].
pub(super) struct SpawnContainerInputs<'a> {
    pub sif_path: &'a Path,
    pub working_dir: &'a Path,
    pub runtime_config_json5: &'a str,
    pub env_vars: &'a [(String, String)],
    pub mount_paths: &'a [String],
    pub apptainer_run_extra_args: &'a [String],
    pub lima_shell_extra_args: &'a [String],
    pub log_file: &'a Arc<StdMutex<File>>,
    pub feedback_tx: &'a mpsc::UnboundedSender<FeedbackLine>,
    pub peppy_dirs: &'a PeppyDirs,
}

/// Starts a container node using the Apptainer runtime.
///
/// Returns a tokio [`Child`] with piped stdout/stderr for async output
/// capture plus the path of the written runtime config temp file.
pub(super) async fn spawn_container_node(
    inputs: SpawnContainerInputs<'_>,
) -> std::io::Result<(Child, PathBuf)> {
    let SpawnContainerInputs {
        sif_path,
        working_dir,
        runtime_config_json5,
        env_vars,
        mount_paths,
        apptainer_run_extra_args,
        lima_shell_extra_args,
        log_file,
        feedback_tx,
        peppy_dirs,
    } = inputs;
    // Apptainer initialization is expensive (it may bootstrap a Lima VM on
    // macOS). Run it on a blocking pool so the tokio runtime isn't stalled.
    let mut apptainer = tokio::task::spawn_blocking(containers::Apptainer::new)
        .await
        .map_err(|e| std::io::Error::other(format!("Apptainer init task failed: {}", e)))?
        .map_err(|e| {
            std::io::Error::other(format!("Failed to initialize Apptainer runtime: {}", e))
        })?;

    let runtime_config_path = write_runtime_config_temp(peppy_dirs, runtime_config_json5)?;
    // Guard that deletes `runtime_config_path` on any early return between
    // here and the successful spawn. On success we `defuse()` it so that
    // ownership of the temp file transfers to `StartedInstanceCtx`, which
    // already removes it during instance teardown.
    let mut runtime_config_guard = TempFileGuard::new(runtime_config_path.clone());

    let sif_str = sif_path
        .to_str()
        .ok_or_else(|| std::io::Error::other("SIF path is not valid UTF-8"))?;

    // Collect all bind mounts (runtime config + user-specified mount_paths).
    let binds = collect_container_binds(&runtime_config_path, mount_paths);

    // Ensure user-specified bind mount sources exist on the host.
    // Skip binds[0] (runtime config file) — its parent dir is already created above.
    //
    // Behaviour:
    //   - If the path already exists, leave it alone (it may be a file,
    //     device, socket, or directory; we must not touch it).
    //   - If the path is under a device/virtual filesystem (`/dev`, `/proc`,
    //     `/sys`), accept it — those nodes are created by the kernel and
    //     may not exist on the host running the daemon.
    //   - Otherwise, `mkdir -p` the source so node-owned scratch / output
    //     directories Just Work. This used to be silent, which masked file
    //     bind typos by turning them into empty directories. We now emit a
    //     loud warning to the per-instance start log for every auto-create
    //     so an unintended mkdir is still visible to the operator.
    ensure_bind_sources(&binds[1..], log_file, feedback_tx)?;

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

    let bind_mounts_str = format!("[{}]", mount_paths.join(", "));
    crate::build_io::log_cmd_header(
        log_file,
        "apptainer run",
        &sif_path.display().to_string(),
        working_dir,
        &[("bind_mounts", &bind_mounts_str)],
    );

    // Get the fully-built std::process::Command from the Apptainer facade,
    // then convert to tokio::process::Command for async stdio piping.
    let std_cmd = apptainer_cmd
        .into_std_command()
        .map_err(|e| std::io::Error::other(format!("Failed to build apptainer command: {}", e)))?;

    let mut command = Command::from(std_cmd);
    command
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command.spawn().map_err(|e| {
        std::io::Error::other(format!(
            "failed to execute apptainer run for `{}`: {}",
            sif_path.display(),
            e
        ))
    })?;
    runtime_config_guard.defuse();
    Ok((child, runtime_config_path))
}

/// Ensures every bind mount source path is usable by the container runtime.
///
/// For each entry:
///   - existing paths are left untouched (they may be files, sockets, devices,
///     or directories — we must not modify them);
///   - paths under kernel-managed virtual filesystems (`/dev`, `/proc`,
///     `/sys`) are accepted as-is, since the kernel materializes them and the
///     daemon's host may legitimately not have the device node;
///   - any other missing path is auto-created with `mkdir -p`, and a warning
///     line is emitted both to the daemon `tracing` log and to the
///     per-instance start log via `feedback_sink`. The warning is the only
///     line of defence against a typo'd file bind being silently turned into
///     an empty directory, so callers MUST pass the per-instance log sink.
///
/// Returns the underlying `io::Error` (with the offending path embedded in
/// the message) if `create_dir_all` fails.
pub(super) fn ensure_bind_sources(
    binds: &[ContainerBind],
    feedback_sink: &Arc<StdMutex<File>>,
    feedback_tx: &mpsc::UnboundedSender<FeedbackLine>,
) -> std::io::Result<()> {
    for bind in binds {
        let src_path = Path::new(&bind.src);
        if src_path.exists() {
            continue;
        }
        let in_special_fs = src_path.starts_with("/dev")
            || src_path.starts_with("/proc")
            || src_path.starts_with("/sys");
        if in_special_fs {
            continue;
        }
        std::fs::create_dir_all(src_path).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "bind mount source does not exist: {} (auto-create failed: {})",
                    bind.src, e
                ),
            )
        })?;
        let warning = format!(
            "auto-created missing bind mount source: {} \
             (if you intended to bind an existing file, this is a typo)",
            bind.src
        );
        warn!("{}", warning);
        write_feedback_log_line(feedback_sink, FeedbackStream::Warning, &warning);
        let _ = feedback_tx.send(FeedbackLine {
            stream: FeedbackStream::Warning,
            line: warning,
        });
    }
    Ok(())
}

/// Drop guard that removes a temp file unless explicitly defused. Used by
/// the start-steps spawners to make sure a freshly-written
/// `runtime_config_*.json5` is cleaned up if any later step fails before
/// ownership can be transferred to `StartedInstanceCtx`.
struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn defuse(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(&path);
        }
    }
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
    output_reader_handles: Vec<JoinHandle<std::io::Result<()>>>,
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
        let guard = stderr_buffer.lock();
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
    use std::io::{Read, Seek, SeekFrom};

    // Read only the tail of the log: chatty nodes can produce many MB and we
    // only need the last `STDERR_TAIL_LINES` `[stderr]` lines anyway.
    const TAIL_BYTES: u64 = 64 * 1024;

    let content = {
        let mut f = log_file.lock();
        let end = match f.seek(SeekFrom::End(0)) {
            Ok(p) => p,
            Err(_) => return String::new(),
        };
        let start = end.saturating_sub(TAIL_BYTES);
        if f.seek(SeekFrom::Start(start)).is_err() {
            return String::new();
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return String::new();
        }
        buf
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

    fn make_log_sink() -> (Arc<StdMutex<File>>, tempfile::NamedTempFile) {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let file = tmp.reopen().expect("reopen tempfile");
        (Arc::new(StdMutex::new(file)), tmp)
    }

    fn make_feedback_channel() -> (
        mpsc::UnboundedSender<FeedbackLine>,
        mpsc::UnboundedReceiver<FeedbackLine>,
    ) {
        mpsc::unbounded_channel()
    }

    #[test]
    fn ensure_bind_sources_leaves_existing_paths_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (sink, log_file) = make_log_sink();
        let binds = vec![ContainerBind {
            src: tmp.path().to_string_lossy().into_owned(),
            dest: None,
            opts: None,
        }];
        let (tx, mut rx) = make_feedback_channel();
        ensure_bind_sources(&binds, &sink, &tx).expect("existing dir should be accepted");
        assert!(rx.try_recv().is_err(), "no warning should be sent");
        let log_contents = std::fs::read_to_string(log_file.path()).unwrap_or_default();
        assert!(
            !log_contents.contains("auto-created"),
            "no warning expected on happy path, got: {log_contents}"
        );
    }

    #[test]
    fn ensure_bind_sources_accepts_special_fs_paths_without_creating() {
        let (sink, log_file) = make_log_sink();
        let binds = vec![
            ContainerBind {
                src: "/dev/does-not-exist-xyz".to_string(),
                dest: None,
                opts: None,
            },
            ContainerBind {
                src: "/proc/missing".to_string(),
                dest: None,
                opts: None,
            },
            ContainerBind {
                src: "/sys/missing".to_string(),
                dest: None,
                opts: None,
            },
        ];
        let (tx, mut rx) = make_feedback_channel();
        ensure_bind_sources(&binds, &sink, &tx).expect("special-fs paths should be accepted");
        assert!(rx.try_recv().is_err(), "no warning should be sent");
        assert!(!Path::new("/dev/does-not-exist-xyz").exists());
        let log_contents = std::fs::read_to_string(log_file.path()).unwrap_or_default();
        assert!(!log_contents.contains("auto-created"));
    }

    #[test]
    fn ensure_bind_sources_creates_missing_dir_and_warns() {
        let parent = tempfile::tempdir().expect("tempdir");
        let target = parent.path().join("scratch").join("nested");
        assert!(!target.exists());
        let (sink, log_file) = make_log_sink();
        let binds = vec![ContainerBind {
            src: target.to_string_lossy().into_owned(),
            dest: None,
            opts: None,
        }];

        let (tx, mut rx) = make_feedback_channel();
        ensure_bind_sources(&binds, &sink, &tx).expect("missing dir should be auto-created");

        assert!(target.is_dir(), "target dir must have been created");
        // Drop the sink mutex's writer view so the warning bytes are flushed.
        drop(sink);
        let log_contents = std::fs::read_to_string(log_file.path()).expect("read log");
        assert!(
            log_contents.contains("auto-created missing bind mount source:"),
            "warning line missing from feedback log, got: {log_contents}"
        );
        assert!(
            log_contents.contains(target.to_string_lossy().as_ref()),
            "warning should mention the offending path, got: {log_contents}"
        );
        // The warning must also be pushed onto the feedback channel as a
        // Warning-stream line so launch forwarders can route it to a
        // high-visibility sink.
        let received = rx
            .try_recv()
            .expect("warning should be pushed to feedback channel");
        assert_eq!(received.stream, FeedbackStream::Warning);
        assert!(
            received
                .line
                .contains("auto-created missing bind mount source:")
        );
        assert!(received.line.contains(target.to_string_lossy().as_ref()));
        assert!(rx.try_recv().is_err(), "exactly one warning expected");
    }

    #[test]
    fn ensure_bind_sources_propagates_create_dir_failures() {
        // /proc/1/<x> is a kernel-managed path that mkdir cannot create. We
        // bypass the /proc shortcut by using /proc/1 as the parent (the
        // shortcut only applies to paths whose top-level component is
        // /dev|/proc|/sys, and ours starts with /proc, so we use a sibling
        // path under a non-special root that we make non-writable instead).
        let parent = tempfile::tempdir().expect("tempdir");
        let ro_parent = parent.path().join("ro");
        std::fs::create_dir(&ro_parent).expect("mkdir ro");
        // Make the parent read-only so create_dir_all fails on the child.
        let mut perms = std::fs::metadata(&ro_parent)
            .expect("metadata")
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o500);
        }
        std::fs::set_permissions(&ro_parent, perms).expect("set perms");
        let target = ro_parent.join("child");

        let (sink, _log) = make_log_sink();
        let binds = vec![ContainerBind {
            src: target.to_string_lossy().into_owned(),
            dest: None,
            opts: None,
        }];

        let (tx, _rx) = make_feedback_channel();
        let err = ensure_bind_sources(&binds, &sink, &tx)
            .expect_err("read-only parent should make auto-create fail");
        let msg = err.to_string();
        assert!(
            msg.contains("bind mount source does not exist"),
            "error must preserve the canonical phrase, got: {msg}"
        );
        assert!(
            msg.contains(target.to_string_lossy().as_ref()),
            "error must mention the offending path, got: {msg}"
        );

        // Restore permissions so the tempdir can be cleaned up.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&ro_parent)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&ro_parent, perms).expect("restore perms");
        }
    }

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
