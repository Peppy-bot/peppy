//! Archive handling for node sources: detection of supported extensions
//! (`.tar.zst`/`.tar.zstd`/`.tzst`), extraction of a local archive into
//! a tempdir, and locating the node root inside an already-extracted
//! bundle. The extraction itself is `node_stack::extract_tar_zst`, under
//! the rules of a node source (`ArchiveOrigin::NodeSource`).

use super::parse_node_config_file;
use config::consts::NODE_CONFIG_FILE;
use config::node::NodeConfig;
use std::path::{Path, PathBuf};

/// Extracts the archive of a node source at `archive_path` into
/// `destination`: files, directories, and links that stay in the archive,
/// up to `node_stack::MAX_NODE_SOURCE_UNPACKED_BYTES`.
pub(crate) fn extract_node_source_archive(
    archive_path: &Path,
    destination: &Path,
) -> Result<(), String> {
    node_stack::extract_tar_zst(
        archive_path,
        destination,
        node_stack::ArchiveOrigin::NodeSource,
    )
}

pub(crate) struct ResolvedLocalArchiveSource {
    pub(crate) node_config: NodeConfig,
    pub(crate) source_path: PathBuf,
    pub(crate) temp_dir: tempfile::TempDir,
}

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the path is in lower case here"
)]
fn is_supported_archive_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".tar.zst") || path.ends_with(".tar.zstd") || path.ends_with(".tzst")
}

pub(crate) fn is_supported_fs_archive(path: &Path) -> bool {
    is_supported_archive_path(path.to_string_lossy().as_ref())
}

pub(crate) fn is_supported_http_archive(url: &url::Url) -> bool {
    is_supported_archive_path(url.path())
}

pub(crate) fn locate_node_root_dir(extracted_dir: &Path) -> Result<PathBuf, String> {
    let direct = extracted_dir.join(NODE_CONFIG_FILE);
    if direct.is_file() {
        return Ok(extracted_dir.to_path_buf());
    }

    let mut candidate_dirs = Vec::new();
    for entry in std::fs::read_dir(extracted_dir).map_err(|e| {
        format!(
            "Failed to list extracted bundle directory {}: {}",
            extracted_dir.display(),
            e
        )
    })? {
        let entry = entry.map_err(|e| {
            format!(
                "Failed to read extracted bundle directory entry in {}: {}",
                extracted_dir.display(),
                e
            )
        })?;
        let file_type = entry.file_type().map_err(|e| {
            format!(
                "Failed to read file type for extracted bundle entry {}: {}",
                entry.path().display(),
                e
            )
        })?;
        if file_type.is_dir() {
            candidate_dirs.push(entry.path());
        }
    }

    let mut matching_dirs: Vec<PathBuf> = candidate_dirs
        .into_iter()
        .filter(|candidate| candidate.join(NODE_CONFIG_FILE).is_file())
        .collect();
    if matching_dirs.len() == 1 {
        return Ok(matching_dirs.pop().expect("matching dir should exist"));
    }

    Err(format!(
        "Bundle does not contain {NODE_CONFIG_FILE} at the root (or exactly one top-level folder)"
    ))
}

pub(crate) fn resolve_local_archive_source(
    archive_path: &Path,
) -> Result<ResolvedLocalArchiveSource, String> {
    let temp_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create temporary directory: {e}"))?;

    extract_node_source_archive(archive_path, temp_dir.path())?;
    let source_path = locate_node_root_dir(temp_dir.path())?;
    let node_config = parse_node_config_file(&source_path.join(NODE_CONFIG_FILE))?;

    Ok(ResolvedLocalArchiveSource {
        node_config,
        source_path,
        temp_dir,
    })
}

