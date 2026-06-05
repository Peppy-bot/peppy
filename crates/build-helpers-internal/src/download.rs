//! Downloading files and extracting entries from zip archives.

use std::path::Path;
use std::process::Command;

use crate::command::run_command;

/// Downloads `url` to `dest` using `curl`. Returns `true` on success.
///
/// On failure, removes any partially written file so a later retry starts from
/// a clean slate. Reuses [`run_command`] so failures surface as `cargo:warning`.
pub fn download_file(url: &str, dest: &Path) -> bool {
    let dest_str = dest
        .to_str()
        .expect("download destination path is not valid UTF-8");
    let ok = run_command(
        Command::new("curl").args(["-fSL", "-o", dest_str, url]),
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

    #[test]
    fn extract_zip_entry_roundtrips_named_entry() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("temp dir");
        let zip_path = dir.path().join("archive.zip");
        let contents = b"#!/bin/sh\necho zenohd\n";

        // Build a small archive with two entries; we only want the named one.
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&zip_path).expect("create zip"));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("readme.txt", options).expect("entry");
        writer.write_all(b"ignore me").expect("write");
        writer.start_file("zenohd", options).expect("entry");
        writer.write_all(contents).expect("write");
        writer.finish().expect("finish zip");

        let dest = dir.path().join("zenohd");
        extract_zip_entry(&zip_path, "zenohd", &dest).expect("extract should succeed");
        assert_eq!(std::fs::read(&dest).expect("read extracted"), contents);
    }

    #[test]
    fn extract_zip_entry_missing_entry_errors() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("temp dir");
        let zip_path = dir.path().join("archive.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&zip_path).expect("create zip"));
        writer
            .start_file(
                "other",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .expect("entry");
        writer.write_all(b"data").expect("write");
        writer.finish().expect("finish zip");

        let dest = dir.path().join("zenohd");
        assert!(extract_zip_entry(&zip_path, "zenohd", &dest).is_err());
    }
}
