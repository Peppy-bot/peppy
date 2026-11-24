//! Release archive extraction, the lock of the installation directory, the
//! recovery of an interrupted update, and the swap of the installed binaries
//! with rollback.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use node_stack::ArchiveOrigin;

use crate::error::{Error, Result};

/// Directories from `bin/` in a release archive that `peppy update` replaces.
pub(crate) const MANAGED_DIRS: &[&str] =
    &["apptainer", "apptainer-x86_64", "apptainer-aarch64", "lima"];

/// Executable files from `bin/` in a release archive that `peppy update` replaces.
pub(crate) const MANAGED_FILES: &[&str] = &["peppy", "zenohd"];

/// Name prefix of the directory in the installation directory that holds the
/// extracted release archive until the swap.
pub(crate) const STAGING_DIR_PREFIX: &str = ".peppy-update-stage-";

/// Name prefix of the directory in the installation directory that holds the
/// replaced entries until the swap ends.
const BACKUP_DIR_PREFIX: &str = ".peppy-update-backup-";

/// Name prefix of the file that [`check_dir_writable`] creates and removes.
const WRITE_PROBE_PREFIX: &str = ".peppy-update-probe-";

/// Extracts the `.tgz` release archive that `compressed` streams into
/// `staging_dir`, under the rules of [`ArchiveOrigin::PeppyRelease`].
///
/// After the tar stream ends, the gzip decoder is drained to EOF so the gzip
/// CRC32 checksum and uncompressed size trailer are always verified.
pub(crate) fn extract_release_archive(
    compressed: impl Read,
    archive_name: &str,
    staging_dir: &Path,
) -> Result<()> {
    let mut decoder = GzDecoder::new(compressed);
    node_stack::extract_tar(
        &mut decoder,
        archive_name,
        staging_dir,
        ArchiveOrigin::PeppyRelease,
    )
    .map_err(|e| Error::ExecutionFailed(format!("Failed to extract the release archive: {e}")))?;

    // Drain any trailing zero-blocks and the gzip trailer so `GzDecoder`
    // verifies the CRC32 checksum and ISIZE even when `tar` stopped at the
    // two-block EOF marker.
    io::copy(&mut decoder, &mut io::sink()).map_err(|e| {
        Error::ExecutionFailed(format!(
            "Release archive failed integrity verification: {e}"
        ))
    })?;

    let staged_peppy = staging_dir.join("bin").join("peppy");
    if !staged_peppy.is_file() {
        return Err(Error::ExecutionFailed(
            "Downloaded release archive does not contain `bin/peppy`.".to_owned(),
        ));
    }
    ensure_executable(&staged_peppy)?;

    let staged_zenohd = staging_dir.join("bin").join("zenohd");
    if staged_zenohd.is_file() {
        ensure_executable(&staged_zenohd)?;
    }

    Ok(())
}

fn ensure_executable(path: &Path) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    let mode = perms.mode();
    if mode & 0o111 == 0 {
        perms.set_mode(mode | 0o755);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Verifies that `dir` is writable by creating and dropping a probe file in it.
/// Called before downloading the release archive so permission errors fail fast.
pub(crate) fn check_dir_writable(dir: &Path) -> Result<()> {
    tempfile::Builder::new()
        .prefix(WRITE_PROBE_PREFIX)
        .tempfile_in(dir)
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Cannot write to `{}`: {e}. Check directory permissions or re-run with sudo.",
                dir.display()
            ))
        })?;
    Ok(())
}

/// The exclusive lock of an installation directory, which one `peppy update`
/// holds at a time. The lock ends when the value drops (or the process
/// ends).
pub(crate) struct InstallDirLock {
    _directory: fs::File,
}

/// Takes the lock of `install_bin_dir`, or fails when another `peppy update`
/// holds it.
pub(crate) fn lock_install_dir(install_bin_dir: &Path) -> Result<InstallDirLock> {
    let directory = fs::File::open(install_bin_dir).map_err(|e| {
        Error::ExecutionFailed(format!(
            "Could not open the installation directory `{}`: {e}",
            install_bin_dir.display()
        ))
    })?;
    match directory.try_lock() {
        Ok(()) => Ok(InstallDirLock {
            _directory: directory,
        }),
        Err(fs::TryLockError::WouldBlock) => Err(Error::ExecutionFailed(format!(
            "Another `peppy update` is updating `{}`. Wait for it to end, then try again.",
            install_bin_dir.display()
        ))),
        Err(fs::TryLockError::Error(e)) => Err(Error::ExecutionFailed(format!(
            "Could not lock the installation directory `{}`: {e}",
            install_bin_dir.display()
        ))),
    }
}