/// The `(name, tag)` of the node in the archive of a node source at
/// `archive_path`, read from the stream of the archive with nothing written
/// to the disk. The config is the `peppy.json5` at the root of the archive,
/// else the one of its only top-level directory that holds one, as for
/// [`locate_node_root_dir`]. `None` when the archive does not read or holds
/// no such config.
pub(crate) fn node_named_in_archive(archive_path: &Path) -> Option<(String, String)> {
    let file = std::fs::File::open(archive_path).ok()?;
    let decoder = zstd::stream::read::Decoder::new(file).ok()?;
    let bounded = std::io::Read::take(decoder, node_stack::MAX_NODE_SOURCE_UNPACKED_BYTES);
    let mut archive = tar::Archive::new(bounded);
    let mut in_a_top_level_dir = Vec::new();
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let path = entry.path().ok()?.into_owned();
        let components: Vec<_> = path
            .components()
            .filter(|component| !matches!(component, std::path::Component::CurDir))
            .collect();
        let at_the_root = match components.as_slice() {
            [_] => true,
            [std::path::Component::Normal(_), _] => false,
            _ => continue,
        };
        let Some(std::path::Component::Normal(file_name)) = components.last() else {
            continue;
        };
        if *file_name != NODE_CONFIG_FILE {
            continue;
        }
        let mut content = String::new();
        std::io::Read::read_to_string(&mut entry, &mut content).ok()?;
        if at_the_root {
            return name_and_tag_of(&content);
        }
        in_a_top_level_dir.push(content);
    }
    match in_a_top_level_dir.as_slice() {
        [content] => name_and_tag_of(content),
        _ => None,
    }
}

/// The `(name, tag)` of the node config `content`.
fn name_and_tag_of(content: &str) -> Option<(String, String)> {
    let config = config::node::NodeConfigParser::from_content(content).ok()?;
    Some((
        config.manifest.name.as_str().to_owned(),
        config.manifest.tag,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"{
        peppy_schema: "node/v1",
        manifest: { name: "archived", tag: "v2", description: ["A node in an archive."] },
        execution: { language: "rust", container: { def_file: "apptainer.def" } },
    }"#;

    /// A `.tar.zst` with a file of `content` at each path of `files`.
    fn archive_of(files: &[(&str, &str)]) -> tempfile::NamedTempFile {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(content.len() as u64);
            builder
                .append_data(&mut header, path, content.as_bytes())
                .expect("append a file");
        }
        let tar_bytes = builder.into_inner().expect("finish the tar");
        let archive = tempfile::Builder::new()
            .suffix(".tar.zst")
            .tempfile()
            .expect("archive file");
        let mut encoder =
            zstd::Encoder::new(archive.reopen().expect("reopen"), 1).expect("encoder");
        std::io::Write::write_all(&mut encoder, &tar_bytes).expect("write");
        encoder.finish().expect("finish");
        archive
    }

    fn named(archive: &tempfile::NamedTempFile) -> Option<(String, String)> {
        node_named_in_archive(archive.path())
    }

    #[test]
    fn the_node_of_an_archive_is_read_at_its_root_or_in_its_only_folder() {
        let expected = Some(("archived".to_owned(), "v2".to_owned()));
        assert_eq!(named(&archive_of(&[("peppy.json5", CONFIG)])), expected);
        assert_eq!(named(&archive_of(&[("./peppy.json5", CONFIG)])), expected);
        assert_eq!(
            named(&archive_of(&[
                ("src/main.rs", ""),
                ("node/peppy.json5", CONFIG)
            ])),
            expected
        );
        // The config at the root wins over one in a folder.
        assert_eq!(
            named(&archive_of(&[
                ("other/peppy.json5", "{"),
                ("peppy.json5", CONFIG)
            ])),
            expected
        );
    }

    #[test]
    fn an_archive_with_no_single_node_names_none() {
        assert_eq!(named(&archive_of(&[("src/main.rs", "")])), None);
        assert_eq!(
            named(&archive_of(&[
                ("a/peppy.json5", CONFIG),
                ("b/peppy.json5", CONFIG)
            ])),
            None
        );
        assert_eq!(named(&archive_of(&[("a/b/peppy.json5", CONFIG)])), None);
        assert_eq!(named(&archive_of(&[("peppy.json5", "not json5")])), None);
    }
}
