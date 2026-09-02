//! Concrete build I/O steps invoked from [`super::entity::NodeEntity::build`].

use parking_lot::Mutex as StdMutex;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use daemon_config::consts::PeppyDirs;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use zstd::stream::write::Encoder as ZstdEncoder;

use crate::build_io::{
    FeedbackLine, FeedbackStream, announce, spawn_in_process_group, stream_child_output,
};
use crate::build_progress::BuildProgressMonitor;
use crate::node_stack::container_build_cache;
use config::node::PeppygenLanguage;

/// Validates that `node_tag` is safe to splice into a filename joined under
/// the storage directory. Re-validates the raw `Manifest::tag` string before
/// it ever reaches `storage_dir.join(...)` to prevent path traversal or
/// absolute-path injection (e.g. a tag like `../etc/passwd`).
pub(super) fn validate_node_tag(node_tag: &str) -> std::io::Result<()> {
    config::repo_node_id::validate_repo_node_tag(node_tag, "node tag")
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
}

/// Archives the contents of `source_dir` into the `.tar.zst` file at
/// `destination`, the artifact slot the build resolved (see
/// [`super::build_artifact_cache`]). Missing parent directories are created.
///
/// Uses zstd compression level 1 (fastest speed).
pub(super) fn archive_dir_to_storage(
    source_dir: &Path,
    destination: &Path,
) -> std::io::Result<PathBuf> {
    daemon_config::atomic_write::publish_atomic(destination, |tmp_path| {
        let file = File::create(tmp_path)?;
        let encoder = ZstdEncoder::new(file, 1)?;
        let mut tar_builder = tar::Builder::new(encoder);
        // DO NOT follow symlinks, otherwise it could create unintended behavior
        // for the user who modify files in the path pointed by the symlink
        tar_builder.follow_symlinks(false);
        tar_builder.append_dir_all(".", source_dir)?;
        let encoder = tar_builder.into_inner()?;
        encoder.finish()?;
        Ok(())
    })
}

/// Moves the built `.sif` container image from the working directory to
/// `destination`, the artifact slot the build resolved (see
/// [`super::build_artifact_cache`]). Missing parent directories are created.
///
/// The image is expected at `working_dir/{node_name}_{node_tag}.sif`, which is the
/// conventional output path produced by `apptainer build`.
pub(super) fn move_sif_to_storage(
    working_dir: &Path,
    node_name: &str,
    node_tag: &str,
    destination: &Path,
) -> std::io::Result<PathBuf> {
    validate_node_tag(node_tag)?;
    let sif_name = format!("{}_{}.sif", node_name, node_tag);
    let sif_source = working_dir.join(&sif_name);

    // Copy + rename (not rename alone) because the working dir may be on a
    // different filesystem than storage.
    daemon_config::atomic_write::publish_atomic(destination, |tmp_path| {
        std::fs::copy(&sif_source, tmp_path)
            .map(|_| ())
            .map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "Expected container image at {}: {}",
                        sif_source.display(),
                        e
                    ),
                )
            })
    })
}

/// Inputs needed to drive an apptainer container build to completion.
pub(super) struct ContainerBuildInputs<'a> {
    pub working_dir: &'a Path,
    pub node_name: &'a str,
    pub node_tag: &'a str,
    pub def_file: &'a str,
    pub apptainer_build_extra_args: &'a [String],
    pub lima_shell_extra_args: &'a [String],
    pub language: PeppygenLanguage,
    pub feedback_tx: &'a mpsc::UnboundedSender<FeedbackLine>,
    pub log_file: Arc<StdMutex<File>>,
    /// Needed to register the peppy data root as a Lima mount: `working_dir`
    /// lives under `tmp_dir()`, which sits outside `$HOME` whenever the root
    /// does (dev builds root at `$TMPDIR/.peppy`), and the guest VM cannot
    /// see it otherwise.
    pub peppy_dirs: &'a PeppyDirs,
    /// Fired when a `--force` build supersedes this one. On Linux the
    /// host process-group SIGKILL is enough; on macOS the guest-side apptainer
    /// (and its `%post` children) live in a separate kernel and are killed via
    /// [`containers::Apptainer::kill_guest_process_group`].
    pub cancel_token: &'a CancellationToken,
}

