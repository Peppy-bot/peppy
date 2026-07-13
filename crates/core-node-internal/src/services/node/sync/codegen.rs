use super::interfaces::{collect_all_deployment_interfaces, stack_resolver};
use daemon_config::consts::PeppyDirs;
use generator::DeploymentInterface;
use node_stack::NodeStack;
use tracing::debug;

/// Safely removes the `.peppy` directory by atomically renaming it first.
///
/// This avoids TOCTOU (Time Of Check, Time Of Use) race conditions that can occur
/// when using `exists()` followed by `remove_dir_all()`. If multiple processes try
/// to generate for the same node concurrently, the simple pattern can fail with
/// "Directory not empty" errors when one process adds files while another is deleting.
///
/// The atomic rename approach:
/// 1. Renames `.peppy` → `.peppy-old-{pid}-{timestamp}` (atomic operation)
/// 2. Synchronously deletes the renamed directory
/// 3. Lets the generator create a fresh `.peppy` directory
///
/// The deletion is intentionally synchronous: the next pipeline stage
/// (`process_node_add`) copies the source directory recursively and walks
/// `.peppy-old-{pid}-{timestamp}` (which is not in the excluded list), which
/// would race with a concurrent background deletion and surface as intermittent
/// "No such file or directory" errors. Callers already run inside
/// `tokio::task::spawn_blocking`, so the synchronous cost is acceptable.
pub(super) fn remove_previous_peppy_dir(node_root_dir: &std::path::Path) {
    let peppy_output_dir = node_root_dir.join(daemon_config::consts::PEPPY_OUTPUT_DIR);

    // Path-string sanity check; pure CPU, no syscall.
    if peppy_output_dir.file_name()
        != Some(std::ffi::OsStr::new(
            daemon_config::consts::PEPPY_OUTPUT_DIR,
        ))
    {
        debug!(
            "Unexpected directory name, expected {}: {}",
            daemon_config::consts::PEPPY_OUTPUT_DIR,
            peppy_output_dir.display()
        );
        return;
    }

    // One `metadata` call decides "missing", "is a file", or "is a dir".
    // We refuse to rename a non-directory because a stray file at this
    // path would otherwise be silently moved to `.peppy-old-{pid}-{ts}`
    // and stranded there.
    match std::fs::metadata(&peppy_output_dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            debug!(
                "Expected .peppy to be a directory, but it's a file: {}",
                peppy_output_dir.display()
            );
            return;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            debug!(
                "Cannot stat .peppy at {}: {}, proceeding with rename anyway",
                peppy_output_dir.display(),
                e
            );
        }
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let old_peppy_dir =
        node_root_dir.join(format!(".peppy-old-{}-{}", std::process::id(), timestamp));

    match std::fs::rename(&peppy_output_dir, &old_peppy_dir) {
        Ok(()) => {
            if let Err(e) = std::fs::remove_dir_all(&old_peppy_dir) {
                // Best-effort: the next stage may copy this stray directory and
                // fail, but that surfaces a real error rather than silently
                // leaving the dir behind.
                debug!(
                    "Failed to remove renamed .peppy directory at {}: {}",
                    old_peppy_dir.display(),
                    e
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Directory was already removed by another process, that's fine
        }
        Err(e) => {
            // Log warning but proceed - generator will create directories as needed.
            // This handles edge cases like permission issues or concurrent renames.
            debug!(
                "Failed to move old .peppy directory: {}, proceeding with generation",
                e
            );
        }
    }
}

/// Returns `true` when the `.peppy` directory under `node_root_dir` is absent
/// or incomplete and must be (re-)generated.
///
/// A complete `.peppy` directory contains:
/// - `git.hash` (non-empty)
/// - `libs/peppygen/peppy.json5.sha256` (non-empty)
fn needs_sync(node_root_dir: &std::path::Path) -> bool {
    let peppy_dir = node_root_dir.join(daemon_config::consts::PEPPY_OUTPUT_DIR);
    if !peppy_dir.exists() {
        return true;
    }

    // git.hash must be a regular non-empty file
    match std::fs::metadata(peppy_dir.join("git.hash")) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => {}
        _ => return true,
    }

    // peppygen fingerprint must be a regular non-empty file
    match std::fs::metadata(peppy_dir.join("libs/peppygen/peppy.json5.sha256")) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => {}
        _ => return true,
    }

    false
}

/// Parameters for [`auto_sync_if_missing`].
pub struct AutoSyncParams<'a> {
    pub node_dir: &'a std::path::Path,
    pub execution_language: config::node::PeppygenLanguage,
    pub manifest: &'a config::node::Manifest,
    pub interfaces: &'a config::node::Interfaces,
    pub git_hash: &'a str,
    /// Receives progress lines emitted by `ensure_checkout` when a
    /// git-sourced contract document needs to be materialized.
    pub on_feedback: &'a dyn Fn(&str),
}

