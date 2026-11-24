//! Safe release archive extraction, staged binary verification, and atomic
//! installation swap with rollback.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use tar::{Archive, EntryType};

use crate::error::{Error, Result};

/// Directories from `bin/` in a release archive that `peppy update` replaces.
pub(crate) const MANAGED_DIRS: &[&str] =
    &["apptainer", "apptainer-x86_64", "apptainer-aarch64", "lima"];

/// Executable files from `bin/` in a release archive that `peppy update` replaces.
pub(crate) const MANAGED_FILES: &[&str] = &["peppy", "zenohd"];

/// Extracts a `.tgz` release archive from `reader` into `staging_dir`.
///
/// Every entry is validated before extraction so no regular file, directory,
/// symlink, or hard link can escape `staging_dir`, and device/FIFO entries are
/// refused. After the tar stream ends, the gzip decoder is drained to EOF so
/// the gzip CRC32 checksum and uncompressed size trailer are always verified.
pub(crate) fn extract_tgz_safely<R: Read>(reader: R, staging_dir: &Path) -> Result<()> {
    let mut decoder = GzDecoder::new(reader);
    {
        let mut archive = Archive::new(&mut decoder);
        archive.set_preserve_permissions(true);
        let entries = archive
            .entries()
            .map_err(|e| Error::ExecutionFailed(format!("Failed to read release archive: {e}")))?;

        for entry_result in entries {
            let mut entry = entry_result.map_err(|e| {
                Error::ExecutionFailed(format!("Failed to read release archive entry: {e}"))
            })?;
            validate_entry(&entry)?;
            entry.unpack_in(staging_dir).map_err(|e| {
                Error::ExecutionFailed(format!("Failed to extract release archive entry: {e}"))
            })?;
        }
    }

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

/// Validates that a tar entry stays within the extraction root and has a safe
/// entry type.
fn validate_entry<R: Read>(entry: &tar::Entry<'_, R>) -> Result<()> {
    let path = entry.path().map_err(|e| {
        Error::ExecutionFailed(format!("Release archive contains an invalid path: {e}"))
    })?;
    let normalized = normalize_relative_path(&path).ok_or_else(|| {
        Error::ExecutionFailed(format!(
            "Release archive contains an unsafe path: `{}`",
            path.display()
        ))
    })?;

    let entry_type = entry.header().entry_type();
    match entry_type {
        EntryType::Regular
        | EntryType::Directory
        | EntryType::GNULongName
        | EntryType::GNULongLink
        | EntryType::XHeader
        | EntryType::XGlobalHeader => Ok(()),
        EntryType::Symlink => {
            let target = entry
                .link_name()
                .map_err(|e| {
                    Error::ExecutionFailed(format!(
                        "Release archive contains an unreadable symlink target: {e}"
                    ))
                })?
                .ok_or_else(|| {
                    Error::ExecutionFailed(format!(
                        "Release archive symlink `{}` has no target",
                        path.display()
                    ))
                })?;
            if target.is_absolute() {
                return Err(Error::ExecutionFailed(format!(
                    "Release archive contains an absolute symlink `{}` -> `{}`",
                    path.display(),
                    target.display()
                )));
            }
            let parent = normalized.parent().unwrap_or_else(|| Path::new(""));
            if normalize_relative_path(&parent.join(&target)).is_none() {
                return Err(Error::ExecutionFailed(format!(
                    "Release archive symlink `{}` -> `{}` escapes extraction directory",
                    path.display(),
                    target.display()
                )));
            }
            Ok(())
        }
        EntryType::Link => {
            let target = entry
                .link_name()
                .map_err(|e| {
                    Error::ExecutionFailed(format!(
                        "Release archive contains an unreadable hard link target: {e}"
                    ))
                })?
                .ok_or_else(|| {
                    Error::ExecutionFailed(format!(
                        "Release archive hard link `{}` has no target",
                        path.display()
                    ))
                })?;
            if normalize_relative_path(&target).is_none() {
                return Err(Error::ExecutionFailed(format!(
                    "Release archive hard link `{}` -> `{}` escapes extraction directory",
                    path.display(),
                    target.display()
                )));
            }
            Ok(())
        }
        other => Err(Error::ExecutionFailed(format!(
            "Release archive contains an unsupported entry type ({other:?}) at `{}`",
            path.display()
        ))),
    }
}

/// Normalizes a relative path without filesystem access, returning `None` if it
/// is absolute or traverses above the root with `..`.
fn normalize_relative_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
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

/// Verifies that the staged `peppy` binary is runnable on this machine before
/// the existing installation or daemon is touched.
pub(crate) fn verify_staged_binary(staged_peppy: &Path) -> Result<String> {
    let output = Command::new(staged_peppy)
        .arg("--version")
        .output()
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Downloaded peppy binary failed to execute (`{}`): {e}",
                staged_peppy.display()
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::ExecutionFailed(format!(
            "Downloaded peppy binary failed `--version` check (exit {}): {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Verifies that `dir` is writable by creating and dropping a probe file in it.
/// Called before downloading the release archive so permission errors fail fast.
pub(crate) fn check_dir_writable(dir: &Path) -> Result<()> {
    tempfile::Builder::new()
        .prefix(".peppy-update-probe-")
        .tempfile_in(dir)
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Cannot write to `{}`: {e}. Check directory permissions or re-run with sudo.",
                dir.display()
            ))
        })?;
    Ok(())
}

