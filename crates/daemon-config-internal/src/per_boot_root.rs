//! Clears of a [`RootLifetime::PerBoot`] data root, the default root of a dev
//! build (`~/.cache/peppy-dev`).
//!
//! The root sits on disk, so no tmpfs quota limits it, and peppy clears it
//! itself, much as the OS clears `/tmp`:
//!
//! - [`clear_if_new_boot`]: the first peppy process of each boot of the
//!   machine clears the root, before it reads or writes it. A record of the
//!   boot that last used the root ([`PeppyDirs::root_boot_id_path`]) tells
//!   which boot that was. A root without that record counts as the root of an
//!   earlier boot.
//! - [`clear_if_over_size_limit`]: the daemon clears the root when it starts
//!   with the root larger than the limit of
//!   [`PEPPY_DEV_ROOT_MAX_SIZE_ENV`](crate::consts::PEPPY_DEV_ROOT_MAX_SIZE_ENV).
//!   The daemon calls it while it holds its singleton lock, so no other daemon
//!   uses the root. A peppy command that runs at the same time can lose the
//!   files it writes.
//!
//! A clear removes every entry of the root except:
//!
//! - [`PeppyDirs::conf_dir`]: the configuration the developer writes
//!   (repositories, credentials, `peppy_config.json5`, the enrollment). It is
//!   small, and a tool can write it without a peppy process, before the first
//!   peppy process of the boot clears the root.
//! - the two lock files: a lock file that is unlinked and recreated lets two
//!   processes lock two different inodes behind the same path.
//! - the boot record.
//!
//! The clears run under [`PeppyDirs::root_clear_lock_path`], so two processes
//! that start together after a reboot clear the root once. A clear continues past an entry that it
//! cannot remove, and logs a warning that names the entry.

use std::fs::File;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use tracing::{info, warn};

use crate::internal::consts::{PeppyDirs, RootLifetime};

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

/// A size limit of a data root: a number of bytes greater than zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootSizeLimit(NonZeroU64);

impl RootSizeLimit {
    pub fn bytes(self) -> u64 {
        self.0.get()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidRootSizeLimit {
    #[error(
        "`{0}` is not a size: give a whole number with an optional unit B, K, M, G or T \
         (powers of 1024), for example `20G`"
    )]
    NotASize(String),
    #[error("`{0}` is zero: give a size greater than zero")]
    Zero(String),
    #[error("`{0}` is more than {max} bytes", max = u64::MAX)]
    TooLarge(String),
}

impl FromStr for RootSizeLimit {
    type Err = InvalidRootSizeLimit;

    /// Parses a whole number with an optional unit, case-insensitive: `B`
    /// (the default), `K`, `M`, `G` or `T`, each a power of 1024.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let text = raw.trim();
        let unit_start = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (digits, unit) = text.split_at(unit_start);
        if digits.is_empty() {
            return Err(InvalidRootSizeLimit::NotASize(raw.to_owned()));
        }
        let unit_bytes =
            unit_bytes(unit).ok_or_else(|| InvalidRootSizeLimit::NotASize(raw.to_owned()))?;
        // `digits` holds ASCII digits only, so a parse failure is an overflow.
        let bytes = digits
            .parse::<u64>()
            .ok()
            .and_then(|count| count.checked_mul(unit_bytes))
            .ok_or_else(|| InvalidRootSizeLimit::TooLarge(raw.to_owned()))?;
        NonZeroU64::new(bytes)
            .map(Self)
            .ok_or_else(|| InvalidRootSizeLimit::Zero(raw.to_owned()))
    }
}

