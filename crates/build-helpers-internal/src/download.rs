//! Downloading files and extracting entries from zip archives.

use std::path::Path;
use std::process::Command;

use crate::command::run_command;

/// Downloads `url` to `dest` using `curl`. Returns `true` on success.
///
/// On failure, removes any partially written file so a later retry starts from
/// a clean slate. Reuses [`run_command`] so failures surface as `cargo:warning`.
pub fn download_file(url: &str, dest: &Path) -> bool {
    let ok = run_command(
        Command::new("curl").args(["-fSL", "-o"]).arg(dest).arg(url),
        &format!("download {url}"),
    );
    if !ok {
        std::fs::remove_file(dest).ok();
    }
    ok
}

/// Extracts a single named `entry` from the zip archive at `zip_path` and
/// writes it to `dest`. Used to pull one binary (e.g. `zenohd`) out of a
/// release archive without unpacking the whole thing.
pub fn extract_zip_entry(zip_path: &Path, entry: &str, dest: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut zip_entry = archive
        .by_name(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))?;
    let mut dest_file = std::fs::File::create(dest)?;
    std::io::copy(&mut zip_entry, &mut dest_file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skip guard: keeps the download tests green on hosts whose curl cannot
    /// serve `file://` URLs (or that lack curl entirely).
    fn curl_supports_file_urls() -> bool {
        let Ok(output) = Command::new("curl").arg("--version").output() else {
            return false;
        };
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .lines()
                .find(|line| line.starts_with("Protocols:"))
                .is_some_and(|line| line.split_whitespace().any(|p| p == "file"))
    }

    #[test]
    fn download_file_copies_file_url_to_dest() {
        if !curl_supports_file_urls() {
            eprintln!("skipping: curl lacks file:// support");
            return;
        }
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("src.bin");
        let contents = b"download me";
        std::fs::write(&src, contents).expect("write src");

        let dest = dir.path().join("dest.bin");
        assert!(download_file(&format!("file://{}", src.display()), &dest));
        assert_eq!(std::fs::read(&dest).expect("read dest"), contents);
    }

    #[test]
    fn download_file_failure_removes_partial_dest() {
        if !curl_supports_file_urls() {
            eprintln!("skipping: curl lacks file:// support");
            return;
        }
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("dest.bin");
        std::fs::write(&dest, b"stale partial data").expect("pre-create dest");

        // curl exits non-zero for an unreadable file:// source; the helper
        // must then remove the pre-existing dest so a retry starts from a
        // clean slate (pmi-internal's build.rs relies on this).
        let missing = dir.path().join("missing.bin");
        assert!(!download_file(
            &format!("file://{}", missing.display()),
            &dest
        ));
        assert!(!dest.exists());
    }

    /// Builds a stored (uncompressed) zip at `path` containing the given
    /// `(name, contents)` entries.
    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        use std::io::Write;

        let mut writer = zip::ZipWriter::new(std::fs::File::create(path).expect("create zip"));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, contents) in entries {
            writer.start_file(*name, options).expect("entry");
            writer.write_all(contents).expect("write");
        }
        writer.finish().expect("finish zip");
    }

    #[test]
    fn extract_zip_entry_roundtrips_named_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let zip_path = dir.path().join("archive.zip");
        let contents = b"#!/bin/sh\necho zenohd\n";

        // Build a small archive with two entries; we only want the named one.
        write_zip(
            &zip_path,
            &[("readme.txt", b"ignore me"), ("zenohd", contents)],
        );

        let dest = dir.path().join("zenohd");
        extract_zip_entry(&zip_path, "zenohd", &dest).expect("extract should succeed");
        assert_eq!(std::fs::read(&dest).expect("read extracted"), contents);
    }

    #[test]
    fn extract_zip_entry_missing_entry_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let zip_path = dir.path().join("archive.zip");
        write_zip(&zip_path, &[("other", b"data")]);

        let dest = dir.path().join("zenohd");
        assert!(extract_zip_entry(&zip_path, "zenohd", &dest).is_err());
    }
}
