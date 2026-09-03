//! Build artifact cache: a `node build` whose staged sources are
//! byte-identical to an earlier build's reuses that build's artifact.
//!
//! The staged working directory is everything a build consumes: the node
//! sources, its `peppy.json5`, the apptainer def file, the generated `.peppy/`
//! tree and the vendored peppylib. A fingerprint of that tree, mixed with the
//! peppy version, the host platform and the artifact kind, names the artifact
//! in storage:
//!
//! ```text
//! <root>/built_nodes/<name>_<tag>/<fingerprint>.sif       container node
//! <root>/built_nodes/<name>_<tag>/<fingerprint>.tar.zst   process node
//! ```
//!
//! A build first resolves its [`ArtifactSlot`]; when the file is already
//! there the build is skipped and the entity is published from it. Storage
//! keeps one artifact per node identity: publishing a new fingerprint prunes
//! the others (see [`prune_siblings`]). Nothing here is time based, so the
//! cache survives daemon restarts and disks moved between machines of the
//! same platform.
//!
//! Not part of the fingerprint: the environment forwarded to `build_cmd`, the
//! digest behind a floating base image tag in the def file, and the host
//! toolchain of a process node. A build that must not reuse an artifact for
//! one of those reasons sets `rebuild` on its goal.
//!
//! The generated `.peppy/` tree is part of the fingerprint, so generation
//! into a staged copy has to be a pure function of the sources: nothing in
//! it may name the staging directory (see `generator::NodeTree::Staged`).

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use daemon_config::consts::{PEPPY_VERSION, PeppyDirs};
use sha2::{Digest, Sha256};
use tracing::warn;

use super::build_steps::validate_node_tag;

/// Start of the feedback line a build emits when it reuses a cached
/// artifact instead of building. Tests and log readers match on it.
pub const CACHED_BUILD_REUSE_PREFIX: &str = "Reusing cached build of";

/// Number of hex characters in a [`BuildFingerprint`]: the first 8 bytes of
/// the key digest, the same width as the git checkout cache key.
const FINGERPRINT_HEX_LEN: usize = 16;

/// What a build publishes, which decides the artifact's extension and is
/// part of its fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ArtifactKind {
    /// An apptainer `.sif` image.
    Container,
    /// A `.tar.zst` archive of the working directory after `build_cmd`.
    Process,
}

impl ArtifactKind {
    pub(super) fn extension(self) -> &'static str {
        match self {
            ArtifactKind::Container => "sif",
            ArtifactKind::Process => "tar.zst",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ArtifactKind::Container => "container",
            ArtifactKind::Process => "process",
        }
    }
}

/// The identity of a build's inputs: [`FINGERPRINT_HEX_LEN`] lowercase hex
/// characters, only ever produced by fingerprinting a staged tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BuildFingerprint(String);

impl fmt::Display for BuildFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a build's artifact lives in storage and whether it is already there.
#[derive(Debug)]
pub(super) struct ArtifactSlot {
    pub fingerprint: BuildFingerprint,
    /// `<built_node_dir>/<fingerprint>.<extension>`.
    pub path: PathBuf,
    /// A non-empty file already sits at `path`.
    pub cached: bool,
}

/// Fingerprints the staged tree at `working_dir` and locates the artifact
/// slot for it. Blocking: reads every file under `working_dir`.
pub(super) fn resolve_slot(
    peppy_dirs: &PeppyDirs,
    node_name: &str,
    node_tag: &str,
    working_dir: &Path,
    kind: ArtifactKind,
) -> io::Result<ArtifactSlot> {
    validate_node_tag(node_tag)?;
    let fingerprint = fingerprint_staged_tree(working_dir, kind)?;
    let path = peppy_dirs
        .built_node_dir(node_name, node_tag)
        .join(format!("{fingerprint}.{}", kind.extension()));
    let cached = fs::metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() > 0);
    Ok(ArtifactSlot {
        fingerprint,
        path,
        cached,
    })
}

/// The line announcing that `path` is reused for `node_name:node_tag`.
pub(super) fn reuse_line(
    node_name: &str,
    node_tag: &str,
    fingerprint: &BuildFingerprint,
    path: &Path,
) -> String {
    format!(
        "{CACHED_BUILD_REUSE_PREFIX} {node_name}:{node_tag} (fingerprint {fingerprint}) at {}",
        path.display()
    )
}

