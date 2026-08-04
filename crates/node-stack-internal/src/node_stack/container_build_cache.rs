//! Persistent build caches for Rust container nodes.
//!
//! `apptainer build` compiles every Rust container node from scratch: the
//! `%post` scriptlet runs in an ephemeral sandbox, so the cargo registry and
//! all compiled artifacts are discarded with it. This module provisions a
//! host-side cache directory that is bind mounted into the build at `/mnt`
//! (a mount point every FHS base image already has; build-time binds cannot
//! create their destination) and activated through `APPTAINERENV_*`
//! variables, which apptainer injects into the `%post` environment:
//!
//! * `cargo-home/` persists the crates.io registry between builds via
//!   `CARGO_HOME`. Toolchain discovery is unaffected: rustup binaries are
//!   found via `PATH` and `RUSTUP_HOME`, not `CARGO_HOME`.
//! * `sccache` + `sccache-cache/` cache rustc invocations via
//!   `RUSTC_WRAPPER`, when a statically linked sccache is found on the
//!   daemon's PATH. A dynamically linked binary is rejected because the
//!   host libc it needs is not present in the container.
//!
//! Caching is best effort and fails open: when anything is missing or goes
//! wrong during setup, the build proceeds exactly as it would without this
//! module.

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};

use config::node::PeppygenLanguage;
use daemon_config::consts::PeppyDirs;
use tracing::warn;

/// Setting this to any value disables container build caching.
const NO_CONTAINER_BUILD_CACHE_ENV_VAR: &str = "PEPPY_NO_CONTAINER_BUILD_CACHE";

/// Container-side mount point for the cache directory.
pub(super) const BIND_DEST: &str = "/mnt";

/// Subdirectory names inside the cache, shared by the host-side layout and
/// the container-side environment so the two can never desynchronize.
const CARGO_HOME_SUBDIR: &str = "cargo-home";
const SCCACHE_BIN_SUBDIR: &str = "sccache";
const SCCACHE_CACHE_SUBDIR: &str = "sccache-cache";

/// Cache binds and environment for one container build.
///
/// `env` keys are plain variable names; the build step prefixes them with
/// `APPTAINERENV_` so apptainer forwards them into `%post`.
pub(super) struct ContainerBuildCache {
    pub host_dir: PathBuf,
    /// Host path of the installed sccache binary, bound read-only over its
    /// location inside [`BIND_DEST`] so `%post` cannot replace the executable
    /// that later builds run. Set only when sccache is active.
    pub sccache_bin: Option<PathBuf>,
    pub env: Vec<(&'static str, String)>,
    /// One-line summary streamed to the build feedback channel.
    pub summary: String,
}

/// Container-side path of the read-only sccache binary bind.
pub(super) fn sccache_bin_dest() -> String {
    format!("{BIND_DEST}/{SCCACHE_BIN_SUBDIR}")
}

/// Prepares the shared build cache for a container build, or `None` when
/// caching does not apply: non-Rust node, user opt-out, a build whose
/// configuration conflicts with the cache, or cache directory setup failure.
pub(super) fn prepare(
    peppy_dirs: &PeppyDirs,
    language: PeppygenLanguage,
    def_contents: &str,
    apptainer_build_extra_args: &[String],
) -> Option<ContainerBuildCache> {
    let enabled = language == PeppygenLanguage::Rust
        && std::env::var_os(NO_CONTAINER_BUILD_CACHE_ENV_VAR).is_none();
    if !enabled {
        return None;
    }
    if let Some(marker) = cache_conflict_marker(def_contents, apptainer_build_extra_args) {
        warn!(
            "container build cache disabled: the def file or \
             apptainer_build_extra_args mention `{marker}`, which the cache \
             mount or its environment overrides would interfere with"
        );
        return None;
    }
    prepare_in(
        &peppy_dirs.container_build_cache_dir(),
        find_static_sccache(),
    )
}

/// Returns the first marker showing the build's own configuration would
/// collide with the cache: the mount point itself, a rustup install that the
/// `CARGO_HOME` override would misplace, or one of the environment variables
/// the cache manages. Plain substring matching errs toward skipping the
/// cache.
fn cache_conflict_marker(
    def_contents: &str,
    apptainer_build_extra_args: &[String],
) -> Option<&'static str> {
    const CONFLICT_MARKERS: [&str; 6] = [
        BIND_DEST,
        "rustup",
        "CARGO_HOME",
        "RUSTC_WRAPPER",
        "SCCACHE_DIR",
        "SCCACHE_SERVER_PORT",
    ];
    CONFLICT_MARKERS.into_iter().find(|marker| {
        def_contents.contains(marker)
            || apptainer_build_extra_args
                .iter()
                .any(|arg| arg.contains(marker))
    })
}