fn unit_bytes(unit: &str) -> Option<u64> {
    match unit.to_ascii_uppercase().as_str() {
        "" | "B" => Some(1),
        "K" => Some(1 << 10),
        "M" => Some(1 << 20),
        "G" => Some(1 << 30),
        "T" => Some(1 << 40),
        _ => None,
    }
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

/// Clears the root when the size of the files a clear removes is more than
/// `limit`. The size is the sum of the lengths of the files, so a file with
/// two hard links counts twice. The files a clear keeps do not count, so a
/// limit smaller than them does not clear the root at each start.
pub fn clear_if_over_size_limit(
    peppy_dirs: &PeppyDirs,
    limit: RootSizeLimit,
) -> io::Result<ClearOutcome> {
    let _clear_lock = lock_root_clear(peppy_dirs)?;
    let size = tree_size_bytes(peppy_dirs.root(), &kept_paths(peppy_dirs));
    if size <= limit.bytes() {
        return Ok(ClearOutcome::Kept);
    }
    let not_removed = clear_root(peppy_dirs);
    info!(
        "Cleared the dev data root {}: it held {size} bytes, more than the limit of {} bytes",
        peppy_dirs.root().display(),
        limit.bytes()
    );
    Ok(ClearOutcome::Cleared { not_removed })
}

/// The size limit of a data root from the raw value of
/// [`PEPPY_DEV_ROOT_MAX_SIZE_ENV`](crate::consts::PEPPY_DEV_ROOT_MAX_SIZE_ENV).
/// An unset or empty value gives no limit. Only a per-boot root takes a limit:
/// for a persistent root, a value gives no limit and a warning.
pub fn root_size_limit(
    lifetime: RootLifetime,
    raw: Option<std::ffi::OsString>,
) -> Result<Option<RootSizeLimit>, InvalidRootSizeLimit> {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let raw = raw
        .into_string()
        .map_err(|value| InvalidRootSizeLimit::NotASize(value.to_string_lossy().into_owned()))?;
    if lifetime == RootLifetime::Persistent {
        warn!(
            "{} applies only to the default data root of a dev build; \
             this data root keeps all of its content",
            crate::consts::PEPPY_DEV_ROOT_MAX_SIZE_ENV
        );
        return Ok(None);
    }
    raw.parse().map(Some)
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

/// The entries of the root that a clear keeps: the configuration, the lock
/// files and the boot record.
fn kept_paths(peppy_dirs: &PeppyDirs) -> [PathBuf; 4] {
    [
        peppy_dirs.conf_dir(),
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

/// The sum of the lengths of the files under `root`, except the paths in
/// `excluded` and the files under them. Symlinks count with their own length,
/// not the length of their target, and an entry that cannot be read counts as
/// zero.
fn tree_size_bytes(root: &Path, excluded: &[PathBuf]) -> u64 {
    let mut total = 0u64;
    let mut pending_dirs = vec![root.to_path_buf()];
    while let Some(dir) = pending_dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if excluded.contains(&entry.path()) {
                continue;
            }
            // `DirEntry::metadata` does not follow symlinks.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                pending_dirs.push(entry.path());
                continue;
            }
            total = total.saturating_add(metadata.len());
        }
    }
    total
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

    fn assert_cleared_to_the_kept_files(peppy_dirs: &PeppyDirs) {
        assert_eq!(root_entries(peppy_dirs), ["conf", "runtime"]);
        assert!(peppy_dirs.conf_dir().join("repositories.json5").exists());
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
    fn root_size_limit_parses_units_as_powers_of_1024() {
        let parse = |raw: &str| raw.parse::<RootSizeLimit>().unwrap().bytes();
        assert_eq!(parse("512"), 512);
        assert_eq!(parse("512B"), 512);
        assert_eq!(parse("4k"), 4 << 10);
        assert_eq!(parse("3M"), 3 << 20);
        assert_eq!(parse("20G"), 20 << 30);
        assert_eq!(parse(" 2t "), 2 << 40);
    }

    #[test]
    fn root_size_limit_rejects_what_is_not_a_whole_size() {
        for raw in ["", "G", "1.5G", "-1G", "20GB", "20 G", "twenty"] {
            assert_eq!(
                raw.parse::<RootSizeLimit>(),
                Err(InvalidRootSizeLimit::NotASize(raw.to_owned())),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn root_size_limit_rejects_zero_and_overflow() {
        assert_eq!(
            "0G".parse::<RootSizeLimit>(),
            Err(InvalidRootSizeLimit::Zero("0G".to_owned()))
        );
        assert_eq!(
            "16777216T".parse::<RootSizeLimit>(),
            Err(InvalidRootSizeLimit::TooLarge("16777216T".to_owned()))
        );
        assert_eq!(
            "99999999999999999999".parse::<RootSizeLimit>(),
            Err(InvalidRootSizeLimit::TooLarge(
                "99999999999999999999".to_owned()
            ))
        );
    }

    #[test]
    fn root_size_limit_from_env_is_none_when_unset_or_empty() {
        assert_eq!(root_size_limit(RootLifetime::PerBoot, None), Ok(None));
        assert_eq!(
            root_size_limit(RootLifetime::PerBoot, Some("".into())),
            Ok(None)
        );
    }

    #[test]
    fn root_size_limit_from_env_applies_to_a_per_boot_root_only() {
        assert_eq!(
            root_size_limit(RootLifetime::PerBoot, Some("1K".into())),
            Ok(Some("1K".parse().unwrap()))
        );
        assert_eq!(
            root_size_limit(RootLifetime::Persistent, Some("1K".into())),
            Ok(None)
        );
    }

    #[test]
    fn root_size_limit_from_env_reports_an_invalid_value() {
        assert_eq!(
            root_size_limit(RootLifetime::PerBoot, Some("lots".into())),
            Err(InvalidRootSizeLimit::NotASize("lots".to_owned()))
        );
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
        assert!(!peppy_dirs.container_build_cache_dir().exists());
    }

    #[test]
    fn a_root_over_the_size_limit_is_cleared_and_keeps_its_boot_record() {
        let (_tmp, peppy_dirs) = populated_root();
        clear_if_boot_changed(&peppy_dirs, &boot("boot-1")).unwrap();
        write_file(&peppy_dirs.container_build_cache_dir().join("big"), 2048);

        let outcome = clear_if_over_size_limit(&peppy_dirs, "1K".parse().unwrap()).unwrap();

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
    fn a_root_at_or_under_the_size_limit_is_kept() {
        let (_tmp, peppy_dirs) = populated_root();
        let size = tree_size_bytes(peppy_dirs.root(), &kept_paths(&peppy_dirs));

        let outcome =
            clear_if_over_size_limit(&peppy_dirs, size.to_string().parse().unwrap()).unwrap();

        assert_eq!(outcome, ClearOutcome::Kept);
        assert!(peppy_dirs.root().join("daemon_state.json5").exists());
    }

    #[test]
    fn the_size_limit_does_not_count_the_kept_configuration() {
        let (_tmp, peppy_dirs) = populated_root();
        clear_if_boot_changed(&peppy_dirs, &boot("boot-1")).unwrap();
        write_file(&peppy_dirs.conf_dir().join("peppy_config.json5"), 4096);
        write_file(&peppy_dirs.root().join("daemon_state.json5"), 10);

        let outcome = clear_if_over_size_limit(&peppy_dirs, "1K".parse().unwrap()).unwrap();

        assert_eq!(outcome, ClearOutcome::Kept);
        assert!(peppy_dirs.root().join("daemon_state.json5").exists());
    }

    #[test]
    fn tree_size_skips_the_excluded_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        write_file(&root.join("counted"), 7);
        write_file(&root.join("excluded_dir/file"), 100);
        write_file(&root.join("excluded_file"), 100);

        let size = tree_size_bytes(
            &root,
            &[root.join("excluded_dir"), root.join("excluded_file")],
        );

        assert_eq!(size, 7);
    }

    #[test]
    fn tree_size_sums_the_file_lengths_without_following_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        write_file(&root.join("a"), 100);
        write_file(&root.join("nested/deeper/b"), 23);
        let outside = tmp.path().join("outside");
        write_file(&outside.join("big"), 10_000);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let size = tree_size_bytes(&root, &[]);

        let link_length = std::fs::symlink_metadata(root.join("link"))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        assert_eq!(size, 123 + link_length);
    }
}