/// Auto-generates the `.peppy` directory for a node that has never been synced.
///
/// When the `.peppy` directory is entirely absent (e.g. fresh clone), this
/// function generates peppygen.
///
/// Directories whose `.peppy` already exists and contains all required files
/// are skipped (no-op). If `.peppy` exists but is incomplete (e.g. missing
/// `git.hash` or the peppygen fingerprint), it is removed and regenerated.
pub fn auto_sync_if_missing(
    params: AutoSyncParams<'_>,
    node_stack: &NodeStack,
    peppy_dirs: &PeppyDirs,
) -> crate::Result<()> {
    let peppy_dir = params
        .node_dir
        .join(daemon_config::consts::PEPPY_OUTPUT_DIR);
    if needs_sync(params.node_dir) {
        // Back up existing .peppy so we can restore it on failure.
        let backup_dir = params.node_dir.join(format!(
            ".peppy-backup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let had_backup = match std::fs::rename(&peppy_dir, &backup_dir) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(crate::Error::Io(e)),
        };

        let gen_result: crate::Result<()> = (|| {
            let consumed = collect_all_deployment_interfaces(
                params.manifest,
                params.interfaces,
                stack_resolver(node_stack),
                peppy_dirs,
                params.on_feedback,
            )
            .map_err(|reason| crate::Error::Io(std::io::Error::other(reason)))?;
            generate_peppygen_for_node(
                params.execution_language,
                params.node_dir,
                consumed,
                params.git_hash,
                peppy_dirs,
                generator::CrateDeployMode::default(),
                None,
            )?;
            Ok(())
        })();

        match gen_result {
            Ok(()) => {
                // Clean up the backup synchronously. We *must not* defer this
                // to a background thread: the next stage (`process_node_add`)
                // copies the source directory recursively, walks
                // `.peppy-backup-PID-NANOS` (which is not in the excluded
                // list), and would race with a concurrent deletion, surfacing
                // as intermittent "No such file or directory" errors.
                //
                // A failure here must be surfaced rather than silently
                // ignored: leaving the backup behind would still trip the
                // recursive copy described above.
                if had_backup {
                    std::fs::remove_dir_all(&backup_dir).map_err(|e| {
                        crate::Error::Io(std::io::Error::other(format!(
                            "failed to clean up .peppy backup at {}: {}",
                            backup_dir.display(),
                            e
                        )))
                    })?;
                }
            }
            Err(e) => {
                // Generation failed; remove partial .peppy and restore backup.
                let _ = std::fs::remove_dir_all(&peppy_dir);
                if had_backup && let Err(restore_err) = std::fs::rename(&backup_dir, &peppy_dir) {
                    tracing::error!(
                        "Failed to restore .peppy backup from {}: {}",
                        backup_dir.display(),
                        restore_err,
                    );
                }
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Generates the peppygen library for a node.
///
/// This function takes the pre-collected data and generates the peppygen
/// library in the node directory. Use `collect_consumed_interfaces` to
/// gather the consumed interfaces before calling this function.
///
/// This function is designed to be called from within `spawn_blocking` contexts
/// where the data has already been extracted and can be moved into the closure.
pub fn generate_peppygen_for_node(
    language: config::node::PeppygenLanguage,
    node_dir: impl AsRef<std::path::Path>,
    consumed_interfaces: Vec<DeploymentInterface>,
    git_hash: &str,
    peppy_dirs: &PeppyDirs,
    deploy_mode: generator::CrateDeployMode,
    config_path: Option<&std::path::Path>,
) -> crate::Result<()> {
    generator::generate_peppygen_lib(
        language,
        node_dir,
        consumed_interfaces,
        git_hash,
        peppy_dirs,
        deploy_mode,
        config_path,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_sync_returns_true_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(needs_sync(tmp.path()));
    }

    #[test]
    fn needs_sync_returns_true_when_git_hash_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".peppy")).unwrap();
        assert!(needs_sync(tmp.path()));
    }

    #[test]
    fn needs_sync_returns_true_when_git_hash_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        std::fs::create_dir_all(&peppy).unwrap();
        std::fs::write(peppy.join("git.hash"), b"").unwrap();
        assert!(needs_sync(tmp.path()));
    }

    #[test]
    fn needs_sync_returns_true_when_fingerprint_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        std::fs::create_dir_all(&peppy).unwrap();
        std::fs::write(peppy.join("git.hash"), b"abc123").unwrap();
        assert!(needs_sync(tmp.path()));
    }

    #[test]
    fn needs_sync_returns_false_when_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        let peppygen = peppy.join("libs/peppygen");
        std::fs::create_dir_all(&peppygen).unwrap();
        std::fs::write(peppy.join("git.hash"), b"abc123").unwrap();
        std::fs::write(peppygen.join("peppy.json5.sha256"), b"deadbeef").unwrap();
        assert!(!needs_sync(tmp.path()));
    }
}
