//! The scratch directory of one container command of the daemon: a `run`
//! of a container node, or an `exec` in the image of a build.
//!
//! Apptainer writes the temporary files of the command there, and unpacks
//! there an image that it does not mount. It removes them when the command
//! ends, and a command that is killed leaves them: an unpacked image can
//! take as much disk space as the node has files. With a directory for each
//! command, the daemon removes what a killed command left as soon as the
//! command is over, and removes at its boot what an earlier daemon left (see
//! [`remove_all`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use daemon_config::consts::PeppyDirs;

/// Process-wide counter that gives each scratch directory of this daemon a
/// name of its own.
static SCRATCH_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The scratch directory of one container command, which is removed when
/// this guard is dropped unless [`ContainerScratch::keep`] took it.
#[derive(Debug)]
pub(crate) struct ContainerScratch {
    path: PathBuf,
    kept: bool,
}

impl ContainerScratch {
    /// Makes a scratch directory that no other command of this daemon has.
    pub(crate) fn create(peppy_dirs: &PeppyDirs) -> std::io::Result<Self> {
        let counter = SCRATCH_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = peppy_dirs
            .container_scratch_dir()
            .join(format!("{}_{counter}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path, kept: false })
    }

    /// The directory, for [`containers::ApptainerCommand::scratch_dir`].
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Hands the directory over to a caller whose command outlives this
    /// guard. That caller removes it with [`remove`] when the command is
    /// over.
    pub(crate) fn keep(mut self) -> PathBuf {
        self.kept = true;
        std::mem::take(&mut self.path)
    }
}

impl Drop for ContainerScratch {
    fn drop(&mut self) {
        if !self.kept {
            remove(&self.path);
        }
    }
}

/// Removes the scratch directory `dir` of a command that is over, with all
/// that the command left in it. Best-effort: a directory that stays is
/// logged, and does not stop the caller.
pub(crate) fn remove(dir: &Path) {
    if let Err(error) = containers::remove_scratch_dir(dir) {
        tracing::warn!(
            "Failed to remove the container scratch directory {}: {error}",
            dir.display()
        );
    }
}

/// Removes the scratch directory of each container command of an earlier
/// daemon of this data root. A daemon calls it at its boot, before it starts
/// a container: no command of that earlier daemon is still its own.
pub fn remove_all(peppy_dirs: &PeppyDirs) {
    remove(&peppy_dirs.container_scratch_dir());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peppy_dirs() -> (tempfile::TempDir, PeppyDirs) {
        let root = tempfile::tempdir().expect("create the data root");
        let peppy_dirs = PeppyDirs::new(root.path());
        (root, peppy_dirs)
    }

    #[test]
    fn each_command_has_a_directory_of_its_own() {
        let (_root, peppy_dirs) = peppy_dirs();

        let first = ContainerScratch::create(&peppy_dirs).expect("create the first");
        let second = ContainerScratch::create(&peppy_dirs).expect("create the second");

        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());
        assert!(second.path().is_dir());
        assert!(first.path().starts_with(peppy_dirs.container_scratch_dir()));
    }

    #[test]
    fn a_dropped_guard_removes_its_directory_with_its_content() {
        let (_root, peppy_dirs) = peppy_dirs();
        let scratch = ContainerScratch::create(&peppy_dirs).expect("create the directory");
        let path = scratch.path().to_path_buf();
        std::fs::create_dir_all(path.join("rootfs-1/root")).expect("create an unpacked image");

        drop(scratch);

        assert!(!path.exists());
    }

    #[test]
    fn a_kept_directory_stays_until_it_is_removed() {
        let (_root, peppy_dirs) = peppy_dirs();
        let scratch = ContainerScratch::create(&peppy_dirs).expect("create the directory");

        let path = scratch.keep();
        assert!(path.is_dir());

        remove(&path);
        assert!(!path.exists());
    }

    /// What a killed command of an earlier daemon left goes at the boot of
    /// the next one, with the directories of each other command.
    #[test]
    fn remove_all_takes_the_directory_of_each_command() {
        let (_root, peppy_dirs) = peppy_dirs();
        let left = ContainerScratch::create(&peppy_dirs)
            .expect("create the directory")
            .keep();
        std::fs::create_dir_all(left.join("rootfs-1/root")).expect("create an unpacked image");

        remove_all(&peppy_dirs);

        assert!(!peppy_dirs.container_scratch_dir().exists());
        // A data root that has no scratch directory is no error.
        remove_all(&peppy_dirs);
    }
}