/// Removes what an interrupted `peppy update` (Ctrl-C, a crash, a power
/// loss) left in `install_bin_dir`: its staging directories and probe files,
/// and its backup directories after each entry that the swap moved out and
/// did not replace is back at its path. The lock proves that no other update
/// uses these entries.
///
/// Returns a description of each leftover that could not be removed or put
/// back; the update continues without it.
pub(crate) fn recover_interrupted_updates(
    install_bin_dir: &Path,
    _lock: &InstallDirLock,
) -> Vec<String> {
    let entries = match fs::read_dir(install_bin_dir) {
        Ok(entries) => entries,
        Err(e) => {
            return vec![format!(
                "Could not read `{}` to find what an interrupted update left: {e}",
                install_bin_dir.display()
            )];
        }
    };
    let mut problems = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_backup = name.starts_with(BACKUP_DIR_PREFIX);
        let is_leftover = is_backup
            || name.starts_with(STAGING_DIR_PREFIX)
            || name.starts_with(WRITE_PROBE_PREFIX);
        if !is_leftover {
            continue;
        }
        let path = entry.path();
        if is_backup {
            problems.extend(restore_entries_missing_from_install(&path, install_bin_dir));
        }
        if let Err(e) = remove_entry(&path) {
            problems.push(format!(
                "Could not remove `{}`, which an interrupted update left: {e}",
                path.display()
            ));
        }
    }
    problems
}

/// Moves each entry of `backup_dir` whose path in `install_bin_dir` names
/// nothing back to that path.
fn restore_entries_missing_from_install(backup_dir: &Path, install_bin_dir: &Path) -> Vec<String> {
    let entries = match fs::read_dir(backup_dir) {
        Ok(entries) => entries,
        Err(e) => return vec![format!("Could not read `{}`: {e}", backup_dir.display())],
    };
    let mut problems = Vec::new();
    for entry in entries.flatten() {
        let target = install_bin_dir.join(entry.file_name());
        if path_names_an_entry(&target) {
            continue;
        }
        if let Err(e) = fs::rename(entry.path(), &target) {
            problems.push(format!(
                "Could not put `{}` back from `{}`: {e}",
                target.display(),
                backup_dir.display()
            ));
        }
    }
    problems
}

/// Whether `path` names an entry, a symbolic link to nothing included.
fn path_names_an_entry(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Removes the file, symbolic link, or directory tree at `path`.
fn remove_entry(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Where the old content of an entry is during the swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OldEntry {
    /// The entry did not exist before the swap.
    Absent,
    /// A hard link in the backup directory keeps the old file. The old file
    /// stays at its path until the rename of the new file replaces it.
    LinkedInBackup,
    /// The old entry moved into the backup directory, so its path names
    /// nothing until the new entry moves in.
    MovedToBackup,
}

/// An entry that the swap changed, with where its old content is.
struct ChangedEntry {
    name: &'static str,
    old: OldEntry,
}

/// Replaces the managed binaries and tool directories of `install_bin_dir`
/// with those of `staging_bin_dir`, one entry after the other.
///
/// Both directories must sit on the same filesystem (the caller stages inside
/// `install_bin_dir`). A file is replaced with one `rename` over the old file,
/// so the path of `peppy` always names a complete binary; a hard link in a
/// backup directory keeps the old file. A directory cannot replace a
/// directory that has content with one `rename`, so the old directory moves
/// into the backup directory first, and for a moment its path names nothing.
///
/// When a step fails, each changed entry gets its old content back and each
/// new entry is removed. The error tells each entry that this rollback could
/// not restore.
pub(crate) fn swap_installation(staging_bin_dir: &Path, install_bin_dir: &Path) -> Result<()> {
    let backup_dir = tempfile::Builder::new()
        .prefix(BACKUP_DIR_PREFIX)
        .tempdir_in(install_bin_dir)
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to create backup directory in `{}`: {e}",
                install_bin_dir.display()
            ))
        })?;

    let staged_dirs = MANAGED_DIRS
        .iter()
        .filter(|name| staging_bin_dir.join(name).is_dir());
    let staged_files = MANAGED_FILES
        .iter()
        .filter(|name| staging_bin_dir.join(name).is_file());

    let mut changed: Vec<ChangedEntry> = Vec::new();
    for name in staged_dirs.chain(staged_files) {
        let paths = SwapPaths {
            staged: staging_bin_dir.join(name),
            target: install_bin_dir.join(name),
            backup: backup_dir.path().join(name),
        };
        match replace_entry(&paths) {
            Ok(old) => changed.push(ChangedEntry { name, old }),
            Err(failure) => {
                if let Some(old) = failure.changed {
                    changed.push(ChangedEntry { name, old });
                }
                let rollback_problems = rollback(&changed, install_bin_dir, backup_dir.path());
                return Err(swap_error(&failure.message, &rollback_problems));
            }
        }
    }

    // `backup_dir` drops here, removing the old replaced binaries and dirs.
    Ok(())
}

