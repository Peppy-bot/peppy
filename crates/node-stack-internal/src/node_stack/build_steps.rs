//! Concrete build I/O steps invoked from [`super::entity::NodeEntity::build`].
//!
//! These helpers were originally part of `core-node-internal::services::node::add`
//! and were moved here so that the lifecycle transition `Added → Built` can run
//! without crossing the crate boundary back into core-node-internal.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};

use chrono::Local;
use config::consts::PeppyDirs;
use tokio::sync::mpsc;
use tracing::debug;
use zstd::stream::write::Encoder as ZstdEncoder;

use crate::build_io::{FeedbackLine, FeedbackStream, stream_child_output};

/// Per-process counter used to make build-staging tmp filenames unique so
/// concurrent builds for the same node:tag cannot clobber each other.
static STAGING_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Validates that `node_tag` is safe to splice into a filename joined under
/// the storage directory. Manifest names are constrained by `Name`'s parser,
/// but `Manifest::tag` is a raw `String` — so we re-validate it here before
/// it ever reaches `storage_dir.join(...)` to prevent path traversal or
/// absolute-path injection (e.g. a tag like `../etc/passwd`).
pub(super) fn validate_node_tag(node_tag: &str) -> std::io::Result<()> {
    if node_tag.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "node tag must not be empty",
        ));
    }
    if node_tag == "." || node_tag == ".." || node_tag.starts_with('.') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("node tag must not start with '.': {}", node_tag),
        ));
    }
    if node_tag.contains("..") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("node tag must not contain '..': {}", node_tag),
        ));
    }
    for c in node_tag.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-';
        if !ok {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "node tag contains disallowed character {:?}: {}",
                    c, node_tag
                ),
            ));
        }
    }
    Ok(())
}

/// Archives the contents of `source_dir` into a `.tar.zst` file in the
/// peppy added nodes directory.
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
    let storage_dir = peppy_dirs.added_nodes_dir();
    std::fs::create_dir_all(&storage_dir)?;

    let archive_name = format!("{}_{}.tar.zst", node_name, node_tag);
    let archive_path = storage_dir.join(&archive_name);
    // Per-build unique staging path so concurrent builds for the same
    // node:tag cannot clobber each other's in-flight tmp file. The final
    // rename to `archive_path` is atomic and is what publishes the artifact.
    let tmp_path = storage_dir.join(format!(
        "{}.{}.{}.tmp",
        archive_name,
        std::process::id(),
        STAGING_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));

    let file = File::create(&tmp_path)?;
    let encoder = ZstdEncoder::new(file, 1)?;
    let mut tar_builder = tar::Builder::new(encoder);
    // DO NOT follow symlinks, otherwise it could create unintended behavior for the user who modify files in the path pointed by the symlink
    tar_builder.follow_symlinks(false);
    if let Err(e) = tar_builder.append_dir_all(".", source_dir) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    let encoder = match tar_builder.into_inner() {
        Ok(e) => e,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
    };
    if let Err(e) = encoder.finish() {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    std::fs::rename(&tmp_path, &archive_path)?;

    Ok(archive_path)
}

