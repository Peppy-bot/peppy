//! Clears of a [`RootLifetime::PerBoot`](crate::consts::RootLifetime::PerBoot)
//! data root, the default root of a dev build (`~/.cache/peppy-dev`).
//!
//! The root sits on disk, so no tmpfs quota limits it, and peppy clears it
//! itself at each boot, much as the OS clears `/tmp`.
//!
//! With [`clear_if_new_boot`], the first peppy process of each boot of the
//! machine clears the root, before it reads or writes it. A record of the boot that
//! last used the root ([`PeppyDirs::root_boot_id_path`]) tells which boot that
//! was. A root without that record counts as the root of an earlier boot.
//!
//! A clear removes every entry of the root except:
//!
//! - [`PeppyDirs::conf_dir`]: the configuration the developer writes
//!   (repositories, credentials, `peppy_config.json5`, the enrollment). It is
//!   small, and a tool can write it without a peppy process, before the first
//!   peppy process of the boot clears the root.
//! - [`PeppyDirs::container_build_cache_dir`]: the caches container builds
//!   share (the cargo registry, sccache, the uv packages and interpreters,
//!   the pinned downloads). Every entry in it is keyed by its content or its
//!   version, so no boot makes one wrong, and without them the first stack
//!   launch of a boot compiles and downloads every node from nothing.
//! - the two lock files: a lock file that is unlinked and recreated lets two
//!   processes lock two different inodes behind the same path.
//! - the boot record.
//!
//! The clear runs under [`PeppyDirs::root_clear_lock_path`], so two processes
//! that start together after a reboot clear the root once. A clear continues
//! past an entry that it cannot remove, and logs a warning that names the
//! entry.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::internal::consts::PeppyDirs;

/// The identity of one boot of the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootId(String);

impl BootId {
    /// Parses the text of a boot identity source. Surrounding whitespace is
    /// not part of the identity, and a text with no identity gives `None`.
    pub fn parse(raw: &str) -> Option<Self> {
        let identity = raw.trim();
        if identity.is_empty() {
            return None;
        }
        Some(Self(identity.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Reads the identity of the current boot from the kernel.
#[cfg(target_os = "linux")]
pub fn current_boot_id() -> io::Result<BootId> {
    const BOOT_ID_SOURCE: &str = "/proc/sys/kernel/random/boot_id";
    let raw = std::fs::read_to_string(BOOT_ID_SOURCE)?;
    BootId::parse(&raw).ok_or_else(|| io::Error::other(format!("{BOOT_ID_SOURCE} is empty")))
}

/// Reads the identity of the current boot from the kernel.
#[cfg(target_os = "macos")]
pub fn current_boot_id() -> io::Result<BootId> {
    let output = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.bootsessionuuid"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "`sysctl -n kern.bootsessionuuid` failed: {}",
            output.status
        )));
    }
    BootId::parse(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| io::Error::other("`sysctl -n kern.bootsessionuuid` printed nothing"))
}

/// Reads the identity of the current boot from the kernel.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn current_boot_id() -> io::Result<BootId> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peppy reads no boot identity on this platform",
    ))
}

/// What a clear check did to the root.
#[derive(Debug, PartialEq, Eq)]
pub enum ClearOutcome {
    /// The root stays as it is.
    Kept,
    /// The clear removed the root's entries, except the entries it keeps
    /// and the entries in `not_removed`, which it could not remove.
    Cleared { not_removed: Vec<PathBuf> },
}

/// Clears the root when the boot that last used it is not the current boot,
/// then records the current boot. Call it before the process reads or writes
/// the root.
pub fn clear_if_new_boot(peppy_dirs: &PeppyDirs) -> io::Result<ClearOutcome> {
    clear_if_boot_changed(peppy_dirs, &current_boot_id()?)
}

/// [`clear_if_new_boot`] with the current boot made explicit, so tests can
/// give it a boot.
fn clear_if_boot_changed(
    peppy_dirs: &PeppyDirs,
    current_boot: &BootId,
) -> io::Result<ClearOutcome> {
    let _clear_lock = lock_root_clear(peppy_dirs)?;
    if recorded_boot_id(peppy_dirs)?.as_ref() == Some(current_boot) {
        return Ok(ClearOutcome::Kept);
    }
    let not_removed = clear_root(peppy_dirs);
    std::fs::write(peppy_dirs.root_boot_id_path(), current_boot.as_str())?;
    info!(
        "Cleared the dev data root {}: an earlier boot of the machine used it",
        peppy_dirs.root().display()
    );
    Ok(ClearOutcome::Cleared { not_removed })
}