/// The paths of one entry of the swap.
struct SwapPaths {
    staged: PathBuf,
    target: PathBuf,
    backup: PathBuf,
}

/// A step of the swap of one entry that failed.
struct ReplaceFailure {
    message: String,
    /// Where the old content is when the failure left the entry changed.
    changed: Option<OldEntry>,
}

/// Moves the staged entry to its target path, and keeps the old content of
/// the target in the backup directory.
fn replace_entry(paths: &SwapPaths) -> std::result::Result<OldEntry, ReplaceFailure> {
    let unchanged = |message: String| ReplaceFailure {
        message,
        changed: None,
    };
    let target_metadata = match fs::symlink_metadata(&paths.target) {
        Ok(metadata) => Some(metadata),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(unchanged(format!(
                "Failed to inspect `{}`: {e}",
                paths.target.display()
            )));
        }
    };
    let old = match target_metadata {
        None => Ok(OldEntry::Absent),
        Some(_) if paths.staged.is_dir() => move_to_backup(paths),
        Some(metadata) if metadata.is_dir() => {
            return Err(unchanged(format!(
                "`{}` is a directory, but the release holds a file at its place",
                paths.target.display()
            )));
        }
        Some(_) => link_or_move_to_backup(paths),
    }
    .map_err(|e| {
        unchanged(format!(
            "Failed to back up `{}`: {e}",
            paths.target.display()
        ))
    })?;

    if let Err(e) = fs::rename(&paths.staged, &paths.target) {
        // Only a move to the backup changed the target before this rename.
        return Err(ReplaceFailure {
            message: format!("Failed to install `{}`: {e}", paths.target.display()),
            changed: (old == OldEntry::MovedToBackup).then_some(old),
        });
    }
    Ok(old)
}

fn move_to_backup(paths: &SwapPaths) -> io::Result<OldEntry> {
    fs::rename(&paths.target, &paths.backup).map(|()| OldEntry::MovedToBackup)
}

/// Keeps the old file with a hard link, so that the rename of the new file
/// replaces it in one step. A file that cannot get a hard link (a filesystem
/// without them, or a file of another owner under `fs.protected_hardlinks`)
/// moves to the backup instead.
fn link_or_move_to_backup(paths: &SwapPaths) -> io::Result<OldEntry> {
    match fs::hard_link(&paths.target, &paths.backup) {
        Ok(()) => Ok(OldEntry::LinkedInBackup),
        Err(_) => move_to_backup(paths),
    }
}

/// Gives each entry of `changed` its old content back, the last one first.
/// Returns a description of each entry that could not be restored.
fn rollback(changed: &[ChangedEntry], install_bin_dir: &Path, backup_dir: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    for entry in changed.iter().rev() {
        let target = install_bin_dir.join(entry.name);
        let backup = backup_dir.join(entry.name);
        let restored = match entry.old {
            OldEntry::Absent => remove_entry(&target),
            OldEntry::LinkedInBackup => fs::rename(&backup, &target),
            OldEntry::MovedToBackup => {
                remove_entry_if_present(&target).and_then(|()| fs::rename(&backup, &target))
            }
        };
        if let Err(e) = restored {
            problems.push(format!("`{}`: {e}", target.display()));
        }
    }
    problems
}