/// Total attempts (initial try plus retries) for an `apptainer build` whose
/// base image could not be fetched from its registry.
const CONTAINER_BUILD_ATTEMPTS: usize = 3;
/// Backoff before retry N+1. Registry-side fetch failures are either a
/// per-request hiccup, which the next request survives, or an exhausted
/// pull quota, which no short backoff can fix; the delays stay short so the
/// second kind surfaces quickly instead of stalling the build.
const CONTAINER_BUILD_RETRY_DELAYS: [Duration; CONTAINER_BUILD_ATTEMPTS - 1] =
    [Duration::from_secs(1), Duration::from_secs(5)];

/// Whether a failed `apptainer build` died while acquiring its base image.
///
/// Apptainer prefixes every source-acquisition error with
/// `conveyor failed to get:` (registry token, manifest, and layer requests
/// all surface through it), and that phase runs before `%post` and SIF
/// assembly. A failure carrying this signature therefore lost no build work:
/// re-running the build repeats only the fetch, and layers the failed attempt
/// already downloaded sit in apptainer's content-addressed cache for the
/// retry. A registry that answered with an empty or truncated body (the
/// `unexpected end of JSON input` variant) or dropped a transfer mid-stream
/// (the EOF variants) is a transient, per-request condition, so a retry can
/// succeed where the first attempt failed.
fn failed_fetching_base_image(stderr_tail: &[String]) -> bool {
    stderr_tail
        .iter()
        .any(|line| line.contains("conveyor failed to get"))
}

