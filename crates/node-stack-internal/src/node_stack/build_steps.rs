//! Concrete build I/O steps invoked from [`super::entity::NodeEntity::build`].

use parking_lot::Mutex as StdMutex;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use config::consts::PeppyDirs;
use tokio::sync::mpsc;
use tracing::debug;
use zstd::stream::write::Encoder as ZstdEncoder;

use crate::build_io::{FeedbackLine, FeedbackStream, spawn_in_process_group, stream_child_output};

/// Per-process counter used to make build-staging tmp filenames unique so
/// concurrent builds for the same node:tag cannot clobber each other.
static STAGING_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Unique staging path so concurrent builds for the same `node:tag` cannot
/// clobber each other's in-flight file.
fn staging_tmp_path(storage_dir: &Path, final_name: &str) -> PathBuf {
    storage_dir.join(format!(
        "{}.{}.{}.tmp",
        final_name,
        std::process::id(),
        STAGING_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ))
}

/// Publishes an artifact into `storage_dir/final_name` atomically: allocates a
/// unique tmp path, lets `write` populate it, then renames. If `write` (or
/// any later step) fails, the tmp path is cleaned up before the error is
/// returned. Used by both [`archive_dir_to_storage`] and [`move_sif_to_storage`].
fn publish_to_storage_atomic<F>(
    storage_dir: &Path,
    final_name: &str,
    write: F,
) -> std::io::Result<PathBuf>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    std::fs::create_dir_all(storage_dir)?;
    let tmp_path = staging_tmp_path(storage_dir, final_name);
    let final_path = storage_dir.join(final_name);
    if let Err(e) = write(&tmp_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(final_path)
}

/// Validates that `node_tag` is safe to splice into a filename joined under
/// the storage directory. Re-validates the raw `Manifest::tag` string before
/// it ever reaches `storage_dir.join(...)` to prevent path traversal or
/// absolute-path injection (e.g. a tag like `../etc/passwd`).
pub(super) fn validate_node_tag(node_tag: &str) -> std::io::Result<()> {
    config::repo_node_id::validate_repo_node_tag(node_tag, "node tag")
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
}

/// Archives the contents of `source_dir` into a `.tar.zst` file in the
/// peppy built nodes directory.
///
/// The archive path follows the format: `<storage_dir>/<node_name>_<tag>.tar.zst`
///
/// Uses zstd compression level 1 (fastest speed).
pub(super) fn archive_dir_to_storage(
    source_dir: &Path,
    node_name: &str,
    node_tag: &str,
    peppy_dirs: &PeppyDirs,
) -> std::io::Result<PathBuf> {
    validate_node_tag(node_tag)?;
    let storage_dir = peppy_dirs.built_nodes_dir();
    let archive_name = format!("{}_{}.tar.zst", node_name, node_tag);
    publish_to_storage_atomic(&storage_dir, &archive_name, |tmp_path| {
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

/// Moves the built `.sif` container image from the working directory to peppy storage.
///
/// The image is expected at `working_dir/{node_name}_{node_tag}.sif`, which is the
/// conventional output path produced by `apptainer build`.
///
/// Returns the final storage path: `<built_nodes_dir>/<node_name>_<tag>.sif`.
pub(super) fn move_sif_to_storage(
    working_dir: &Path,
    node_name: &str,
    node_tag: &str,
    peppy_dirs: &PeppyDirs,
) -> std::io::Result<PathBuf> {
    validate_node_tag(node_tag)?;
    let sif_name = format!("{}_{}.sif", node_name, node_tag);
    let sif_source = working_dir.join(&sif_name);
    let storage_dir = peppy_dirs.built_nodes_dir();

    // Copy + rename (not rename alone) because the working dir may be on a
    // different filesystem than storage.
    publish_to_storage_atomic(&storage_dir, &sif_name, |tmp_path| {
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
    pub feedback_tx: &'a mpsc::UnboundedSender<FeedbackLine>,
    pub log_file: Arc<StdMutex<File>>,
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
    // *after* apptainer has run — too late to prevent the escape.
    validate_node_tag(inputs.node_tag).map_err(|e| format!("invalid node tag: {}", e))?;

    if !containers::Apptainer::is_lima_ready() {
        let _ = inputs.feedback_tx.send(FeedbackLine {
            stream: FeedbackStream::Stdout,
            line: "Initializing Lima VM for container build (first run may take a few minutes)..."
                .to_string(),
        });
    }

    let apptainer = tokio::task::spawn_blocking(containers::Apptainer::new)
        .await
        .map_err(|e| format!("Apptainer initialization task failed: {}", e))?
        .map_err(|e| format!("Failed to initialize Apptainer runtime: {}", e))?;

    let sif_name = format!("{}_{}.sif", inputs.node_name, inputs.node_tag);
    let output_path = inputs.working_dir.join(&sif_name);
    let def_path = inputs.working_dir.join(inputs.def_file);

    let mut cmd_builder = apptainer.build(&output_path, &def_path);
    for arg in inputs.apptainer_build_extra_args {
        cmd_builder = cmd_builder.raw_flag(arg);
    }
    cmd_builder = cmd_builder.lima_shell_extra_args(inputs.lima_shell_extra_args);

    let std_cmd = cmd_builder
        .into_std_command()
        .map_err(|e| format!("Failed to build apptainer command: {}", e))?;

    let mut cmd = tokio::process::Command::from(std_cmd);
    // Set the working directory so `%files . /opt/{name}` in the .def file
    // copies from the node's source directory, not the daemon's cwd.
    cmd.current_dir(inputs.working_dir);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());

    let child = spawn_in_process_group(cmd)
        .map_err(|e| format!("Failed to spawn apptainer build: {}", e))?;

    let (status, stderr_tail) =
        stream_child_output(child, inputs.feedback_tx, inputs.log_file, true).await?;

    if !status.success() {
        let mut msg = format!("apptainer build failed with status {}", status);
        if !stderr_tail.is_empty() {
            msg.push_str("\n\n--- stderr (last lines) ---\n");
            msg.push_str(&stderr_tail.join("\n"));
        }
        return Err(msg);
    }

    Ok(())
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
    // references — let `sh -c` expand them at runtime against the env vars
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
        .map_err(|e| format!("failed to execute build_cmd `{}`: {}", full_cmd_display, e))?;

    let (status, _) = stream_child_output(child, feedback_tx, log_file, false).await?;

    if !status.success() {
        return Err(format!(
            "build_cmd `{}` failed with status {}",
            full_cmd_display, status
        ));
    }

    debug!("build_cmd completed successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_node_tag_accepts_safe_tags() {
        for tag in ["1.2.3", "v1.0", "latest", "1.0.0-rc1", "abc_def", "A1", "0"] {
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
        let err = build_container_image(ContainerBuildInputs {
            working_dir,
            node_name: "sensor",
            node_tag: "../evil",
            def_file: "sensor.def",
            apptainer_build_extra_args: &[],
            lima_shell_extra_args: &[],
            feedback_tx: &feedback_tx,
            log_file,
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
            "", "..", ".", ".hidden", "../etc", "foo/bar", "a\\b", "a b", "tag$", "/abs",
        ] {
            assert!(
                validate_node_tag(tag).is_err(),
                "expected {:?} to be rejected",
                tag
            );
        }
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
}