fn remove_entry_if_present(path: &Path) -> io::Result<()> {
    if path_names_an_entry(path) {
        remove_entry(path)
    } else {
        Ok(())
    }
}

fn swap_error(message: &str, rollback_problems: &[String]) -> Error {
    if rollback_problems.is_empty() {
        return Error::ExecutionFailed(format!(
            "{message}. The previous installation is back in place."
        ));
    }
    Error::ExecutionFailed(format!(
        "{message}. The rollback could not restore these entries of the previous \
         installation:\n  {}\nInstall peppy again with: {}",
        rollback_problems.join("\n  "),
        super::INSTALL_SCRIPT_COMMAND
    ))
}

#[cfg(test)]
pub(crate) mod test_support {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    /// Builds an in-memory `.tgz` containing `(path, contents, mode)` regular
    /// files, for HTTP and update tests.
    pub(crate) fn build_tgz(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for &(path, data, mode) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            builder.append_data(&mut header, path, data).unwrap();
        }
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::EntryType;

    #[test]
    fn extract_release_archive_unpacks_valid_release_with_relative_symlinks() {
        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);

        let mut peppy_hdr = tar::Header::new_gnu();
        let peppy_body = b"#!/bin/sh\necho peppy v0.40.0\n";
        peppy_hdr.set_size(peppy_body.len() as u64);
        peppy_hdr.set_mode(0o644);
        peppy_hdr.set_cksum();
        builder
            .append_data(&mut peppy_hdr, "./bin/peppy", &peppy_body[..])
            .unwrap();

        let mut app_hdr = tar::Header::new_gnu();
        let app_body = b"apptainer-bin";
        app_hdr.set_size(app_body.len() as u64);
        app_hdr.set_mode(0o755);
        app_hdr.set_cksum();
        builder
            .append_data(&mut app_hdr, "./bin/apptainer/bin/apptainer", &app_body[..])
            .unwrap();

        let mut link_hdr = tar::Header::new_gnu();
        link_hdr.set_entry_type(EntryType::Symlink);
        link_hdr.set_size(0);
        link_hdr.set_mode(0o777);
        link_hdr.set_cksum();
        builder
            .append_link(
                &mut link_hdr,
                "./bin/apptainer/bin/singularity",
                "apptainer",
            )
            .unwrap();

        let tgz = builder.into_inner().unwrap().finish().unwrap();

        let staging = tempfile::tempdir().unwrap();
        extract_release_archive(tgz.as_slice(), "peppy.tgz", staging.path())
            .expect("valid archive should extract");