/// Builds a container image using the Apptainer facade.
///
/// Runs `apptainer build {node_name}_{node_tag}.sif {def_file}` in the
/// working directory. Build output is streamed to both the CLI (via the feedback
/// publisher) and the log file. On failure, the last
/// [`crate::build_io::STDERR_TAIL_LINES`] lines of stderr are included in the
/// error message.
///
/// The resulting `.sif` file is left in `working_dir` for [`move_sif_to_storage`]
/// to relocate.
pub(super) async fn build_container_image(
    inputs: ContainerBuildInputs<'_>,
) -> std::result::Result<(), String> {
    // Validate the tag *before* it gets spliced into the SIF filename and
    // joined onto the working dir. Without this, a tag like `../evil` would
    // make `output_path` escape `working_dir` and apptainer would happily
    // write the image outside the build sandbox. The downstream
    // `move_sif_to_storage` already calls `validate_node_tag`, but only
    // *after* apptainer has run, too late to prevent the escape.
    validate_node_tag(inputs.node_tag).map_err(|e| format!("invalid node tag: {}", e))?;

    if !containers::Apptainer::is_lima_ready() {
        let _ = inputs.feedback_tx.send(FeedbackLine {
            stream: FeedbackStream::Stdout,
            line: "Initializing Lima VM for container build (first run may take a few minutes)..."
                .to_string(),
        });
    }

    let mut apptainer = tokio::task::spawn_blocking(containers::Apptainer::new)
        .await
        .map_err(|e| format!("Apptainer initialization task failed: {}", e))?
        .map_err(|e| format!("Failed to initialize Apptainer runtime: {}", e))?;

    // The build's working dir (def file, `%files` sources, output .sif) lives
    // under the peppy data root. When that root is outside `$HOME` (dev roots
    // at `$TMPDIR/.peppy`), the Lima guest cannot see it unless it is
    // registered as an explicit mount; `ensure_host_mounts` is a no-op for
    // home-relative roots and on the native (Linux) backend. Runs on the
    // blocking pool because a first-time mount registration restarts the VM.
    let peppy_root = inputs
        .peppy_dirs
        .root()
        .to_str()
        .ok_or_else(|| "peppy root path is not valid UTF-8".to_string())?
        .to_owned();
    let apptainer = tokio::task::spawn_blocking(move || {
        apptainer
            .ensure_host_mounts(&[&peppy_root])
            .map(|()| apptainer)
    })
    .await
    .map_err(|e| format!("Host mount registration task failed: {}", e))?
    .map_err(|e| format!("Failed to ensure peppy root is mounted in the VM: {}", e))?;

    let sif_name = format!("{}_{}.sif", inputs.node_name, inputs.node_tag);
    let output_path = inputs.working_dir.join(&sif_name);
    let def_path = inputs.working_dir.join(inputs.def_file);

    // Cache preparation is filesystem work (def read, layout creation, an
    // ELF inspection, potentially a binary download), so it runs on the
    // blocking pool like the other build I/O above. A def file that cannot
    // be read as UTF-8 skips caching outright, since the conflict scan
    // cannot see what such a build references; a missing def file surfaces
    // as an apptainer error below either way.
    let build_cache = {
        let peppy_dirs = inputs.peppy_dirs.clone();
        let language = inputs.language;
        let extra_args = inputs.apptainer_build_extra_args.to_vec();
        let def_path = def_path.clone();
        tokio::task::spawn_blocking(move || {
            let def_contents = std::fs::read_to_string(&def_path).ok()?;
            container_build_cache::prepare(&peppy_dirs, language, &def_contents, &extra_args)
        })
        .await
        .map_err(|e| format!("Build cache preparation task failed: {}", e))?
    };
    if let Some(cache) = &build_cache {
        announce(inputs.feedback_tx, &inputs.log_file, cache.summary.clone());
    }

    // On macOS the build runs inside a Lima VM, so SIGKILL'ing the host
    // `limactl shell` does not reach the guest `apptainer build` or its
    // `%post` children. The facade therefore runs the guest build as a
    // process-group leader and records its PGID to a guest-native file keyed by
    // this build key, which `kill_guest_process_group` (called with the same key)
    // uses on cancel to SIGKILL the whole guest group. The working-dir basename
    // is a unique, filesystem-safe key. A no-op on the native backend.
    let build_key = inputs
        .working_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned);

    // The working dir must go through the facade (not `Command::current_dir`)
    // so `%files . /opt/{name}` in the .def file copies from the node's source
    // directory on both backends: under Lima the facade `cd`s inside the guest
    // and aborts if the directory is not mounted, where a host-side
    // `current_dir` would be canonicalized by `limactl shell`, miss the mount,
    // and silently fall back to the guest home directory.
    //
    // One attempt per loop iteration; a failed base-image fetch (see
    // [`failed_fetching_base_image`]) re-runs the whole command, since a
    // conveyor-phase failure precedes every other build phase and the retry
    // only repeats the fetch itself.
    let mut attempt = 0;
    loop {
        attempt += 1;

        let mut cmd_builder = apptainer
            .build(&output_path, &def_path)
            .working_dir(inputs.working_dir);
        if let Some(key) = &build_key {
            cmd_builder = cmd_builder.cancel_pgid(key);
        }
        if let Some(cache) = &build_cache {
            cmd_builder = cmd_builder.bind(
                &cache.host_dir.to_string_lossy(),
                Some(container_build_cache::BIND_DEST),
                None,
            );
            for (key, value) in &cache.env {
                cmd_builder = cmd_builder.apptainer_env(key, value);
            }
        }
        for arg in inputs.apptainer_build_extra_args {
            cmd_builder = cmd_builder.raw_flag(arg);
        }
        cmd_builder = cmd_builder.lima_shell_extra_args(inputs.lima_shell_extra_args);

        let std_cmd = cmd_builder
            .into_std_command()
            .map_err(|e| format!("Failed to build apptainer command: {}", e))?;

        let mut cmd = tokio::process::Command::from(std_cmd);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::null());

        let child = spawn_in_process_group(cmd)
            .map_err(|e| format!("Failed to spawn apptainer build: {}", e))?;

        // Disk-growth progress: apptainer suppresses per-blob download progress
        // off-TTY, so a slow base-image pull (and the silent "Creating SIF file..."
        // stretch) would otherwise produce no feedback for minutes and trip the
        // idle timeout. The probe samples every surface this build writes to —
        // apptainer's cache and scratch, the build cache bind (a `%post` that
        // compiles writes mostly there), and the output SIF — and the monitor
        // emits a line only when the total grew, so genuine progress resets the
        // idle clocks while a wedged build still times out. The guard lives on
        // this future's stack: the phase runner dropping the future on timeout,
        // cancellation, and normal completion all abort the monitor.
        let usage_probe = {
            let mut extra_roots = vec![output_path.clone()];
            if let Some(cache) = &build_cache {
                extra_roots.push(cache.host_dir.clone());
            }
            apptainer.cache_usage_probe(extra_roots)
        };
        let progress_monitor = BuildProgressMonitor::spawn(
            move || usage_probe.usage_bytes(),
            inputs.feedback_tx.clone(),
        );

        let stream_result = stream_child_output(
            child,
            inputs.feedback_tx,
            Arc::clone(&inputs.log_file),
            true,
            inputs.cancel_token,
        )
        .await;
        drop(progress_monitor);

        let (status, stderr_tail) = match stream_result {
            Ok(result) => result,
            Err(stream_err) => {
                // A `--force` supersede SIGKILL'd + reaped the host child
                // above; now reach into the VM and kill the guest process
                // group too (no-op on Linux). Reuses the already-initialized
                // facade. Runs on a blocking thread because the guest kill
                // shells out to `limactl`.
                if inputs.cancel_token.is_cancelled()
                    && let Some(key) = build_key
                {
                    match tokio::task::spawn_blocking(move || {
                        apptainer.kill_guest_process_group(&key)
                    })
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            debug!("Failed to kill guest process group on build cancellation: {e}")
                        }
                        Err(e) => debug!("Guest-kill task failed on build cancellation: {e}"),
                    }
                }
                return Err(stream_err);
            }
        };

        if status.success() {
            return Ok(());
        }

        if attempt < CONTAINER_BUILD_ATTEMPTS
            && !inputs.cancel_token.is_cancelled()
            && failed_fetching_base_image(&stderr_tail)
        {
            let delay = CONTAINER_BUILD_RETRY_DELAYS[attempt - 1];
            announce(
                inputs.feedback_tx,
                &inputs.log_file,
                format!(
                    "Base image fetch failed; retrying apptainer build in {delay:?} \
                     (attempt {attempt} of {CONTAINER_BUILD_ATTEMPTS})"
                ),
            );
            tokio::time::sleep(delay).await;
            continue;
        }

        let mut msg = format!("apptainer build failed with status {}", status);
        if !stderr_tail.is_empty() {
            msg.push_str("\n\n--- stderr (last lines) ---\n");
            msg.push_str(&stderr_tail.join("\n"));
        }
        return Err(msg);
    }
}