/// Fingerprints `working_dir` for a build of `kind` performed by this peppy
/// on this host platform.
pub(super) fn fingerprint_staged_tree(
    working_dir: &Path,
    kind: ArtifactKind,
) -> io::Result<BuildFingerprint> {
    fingerprint_tree_for(
        working_dir,
        kind,
        PEPPY_VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

/// Testable core of [`fingerprint_staged_tree`]: sha256 of the tree digest
/// (see [`hash_dir`]) followed by NUL-separated `peppy_version`, `os`, `arch`
/// and the artifact kind, truncated to [`FINGERPRINT_HEX_LEN`] hex chars.
pub(super) fn fingerprint_tree_for(
    working_dir: &Path,
    kind: ArtifactKind,
    peppy_version: &str,
    os: &str,
    arch: &str,
) -> io::Result<BuildFingerprint> {
    let mut tree = Sha256::new();
    hash_dir(&mut tree, working_dir, working_dir)?;
    let tree_digest = tree.finalize();

    let mut key = Sha256::new();
    key.update(tree_digest);
    for part in [peppy_version, os, arch, kind.label()] {
        key.update([0u8]);
        key.update(part.as_bytes());
    }
    let digest = key.finalize();
    let hex: String = digest[..FINGERPRINT_HEX_LEN / 2]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(BuildFingerprint(hex))
}

/// Feeds `dir` into `hasher`, entries in byte order of their names. Every
/// entry contributes a kind tag and its length-prefixed path relative to
/// `root`; a symlink adds its target (not followed), a regular file adds its
/// executable bit, size and content, and a directory recurses (an empty one
/// still contributes: tar and `%files` preserve it). Anything else fails,
/// since a fifo or device cannot be part of a reproducible tree.
fn hash_dir(hasher: &mut Sha256, root: &Path, dir: &Path) -> io::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by(|a, b| a.file_name().as_bytes().cmp(b.file_name().as_bytes()));

    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .expect("a read_dir entry lives under the root it was listed from");
        let rel = rel.as_os_str().as_bytes();
        let file_type = entry.file_type()?;

        if file_type.is_symlink() {
            let target = fs::read_link(&path)?;
            hasher.update(b"l");
            update_len_prefixed(hasher, rel);
            update_len_prefixed(hasher, target.as_os_str().as_bytes());
        } else if file_type.is_dir() {
            hasher.update(b"d");
            update_len_prefixed(hasher, rel);
            hash_dir(hasher, root, &path)?;
        } else if file_type.is_file() {
            let metadata = entry.metadata()?;
            let executable = metadata.permissions().mode() & 0o111 != 0;
            hasher.update(b"f");
            update_len_prefixed(hasher, rel);
            hasher.update([u8::from(executable)]);
            hasher.update(metadata.len().to_le_bytes());
            let copied = hash_file_contents(hasher, &path)?;
            if copied != metadata.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} changed while fingerprinting", path.display()),
                ));
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is neither a regular file, a directory nor a symlink",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Streams `path` into `hasher` in fixed-size chunks (O(1) memory whatever
/// the file size) and returns the number of bytes hashed.
fn hash_file_contents(hasher: &mut Sha256, path: &Path) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    let mut copied = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(copied);
        }
        hasher.update(&buf[..n]);
        copied += n as u64;
    }
}