/// Takes the lock that serializes the clears of the root, and waits while
/// another process holds it. The returned [`File`] is the lock.
fn lock_root_clear(peppy_dirs: &PeppyDirs) -> io::Result<File> {
    std::fs::create_dir_all(peppy_dirs.runtime_config_dir())?;
    let lock_file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(peppy_dirs.root_clear_lock_path())?;
    lock_file.lock()?;
    Ok(lock_file)
}

/// The boot recorded in the root, or `None` when the root has no record.
fn recorded_boot_id(peppy_dirs: &PeppyDirs) -> io::Result<Option<BootId>> {
    match std::fs::read_to_string(peppy_dirs.root_boot_id_path()) {
        Ok(raw) => Ok(BootId::parse(&raw)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The entries of the root that a clear keeps: the configuration, the
/// container build caches, the lock files and the boot record.
fn kept_paths(peppy_dirs: &PeppyDirs) -> [PathBuf; 5] {
    [
        peppy_dirs.conf_dir(),
        peppy_dirs.container_build_cache_dir(),
        peppy_dirs.daemon_lock_path(),
        peppy_dirs.root_clear_lock_path(),
        peppy_dirs.root_boot_id_path(),
    ]
}

/// Removes every entry of the root except the [`kept_paths`], and returns the
/// entries it could not remove.
fn clear_root(peppy_dirs: &PeppyDirs) -> Vec<PathBuf> {
    let mut not_removed = Vec::new();
    remove_dir_entries_except(peppy_dirs.root(), &kept_paths(peppy_dirs), &mut not_removed);
    not_removed
}

/// Removes every entry of `dir` except the paths in `kept` and the
/// directories that contain one of them, which it clears the same way.
fn remove_dir_entries_except(dir: &Path, kept: &[PathBuf], not_removed: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(e) => {
            warn!("Cannot read {} to clear it: {e}", dir.display());
            not_removed.push(dir.to_path_buf());
            return;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                warn!("Cannot read an entry of {} to clear it: {e}", dir.display());
                not_removed.push(dir.to_path_buf());
                continue;
            }
        };
        if kept.contains(&path) {
            continue;
        }
        if kept.iter().any(|kept_path| kept_path.starts_with(&path)) {
            remove_dir_entries_except(&path, kept, not_removed);
            continue;
        }
        if let Err(e) = remove_entry(&path) {
            warn!(
                "Cannot remove {}: {e}. Remove it by hand to free its space",
                path.display()
            );
            not_removed.push(path);
        }
    }
}

/// Removes a file, a symlink (not its target) or a directory tree.
fn remove_entry(path: &Path) -> io::Result<()> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(identity: &str) -> BootId {
        BootId::parse(identity).unwrap()
    }

    fn write_file(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    /// A root with the content of a used dev root: state, caches, the
    /// daemon lock, and a file beside the lock under `runtime/`.
    fn populated_root() -> (tempfile::TempDir, PeppyDirs) {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path().join("peppy-dev"));
        write_file(&peppy_dirs.conf_dir().join("repositories.json5"), 10);
        write_file(&peppy_dirs.tmp_dir().join("apptainer/rootfs/opt/file"), 100);
        write_file(
            &peppy_dirs.container_build_cache_dir().join("cargo-home/x"),
            1000,
        );
        write_file(&peppy_dirs.cache_dir().join("nodes.json5"), 10);
        write_file(&peppy_dirs.git_checkouts_dir().join("hub-0123/file"), 10);
        write_file(&peppy_dirs.built_nodes_dir().join("node_v1/0123.sif"), 10);
        write_file(&peppy_dirs.root().join("daemon_state.json5"), 10);
        write_file(&peppy_dirs.runtime_config_dir().join("node.json5"), 10);
        write_file(&peppy_dirs.daemon_lock_path(), 0);
        (tmp, peppy_dirs)
    }

    fn root_entries(peppy_dirs: &PeppyDirs) -> Vec<String> {
        let mut entries: Vec<String> = std::fs::read_dir(peppy_dirs.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        entries
    }

    fn runtime_entries(peppy_dirs: &PeppyDirs) -> Vec<String> {
        let mut entries: Vec<String> = std::fs::read_dir(peppy_dirs.runtime_config_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        entries
    }

    fn cache_entries(peppy_dirs: &PeppyDirs) -> Vec<String> {
        let mut entries: Vec<String> = std::fs::read_dir(peppy_dirs.cache_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        entries
    }

    fn assert_cleared_to_the_kept_files(peppy_dirs: &PeppyDirs) {
        assert_eq!(root_entries(peppy_dirs), ["cache", "conf", "runtime"]);
        assert!(peppy_dirs.conf_dir().join("repositories.json5").exists());
        assert_eq!(cache_entries(peppy_dirs), ["container_build"]);
        assert!(
            peppy_dirs
                .container_build_cache_dir()
                .join("cargo-home/x")
                .exists()
        );
        assert_eq!(
            runtime_entries(peppy_dirs),
            ["boot_id", "daemon.lock", "root_clear.lock"]
        );
    }

    #[test]
    fn boot_id_parse_trims_whitespace_and_rejects_an_empty_text() {
        assert_eq!(boot("  abc-123\n").as_str(), "abc-123");
        assert_eq!(BootId::parse(""), None);
        assert_eq!(BootId::parse(" \n"), None);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn current_boot_id_reads_the_same_identity_twice() {
        assert_eq!(current_boot_id().unwrap(), current_boot_id().unwrap());
    }

    #[test]
    fn a_root_without_a_boot_record_is_cleared_and_records_the_boot() {
        let (_tmp, peppy_dirs) = populated_root();

        let outcome = clear_if_boot_changed(&peppy_dirs, &boot("boot-1")).unwrap();

        assert_eq!(
            outcome,
            ClearOutcome::Cleared {
                not_removed: vec![]
            }
        );
        assert_cleared_to_the_kept_files(&peppy_dirs);
        assert_eq!(recorded_boot_id(&peppy_dirs).unwrap(), Some(boot("boot-1")));
    }

    #[test]
    fn a_root_of_the_current_boot_is_kept() {
        let (_tmp, peppy_dirs) = populated_root();
        clear_if_boot_changed(&peppy_dirs, &boot("boot-1")).unwrap();
        write_file(&peppy_dirs.root().join("daemon_state.json5"), 10);

        let outcome = clear_if_boot_changed(&peppy_dirs, &boot("boot-1")).unwrap();

        assert_eq!(outcome, ClearOutcome::Kept);
        assert!(peppy_dirs.root().join("daemon_state.json5").exists());
    }

    #[test]
    fn a_root_of_an_earlier_boot_is_cleared() {
        let (_tmp, peppy_dirs) = populated_root();
        clear_if_boot_changed(&peppy_dirs, &boot("boot-1")).unwrap();
        write_file(&peppy_dirs.root().join("daemon_state.json5"), 10);

        let outcome = clear_if_boot_changed(&peppy_dirs, &boot("boot-2")).unwrap();

        assert_eq!(
            outcome,
            ClearOutcome::Cleared {
                not_removed: vec![]
            }
        );
        assert_cleared_to_the_kept_files(&peppy_dirs);
        assert_eq!(recorded_boot_id(&peppy_dirs).unwrap(), Some(boot("boot-2")));
    }

    #[test]
    fn a_missing_root_is_created_with_the_boot_record() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path().join("peppy-dev"));

        let outcome = clear_if_boot_changed(&peppy_dirs, &boot("boot-1")).unwrap();

        assert_eq!(
            outcome,
            ClearOutcome::Cleared {
                not_removed: vec![]
            }
        );
        assert_eq!(recorded_boot_id(&peppy_dirs).unwrap(), Some(boot("boot-1")));
    }

    #[test]
    fn a_boot_clear_keeps_the_daemon_lock_inode() {
        let (_tmp, peppy_dirs) = populated_root();
        let held_lock = File::open(peppy_dirs.daemon_lock_path()).unwrap();
        held_lock.try_lock().unwrap();

        clear_if_boot_changed(&peppy_dirs, &boot("boot-1")).unwrap();

        // A second open of the same path still meets the held lock: the
        // clear did not unlink and recreate the lock file.
        let second_open = File::open(peppy_dirs.daemon_lock_path()).unwrap();
        assert!(second_open.try_lock().is_err());
    }

    #[test]
    fn a_clear_removes_a_symlink_and_not_its_target() {
        let (tmp, peppy_dirs) = populated_root();
        let outside = tmp.path().join("outside");
        write_file(&outside.join("keep_me"), 10);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, peppy_dirs.root().join("link")).unwrap();

        clear_if_boot_changed(&peppy_dirs, &boot("boot-1")).unwrap();

        assert_cleared_to_the_kept_files(&peppy_dirs);
        assert!(outside.join("keep_me").exists());
    }

    #[test]
    #[cfg(unix)]
    fn a_clear_continues_past_an_entry_it_cannot_remove() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, peppy_dirs) = populated_root();
        let locked_dir = peppy_dirs.tmp_dir().join("apptainer");
        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        if File::create(locked_dir.join("probe")).is_ok() {
            // This process ignores file modes (it runs as root), so it can
            // remove every entry and the case does not occur.
            return;
        }

        let outcome = clear_if_boot_changed(&peppy_dirs, &boot("boot-1")).unwrap();
        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            outcome,
            ClearOutcome::Cleared {
                not_removed: vec![peppy_dirs.tmp_dir()]
            }
        );
        assert!(!peppy_dirs.root().join("daemon_state.json5").exists());
        assert!(!peppy_dirs.built_nodes_dir().exists());
        assert!(peppy_dirs.container_build_cache_dir().exists());
    }
}