        let peppy = staging.path().join("bin/peppy");
        assert!(peppy.is_file());
        assert_ne!(
            fs::metadata(&peppy).unwrap().permissions().mode() & 0o111,
            0,
            "bin/peppy is made executable"
        );
        assert!(staging.path().join("bin/apptainer/bin/apptainer").is_file());
        let target = fs::read_link(staging.path().join("bin/apptainer/bin/singularity")).unwrap();
        assert_eq!(target, Path::new("apptainer"));
    }

    #[test]
    fn extract_release_archive_rejects_missing_peppy_binary() {
        let tgz = test_support::build_tgz(&[("bin/zenohd", b"zenoh", 0o755)]);
        let staging = tempfile::tempdir().unwrap();
        let err = extract_release_archive(tgz.as_slice(), "peppy.tgz", staging.path())
            .expect_err("archive without bin/peppy must fail");
        assert!(err.to_string().contains("bin/peppy"), "{err}");
    }

    #[test]
    fn extract_release_archive_rejects_corrupted_gzip_trailer() {
        let mut tgz = test_support::build_tgz(&[("bin/peppy", b"#!/bin/sh\necho ok\n", 0o755)]);
        // Flip a bit in the trailing CRC32/ISIZE bytes.
        let last = tgz.len() - 1;
        tgz[last] ^= 0xff;

        let staging = tempfile::tempdir().unwrap();
        let err = extract_release_archive(tgz.as_slice(), "peppy.tgz", staging.path())
            .expect_err("corrupted gzip trailer must fail");
        assert!(err.to_string().contains("integrity"), "{err}");
    }

    #[test]
    fn extract_release_archive_refuses_a_symlink_that_leaves_bin() {
        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);

        let mut link_hdr = tar::Header::new_gnu();
        link_hdr.set_entry_type(EntryType::Symlink);
        link_hdr.set_size(0);
        link_hdr.set_mode(0o777);
        link_hdr.set_cksum();
        builder
            .append_link(&mut link_hdr, "bin/apptainer/data", "../..")
            .unwrap();

        let tgz = builder.into_inner().unwrap().finish().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let err = extract_release_archive(tgz.as_slice(), "peppy.tgz", staging.path())
            .expect_err("a link that leaves bin/ must be refused");
        assert!(
            err.to_string().contains("a symbolic link to ../.."),
            "{err}"
        );
    }

    /// An installation directory with a staging directory inside it, as
    /// `peppy update` lays them out.
    struct SwapFixture {
        install_dir: tempfile::TempDir,
        staging_bin: PathBuf,
    }

    impl SwapFixture {
        fn new() -> Self {
            let install_dir = tempfile::tempdir().unwrap();
            let staging_bin = install_dir
                .path()
                .join(format!("{STAGING_DIR_PREFIX}test"))
                .join("bin");
            fs::create_dir_all(&staging_bin).unwrap();
            Self {
                install_dir,
                staging_bin,
            }
        }

        fn installed(&self, name: &str) -> PathBuf {
            self.install_dir.path().join(name)
        }

        fn read_installed(&self, name: &str) -> String {
            fs::read_to_string(self.installed(name)).unwrap()
        }

        fn swap(&self) -> Result<()> {
            swap_installation(&self.staging_bin, self.install_dir.path())
        }

        /// The names in the installation directory, without the staging
        /// directory of the fixture.
        fn installed_names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.install_dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with(STAGING_DIR_PREFIX))
                .collect();
            names.sort();
            names
        }
    }

    fn write_file(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn swap_installation_replaces_managed_entries_and_preserves_unmanaged_files() {
        let fixture = SwapFixture::new();
        write_file(&fixture.installed("peppy"), "old-peppy");
        write_file(&fixture.installed("zenohd"), "old-zenohd");
        write_file(&fixture.installed("capnp"), "keep-capnp");
        write_file(&fixture.installed("apptainer/bin/apptainer"), "old-app");
        write_file(&fixture.staging_bin.join("peppy"), "new-peppy");
        write_file(&fixture.staging_bin.join("zenohd"), "new-zenohd");
        write_file(
            &fixture.staging_bin.join("apptainer/bin/apptainer"),
            "new-app",
        );
        write_file(&fixture.staging_bin.join("lima/bin/limactl"), "new-lima");

        fixture.swap().expect("swap should succeed");

        assert_eq!(fixture.read_installed("peppy"), "new-peppy");
        assert_eq!(fixture.read_installed("zenohd"), "new-zenohd");
        assert_eq!(fixture.read_installed("apptainer/bin/apptainer"), "new-app");
        assert_eq!(fixture.read_installed("lima/bin/limactl"), "new-lima");
        assert_eq!(fixture.read_installed("capnp"), "keep-capnp");
        assert_eq!(
            fixture.installed_names(),
            ["apptainer", "capnp", "lima", "peppy", "zenohd"],
            "the backup directory is removed"
        );
    }

    /// The release holds a file `zenohd` while the installation holds a
    /// directory there: the swap fails after it replaced `apptainer` and
    /// `peppy`, and the rollback restores both and removes the new `lima`.
    #[test]
    fn swap_installation_failure_restores_each_replaced_entry_and_removes_new_ones() {
        let fixture = SwapFixture::new();
        write_file(&fixture.installed("peppy"), "old-peppy");
        write_file(&fixture.installed("apptainer/bin/apptainer"), "old-app");
        write_file(&fixture.installed("zenohd/keep"), "a directory");
        write_file(&fixture.staging_bin.join("peppy"), "new-peppy");
        write_file(&fixture.staging_bin.join("zenohd"), "new-zenohd");
        write_file(
            &fixture.staging_bin.join("apptainer/bin/apptainer"),
            "new-app",
        );
        write_file(&fixture.staging_bin.join("lima/bin/limactl"), "new-lima");

        let err = fixture
            .swap()
            .expect_err("a file cannot replace a directory");

        let message = err.to_string();
        assert!(message.contains("is a directory"), "{message}");
        assert!(
            message.contains("The previous installation is back in place"),
            "{message}"
        );
        assert_eq!(fixture.read_installed("peppy"), "old-peppy");
        assert_eq!(fixture.read_installed("apptainer/bin/apptainer"), "old-app");
        assert_eq!(fixture.read_installed("zenohd/keep"), "a directory");
        assert_eq!(
            fixture.installed_names(),
            ["apptainer", "peppy", "zenohd"],
            "the new lima and the backup directory are removed"
        );
    }

    #[test]
    fn swap_installation_replaces_a_file_with_one_rename_and_keeps_a_link_to_the_old_one() {
        let fixture = SwapFixture::new();
        write_file(&fixture.installed("peppy"), "old-peppy");
        write_file(&fixture.staging_bin.join("peppy"), "new-peppy");
        let backup = tempfile::tempdir().unwrap();
        let paths = SwapPaths {
            staged: fixture.staging_bin.join("peppy"),
            target: fixture.installed("peppy"),
            backup: backup.path().join("peppy"),
        };

        let old = replace_entry(&paths).unwrap_or_else(|failure| panic!("{}", failure.message));

        assert_eq!(old, OldEntry::LinkedInBackup);
        assert_eq!(fixture.read_installed("peppy"), "new-peppy");
        assert_eq!(fs::read_to_string(&paths.backup).unwrap(), "old-peppy");
    }

    #[test]
    fn rollback_reports_each_entry_that_it_cannot_restore() {
        let install_dir = tempfile::tempdir().unwrap();
        let backup_dir = tempfile::tempdir().unwrap();
        write_file(&install_dir.path().join("peppy"), "new-peppy");

        // The backup of `peppy` is missing, so the rollback cannot restore it.
        let problems = rollback(
            &[ChangedEntry {
                name: "peppy",
                old: OldEntry::LinkedInBackup,
            }],
            install_dir.path(),
            backup_dir.path(),
        );

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("peppy"), "{problems:?}");
        let message = swap_error("Failed to install `x`", &problems).to_string();
        assert!(message.contains("could not restore"), "{message}");
        assert!(
            message.contains(super::super::INSTALL_SCRIPT_COMMAND),
            "{message}"
        );
    }

    #[test]
    fn recover_interrupted_updates_puts_moved_entries_back_and_removes_leftovers() {
        let install_dir = tempfile::tempdir().unwrap();
        let bin = install_dir.path();
        write_file(&bin.join("peppy"), "current-peppy");
        write_file(&bin.join("capnp"), "keep-capnp");
        write_file(
            &bin.join(format!("{STAGING_DIR_PREFIX}a1/bin/peppy")),
            "partial download",
        );
        write_file(&bin.join(format!("{WRITE_PROBE_PREFIX}b2")), "");
        // A swap stopped after it moved `apptainer` out and before the new
        // one moved in; the backup of `peppy` is a link to the file in place.
        let backup = bin.join(format!("{BACKUP_DIR_PREFIX}c3"));
        write_file(&backup.join("apptainer/bin/apptainer"), "old-app");
        write_file(&backup.join("peppy"), "old-peppy");

        let lock = lock_install_dir(bin).unwrap();
        let problems = recover_interrupted_updates(bin, &lock);

        assert!(problems.is_empty(), "{problems:?}");
        let mut names: Vec<String> = fs::read_dir(bin)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["apptainer", "capnp", "peppy"]);
        assert_eq!(
            fs::read_to_string(bin.join("apptainer/bin/apptainer")).unwrap(),
            "old-app"
        );
        assert_eq!(
            fs::read_to_string(bin.join("peppy")).unwrap(),
            "current-peppy"
        );
    }

    #[test]
    fn lock_install_dir_refuses_a_second_update_until_the_first_ends() {
        let install_dir = tempfile::tempdir().unwrap();

        let first = lock_install_dir(install_dir.path()).expect("the first update locks");
        let err = lock_install_dir(install_dir.path())
            .err()
            .expect("a second update is refused");
        assert!(err.to_string().contains("Another `peppy update`"), "{err}");

        drop(first);
        lock_install_dir(install_dir.path()).expect("the lock is free again");
    }
}
