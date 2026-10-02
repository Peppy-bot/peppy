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
use crate::node_stack::container_build_cache;
use config::node::PeppygenLanguage;
use containers::BuildActivityProbe;

/// The cargo settings every build runs with: a container build's `%post` and
/// a process node's `build_cmd` alike, whatever the node's language, since a
/// Python build can run cargo too (a Rust extension it compiles).
///
/// Off a terminal, cargo prints a line only when a crate download completes.
/// Until then it holds the crate in memory and burns no CPU, so a large crate
/// on a slow connection gives the build idle clock no output line, no bytes
/// on disk and no CPU time (see [`crate::build_progress`]), and the idle
/// timeout kills a build that is still downloading. With these settings cargo
/// repaints its progress line, ended by a bare `\r`, each time the line
/// changes: the `remaining bytes` of a download change as bytes arrive, and
/// the build output reader forwards such repaints (see
/// [`crate::build_io::LineSplitter`]). A download that receives nothing
/// leaves the line as it is, so cargo prints nothing and the idle timeout
/// still fires.
///
/// Cargo requires a width when it prints progress to a pipe, as a pipe has
/// none to read.
const CARGO_PROGRESS_ENV: [(&str, &str); 2] = [
    ("CARGO_TERM_PROGRESS_WHEN", "always"),
    ("CARGO_TERM_PROGRESS_WIDTH", "80"),
];

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