/// Atomically replaces managed binaries and tool directories in
/// `install_bin_dir` from `staging_bin_dir`.
///
/// Both directories must sit on the same filesystem (the caller stages inside
/// `install_bin_dir`). Existing entries are moved into a backup directory first;
/// if any rename fails, every swapped entry is restored from the backup.
pub(crate) fn swap_installation(staging_bin_dir: &Path, install_bin_dir: &Path) -> Result<()> {
    let backup_dir = tempfile::Builder::new()
        .prefix(".peppy-update-backup-")
        .tempdir_in(install_bin_dir)
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to create backup directory in `{}`: {e}",
                install_bin_dir.display()
            ))
        })?;

    let mut entries_to_swap: Vec<&str> = Vec::new();
    for name in MANAGED_DIRS {
        if staging_bin_dir.join(name).is_dir() {
            entries_to_swap.push(name);
        }
    }
    for name in MANAGED_FILES {
        if staging_bin_dir.join(name).is_file() {
            entries_to_swap.push(name);
        }
    }

    let mut swapped: Vec<(&str, bool)> = Vec::with_capacity(entries_to_swap.len());
    for name in entries_to_swap {
        let staged = staging_bin_dir.join(name);
        let target = install_bin_dir.join(name);
        let backup = backup_dir.path().join(name);

        let had_previous = target.exists() || target.is_symlink();
        if had_previous && let Err(e) = fs::rename(&target, &backup) {
            rollback(&swapped, install_bin_dir, backup_dir.path());
            return Err(Error::ExecutionFailed(format!(
                "Failed to move existing `{}` to backup: {e}",
                target.display()
            )));
        }

        if let Err(e) = fs::rename(&staged, &target) {
            if had_previous {
                let _ = fs::rename(&backup, &target);
            }
            rollback(&swapped, install_bin_dir, backup_dir.path());
            return Err(Error::ExecutionFailed(format!(
                "Failed to install `{}`: {e}",
                target.display()
            )));
        }

        swapped.push((name, had_previous));
    }

    // `backup_dir` drops here, removing the old replaced binaries and dirs.
    Ok(())
}

