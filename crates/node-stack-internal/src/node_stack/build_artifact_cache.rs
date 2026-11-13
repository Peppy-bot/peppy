//! Build artifact cache: a `node build` whose staged sources are
//! byte-identical to an earlier build's reuses that build's artifact.
//!
//! The staged working directory is everything a build consumes: the node
//! sources, its `peppy.json5`, the apptainer def file, the generated `.peppy/`
//! tree and the vendored peppylib. A fingerprint of that tree, mixed with the
//! peppy version, the platform of the artifact and the artifact kind, names
//! the artifact in storage:
//!
//! ```text
//! <root>/built_nodes/<name>_<tag>/<fingerprint>.linux-<arch>.sif   container node built here
//! <root>/built_nodes/<name>_<tag>/<sif sha256>.linux-<arch>.sif    container node pulled
//! <root>/built_nodes/<name>_<tag>/<fingerprint>.tar.zst            process node
//! ```
//!
//! The platform of a container artifact is the Linux platform it runs on
//! (`linux/amd64` or `linux/arm64`), whatever host built it: a publish builds
//! the artifacts of other platforms beside the one of the host. A process
//! artifact runs only on a host like the one that built it, so its
//! fingerprint mixes the OS and the architecture of the host.
//!
//! A build first resolves its [`ArtifactSlot`]; when the file is already
//! there the build is skipped and the entity is published from it. Storage
//! keeps one artifact per node identity and platform: publishing a new
//! artifact prunes the others of its platform (see [`prune_siblings`]).
//! Nothing here is time based, so the cache survives daemon restarts and
//! disks moved between machines of the same platform.
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

use core_node_api::encoding::{ArtifactPlatform, OciDigest};
use daemon_config::consts::{HOST_ARTIFACT_PLATFORM, PEPPY_VERSION, PeppyDirs};
use sha2::{Digest, Sha256};
use tracing::warn;

use super::build_steps::validate_node_tag;

/// Start of the feedback line a build emits when it reuses a cached
/// artifact instead of building. Tests and log readers match on it.
pub const CACHED_BUILD_REUSE_PREFIX: &str = "Reusing cached build of";

/// Number of hex characters in a [`BuildFingerprint`]: the first 8 bytes of
/// the key digest, the same width as the git checkout cache key.
const FINGERPRINT_HEX_LEN: usize = 16;

/// What a build publishes, which decides the end of the artifact's file
/// name and is part of its fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ArtifactKind {
    /// An apptainer `.sif` image that runs on `ArtifactPlatform`.
    Container(ArtifactPlatform),
    /// A `.tar.zst` archive of the working directory after `build_cmd`,
    /// which runs on the host that built it.
    Process,
}

impl ArtifactKind {
    /// The end of the file name after the key: `linux-<arch>.sif` for a
    /// container artifact, `tar.zst` for a process artifact.
    pub(super) fn file_suffix(self) -> String {
        match self {
            ArtifactKind::Container(platform) => container_file_suffix(platform),
            ArtifactKind::Process => "tar.zst".to_owned(),
        }
    }

    /// The platform whose artifacts this one replaces in storage: the
    /// platform of a container artifact, the one of the host for a process
    /// artifact.
    pub(super) fn platform(self) -> ArtifactPlatform {
        match self {
            ArtifactKind::Container(platform) => platform,
            ArtifactKind::Process => HOST_ARTIFACT_PLATFORM,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ArtifactKind::Container(_) => "container",
            ArtifactKind::Process => "process",
        }
    }
}

/// `linux-<arch>.sif`: the end of the file name of a container artifact for
/// `platform`.
fn container_file_suffix(platform: ArtifactPlatform) -> String {
    format!("{}.sif", platform.tag_suffix())
}