/// Moves the `.sif` container image [`build_container_image`] wrote at
/// `sif_source` to `destination`, the artifact slot the build resolved (see
/// [`super::build_artifact_cache`]). Missing parent directories are created.
pub(super) fn move_sif_to_storage(
    sif_source: &Path,
    destination: &Path,
) -> std::io::Result<PathBuf> {
    // Copy + rename (not rename alone) because the working dir may be on a
    // different filesystem than storage.
    daemon_config::atomic_write::publish_atomic(destination, |tmp_path| {
        std::fs::copy(sif_source, tmp_path)
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
/// Returns the path of the resulting `.sif` file, left in `working_dir` for
/// [`move_sif_to_storage`] to relocate.
pub(super) async fn build_container_image(
    inputs: ContainerBuildInputs<'_>,
) -> std::result::Result<PathBuf, String> {
    // Validate the tag *before* it gets spliced into the SIF filename and
    // joined onto the working dir. Without this, a tag like `../evil` would
    // make `output_path` escape `working_dir` and apptainer would happily
    // write the image outside the build sandbox. Nothing downstream checks
    // the tag again.
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
        // A def that exports its own values in `%post` overrides these.
        for (key, value) in CARGO_PROGRESS_ENV {
            cmd_builder = cmd_builder.apptainer_env(key, value);
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

        // Activity progress: apptainer suppresses per-blob download progress
        // off-TTY, so a slow base-image pull (and the silent "Creating SIF file..."
        // stretch) would otherwise produce no feedback for minutes and trip the
        // idle timeout, and a `%post` compiling one large crate is just as
        // silent while it holds the CPU. The probe samples every surface this
        // build writes to (apptainer's cache and scratch, the build cache bind,
        // where a `%post` that compiles writes mostly, and the output SIF) and
        // the CPU time of the build's processes, and the monitor
        // `stream_child_output` runs emits a line only when bytes landed or
        // CPU was burned, so genuine progress resets the idle clocks while a
        // wedged build still times out.
        let activity_probe = {
            let mut extra_roots = vec![output_path.clone()];
            if let Some(cache) = &build_cache {
                extra_roots.push(cache.host_dir.clone());
            }
            apptainer.build_activity_probe(extra_roots, child.id(), build_key.as_deref())
        };

        let stream_result = stream_child_output(
            child,
            move || activity_probe.sample(),
            inputs.feedback_tx,
            Arc::clone(&inputs.log_file),
            true,
            inputs.cancel_token,
        )
        .await;

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
            return Ok(output_path);
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
    // The cargo settings go first, so the goal's own values for them, set
    // next, take precedence.
    for (key, value) in CARGO_PROGRESS_ENV {
        command.env(key, value);
    }
    for (key, value) in env_vars {
        command.env(key, value);
    }
    let child = spawn_in_process_group(command)
        .map_err(|e| spawn_failure_message(&full_cmd_display, &program, working_dir, &e))?;

    // A `build_cmd` compiling one large crate is as silent as a container
    // build's `%post` doing the same, so the same monitor watches it: the
    // working dir is the surface it writes to (`target/` and the like), and
    // the process group it leads, with every descendant of its leader, holds
    // every process it spawned.
    let activity_probe = BuildActivityProbe::host(vec![working_dir.to_path_buf()], child.id());
    let (status, _) = stream_child_output(
        child,
        move || activity_probe.sample(),
        feedback_tx,
        log_file,
        false,
        cancel_token,
    )
    .await?;

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
    ///
    /// The stub is a symlink to a script this repository carries rather than
    /// to one the test writes. A file this process has just written is still
    /// open for writing in every child a sibling test forked, until that
    /// child reaches its own `execve`, and executing it inside that window
    /// fails with `ETXTBSY`. Pointing the name at a file nothing writes keeps
    /// the lookup under test and takes the race out of it.
    ///
    /// The target is a script of this repository's rather than a system tool
    /// because the child is spawned under the stub's own name: `argv[0]` is
    /// `peppy-test-stub-tool`, not whatever the symlink resolves to. A host
    /// whose coreutils are a single multi-call binary (the Rust `uutils`
    /// build several distributions ship, or busybox) reads `argv[0]` as the
    /// utility being asked for and refuses a name it does not know, so a
    /// symlink to `/bin/echo` exits nonzero there and the PATH lookup this
    /// test is about never gets its verdict. What runs a `#!/bin/sh` script
    /// is the interpreter the kernel reads out of its first line, which
    /// `argv[0]` does not reach.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_build_cmd_resolves_the_program_via_the_child_path() {
        use std::os::unix::fs::PermissionsExt;

        let stub = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("stub_tool.sh");
        // A checkout that dropped the mode bit fails the spawn with a message
        // about resolving the program, which is not what would be wrong.
        assert!(
            stub.metadata()
                .unwrap_or_else(|e| panic!("the fixture stands on {}: {e}", stub.display()))
                .permissions()
                .mode()
                & 0o111
                != 0,
            "{} must be executable in the checkout",
            stub.display()
        );
        let tool_dir = tempfile::tempdir().expect("tempdir should succeed");
        // Reachable under this name nowhere but this directory, so only the
        // PATH the test hands the child can resolve it.
        let tool = tool_dir.path().join("peppy-test-stub-tool");
        std::os::unix::fs::symlink(&stub, &tool).expect("symlink stub tool");

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

    /// A shell-form `build_cmd` that prints the cargo progress settings it
    /// runs with.
    #[cfg(unix)]
    const PRINT_CARGO_PROGRESS_SETTINGS: &str =
        r#"echo "progress: $CARGO_TERM_PROGRESS_WHEN $CARGO_TERM_PROGRESS_WIDTH""#;

    /// Runs `script` as a shell-form `build_cmd` with `env_vars`, and returns
    /// the lines that reached the feedback channel.
    #[cfg(unix)]
    async fn build_cmd_output(script: &str, env_vars: &[(String, String)]) -> Vec<String> {
        let working_dir = tempfile::tempdir().expect("tempdir should succeed");
        let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel();
        run_build_cmd(
            Some(&vec![script.to_string()]),
            working_dir.path(),
            env_vars,
            &feedback_tx,
            test_log_file(),
            &CancellationToken::new(),
        )
        .await
        .expect("the build_cmd should succeed");
        let mut lines = Vec::new();
        while let Ok(line) = feedback_rx.try_recv() {
            lines.push(line.line);
        }
        lines
    }

    /// A `build_cmd` runs with [`CARGO_PROGRESS_ENV`], whatever the
    /// environment of the daemon says.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_build_cmd_runs_with_the_cargo_progress_settings() {
        let lines = build_cmd_output(PRINT_CARGO_PROGRESS_SETTINGS, &[]).await;
        assert!(
            lines.contains(&"progress: always 80".to_string()),
            "the build_cmd must see the cargo progress settings, got {lines:?}"
        );
    }

    /// The environment a build goal carries wins over [`CARGO_PROGRESS_ENV`]:
    /// a caller that sets one of the settings gets its own value, and the
    /// other setting still applies.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_build_cmd_keeps_a_cargo_progress_setting_of_the_goal() {
        let env_vars = vec![("CARGO_TERM_PROGRESS_WHEN".to_string(), "never".to_string())];
        let lines = build_cmd_output(PRINT_CARGO_PROGRESS_SETTINGS, &env_vars).await;
        assert!(
            lines.contains(&"progress: never 80".to_string()),
            "the goal's own setting must win, got {lines:?}"
        );
    }

    /// Name of the one crate [`HeldDownloadRegistry`] serves.
    const HELD_CRATE: &str = "held_crate";

    /// A sparse cargo registry on loopback that serves [`HELD_CRATE`] 0.1.0
    /// and sends half of its download, then holds the other half until
    /// `release` is notified. The server task ends with the registry.
    struct HeldDownloadRegistry {
        /// The `index` value of a cargo registry that names this one.
        index_url: String,
        release: Arc<tokio::sync::Notify>,
        server: tokio::task::JoinHandle<()>,
    }

    /// What [`HeldDownloadRegistry`] serves, by request path.
    struct HeldRegistryFiles {
        config: String,
        index_entry: String,
        archive: Vec<u8>,
    }

    impl HeldDownloadRegistry {
        async fn start() -> Self {
            use sha2::Digest;

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("the registry should bind a loopback port");
            let base_url = format!(
                "http://{}",
                listener.local_addr().expect("the registry has an address")
            );
            let archive = held_crate_archive();
            let checksum: String = sha2::Sha256::digest(&archive)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let files = Arc::new(HeldRegistryFiles {
                config: format!(r#"{{"dl":"{base_url}/dl"}}"#),
                index_entry: format!(
                    r#"{{"name":"{HELD_CRATE}","vers":"0.1.0","deps":[],"cksum":"{checksum}","features":{{}},"yanked":false}}"#
                ),
                archive,
            });
            let release = Arc::new(tokio::sync::Notify::new());
            let server = tokio::spawn({
                let release = Arc::clone(&release);
                async move {
                    while let Ok((socket, _)) = listener.accept().await {
                        tokio::spawn(serve_registry_request(
                            socket,
                            Arc::clone(&files),
                            Arc::clone(&release),
                        ));
                    }
                }
            });
            Self {
                index_url: format!("sparse+{base_url}/index/"),
                release,
                server,
            }
        }
    }

    impl Drop for HeldDownloadRegistry {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    /// The `.crate` archive of [`HELD_CRATE`] 0.1.0. Its padding does not
    /// compress, so half of the archive is a download with bytes left.
    fn held_crate_archive() -> Vec<u8> {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let padding: Vec<u8> = (0..64 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let manifest = format!(
            "[package]\nname = \"{HELD_CRATE}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
        );
        let files: [(&str, &[u8]); 3] = [
            ("Cargo.toml", manifest.as_bytes()),
            ("src/lib.rs", b""),
            ("padding.bin", &padding),
        ];
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            builder
                .append_data(&mut header, format!("{HELD_CRATE}-0.1.0/{path}"), contents)
                .expect("append to the crate archive");
        }
        builder
            .into_inner()
            .expect("finish the crate tarball")
            .finish()
            .expect("finish the crate gzip stream")
    }

    /// Answers one request of a connection to [`HeldDownloadRegistry`], then
    /// closes the connection.
    async fn serve_registry_request(
        mut socket: tokio::net::TcpStream,
        files: Arc<HeldRegistryFiles>,
        release: Arc<tokio::sync::Notify>,
    ) -> std::io::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await?;
            if read == 0 {
                return Ok(());
            }
            request.extend_from_slice(&chunk[..read]);
        }
        let request = String::from_utf8_lossy(&request);
        let path = request.split(' ').nth(1).unwrap_or_default();
        let index_path = format!(
            "/index/{}/{}/{HELD_CRATE}",
            &HELD_CRATE[..2],
            &HELD_CRATE[2..4]
        );
        let download_path = format!("/dl/{HELD_CRATE}/0.1.0/download");
        let header = |status: &str, length: usize| {
            format!("HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n")
        };

        if path == download_path {
            let (sent, held) = files.archive.split_at(files.archive.len() / 2);
            socket
                .write_all(header("200 OK", files.archive.len()).as_bytes())
                .await?;
            socket.write_all(sent).await?;
            socket.flush().await?;
            release.notified().await;
            return socket.write_all(held).await;
        }
        let body = match path {
            "/index/config.json" => files.config.as_bytes(),
            path if path == index_path => files.index_entry.as_bytes(),
            _ => {
                return socket
                    .write_all(header("404 Not Found", 0).as_bytes())
                    .await;
            }
        };
        socket
            .write_all(header("200 OK", body.len()).as_bytes())
            .await?;
        socket.write_all(body).await
    }

    /// Reads cargo's stderr until a repaint, a fragment ended by a bare
    /// `\r`, names the bytes a download has left. `Err` carries everything
    /// read when the stream ends without one.
    ///
    /// The fragments are cut here, not by [`crate::build_io::LineSplitter`]:
    /// the splitter holds a fragment ended by `\r` until the next byte shows
    /// it is not half of a `\r\n`, and cargo prints nothing after its repaint
    /// while the download is held. In a build, the next repaint is that byte.
    async fn first_remaining_bytes_repaint(
        stderr: &mut tokio::process::ChildStderr,
    ) -> std::result::Result<String, String> {
        use tokio::io::AsyncReadExt;

        let mut output = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stderr
                .read(&mut chunk)
                .await
                .map_err(|e| format!("reading cargo's stderr failed: {e}"))?;
            if read == 0 {
                return Err(String::from_utf8_lossy(&output).into_owned());
            }
            output.extend_from_slice(&chunk[..read]);
            let repaint = output
                .split_inclusive(|byte| matches!(byte, b'\r' | b'\n'))
                .filter(|fragment| fragment.ends_with(b"\r"))
                .map(String::from_utf8_lossy)
                .find(|fragment| fragment.contains("remaining bytes"));
            if let Some(repaint) = repaint {
                return Ok(repaint.trim_end().to_owned());
            }
        }
    }

    /// [`CARGO_PROGRESS_ENV`] makes cargo report a crate download while it is
    /// in flight, which is what keeps the idle clock of a build alive on a
    /// slow connection: with the registry holding the download half sent,
    /// cargo repaints a progress line, ended by a bare `\r`, that names the
    /// bytes left. The registry holds the download until that repaint
    /// arrives, so the test waits on the repaint and not on a duration.
    /// Without the settings, cargo prints nothing until the download
    /// completes, and the test never sees a repaint.
    ///
    /// The test reads cargo's stderr itself rather than through
    /// [`run_build_cmd`]: cargo prints a progress line only when it changes,
    /// so a held download yields few repaints, and the build output reader
    /// may coalesce one of them away (the other tests here and those of
    /// [`crate::build_io`] cover how the reader forwards repaints).
    #[tokio::test]
    async fn cargo_reports_a_held_crate_download_with_the_progress_settings() {
        use tokio::io::AsyncReadExt;

        // Bounds a hang only: the outcome never depends on how fast cargo is.
        const HANG_GUARD: Duration = Duration::from_secs(300);
        // Cargo and the curl it embeds route a request through these when
        // they are set, and no proxy reaches the loopback registry.
        const PROXY_ENV_VARS: [&str; 7] = [
            "CARGO_HTTP_PROXY",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];

        let registry = HeldDownloadRegistry::start().await;
        let project = tempfile::tempdir().expect("tempdir should succeed");
        std::fs::write(
            project.path().join("Cargo.toml"),
            format!(
                "[package]\nname = \"held_download_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [dependencies]\n{HELD_CRATE} = {{ version = \"=0.1.0\", registry = \"held\" }}\n"
            ),
        )
        .expect("write the probe manifest");
        std::fs::create_dir(project.path().join("src")).expect("create the probe src dir");
        std::fs::write(project.path().join("src").join("lib.rs"), b"")
            .expect("write the probe lib.rs");
        let cargo_home = tempfile::tempdir().expect("tempdir should succeed");

        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = tokio::process::Command::new(cargo);
        // How cargo prints must not depend on where the test runs: CI sets
        // `CARGO_TERM_COLOR=always`, which wraps the status word of the
        // repaint in escape codes, and `CARGO_TERM_QUIET` hides the progress
        // line. Removed before `CARGO_PROGRESS_ENV` is set, as its variables
        // share the prefix.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("CARGO_TERM_") {
                command.env_remove(key);
            }
        }
        for proxy in PROXY_ENV_VARS {
            command.env_remove(proxy);
        }
        command
            .arg("fetch")
            .current_dir(project.path())
            .envs(CARGO_PROGRESS_ENV)
            .env("CARGO_HOME", cargo_home.path())
            .env("CARGO_REGISTRIES_HELD_INDEX", &registry.index_url)
            // Cargo abandons a download that moves less than 10 bytes a
            // second for this many seconds, and the held one moves none.
            .env("CARGO_HTTP_TIMEOUT", "3600")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("cargo should start");
        let mut stderr = child.stderr.take().expect("cargo's stderr is piped");

        let repaint = tokio::time::timeout(HANG_GUARD, first_remaining_bytes_repaint(&mut stderr))
            .await
            .expect(
                "cargo printed no repaint naming the remaining bytes while its download was held",
            )
            .unwrap_or_else(|output| {
                panic!("cargo ended its output without a remaining bytes repaint:\n{output}")
            });
        registry.release.notify_one();

        let mut rest = Vec::new();
        stderr
            .read_to_end(&mut rest)
            .await
            .expect("read the rest of cargo's stderr");
        let status = child.wait().await.expect("wait for cargo");
        assert!(
            status.success(),
            "cargo fetch must finish once the download is released, got {status}:\n{}",
            String::from_utf8_lossy(&rest)
        );
        assert!(
            repaint.contains("Downloading 1 crate, remaining bytes:"),
            "the repaint must name the held download, got {repaint:?}"
        );
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

        let published =
            move_sif_to_storage(&working_dir.path().join("sensor_v1.sif"), &destination)
                .expect("image should publish");

        assert_eq!(published, destination);
        assert_eq!(std::fs::read(&published).expect("read"), b"SIF");
    }

    #[test]
    fn move_sif_to_storage_names_the_missing_image() {
        let working_dir = tempfile::tempdir().expect("tempdir");
        let storage = tempfile::tempdir().expect("tempdir");
        let err = move_sif_to_storage(
            &working_dir.path().join("sensor_v1.sif"),
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