/// Testable core of [`prepare`]: lays out `cache_root` and derives the bind
/// plus environment. `sccache_source` is the host binary to install into the
/// cache, already verified statically linked.
fn prepare_in(cache_root: &Path, sccache_source: Option<PathBuf>) -> Option<ContainerBuildCache> {
    // The path is spliced into `--bind {src}:{dest}`, whose spec grammar has
    // no escaping for its delimiters.
    let has_bind_delimiter = cache_root.to_str().is_none_or(|s| s.contains([':', ',']));
    if has_bind_delimiter {
        warn!(
            "container build cache disabled: cache path {} is not a valid apptainer bind source",
            cache_root.display()
        );
        return None;
    }
    if let Err(e) = std::fs::create_dir_all(cache_root.join(CARGO_HOME_SUBDIR)) {
        warn!(
            "container build cache disabled: cannot create {}: {e}",
            cache_root.display()
        );
        return None;
    }

    let mut env = vec![("CARGO_HOME", format!("{BIND_DEST}/{CARGO_HOME_SUBDIR}"))];
    let mut parts = vec!["cargo registry"];
    let mut sccache_bin = None;

    if let Some(source) = sccache_source {
        match install_sccache(&source, cache_root) {
            Ok(()) => {
                env.push(("RUSTC_WRAPPER", sccache_bin_dest()));
                env.push(("SCCACHE_DIR", format!("{BIND_DEST}/{SCCACHE_CACHE_SUBDIR}")));
                // Concurrent builds share the host network namespace. A unique
                // port per build keeps each sccache server paired with the
                // build that started it; servers die with the build's PID
                // namespace, so ports free up immediately.
                env.push(("SCCACHE_SERVER_PORT", next_server_port().to_string()));
                parts.push("sccache");
                sccache_bin = Some(cache_root.join(SCCACHE_BIN_SUBDIR));
            }
            Err(e) => warn!("sccache disabled for this build: {e}"),
        }
    }

    Some(ContainerBuildCache {
        host_dir: cache_root.to_path_buf(),
        sccache_bin,
        env,
        summary: format!("Container build cache: {}", parts.join(" + ")),
    })
}

/// Finds a statically linked sccache on the daemon's PATH.
fn find_static_sccache() -> Option<PathBuf> {
    let path = which_sccache()?;
    match is_static_linux_elf(&path) {
        Ok(true) => Some(path),
        Ok(false) => {
            warn!(
                "sccache at {} is not a static Linux executable and cannot \
                 run inside the build container; building without sccache",
                path.display()
            );
            None
        }
        Err(e) => {
            warn!("cannot inspect sccache at {}: {e}", path.display());
            None
        }
    }
}

fn which_sccache() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let candidate = dir.join("sccache");
        is_executable_file(&candidate).then_some(candidate)
    })
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Copies the host sccache into the cache dir (as `sccache`) and creates the
/// cache subdirectory. The copy is refreshed when size or mtime differ, and
/// written via a temp file + rename so concurrent builds never observe a
/// partial binary.
fn install_sccache(source: &Path, cache_root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(cache_root.join(SCCACHE_CACHE_SUBDIR))?;

    let dest = cache_root.join(SCCACHE_BIN_SUBDIR);
    let source_meta = std::fs::metadata(source)?;
    let up_to_date = std::fs::metadata(&dest).is_ok_and(|dest_meta| {
        dest_meta.len() == source_meta.len()
            && dest_meta.modified().ok() == source_meta.modified().ok()
    });
    if up_to_date {
        return Ok(());
    }

    let staging = tempfile::NamedTempFile::new_in(cache_root)?;
    std::fs::copy(source, staging.path())?;
    // Re-verify the bytes actually copied: the PATH binary can be replaced
    // between [`find_static_sccache`]'s check and this copy (e.g. a
    // `cargo install sccache` completing, which produces a dynamic binary).
    if !is_static_linux_elf(staging.path())? {
        return Err(std::io::Error::other(format!(
            "{} is no longer a static Linux executable",
            source.display()
        )));
    }
    staging
        .as_file()
        .set_times(std::fs::FileTimes::new().set_modified(source_meta.modified()?))?;
    staging
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    staging.persist(&dest).map_err(|e| e.error).map(|_| ())
}

