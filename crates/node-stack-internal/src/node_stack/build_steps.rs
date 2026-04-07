//! Concrete build I/O steps invoked from [`super::entity::NodeEntity::build`].
//!
//! These helpers were originally part of `core-node-internal::services::node::add`
//! and were moved here so that the lifecycle transition `Added → Built` can run
//! without crossing the crate boundary back into core-node-internal.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};

use config::consts::PeppyDirs;
use tokio::sync::mpsc;
use zstd::stream::write::Encoder as ZstdEncoder;

use crate::build_io::{FeedbackLine, FeedbackStream, stream_child_output};

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
    let storage_dir = peppy_dirs.added_nodes_dir();
    std::fs::create_dir_all(&storage_dir)?;

    let archive_name = format!("{}_{}.tar.zst", node_name, node_tag);
    let archive_path = storage_dir.join(&archive_name);
    let tmp_path = storage_dir.join(format!("{}.tmp", archive_name));

    let file = File::create(&tmp_path)?;
    let encoder = ZstdEncoder::new(file, 1)?;
    let mut tar_builder = tar::Builder::new(encoder);
    // DO NOT follow symlinks, otherwise it could create unintended behavior for the user who modify files in the path pointed by the symlink
    tar_builder.follow_symlinks(false);
    tar_builder.append_dir_all(".", source_dir)?;
    let encoder = tar_builder.into_inner()?;
    encoder.finish()?;

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
    let tmp_path = storage_dir.join(format!("{}.tmp", sif_name));

    // Copy + rename (not rename alone) because the working dir may be on a
    // different filesystem than storage. Matches archive_dir_to_storage pattern.
    std::fs::copy(&sif_source, &tmp_path)?;
    std::fs::rename(&tmp_path, &dest_path)?;

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

    let mut cmd = cmd_builder
        .into_std_command()
        .map_err(|e| format!("Failed to build apptainer command: {}", e))?;

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