fn update_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Removes everything in `artifact_path`'s directory except `artifact_path`
/// itself, so storage keeps one artifact per node identity. Failures are
/// logged and never fail the build that just published: a leftover only
/// costs disk.
pub(super) fn prune_siblings(artifact_path: &Path) {
    let Some(dir) = artifact_path.parent() else {
        return;
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn!(
                "cannot list {} to prune stale build artifacts: {e}",
                dir.display()
            );
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                warn!(
                    "cannot read an entry of {} to prune stale build artifacts: {e}",
                    dir.display()
                );
                continue;
            }
        };
        let path = entry.path();
        if path == artifact_path {
            continue;
        }
        let removed = match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(e) => Err(e),
        };
        if let Err(e) = removed {
            warn!("cannot remove stale build artifact {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const VERSION: &str = "v1.2.3";
    const OS: &str = "linux";
    const ARCH: &str = "aarch64";

    fn fingerprint(dir: &Path) -> BuildFingerprint {
        fingerprint_tree_for(dir, ArtifactKind::Process, VERSION, OS, ARCH)
            .expect("fingerprint should succeed")
    }

    fn write(dir: &Path, rel: &str, contents: &[u8]) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().expect("a file path has a parent")).expect("mkdir");
        fs::write(path, contents).expect("write");
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// A small tree with a nested file, a symlink and an empty directory,
    /// created in the given order so two copies can differ in insertion
    /// order only.
    fn populate(dir: &Path, order: &[&str]) {
        for item in order {
            match *item {
                "main" => write(dir, "src/main.rs", b"fn main() {}"),
                "manifest" => write(dir, "peppy.json5", b"{}"),
                "link" => symlink("src/main.rs", dir.join("entry.rs")).expect("symlink"),
                "empty" => fs::create_dir_all(dir.join("assets")).expect("mkdir"),
                other => panic!("unknown item {other}"),
            }
        }
    }

    #[test]
    fn identical_trees_fingerprint_the_same_whatever_the_creation_order() {
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        populate(a.path(), &["main", "manifest", "link", "empty"]);
        populate(b.path(), &["empty", "link", "manifest", "main"]);

        let fingerprint_a = fingerprint(a.path());
        assert_eq!(fingerprint_a, fingerprint(b.path()));
        let hex = fingerprint_a.to_string();
        assert_eq!(hex.len(), FINGERPRINT_HEX_LEN);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "fingerprint must be lowercase hex, got {hex}"
        );
    }

    #[test]
    fn changing_a_file_changes_the_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        populate(dir.path(), &["main", "manifest"]);
        let before = fingerprint(dir.path());
        write(dir.path(), "src/main.rs", b"fn main() { println!(); }");
        assert_ne!(before, fingerprint(dir.path()));
    }

    #[test]
    fn renaming_a_file_changes_the_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        populate(dir.path(), &["main", "manifest"]);
        let before = fingerprint(dir.path());
        fs::rename(
            dir.path().join("src/main.rs"),
            dir.path().join("src/lib.rs"),
        )
        .expect("rename");
        assert_ne!(before, fingerprint(dir.path()));
    }

    #[test]
    fn the_executable_bit_is_part_of_the_fingerprint_but_other_bits_are_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "run.sh", b"#!/bin/sh\n");
        let script = dir.path().join("run.sh");
        set_mode(&script, 0o644);
        let plain = fingerprint(dir.path());

        set_mode(&script, 0o600);
        assert_eq!(
            plain,
            fingerprint(dir.path()),
            "group/other bits are umask noise and must not split the key"
        );

        set_mode(&script, 0o755);
        assert_ne!(plain, fingerprint(dir.path()));
    }

    #[test]
    fn retargeting_a_symlink_changes_the_fingerprint_without_following_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.txt", b"same");
        write(dir.path(), "b.txt", b"same");
        symlink("a.txt", dir.path().join("link")).expect("symlink");
        let to_a = fingerprint(dir.path());

        fs::remove_file(dir.path().join("link")).expect("unlink");
        symlink("b.txt", dir.path().join("link")).expect("symlink");
        assert_ne!(
            to_a,
            fingerprint(dir.path()),
            "two targets with the same content still differ by target path"
        );

        fs::remove_file(dir.path().join("link")).expect("unlink");
        symlink("missing.txt", dir.path().join("link")).expect("symlink");
        fingerprint(dir.path());
    }

    #[test]
    fn an_empty_directory_is_part_of_the_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        populate(dir.path(), &["main"]);
        let before = fingerprint(dir.path());
        fs::create_dir(dir.path().join("assets")).expect("mkdir");
        assert_ne!(before, fingerprint(dir.path()));
    }

    #[test]
    fn version_platform_and_kind_each_change_the_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        populate(dir.path(), &["main", "manifest"]);
        let base = fingerprint(dir.path());
        let variants = [
            fingerprint_tree_for(dir.path(), ArtifactKind::Process, "v9.9.9", OS, ARCH),
            fingerprint_tree_for(dir.path(), ArtifactKind::Process, VERSION, "macos", ARCH),
            fingerprint_tree_for(dir.path(), ArtifactKind::Process, VERSION, OS, "x86_64"),
            fingerprint_tree_for(dir.path(), ArtifactKind::Container, VERSION, OS, ARCH),
        ];
        for variant in variants {
            assert_ne!(base, variant.expect("fingerprint should succeed"));
        }
    }

    #[test]
    fn non_utf8_file_names_are_fingerprinted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        fs::write(dir.path().join(name), b"x").expect("write");
        fingerprint(dir.path());
    }

    #[test]
    fn an_unreadable_file_fails_the_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "secret.txt", b"x");
        let secret = dir.path().join("secret.txt");
        set_mode(&secret, 0o000);
        let result = fingerprint_tree_for(dir.path(), ArtifactKind::Process, VERSION, OS, ARCH);
        set_mode(&secret, 0o644);
        if nix::unistd::geteuid().is_root() {
            // Root reads a mode 000 file; the guard cannot be observed.
            return;
        }
        assert!(result.is_err(), "an unreadable file must fail closed");
    }

    #[test]
    fn a_missing_root_fails_the_fingerprint() {
        let missing = Path::new("/nonexistent-peppy-fingerprint-root");
        assert!(fingerprint_tree_for(missing, ArtifactKind::Process, VERSION, OS, ARCH).is_err());
    }

    #[test]
    fn a_fifo_fails_the_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        nix::unistd::mkfifo(&dir.path().join("pipe"), nix::sys::stat::Mode::S_IRWXU)
            .expect("mkfifo");
        let err = fingerprint_tree_for(dir.path(), ArtifactKind::Process, VERSION, OS, ARCH)
            .expect_err("a fifo cannot be part of a reproducible tree");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn resolve_slot_names_the_keyed_artifact_and_reports_whether_it_is_cached() {
        let root = tempfile::tempdir().expect("tempdir");
        let peppy_dirs = PeppyDirs::new(root.path());
        let working_dir = tempfile::tempdir().expect("tempdir");
        populate(working_dir.path(), &["main", "manifest"]);

        let slot = resolve_slot(
            &peppy_dirs,
            "sensor",
            "v1",
            working_dir.path(),
            ArtifactKind::Container,
        )
        .expect("resolve slot");
        assert_eq!(
            slot.path,
            root.path()
                .join("built_nodes")
                .join("sensor_v1")
                .join(format!("{}.sif", slot.fingerprint))
        );
        assert!(!slot.cached, "nothing published yet");

        fs::create_dir_all(slot.path.parent().expect("parent")).expect("mkdir");
        fs::write(&slot.path, b"").expect("write");
        let again = resolve_slot(
            &peppy_dirs,
            "sensor",
            "v1",
            working_dir.path(),
            ArtifactKind::Container,
        )
        .expect("resolve slot");
        assert!(!again.cached, "an empty file is not a usable artifact");

        fs::write(&slot.path, b"SIF").expect("write");
        let again = resolve_slot(
            &peppy_dirs,
            "sensor",
            "v1",
            working_dir.path(),
            ArtifactKind::Container,
        )
        .expect("resolve slot");
        assert!(again.cached);
        assert_eq!(again.fingerprint, slot.fingerprint);

        fs::remove_file(&slot.path).expect("unlink");
        fs::create_dir(&slot.path).expect("mkdir in place of the artifact");
        let again = resolve_slot(
            &peppy_dirs,
            "sensor",
            "v1",
            working_dir.path(),
            ArtifactKind::Container,
        )
        .expect("resolve slot");
        assert!(!again.cached, "a directory at the slot is not an artifact");
    }

    #[test]
    fn resolve_slot_rejects_an_unsafe_tag_before_touching_the_tree() {
        let peppy_dirs = PeppyDirs::new("/nonexistent-peppy-root");
        let err = resolve_slot(
            &peppy_dirs,
            "sensor",
            "../evil",
            Path::new("/nonexistent-working-dir"),
            ArtifactKind::Process,
        )
        .expect_err("an unsafe tag must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn reuse_line_starts_with_the_shared_prefix() {
        let line = reuse_line(
            "sensor",
            "v1",
            &BuildFingerprint("0123456789abcdef".to_string()),
            Path::new("/data/built_nodes/sensor_v1/0123456789abcdef.sif"),
        );
        assert_eq!(
            line,
            "Reusing cached build of sensor:v1 (fingerprint 0123456789abcdef) \
             at /data/built_nodes/sensor_v1/0123456789abcdef.sif"
        );
        assert!(line.starts_with(CACHED_BUILD_REUSE_PREFIX));
    }

    #[test]
    fn prune_siblings_keeps_only_the_published_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let published = dir.path().join("0123456789abcdef.sif");
        fs::write(&published, b"new").expect("write");
        fs::write(dir.path().join("fedcba9876543210.sif"), b"old").expect("write");
        fs::write(dir.path().join(".tmpAbC123"), b"leftover staging file").expect("write");
        fs::create_dir(dir.path().join("stray")).expect("mkdir");
        fs::write(dir.path().join("stray/file"), b"x").expect("write");

        prune_siblings(&published);

        let remaining: Vec<_> = fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(
            remaining,
            vec![std::ffi::OsString::from("0123456789abcdef.sif")]
        );
        assert_eq!(fs::read(&published).expect("read"), b"new");
    }

    #[test]
    fn prune_siblings_tolerates_a_missing_directory() {
        prune_siblings(Path::new("/nonexistent-peppy-built-node/abc.sif"));
    }
}