/// ELF `e_machine` for the architecture this daemon runs on. Build containers
/// execute natively, so only a binary matching it can run inside the build.
/// A new target architecture fails compilation here on purpose.
#[cfg(target_arch = "x86_64")]
const HOST_ELF_MACHINE: u16 = 62;
#[cfg(target_arch = "aarch64")]
const HOST_ELF_MACHINE: u16 = 183;

fn next_server_port() -> u16 {
    // Below the default ephemeral range (32768+), so a kernel-assigned
    // loopback port never occupies the server's address. The daemon PID
    // spreads two daemons on one host (e.g. a dev root next to the real
    // one) across the range; the counter separates concurrent builds
    // within one daemon.
    const PORT_RANGE_START: u16 = 24000;
    const PORT_RANGE_LEN: u16 = 2000;
    static COUNTER: AtomicU16 = AtomicU16::new(0);
    let offset = (std::process::id() as u16)
        .wrapping_mul(31)
        .wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed));
    PORT_RANGE_START + offset % PORT_RANGE_LEN
}

/// Returns whether `path` is a 64-bit little-endian ELF executable for this
/// machine's architecture that the kernel can load without a dynamic linker:
/// `ET_EXEC` or `ET_DYN` (static-pie), at least one `PT_LOAD` segment, and no
/// `PT_INTERP`. Glibc-dynamic and foreign-arch binaries do not qualify. Any
/// parse failure or foreign format returns `Ok(false)`.
fn is_static_linux_elf(path: &Path) -> std::io::Result<bool> {
    const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
    const CLASS_64: u8 = 2;
    const DATA_LITTLE_ENDIAN: u8 = 1;
    const EV_CURRENT: u32 = 1;
    const ET_EXEC: u16 = 2;
    const ET_DYN: u16 = 3;
    const PT_LOAD: u32 = 1;
    const PT_INTERP: u32 = 3;
    const MAX_PROGRAM_HEADERS: u16 = 256;

    let mut file = std::fs::File::open(path)?;
    let mut header = [0u8; 64];
    if file.read_exact(&mut header).is_err() {
        return Ok(false);
    }
    if header[0..4] != ELF_MAGIC || header[4] != CLASS_64 || header[5] != DATA_LITTLE_ENDIAN {
        return Ok(false);
    }
    let e_type = u16::from_le_bytes(header[0x10..0x12].try_into().expect("slice is 2 bytes"));
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Ok(false);
    }
    let e_machine = u16::from_le_bytes(header[0x12..0x14].try_into().expect("slice is 2 bytes"));
    if e_machine != HOST_ELF_MACHINE {
        return Ok(false);
    }
    let e_version = u32::from_le_bytes(header[0x14..0x18].try_into().expect("slice is 4 bytes"));
    if e_version != EV_CURRENT {
        return Ok(false);
    }

    let ph_offset = u64::from_le_bytes(header[0x20..0x28].try_into().expect("slice is 8 bytes"));
    let ph_entry_size =
        u16::from_le_bytes(header[0x36..0x38].try_into().expect("slice is 2 bytes"));
    let ph_count = u16::from_le_bytes(header[0x38..0x3a].try_into().expect("slice is 2 bytes"));
    if ph_count == 0 || ph_count > MAX_PROGRAM_HEADERS || ph_entry_size < 4 {
        return Ok(false);
    }

    let file_len = file.metadata()?.len();
    let mut has_load_segment = false;
    for i in 0..ph_count {
        let entry_offset = u64::from(i)
            .checked_mul(u64::from(ph_entry_size))
            .and_then(|table_offset| ph_offset.checked_add(table_offset));
        let in_bounds = entry_offset
            .and_then(|offset| offset.checked_add(4))
            .is_some_and(|end| end <= file_len);
        let Some(entry_offset) = entry_offset.filter(|_| in_bounds) else {
            return Ok(false);
        };
        file.seek(SeekFrom::Start(entry_offset))?;
        let mut p_type = [0u8; 4];
        if file.read_exact(&mut p_type).is_err() {
            return Ok(false);
        }
        match u32::from_le_bytes(p_type) {
            PT_INTERP => return Ok(false),
            PT_LOAD => has_load_segment = true,
            _ => {}
        }
    }
    Ok(has_load_segment)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a structurally valid static-pie 64-bit LE ELF (`ET_DYN`,
    /// current version, host machine) with the given program header types.
    fn elf_with_program_headers(p_types: &[u32]) -> Vec<u8> {
        const HEADER_LEN: usize = 64;
        const PH_ENTRY_SIZE: u16 = 56;
        let mut elf = vec![0u8; HEADER_LEN];
        elf[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        elf[4] = 2; // 64-bit
        elf[5] = 1; // little-endian
        elf[0x10..0x12].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        elf[0x12..0x14].copy_from_slice(&HOST_ELF_MACHINE.to_le_bytes());
        elf[0x14..0x18].copy_from_slice(&1u32.to_le_bytes()); // EV_CURRENT
        elf[0x20..0x28].copy_from_slice(&(HEADER_LEN as u64).to_le_bytes());
        elf[0x36..0x38].copy_from_slice(&PH_ENTRY_SIZE.to_le_bytes());
        elf[0x38..0x3a].copy_from_slice(&(p_types.len() as u16).to_le_bytes());
        for p_type in p_types {
            let mut entry = vec![0u8; PH_ENTRY_SIZE as usize];
            entry[0..4].copy_from_slice(&p_type.to_le_bytes());
            elf.extend_from_slice(&entry);
        }
        elf
    }

    fn write_temp(content: &[u8]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("create temp file");
        std::fs::write(file.path(), content).expect("write temp file");
        file
    }

    #[test]
    fn static_elf_is_accepted() {
        // PT_LOAD (1) and PT_DYNAMIC (2) but no PT_INTERP: static-pie layout.
        let file = write_temp(&elf_with_program_headers(&[1, 2]));
        assert!(is_static_linux_elf(file.path()).expect("readable"));
    }

    #[test]
    fn dynamic_elf_is_rejected() {
        let file = write_temp(&elf_with_program_headers(&[1, 3, 2]));
        assert!(!is_static_linux_elf(file.path()).expect("readable"));
    }

    #[test]
    fn non_elf_is_rejected() {
        let file = write_temp(b"#!/bin/sh\necho not an elf\n");
        assert!(!is_static_linux_elf(file.path()).expect("readable"));
    }

    #[test]
    fn truncated_file_is_rejected() {
        let file = write_temp(&[0x7f, b'E', b'L', b'F']);
        assert!(!is_static_linux_elf(file.path()).expect("readable"));
    }

    #[test]
    fn prepare_without_sccache_still_caches_registry() {
        let root = tempfile::tempdir().expect("create temp dir");
        let cache = prepare_in(root.path(), None).expect("cache prepared");
        assert!(root.path().join("cargo-home").is_dir());
        assert_eq!(
            cache.env,
            vec![("CARGO_HOME", "/mnt/cargo-home".to_string())]
        );
        assert!(cache.sccache_bin.is_none());
        assert_eq!(cache.summary, "Container build cache: cargo registry");
    }

    #[test]
    fn prepare_with_sccache_installs_binary_and_env() {
        let root = tempfile::tempdir().expect("create temp dir");
        let sccache = write_temp(&elf_with_program_headers(&[1]));
        let cache =
            prepare_in(root.path(), Some(sccache.path().to_path_buf())).expect("cache prepared");

        assert!(root.path().join("sccache").is_file());
        assert!(root.path().join("sccache-cache").is_dir());
        assert_eq!(
            cache.sccache_bin.as_deref(),
            Some(&*root.path().join("sccache"))
        );
        let keys: Vec<&str> = cache.env.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            vec![
                "CARGO_HOME",
                "RUSTC_WRAPPER",
                "SCCACHE_DIR",
                "SCCACHE_SERVER_PORT"
            ]
        );
        assert_eq!(
            cache.summary,
            "Container build cache: cargo registry + sccache"
        );
    }

    #[test]
    fn sccache_copy_is_refreshed_when_source_changes() {
        let root = tempfile::tempdir().expect("create temp dir");
        let dest = root.path().join("sccache");

        let version_one = elf_with_program_headers(&[1]);
        let first = write_temp(&version_one);
        install_sccache(first.path(), root.path()).expect("first install");
        assert_eq!(std::fs::read(&dest).expect("read dest"), version_one);

        let version_two = elf_with_program_headers(&[1, 2]);
        let second = write_temp(&version_two);
        install_sccache(second.path(), root.path()).expect("second install");
        assert_eq!(std::fs::read(&dest).expect("read dest"), version_two);
    }

    #[test]
    fn foreign_architecture_elf_is_rejected() {
        let mut elf = elf_with_program_headers(&[1]);
        elf[0x12..0x14].copy_from_slice(&0xff00u16.to_le_bytes());
        let file = write_temp(&elf);
        assert!(!is_static_linux_elf(file.path()).expect("readable"));
    }

    #[test]
    fn elf_with_overflowing_header_offset_is_rejected() {
        let mut elf = elf_with_program_headers(&[1]);
        elf[0x20..0x28].copy_from_slice(&u64::MAX.to_le_bytes());
        let file = write_temp(&elf);
        assert!(!is_static_linux_elf(file.path()).expect("readable"));
    }

    #[test]
    fn install_rejects_a_source_that_became_dynamic() {
        let root = tempfile::tempdir().expect("create temp dir");
        let dynamic = write_temp(&elf_with_program_headers(&[1, 3]));
        let err = install_sccache(dynamic.path(), root.path()).expect_err("must reject");
        assert!(err.to_string().contains("no longer a static"));
        assert!(!root.path().join("sccache").exists());
    }

    #[test]
    fn cache_path_with_bind_delimiter_disables_caching() {
        let root = tempfile::tempdir().expect("create temp dir");
        let with_colon = root.path().join("odd:dir");
        assert!(prepare_in(&with_colon, None).is_none());
    }

    #[test]
    fn conflicting_build_configuration_is_detected() {
        assert_eq!(
            cache_conflict_marker("%post\n    ls /mnt/data\n", &[]),
            Some("/mnt")
        );
        assert_eq!(
            cache_conflict_marker("", &["--bind".to_string(), "/data:/mnt".to_string()]),
            Some("/mnt")
        );
        assert_eq!(
            cache_conflict_marker("curl https://sh.rustup.rs | sh", &[]),
            Some("rustup")
        );
        for var in [
            "CARGO_HOME",
            "RUSTC_WRAPPER",
            "SCCACHE_DIR",
            "SCCACHE_SERVER_PORT",
        ] {
            assert_eq!(
                cache_conflict_marker(&format!("%post\n    export {var}=/opt/custom\n"), &[]),
                Some(var),
                "def file setting {var} must disable the cache"
            );
        }
        assert_eq!(
            cache_conflict_marker(
                "%post\n    cargo build --release\n",
                &["--no-setgroups".to_string()]
            ),
            None
        );
    }

    #[test]
    fn non_loadable_elf_is_rejected() {
        // ET_NONE: structurally an ELF, but the kernel cannot execute it.
        let mut elf = elf_with_program_headers(&[1]);
        elf[0x10..0x12].copy_from_slice(&0u16.to_le_bytes());
        let file = write_temp(&elf);
        assert!(!is_static_linux_elf(file.path()).expect("readable"));

        // No PT_LOAD segment: nothing for the kernel to map.
        let file = write_temp(&elf_with_program_headers(&[2]));
        assert!(!is_static_linux_elf(file.path()).expect("readable"));
    }

    #[test]
    fn missing_sccache_source_falls_back_to_registry_only() {
        let root = tempfile::tempdir().expect("create temp dir");
        let cache = prepare_in(root.path(), Some(root.path().join("does-not-exist")))
            .expect("cache prepared");
        assert_eq!(cache.env.len(), 1);
        assert_eq!(cache.summary, "Container build cache: cargo registry");
    }
}