/// Moves the built `.sif` container image from the working directory to peppy storage.
///
/// The image is expected at `working_dir/{node_name}_{node_tag}.sif`, which is the
/// conventional output path produced by `apptainer build`.
///
/// Returns the final storage path: `<added_nodes_dir>/<node_name>_<tag>.sif`.
pub(super) fn move_sif_to_storage(
    working_dir: &Path,
    node_name: &str,
    node_tag: &str,
    peppy_dirs: &PeppyDirs,
) -> std::io::Result<PathBuf> {
    validate_node_tag(node_tag)?;
    let sif_name = format!("{}_{}.sif", node_name, node_tag);
    let sif_source = working_dir.join(&sif_name);

    if !sif_source.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Expected container image not found at {}",
                sif_source.display()
            ),
        ));
    }

    let storage_dir = peppy_dirs.added_nodes_dir();
    std::fs::create_dir_all(&storage_dir)?;

    let dest_path = storage_dir.join(&sif_name);
    // Per-build unique staging path; see archive_dir_to_storage for rationale.
    let tmp_path = storage_dir.join(format!(
        "{}.{}.{}.tmp",
        sif_name,
        std::process::id(),
        STAGING_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));

    // Copy + rename (not rename alone) because the working dir may be on a
    // different filesystem than storage. Matches archive_dir_to_storage pattern.
    if let Err(e) = std::fs::copy(&sif_source, &tmp_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp_path, &dest_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    Ok(dest_path)
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

    let child = cmd
        .spawn()
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
/// variables. Used by [`run_add_cmd`] before spawning the user-defined
/// `add_cmd` so that variable references in multi-element commands work even
/// though the command is executed directly (not through a shell).
pub(super) fn expand_env_vars(s: &str, env_vars: &[(String, String)]) -> String {
    let mut result = s.to_string();
    for (key, value) in env_vars {
        let pattern = format!("${{{}}}", key);
        if result.contains(&pattern) {
            result = result.replace(&pattern, value);
        }
    }
    result
}

/// Runs the user-defined `add_cmd` for a process node and streams output via
/// the feedback channel. Returns Ok(()) if `add_cmd` is `None` or executes
/// successfully. Used by [`super::entity::NodeEntity::build`] for process
/// nodes after the entity has transitioned to `Building`.
pub(super) async fn run_add_cmd(
    add_cmd: Option<&Vec<String>>,
    working_dir: &Path,
    env_vars: &[(String, String)],
    feedback_tx: &mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
) -> std::result::Result<(), String> {
    let Some(cmd) = add_cmd else {
        return Ok(());
    };

    if cmd.is_empty() {
        return Err("add_cmd is empty".to_string());
    };

    // Build a *display* form (with `${VAR}` references intact) for logs and
    // error messages, and a separate *expanded* form used only to actually
    // spawn the child. Without this split, anything referenced as
    // `${SECRET}` in `add_cmd` would end up in the on-disk log file and in
    // every error string surfaced to clients.
    let display_cmd: Vec<String> = cmd.clone();

    let (display_program, display_args) = if display_cmd.len() == 1 {
        (
            "sh".to_string(),
            vec!["-c".to_string(), display_cmd[0].clone()],
        )
    } else {
        (display_cmd[0].clone(), display_cmd[1..].to_vec())
    };

    // For the shell form (single string), do NOT pre-expand `${VAR}`
    // references — let `sh -c` expand them at runtime against the env vars
    // already set on the spawned command via `.env()`. Pre-expansion would
    // splice user-supplied values straight into the shell command line,
    // turning any metacharacters in env values into shell injection.
    //
    // For the exec form (multi-element), we still expand because the child
    // is launched directly (not via a shell), so no shell will perform the
    // expansion for us.
    let (program, args) = if display_cmd.len() == 1 {
        (
            "sh".to_string(),
            vec!["-c".to_string(), display_cmd[0].clone()],
        )
    } else {
        let expanded_cmd: Vec<String> = cmd.iter().map(|s| expand_env_vars(s, env_vars)).collect();
        (expanded_cmd[0].clone(), expanded_cmd[1..].to_vec())
    };

    debug!(
        "Running add_cmd: {} {:?} in dir {:?}",
        display_program, display_args, working_dir
    );

    let full_cmd_display = std::iter::once(display_program.as_str())
        .chain(display_args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");

    // Log the command being executed to the log file before attempting to spawn
    {
        if let Ok(mut file) = log_file.lock() {
            let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
            let _ = writeln!(
                file,
                "[{}] Executing add_cmd: {} (working_dir: {})",
                timestamp,
                full_cmd_display,
                working_dir.display()
            );
            let _ = file.flush();
        }
    }

    let mut command = tokio::process::Command::new(&program);
    command.args(&args);
    command.current_dir(working_dir);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // Detach stdin so a misbehaving `add_cmd` cannot read from (or block
    // on) the daemon's stdin. Mirrors `build_container_image`.
    command.stdin(Stdio::null());
    for (key, value) in env_vars {
        command.env(key, value);
    }
    let child = command
        .spawn()
        .map_err(|e| format!("failed to execute add_cmd `{}`: {}", full_cmd_display, e))?;

    let (status, _) = stream_child_output(child, feedback_tx, log_file, false).await?;

    if !status.success() {
        return Err(format!(
            "add_cmd `{}` failed with status {}",
            full_cmd_display, status
        ));
    }

    debug!("add_cmd completed successfully");
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