fn rollback(swapped: &[(&str, bool)], install_bin_dir: &Path, backup_dir: &Path) {
    for &(name, had_previous) in swapped.iter().rev() {
        let target = install_bin_dir.join(name);
        if target.is_dir() {
            let _ = fs::remove_dir_all(&target);
        } else {
            let _ = fs::remove_file(&target);
        }
        if had_previous {
            let _ = fs::rename(backup_dir.join(name), &target);
        }
    }
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

    #[test]
    fn extract_tgz_safely_unpacks_valid_release_with_relative_symlinks() {
        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);

        let mut peppy_hdr = tar::Header::new_gnu();
        let peppy_body = b"#!/bin/sh\necho peppy v0.40.0\n";
        peppy_hdr.set_size(peppy_body.len() as u64);
        peppy_hdr.set_mode(0o755);
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
        extract_tgz_safely(tgz.as_slice(), staging.path()).expect("valid archive should extract");

        assert!(staging.path().join("bin/peppy").is_file());
        assert!(staging.path().join("bin/apptainer/bin/apptainer").is_file());
        let target = fs::read_link(staging.path().join("bin/apptainer/bin/singularity")).unwrap();
        assert_eq!(target, Path::new("apptainer"));
    }

    #[test]
    fn extract_tgz_safely_rejects_missing_peppy_binary() {
        let tgz = test_support::build_tgz(&[("bin/zenohd", b"zenoh", 0o755)]);
        let staging = tempfile::tempdir().unwrap();
        let err = extract_tgz_safely(tgz.as_slice(), staging.path())
            .expect_err("archive without bin/peppy must fail");
        assert!(err.to_string().contains("bin/peppy"), "{err}");
    }

    #[test]
    fn extract_tgz_safely_rejects_corrupted_gzip_trailer() {
        let mut tgz = test_support::build_tgz(&[("bin/peppy", b"#!/bin/sh\necho ok\n", 0o755)]);
        // Flip a bit in the trailing CRC32/ISIZE bytes.
        let last = tgz.len() - 1;
        tgz[last] ^= 0xff;

        let staging = tempfile::tempdir().unwrap();
        let err = extract_tgz_safely(tgz.as_slice(), staging.path())
            .expect_err("corrupted gzip trailer must fail");
        assert!(err.to_string().contains("integrity"), "{err}");
    }

    #[test]
    fn extract_tgz_safely_rejects_escaping_symlink() {
        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);

        let mut link_hdr = tar::Header::new_gnu();
        link_hdr.set_entry_type(EntryType::Symlink);
        link_hdr.set_size(0);
        link_hdr.set_mode(0o777);
        link_hdr.set_cksum();
        builder
            .append_link(&mut link_hdr, "bin/escape", "../../etc/passwd")
            .unwrap();

        let tgz = builder.into_inner().unwrap().finish().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let err = extract_tgz_safely(tgz.as_slice(), staging.path())
            .expect_err("escaping symlink must be refused");
        assert!(err.to_string().contains("escapes"), "{err}");
    }

    #[test]
    fn extract_tgz_safely_rejects_absolute_symlink() {
        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);

        let mut link_hdr = tar::Header::new_gnu();
        link_hdr.set_entry_type(EntryType::Symlink);
        link_hdr.set_size(0);
        link_hdr.set_mode(0o777);
        link_hdr.set_cksum();
        builder
            .append_link(&mut link_hdr, "bin/abs", "/etc/passwd")
            .unwrap();

        let tgz = builder.into_inner().unwrap().finish().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let err = extract_tgz_safely(tgz.as_slice(), staging.path())
            .expect_err("absolute symlink must be refused");
        assert!(err.to_string().contains("absolute symlink"), "{err}");
    }

    #[test]
    fn verify_staged_binary_succeeds_for_working_executable_and_fails_for_broken() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("peppy-good");
        fs::write(&good, "#!/bin/sh\necho 'peppy v0.40.0'\n").unwrap();
        fs::set_permissions(&good, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(verify_staged_binary(&good).unwrap(), "peppy v0.40.0");

        let bad = dir.path().join("peppy-bad");
        fs::write(&bad, "#!/bin/sh\necho 'broken' >&2\nexit 1\n").unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
        let err = verify_staged_binary(&bad).expect_err("failing binary must be rejected");
        assert!(err.to_string().contains("broken"), "{err}");
    }

    #[test]
    fn swap_installation_replaces_managed_entries_and_preserves_unmanaged_files() {
        let install_dir = tempfile::tempdir().unwrap();
        let staging_dir = tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(install_dir.path())
            .unwrap();
        let staging_bin = staging_dir.path().join("bin");
        fs::create_dir_all(staging_bin.join("apptainer/bin")).unwrap();

        // Existing installation plus an unmanaged sibling (like capnp).
        fs::write(install_dir.path().join("peppy"), "old-peppy").unwrap();
        fs::write(install_dir.path().join("zenohd"), "old-zenohd").unwrap();
        fs::write(install_dir.path().join("capnp"), "keep-capnp").unwrap();
        fs::create_dir_all(install_dir.path().join("apptainer/bin")).unwrap();
        fs::write(
            install_dir.path().join("apptainer/bin/apptainer"),
            "old-app",
        )
        .unwrap();

        // Staged update.
        fs::write(staging_bin.join("peppy"), "new-peppy").unwrap();
        fs::write(staging_bin.join("zenohd"), "new-zenohd").unwrap();
        fs::write(staging_bin.join("apptainer/bin/apptainer"), "new-app").unwrap();

        swap_installation(&staging_bin, install_dir.path()).expect("swap should succeed");

        assert_eq!(
            fs::read_to_string(install_dir.path().join("peppy")).unwrap(),
            "new-peppy"
        );
        assert_eq!(
            fs::read_to_string(install_dir.path().join("zenohd")).unwrap(),
            "new-zenohd"
        );
        assert_eq!(
            fs::read_to_string(install_dir.path().join("apptainer/bin/apptainer")).unwrap(),
            "new-app"
        );
        assert_eq!(
            fs::read_to_string(install_dir.path().join("capnp")).unwrap(),
            "keep-capnp"
        );
    }

    #[test]
    fn rollback_restores_previous_files_and_removes_newly_added_entries() {
        let install_dir = tempfile::tempdir().unwrap();
        let backup_dir = tempfile::tempdir().unwrap();

        // Simulate state after `peppy` (existed previously) and `lima` (new)
        // were moved into `install_dir`, before a later entry failed.
        fs::write(backup_dir.path().join("peppy"), "old-peppy").unwrap();
        fs::write(install_dir.path().join("peppy"), "new-peppy").unwrap();
        fs::create_dir_all(install_dir.path().join("lima/bin")).unwrap();
        fs::write(install_dir.path().join("lima/bin/limactl"), "new-lima").unwrap();

        rollback(
            &[("peppy", true), ("lima", false)],
            install_dir.path(),
            backup_dir.path(),
        );

        assert_eq!(
            fs::read_to_string(install_dir.path().join("peppy")).unwrap(),
            "old-peppy"
        );
        assert!(
            !install_dir.path().join("lima").exists(),
            "newly introduced directory must be removed on rollback"
        );
    }
}