/// Where the `.sif` of digest `sif_digest`, pulled from an OCI registry for
/// `platform`, lives in storage.
pub fn pulled_artifact_path(
    peppy_dirs: &PeppyDirs,
    node_name: &str,
    node_tag: &str,
    sif_digest: &OciDigest,
    platform: ArtifactPlatform,
) -> io::Result<PathBuf> {
    validate_node_tag(node_tag)?;
    Ok(peppy_dirs.built_node_dir(node_name, node_tag).join(format!(
        "{}.{}",
        sif_digest.hex(),
        container_file_suffix(platform)
    )))
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
        .join(format!("{fingerprint}.{}", kind.file_suffix()));
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
/// on this host.
fn fingerprint_staged_tree(working_dir: &Path, kind: ArtifactKind) -> io::Result<BuildFingerprint> {
    fingerprint_tree_for(
        working_dir,
        kind,
        PEPPY_VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

/// Testable core of [`fingerprint_staged_tree`]: sha256 of the tree digest
/// (see [`hash_dir`]) followed by NUL-separated `peppy_version`, the OS and
/// the architecture of the artifact, and the artifact kind, truncated to
/// [`FINGERPRINT_HEX_LEN`] hex chars. A container artifact is for `linux`
/// and the architecture of its platform; a process artifact is for the host,
/// `host_os` and `host_arch`.
fn fingerprint_tree_for(
    working_dir: &Path,
    kind: ArtifactKind,
    peppy_version: &str,
    host_os: &str,
    host_arch: &str,
) -> io::Result<BuildFingerprint> {
    let (os, arch) = match kind {
        ArtifactKind::Container(platform) => ("linux", platform.architecture()),
        ArtifactKind::Process => (host_os, host_arch),
    };
    let mut tree = Sha256::new();
    hash_dir(&mut tree, working_dir, working_dir, &|_| false)?;
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

/// The directory of a staged tree that holds the generated bindings and the
/// vendored peppylib, each in a directory of its own.
const STAGED_LIBS_DIR: &[u8] = b".peppy/libs";

/// The one directory of [`STAGED_LIBS_DIR`] that is not the vendored
/// peppylib: the bindings generated from the contracts of the node.
const GENERATED_BINDINGS_DIR: &[u8] = b"peppygen";

/// The source digest of the staged tree at `working_dir`: the sha256 of the
/// tree without the vendored peppylib, that is without every directory of
/// `.peppy/libs` but `peppygen`.
///
/// The vendored peppylib is the only part of a staged tree that changes with
/// the platform (the native extension and the Cap'n Proto compiler of a
/// Python node), and the peppy version in an artifact tag names it. So the
/// amd64 and the arm64 staging of one revision have the same source digest,
/// and a change of any source file changes it. Blocking: reads every file.
pub fn source_digest(working_dir: &Path) -> io::Result<OciDigest> {
    let mut tree = Sha256::new();
    hash_dir(&mut tree, working_dir, working_dir, &is_vendored_peppylib)?;
    let hex: String = tree
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(OciDigest::from_sha256_hex(&hex).expect("a sha256 is 64 hexadecimal characters"))
}

/// Whether `rel`, a path relative to the root of a staged tree, is a
/// directory of the vendored peppylib.
fn is_vendored_peppylib(rel: &[u8]) -> bool {
    let Some(rest) = rel.strip_prefix(STAGED_LIBS_DIR) else {
        return false;
    };
    let Some(name) = rest.strip_prefix(b"/") else {
        return false;
    };
    !name.contains(&b'/') && name != GENERATED_BINDINGS_DIR
}

/// Feeds `dir` into `hasher`, entries in byte order of their names. Every
/// entry contributes a kind tag and its length-prefixed path relative to
/// `root`; a symlink adds its target (not followed), a regular file adds its
/// executable bit, size and content, and a directory recurses (an empty one
/// still contributes: tar and `%files` preserve it). Anything else fails,
/// since a fifo or device cannot be part of a reproducible tree. An entry
/// whose relative path `skip` holds is left out, with all it holds.
fn hash_dir(
    hasher: &mut Sha256,
    root: &Path,
    dir: &Path,
    skip: &dyn Fn(&[u8]) -> bool,
) -> io::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by(|a, b| a.file_name().as_bytes().cmp(b.file_name().as_bytes()));

    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .expect("a read_dir entry lives under the root it was listed from");
        let rel = rel.as_os_str().as_bytes();
        if skip(rel) {
            continue;
        }
        let file_type = entry.file_type()?;

        if file_type.is_symlink() {
            let target = fs::read_link(&path)?;
            hasher.update(b"l");
            update_len_prefixed(hasher, rel);
            update_len_prefixed(hasher, target.as_os_str().as_bytes());
        } else if file_type.is_dir() {
            hasher.update(b"d");
            update_len_prefixed(hasher, rel);
            hash_dir(hasher, root, &path, skip)?;
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
/// itself and the container artifacts of the platforms other than
/// `platform`, so storage keeps one artifact per node identity and platform.
/// Failures are logged and never fail the build or the pull that just
/// published: a leftover only costs disk.
pub fn prune_siblings(artifact_path: &Path, platform: ArtifactPlatform) {
    let kept_suffixes: Vec<String> = ArtifactPlatform::ALL
        .into_iter()
        .filter(|other| *other != platform)
        .map(|other| format!(".{}", container_file_suffix(other)))
        .collect();
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
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kept_suffixes
            .iter()
            .any(|suffix| name.ends_with(suffix.as_str()))
        {
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
    use std::assert_matches;
    use std::os::unix::fs::symlink;

    const AMD64: ArtifactKind = ArtifactKind::Container(ArtifactPlatform::LinuxAmd64);
    const ARM64: ArtifactKind = ArtifactKind::Container(ArtifactPlatform::LinuxArm64);

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
            fingerprint_tree_for(dir.path(), ARM64, VERSION, OS, ARCH),
        ];
        for variant in variants {
            assert_ne!(base, variant.expect("fingerprint should succeed"));
        }
    }

    /// A container artifact is keyed by the platform it runs on, not by the
    /// host that built it: a Mac and a Linux arm64 machine name one arm64
    /// build the same, and an amd64 build of the same tree differently.
    #[test]
    fn a_container_fingerprint_is_keyed_by_the_target_platform_not_the_host() {
        let dir = tempfile::tempdir().expect("tempdir");
        populate(dir.path(), &["main", "manifest"]);
        let arm64_on = |os: &str, arch: &str| {
            fingerprint_tree_for(dir.path(), ARM64, VERSION, os, arch).expect("fingerprint")
        };
        assert_eq!(arm64_on("linux", "aarch64"), arm64_on("macos", "aarch64"));
        assert_eq!(arm64_on("linux", "aarch64"), arm64_on("linux", "x86_64"));
        let amd64 = fingerprint_tree_for(dir.path(), AMD64, VERSION, "linux", "aarch64")
            .expect("fingerprint");
        assert_ne!(amd64, arm64_on("linux", "aarch64"));
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

        let slot = resolve_slot(&peppy_dirs, "sensor", "v1", working_dir.path(), AMD64)
            .expect("resolve slot");
        assert_eq!(
            slot.path,
            root.path()
                .join("built_nodes")
                .join("sensor_v1")
                .join(format!("{}.linux-amd64.sif", slot.fingerprint))
        );
        assert!(!slot.cached, "nothing published yet");

        fs::create_dir_all(slot.path.parent().expect("parent")).expect("mkdir");
        fs::write(&slot.path, b"").expect("write");
        let again = resolve_slot(&peppy_dirs, "sensor", "v1", working_dir.path(), AMD64)
            .expect("resolve slot");
        assert!(!again.cached, "an empty file is not a usable artifact");

        fs::write(&slot.path, b"SIF").expect("write");
        let again = resolve_slot(&peppy_dirs, "sensor", "v1", working_dir.path(), AMD64)
            .expect("resolve slot");
        assert!(again.cached);
        assert_eq!(again.fingerprint, slot.fingerprint);

        fs::remove_file(&slot.path).expect("unlink");
        fs::create_dir(&slot.path).expect("mkdir in place of the artifact");
        let again = resolve_slot(&peppy_dirs, "sensor", "v1", working_dir.path(), AMD64)
            .expect("resolve slot");
        assert!(!again.cached, "a directory at the slot is not an artifact");
    }

    /// The first peppy process of a new boot clears a per-boot root and keeps
    /// its node builds, so a build of the same staged tree after a reboot
    /// finds its artifact.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn an_artifact_outlives_the_clear_of_a_new_boot() {
        let root = tempfile::tempdir().expect("tempdir");
        let peppy_dirs = PeppyDirs::new(root.path().join("peppy-dev"));
        let working_dir = tempfile::tempdir().expect("tempdir");
        populate(working_dir.path(), &["main", "manifest"]);
        let resolve = || {
            resolve_slot(&peppy_dirs, "sensor", "v1", working_dir.path(), AMD64)
                .expect("resolve slot")
        };
        let slot = resolve();
        fs::create_dir_all(slot.path.parent().expect("parent")).expect("mkdir");
        fs::write(&slot.path, b"SIF").expect("write");
        fs::create_dir_all(peppy_dirs.runtime_config_dir()).expect("mkdir");
        fs::write(peppy_dirs.root_boot_id_path(), "an-earlier-boot").expect("write");

        let outcome = daemon_config::per_boot_root::clear_if_new_boot(&peppy_dirs).expect("clear");

        assert_matches!(
            outcome,
            daemon_config::per_boot_root::ClearOutcome::Cleared { .. },
            "the root was used by an earlier boot"
        );
        let again = resolve();
        assert!(again.cached, "the build is still there after the clear");
        assert_eq!(again.path, slot.path);
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

    /// A staged tree as a node add leaves it: the sources, the generated
    /// bindings and a vendored peppylib whose native extension is the one of
    /// `arch`.
    fn staged_tree(dir: &Path, arch: &str) {
        populate(dir, &["main", "manifest"]);
        write(dir, ".peppy/libs/peppygen/__init__.py", b"bindings");
        write(
            dir,
            ".peppy/libs/peppylib/peppylib/_peppylib.abi3.so",
            format!("native extension of {arch}").as_bytes(),
        );
        write(dir, ".peppy/libs/peppy-codec/Cargo.toml", b"vendored crate");
    }

    #[test]
    fn the_staging_of_one_revision_for_two_platforms_has_one_source_digest() {
        let amd64 = tempfile::tempdir().expect("tempdir");
        let arm64 = tempfile::tempdir().expect("tempdir");
        staged_tree(amd64.path(), "x86_64");
        staged_tree(arm64.path(), "aarch64");

        assert_eq!(
            source_digest(amd64.path()).expect("digest"),
            source_digest(arm64.path()).expect("digest")
        );
        assert_ne!(
            fingerprint(amd64.path()),
            fingerprint(arm64.path()),
            "the build fingerprint keeps the vendored peppylib"
        );
    }

    #[test]
    fn a_change_of_a_source_file_or_of_the_bindings_changes_the_source_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        staged_tree(dir.path(), "x86_64");
        let before = source_digest(dir.path()).expect("digest");

        write(dir.path(), "src/main.rs", b"fn main() { println!(); }");
        let after_source = source_digest(dir.path()).expect("digest");
        assert_ne!(before, after_source);

        write(
            dir.path(),
            ".peppy/libs/peppygen/__init__.py",
            b"other bindings",
        );
        assert_ne!(after_source, source_digest(dir.path()).expect("digest"));
    }

    #[test]
    fn only_the_directories_of_the_libs_but_the_bindings_are_vendored_peppylib() {
        assert!(is_vendored_peppylib(b".peppy/libs/peppylib"));
        assert!(is_vendored_peppylib(b".peppy/libs/peppy-codec"));
        assert!(!is_vendored_peppylib(b".peppy/libs/peppygen"));
        assert!(!is_vendored_peppylib(b".peppy/libs"));
        assert!(!is_vendored_peppylib(b".peppy/libs/peppylib/x"));
        assert!(!is_vendored_peppylib(b"src/.peppy/libs/peppylib"));
        assert!(!is_vendored_peppylib(b".peppy/libsx/peppylib"));
    }

    #[test]
    fn prune_siblings_keeps_the_published_artifact_and_the_other_platforms() {
        let dir = tempfile::tempdir().expect("tempdir");
        let published = dir.path().join("0123456789abcdef.linux-amd64.sif");
        fs::write(&published, b"new").expect("write");
        fs::write(dir.path().join("fedcba9876543210.linux-amd64.sif"), b"old").expect("write");
        fs::write(
            dir.path()
                .join(format!("{}.linux-amd64.sif", "a".repeat(64))),
            b"pulled",
        )
        .expect("write");
        fs::write(
            dir.path().join("1111111111111111.linux-arm64.sif"),
            b"arm64",
        )
        .expect("write");
        fs::write(dir.path().join("2222222222222222.tar.zst"), b"process").expect("write");
        fs::write(dir.path().join(".tmpAbC123"), b"leftover staging file").expect("write");
        fs::create_dir(dir.path().join("stray")).expect("mkdir");
        fs::write(dir.path().join("stray/file"), b"x").expect("write");

        prune_siblings(&published, ArtifactPlatform::LinuxAmd64);

        let mut remaining: Vec<_> = fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        remaining.sort();
        assert_eq!(
            remaining,
            vec![
                std::ffi::OsString::from("0123456789abcdef.linux-amd64.sif"),
                std::ffi::OsString::from("1111111111111111.linux-arm64.sif"),
            ]
        );
        assert_eq!(fs::read(&published).expect("read"), b"new");
    }

    #[test]
    fn prune_siblings_tolerates_a_missing_directory() {
        prune_siblings(
            Path::new("/nonexistent-peppy-built-node/abc.linux-amd64.sif"),
            ArtifactPlatform::LinuxAmd64,
        );
    }

    #[test]
    fn a_pulled_artifact_is_named_after_its_digest_and_platform() {
        let peppy_dirs = PeppyDirs::new("/data");
        let digest = OciDigest::from_sha256_hex(&"b".repeat(64)).expect("digest");
        assert_eq!(
            pulled_artifact_path(
                &peppy_dirs,
                "waldo",
                "v1",
                &digest,
                ArtifactPlatform::LinuxArm64
            )
            .expect("path"),
            PathBuf::from(format!(
                "/data/built_nodes/waldo_v1/{}.linux-arm64.sif",
                "b".repeat(64)
            ))
        );
        assert!(
            pulled_artifact_path(
                &peppy_dirs,
                "waldo",
                "../x",
                &digest,
                ArtifactPlatform::LinuxArm64
            )
            .is_err()
        );
    }
}