/// Expands `${VAR}` references in a string using the provided environment
/// variables. Used by [`run_build_cmd`] before spawning the user-defined
/// `build_cmd` so that variable references in multi-element commands work even
/// though the command is executed directly (not through a shell).
pub(super) fn expand_env_vars(s: &str, env_vars: &[(String, String)]) -> String {
    // Single-pass scanner: walk the string looking for `${...}`, resolve each
    // match against `env_vars`, and leave unknown references untouched. The
    // previous implementation did a linear `contains` + `replace` per env var,
    // which reallocated the whole string for every match and scaled with
    // O(len(s) * len(env_vars)) even when nothing needed expanding.
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'{'
            && let Some(end_rel) = s[i + 2..].find('}')
        {
            let end = i + 2 + end_rel;
            let key = &s[i + 2..end];
            if let Some((_, value)) = env_vars.iter().find(|(k, _)| k == key) {
                out.push_str(value);
            } else {
                out.push_str(&s[i..end + 1]);
            }
            i = end + 1;
            continue;
        }
        out.push(s[i..].chars().next().unwrap());
        i += s[i..].chars().next().unwrap().len_utf8();
    }
    out
}

/// Runs the user-defined `build_cmd` for a process node and streams output via
/// the feedback channel. Returns Ok(()) if `build_cmd` is `None` or executes
/// successfully. Used by [`super::entity::NodeEntity::build`] for process
/// nodes after the entity has transitioned to `Building`.
pub(super) async fn run_build_cmd(
    build_cmd: Option<&Vec<String>>,
    working_dir: &Path,
    env_vars: &[(String, String)],
    feedback_tx: &mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    cancel_token: &CancellationToken,
) -> std::result::Result<(), String> {
    let Some(cmd) = build_cmd else {
        return Ok(());
    };

    if cmd.is_empty() {
        return Err("build_cmd is empty".to_string());
    };

    // Build a *display* form (with `${VAR}` references intact) for logs and
    // error messages, and a separate *expanded* form used only to actually
    // spawn the child. Without this split, anything referenced as
    // `${SECRET}` in `build_cmd` would end up in the on-disk log file and in
    // every error string surfaced to clients.
    //
    // For the shell form (single string), do NOT pre-expand `${VAR}`
    // references; let `sh -c` expand them at runtime against the env vars
    // already set on the spawned command via `.env()`. Pre-expansion would
    // splice user-supplied values straight into the shell command line,
    // turning any metacharacters in env values into shell injection.
    //
    // For the exec form (multi-element), we still expand because the child
    // is launched directly (not via a shell), so no shell will perform the
    // expansion for us.
    let (display_program, display_args, program, args) = if cmd.len() == 1 {
        let shell_args = vec!["-c".to_string(), cmd[0].clone()];
        (
            "sh".to_string(),
            shell_args.clone(),
            "sh".to_string(),
            shell_args,
        )
    } else {
        let expanded_cmd: Vec<String> = cmd.iter().map(|s| expand_env_vars(s, env_vars)).collect();
        (
            cmd[0].clone(),
            cmd[1..].to_vec(),
            expanded_cmd[0].clone(),
            expanded_cmd[1..].to_vec(),
        )
    };

    debug!(
        "Running build_cmd: {} {:?} in dir {:?}",
        display_program, display_args, working_dir
    );

    let full_cmd_display = std::iter::once(display_program.as_str())
        .chain(display_args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");

    crate::build_io::log_cmd_header(&log_file, "build_cmd", &full_cmd_display, working_dir, &[]);

    let mut command = tokio::process::Command::new(&program);
    command.args(&args);
    command.current_dir(working_dir);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // Detach stdin so a misbehaving `build_cmd` cannot read from (or block
    // on) the daemon's stdin. Mirrors `build_container_image`.
    command.stdin(Stdio::null());
    for (key, value) in env_vars {
        command.env(key, value);
    }
    let child = spawn_in_process_group(command)
        .map_err(|e| spawn_failure_message(&full_cmd_display, &program, working_dir, &e))?;

    let (status, _) =
        stream_child_output(child, feedback_tx, log_file, false, cancel_token).await?;

    if !status.success() {
        return Err(format!(
            "build_cmd `{}` failed with status {}",
            full_cmd_display, status
        ));
    }

    debug!("build_cmd completed successfully");
    Ok(())
}

/// Explains a failed `build_cmd` spawn. The raw OS error for the common
/// failure, ENOENT, names no path at all, and it covers two very different
/// repairs: the program is not on the PATH the daemon runs build commands
/// with (a host missing a toolchain), or the node's working directory
/// vanished. The error an operator sees for a launch that failed on another
/// machine has to say which one it is.
fn spawn_failure_message(
    full_cmd_display: &str,
    program: &str,
    working_dir: &Path,
    error: &std::io::Error,
) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        if !working_dir.exists() {
            return format!(
                "failed to execute build_cmd `{full_cmd_display}`: \
                 working directory {working_dir:?} does not exist"
            );
        }
        return format!(
            "failed to execute build_cmd `{full_cmd_display}`: program `{program}` \
             not found on the PATH the daemon runs build commands with"
        );
    }
    format!("failed to execute build_cmd `{full_cmd_display}`: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_node_tag_accepts_safe_tags() {
        for tag in ["v1", "v123", "latest", "v2-rc1", "abc_def", "A1", "donut"] {
            assert!(
                validate_node_tag(tag).is_ok(),
                "expected {:?} to be accepted",
                tag
            );
        }
    }

    #[tokio::test]
    async fn build_container_image_rejects_unsafe_tag_before_spawning_apptainer() {
        // Drives the public entry point with a `..` tag and asserts the
        // function fails before any apptainer subprocess is invoked. We
        // detect "before spawn" by passing a working_dir that does not
        // exist on disk: spawning apptainer with a missing cwd would
        // surface a different error (a spawn IO error), whereas the
        // up-front validation rejects with the "invalid node tag" prefix.
        let working_dir = std::path::Path::new("/nonexistent-peppy-test-dir");
        let (feedback_tx, _feedback_rx) = mpsc::unbounded_channel();
        let log_file = Arc::new(StdMutex::new(
            tempfile::tempfile().expect("tempfile should succeed"),
        ));
        let cancel_token = CancellationToken::new();
        let peppy_dirs = PeppyDirs::new("/nonexistent-peppy-test-root");
        let err = build_container_image(ContainerBuildInputs {
            working_dir,
            node_name: "sensor",
            node_tag: "../evil",
            def_file: "sensor.def",
            apptainer_build_extra_args: &[],
            lima_shell_extra_args: &[],
            language: PeppygenLanguage::Rust,
            feedback_tx: &feedback_tx,
            log_file,
            peppy_dirs: &peppy_dirs,
            cancel_token: &cancel_token,
        })
        .await
        .expect_err("unsafe tag must be rejected");
        assert!(
            err.starts_with("invalid node tag"),
            "expected up-front validation rejection, got: {}",
            err
        );
    }

    #[test]
    fn validate_node_tag_rejects_unsafe_tags() {
        for tag in [
            "", "..", ".", ".hidden", "../etc", "foo/bar", "a\\b", "a b", "tag$", "/abs", "1.2.3",
            "v1.0", "1", "0", "1v",
        ] {
            assert!(
                validate_node_tag(tag).is_err(),
                "expected {:?} to be rejected",
                tag
            );
        }
    }

    fn test_log_file() -> Arc<StdMutex<File>> {
        Arc::new(StdMutex::new(
            tempfile::tempfile().expect("tempfile should succeed"),
        ))
    }

    #[tokio::test]
    async fn run_build_cmd_names_a_missing_program() {
        let working_dir = tempfile::tempdir().expect("tempdir should succeed");
        let (feedback_tx, _feedback_rx) = mpsc::unbounded_channel();
        let cmd = vec!["peppy-test-no-such-tool".to_string(), "sync".to_string()];
        let err = run_build_cmd(
            Some(&cmd),
            working_dir.path(),
            &[],
            &feedback_tx,
            test_log_file(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("a nonexistent program must fail to spawn");
        assert!(
            err.contains("program `peppy-test-no-such-tool` not found on the PATH"),
            "the error must name the missing program, got: {err}"
        );
    }

    #[tokio::test]
    async fn run_build_cmd_names_a_missing_working_dir() {
        let working_dir = std::path::Path::new("/nonexistent-peppy-build-cwd");
        let (feedback_tx, _feedback_rx) = mpsc::unbounded_channel();
        let cmd = vec!["true".to_string()];
        let err = run_build_cmd(
            Some(&cmd),
            working_dir,
            &[],
            &feedback_tx,
            test_log_file(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("a missing working directory must fail the spawn");
        assert!(
            err.contains("working directory \"/nonexistent-peppy-build-cwd\" does not exist"),
            "the error must name the missing working directory, got: {err}"
        );
    }

    /// A PATH handed to the child through `env_vars` is what the spawned
    /// program is looked up against. This is what makes the environment a
    /// build goal carries decisive: a goal with no PATH of its own resolves
    /// programs against the daemon's environment, and one that carries a
    /// PATH resolves against that PATH alone.
    #[tokio::test]
    async fn run_build_cmd_resolves_the_program_via_the_child_path() {
        let tool_dir = tempfile::tempdir().expect("tempdir should succeed");
        let tool = tool_dir.path().join("peppy-test-stub-tool");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").expect("write stub tool");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub tool");
        }

        let working_dir = tempfile::tempdir().expect("tempdir should succeed");
        let (feedback_tx, _feedback_rx) = mpsc::unbounded_channel();
        let cmd = vec!["peppy-test-stub-tool".to_string(), "sync".to_string()];
        let env_vars = vec![("PATH".to_string(), tool_dir.path().display().to_string())];
        run_build_cmd(
            Some(&cmd),
            working_dir.path(),
            &env_vars,
            &feedback_tx,
            test_log_file(),
            &CancellationToken::new(),
        )
        .await
        .expect("the stub tool must resolve through the env_vars PATH alone");
    }

    /// The destination is the keyed slot under `built_nodes/<name>_<tag>/`,
    /// which does not exist before the first build of that identity.
    #[test]
    fn archive_dir_to_storage_publishes_at_a_nested_destination() {
        let source = tempfile::tempdir().expect("tempdir");
        std::fs::write(source.path().join("hello.txt"), b"hi").expect("write");
        let storage = tempfile::tempdir().expect("tempdir");
        let destination = storage
            .path()
            .join("built_nodes")
            .join("sensor_v1")
            .join("0123456789abcdef.tar.zst");

        let published =
            archive_dir_to_storage(source.path(), &destination).expect("archive should publish");

        assert_eq!(published, destination);
        let file = std::fs::File::open(&published).expect("open archive");
        let decoder = zstd::stream::read::Decoder::new(file).expect("zstd decoder");
        let mut archive = tar::Archive::new(decoder);
        let names: Vec<PathBuf> = archive
            .entries()
            .expect("tar entries")
            .map(|entry| entry.expect("tar entry").path().expect("path").into_owned())
            .collect();
        assert!(
            names.iter().any(|p| p.ends_with("hello.txt")),
            "archive should contain hello.txt, got {names:?}"
        );
    }

    #[test]
    fn move_sif_to_storage_publishes_at_a_nested_destination() {
        let working_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(working_dir.path().join("sensor_v1.sif"), b"SIF").expect("write");
        let storage = tempfile::tempdir().expect("tempdir");
        let destination = storage
            .path()
            .join("built_nodes")
            .join("sensor_v1")
            .join("0123456789abcdef.sif");

        let published = move_sif_to_storage(working_dir.path(), "sensor", "v1", &destination)
            .expect("image should publish");

        assert_eq!(published, destination);
        assert_eq!(std::fs::read(&published).expect("read"), b"SIF");
    }

    #[test]
    fn move_sif_to_storage_names_the_missing_image() {
        let working_dir = tempfile::tempdir().expect("tempdir");
        let storage = tempfile::tempdir().expect("tempdir");
        let err = move_sif_to_storage(
            working_dir.path(),
            "sensor",
            "v1",
            &storage.path().join("sensor_v1").join("abc.sif"),
        )
        .expect_err("a missing image must fail");
        assert!(
            err.to_string().contains("Expected container image at"),
            "got: {err}"
        );
    }

    #[test]
    fn expand_env_vars_replaces_braced_refs() {
        let env = vec![
            ("FOO".to_string(), "bar".to_string()),
            ("BAZ".to_string(), "qux".to_string()),
        ];
        assert_eq!(expand_env_vars("hello ${FOO}", &env), "hello bar");
        assert_eq!(expand_env_vars("${FOO}-${BAZ}-${FOO}", &env), "bar-qux-bar");
        // Unknown variables are left alone (no expansion).
        assert_eq!(expand_env_vars("${UNKNOWN}", &env), "${UNKNOWN}");
        // Plain strings pass through unchanged.
        assert_eq!(expand_env_vars("nothing here", &env), "nothing here");
    }

    /// The exact stderr shape of a registry answering apptainer's image fetch
    /// with a body it cannot parse, as first seen failing a CI e2e build.
    #[test]
    fn unparseable_registry_response_is_a_fetch_failure() {
        let stderr_tail = [
            "INFO:    Starting build...",
            "INFO:    Fetching OCI image...",
            "FATAL:   While performing build: conveyor failed to get: unexpected end of JSON input",
        ]
        .map(String::from);
        assert!(failed_fetching_base_image(&stderr_tail));
    }

    /// A transfer the registry dropped mid-stream fails the same conveyor
    /// phase and must classify the same way.
    #[test]
    fn interrupted_transfer_is_a_fetch_failure() {
        let stderr_tail =
            ["FATAL:   While performing build: conveyor failed to get: unexpected EOF"]
                .map(String::from);
        assert!(failed_fetching_base_image(&stderr_tail));
    }

    /// Failures from any later phase (`%post`, SIF assembly) carry build work
    /// a retry would repeat, so they must not classify as fetch failures.
    #[test]
    fn post_and_assembly_failures_are_not_fetch_failures() {
        let stderr_tail = [
            "INFO:    Starting build...",
            "INFO:    Fetching OCI image...",
            "INFO:    Running post scriptlet",
            "error: linking with `cc` failed: exit status: 1",
            "FATAL:   While performing build: failed to run %post script: exit status 1",
        ]
        .map(String::from);
        assert!(!failed_fetching_base_image(&stderr_tail));

        // No stderr at all (process killed before logging) is equally not a
        // fetch signature.
        assert!(!failed_fetching_base_image(&[]));
    }
}
