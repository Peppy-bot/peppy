//! Persistent build caches for Rust and Python container nodes.
//!
//! `apptainer build` runs the `%post` scriptlet in an ephemeral sandbox, so
//! everything a build downloads or compiles is discarded with it: a Rust node
//! fetches and compiles every crate again, and a Python node downloads every
//! package and its interpreter again. This module provisions a host-side
//! cache directory that is bind mounted into the build at [`BIND_DEST`] and
//! activated through `APPTAINERENV_*` variables, which apptainer injects into
//! the `%post` environment. What a build caches depends on its language (see
//! [`CacheProfile`]):
//!
//! * Rust: `cargo-home/` persists the crates.io registry between builds via
//!   `CARGO_HOME`. Toolchain discovery is unaffected: rustup binaries are
//!   found via `PATH` and `RUSTUP_HOME`, not `CARGO_HOME`. `sccache-cache/`
//!   persists compiled artifacts via `RUSTC_WRAPPER` and `SCCACHE_DIR`. The
//!   sccache executable ships inside the peppy Rust base image, so the
//!   wrapper is only activated for defs that bootstrap from it.
//! * Python: `uv-cache/` persists the packages uv downloads and builds via
//!   `UV_CACHE_DIR`, and `uv-python/` the interpreter archives
//!   `uv python install` downloads via `UV_PYTHON_CACHE_DIR`.
//!   `UV_LINK_MODE=copy` makes uv copy files from its cache into the
//!   environment it installs: the cache is a bind mount, where uv cannot
//!   hard-link into the image, and a copy keeps every file of the image
//!   independent of the host cache.
//! * Both: `downloads/` holds files a `%post` fetches from the network, named
//!   by their sha256, via `PEPPY_DOWNLOAD_CACHE`. No tool reads that variable
//!   on its own: a def opts in by looking a pinned file up there before it
//!   downloads it, and by storing what it downloads under its checksum.
//!
//! Caching is best effort and fails open: when the main directory of a
//! profile cannot be set up, the build proceeds exactly as it would without
//! this module, and when an optional part cannot, the build gets the other
//! parts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};

use config::node::PeppygenLanguage;
use daemon_config::consts::PeppyDirs;
use tracing::warn;

/// Setting this to any value disables container build caching.
const NO_CONTAINER_BUILD_CACHE_ENV_VAR: &str = "PEPPY_NO_CONTAINER_BUILD_CACHE";

/// Container-side mount point for the cache directory. Build-time binds
/// cannot create their destination, so the image must already contain this
/// directory: the peppy base images ship it, and custom base images opt in
/// by creating it (see [`def_provides_cache_mount`]).
pub(super) const BIND_DEST: &str = "/peppy-cache";

/// Base images guaranteed to contain [`BIND_DEST`]: the directory has been
/// part of these images since their first publication under this namespace.
const CACHE_MOUNT_IMAGES: [&str; 2] = ["peppybot/rust-cargo-base", "peppybot/python-uv-base"];

/// The base image that additionally ships the sccache executable, which is
/// what allows `RUSTC_WRAPPER=sccache` to be set without any host-side
/// provisioning.
const SCCACHE_IMAGE: &str = "peppybot/rust-cargo-base";

/// Subdirectory names inside the cache, shared by the host-side layout and
/// the container-side environment so the two can never desynchronize.
const CARGO_HOME_SUBDIR: &str = "cargo-home";
const SCCACHE_CACHE_SUBDIR: &str = "sccache-cache";
const UV_CACHE_SUBDIR: &str = "uv-cache";
const UV_PYTHON_SUBDIR: &str = "uv-python";
const DOWNLOADS_SUBDIR: &str = "downloads";

/// Names the downloads directory inside the build. Its presence is what tells
/// a `%post` that the cache is bind mounted: without the bind, `/peppy-cache`
/// is a plain directory of the image and whatever a build writes there ships
/// in the image.
const DOWNLOAD_CACHE_ENV_VAR: &str = "PEPPY_DOWNLOAD_CACHE";

/// Markers showing a Rust build's own configuration would collide with its
/// cache: a rustup install that the `CARGO_HOME` override would misplace, or
/// one of the environment variables the Rust profile sets.
const RUST_CONFLICT_MARKERS: [&str; 5] = [
    "rustup",
    "CARGO_HOME",
    "RUSTC_WRAPPER",
    "SCCACHE_DIR",
    "SCCACHE_SERVER_PORT",
];

/// Markers showing a Python build's own configuration would collide with its
/// cache: one of the environment variables the Python profile sets, or uv's
/// `--link-mode` flag. The flag overrides `UV_LINK_MODE`, and a symlink mode
/// would link the files of the image to the bind-mounted cache, which the
/// image does not keep. The profile sets no cargo variable, so a Python def
/// that installs rustup (to build a Rust extension, say) keeps its uv cache.
const PYTHON_CONFLICT_MARKERS: [&str; 4] = [
    "UV_CACHE_DIR",
    "UV_PYTHON_CACHE_DIR",
    "UV_LINK_MODE",
    "--link-mode",
];

/// What a container build caches, parsed from its language and def file
/// before any filesystem work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheProfile {
    /// The crates.io registry, and compiled artifacts when
    /// `sccache_in_image`: whether the build's base image ships the sccache
    /// executable.
    Rust { sccache_in_image: bool },
    /// The packages uv installs and the interpreters it downloads.
    Python,
}

impl CacheProfile {
    fn new(language: PeppygenLanguage, def_contents: &str) -> Self {
        match language {
            PeppygenLanguage::Rust => Self::Rust {
                sccache_in_image: def_contents.contains(SCCACHE_IMAGE),
            },
            PeppygenLanguage::Python => Self::Python,
        }
    }

    fn conflict_markers(self) -> &'static [&'static str] {
        match self {
            Self::Rust { .. } => &RUST_CONFLICT_MARKERS,
            Self::Python => &PYTHON_CONFLICT_MARKERS,
        }
    }
}

/// Cache bind and environment for one container build.
///
/// `env` keys are plain variable names; the build step prefixes them with
/// `APPTAINERENV_` so apptainer forwards them into `%post`.
pub(super) struct ContainerBuildCache {
    pub host_dir: PathBuf,
    pub env: Vec<(&'static str, String)>,
    /// One-line summary streamed to the build feedback channel.
    pub summary: String,
}

/// Prepares the shared build cache for a container build, or `None` when
/// caching does not apply: user opt-out, an image without the mount point, a
/// build whose configuration conflicts with the cache, or cache directory
/// setup failure.
pub(super) fn prepare(
    peppy_dirs: &PeppyDirs,
    language: PeppygenLanguage,
    def_contents: &str,
    apptainer_build_extra_args: &[String],
) -> Option<ContainerBuildCache> {
    if std::env::var_os(NO_CONTAINER_BUILD_CACHE_ENV_VAR).is_some() {
        return None;
    }
    let profile = cache_profile_for(language, def_contents, apptainer_build_extra_args)?;
    prepare_in(&peppy_dirs.container_build_cache_dir(), profile)
}

/// The cache profile of a build, or `None` when the build cannot use the
/// cache: its image is not known to contain the mount point, or its own
/// configuration mentions one of the profile's markers.
fn cache_profile_for(
    language: PeppygenLanguage,
    def_contents: &str,
    apptainer_build_extra_args: &[String],
) -> Option<CacheProfile> {
    if !def_provides_cache_mount(def_contents) {
        tracing::debug!(
            "container build cache disabled: the def file's base image is not \
             known to contain {BIND_DEST} (create it in the image to opt in)"
        );
        return None;
    }
    let profile = CacheProfile::new(language, def_contents);
    if let Some(marker) = cache_conflict_marker(profile, def_contents, apptainer_build_extra_args) {
        warn!(
            "container build cache disabled: the def file or \
             apptainer_build_extra_args mention `{marker}`, which the cache \
             environment overrides would interfere with"
        );
        return None;
    }
    Some(profile)
}

/// Whether the build's image is known to contain [`BIND_DEST`]: it either
/// bootstraps from a peppy base image, or the def file mentions the mount
/// point itself, the documented opt-in for custom base images that create
/// the directory.
fn def_provides_cache_mount(def_contents: &str) -> bool {
    CACHE_MOUNT_IMAGES
        .iter()
        .any(|image| def_contents.contains(image))
        || def_contents.contains(BIND_DEST)
}

/// Returns the first of the profile's markers that the def file or the extra
/// build arguments mention. Plain substring matching errs toward skipping
/// the cache.
fn cache_conflict_marker(
    profile: CacheProfile,
    def_contents: &str,
    apptainer_build_extra_args: &[String],
) -> Option<&'static str> {
    profile.conflict_markers().iter().copied().find(|marker| {
        def_contents.contains(marker)
            || apptainer_build_extra_args
                .iter()
                .any(|arg| arg.contains(marker))
    })
}

/// Testable core of [`prepare`]: lays out `cache_root` for `profile` and
/// derives the bind plus environment. The main directory of the profile
/// (`cargo-home/` or `uv-cache/`) is required; each other part is optional,
/// and a build that cannot have one gets the rest.
fn prepare_in(cache_root: &Path, profile: CacheProfile) -> Option<ContainerBuildCache> {
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
    // The main part of each profile: its subdirectory, the variable that
    // points the toolchain at it, and its name in the summary.
    let (main_subdir, main_env_var, main_part) = match profile {
        CacheProfile::Rust { .. } => (CARGO_HOME_SUBDIR, "CARGO_HOME", "cargo registry"),
        CacheProfile::Python => (UV_CACHE_SUBDIR, "UV_CACHE_DIR", "uv packages"),
    };
    let main_dir = cache_root.join(main_subdir);
    if let Err(e) = std::fs::create_dir_all(&main_dir) {
        warn!(
            "container build cache disabled: cannot create {}: {e}",
            main_dir.display()
        );
        return None;
    }

    let mut env = vec![(main_env_var, format!("{BIND_DEST}/{main_subdir}"))];
    let mut parts = vec![main_part];
    match profile {
        CacheProfile::Rust { sccache_in_image } => {
            if sccache_in_image && create_optional_part(cache_root, SCCACHE_CACHE_SUBDIR, "sccache")
            {
                env.push(("RUSTC_WRAPPER", "sccache".to_string()));
                env.push(("SCCACHE_DIR", format!("{BIND_DEST}/{SCCACHE_CACHE_SUBDIR}")));
                // Concurrent builds share the host network namespace. A unique
                // port per build keeps each sccache server paired with the
                // build that started it; servers die with the build's PID
                // namespace, so ports free up immediately.
                env.push(("SCCACHE_SERVER_PORT", next_server_port().to_string()));
                parts.push("sccache");
            }
        }
        CacheProfile::Python => {
            env.push(("UV_LINK_MODE", "copy".to_string()));
            if create_optional_part(cache_root, UV_PYTHON_SUBDIR, "Python interpreter cache") {
                env.push((
                    "UV_PYTHON_CACHE_DIR",
                    format!("{BIND_DEST}/{UV_PYTHON_SUBDIR}"),
                ));
                parts.push("Python interpreters");
            }
        }
    }

    if create_optional_part(cache_root, DOWNLOADS_SUBDIR, "download cache") {
        env.push((
            DOWNLOAD_CACHE_ENV_VAR,
            format!("{BIND_DEST}/{DOWNLOADS_SUBDIR}"),
        ));
        parts.push("downloads");
    }

    Some(ContainerBuildCache {
        host_dir: cache_root.to_path_buf(),
        env,
        summary: format!("Container build cache: {}", parts.join(" + ")),
    })
}

/// Creates the `subdir` of an optional cache part. A failure costs the build
/// that part alone, so it is a warning that names `part`.
fn create_optional_part(cache_root: &Path, subdir: &str, part: &str) -> bool {
    match std::fs::create_dir_all(cache_root.join(subdir)) {
        Ok(()) => true,
        Err(e) => {
            warn!("{part} disabled for this build: {e}");
            false
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    const PYTHON_DEF: &str = "Bootstrap: docker\nFrom: peppybot/python-uv-base:latest\n\
        %post\n    export UV_PYTHON_INSTALL_DIR=/opt/uv-python\n    uv python install\n    \
        uv sync --no-editable --no-dev\n";

    const RUST_DEF: &str = "Bootstrap: docker\nFrom: peppybot/rust-cargo-base:latest\n\
        %post\n    cargo build --release\n";

    fn env_keys(cache: &ContainerBuildCache) -> Vec<&'static str> {
        cache.env.iter().map(|(key, _)| *key).collect()
    }

    #[test]
    fn cache_mount_detection_requires_a_known_image_or_explicit_mention() {
        assert!(def_provides_cache_mount(RUST_DEF));
        assert!(def_provides_cache_mount(PYTHON_DEF));
        assert!(def_provides_cache_mount(
            "From: myorg/custom\n# image ships /peppy-cache\n"
        ));
        assert!(!def_provides_cache_mount(
            "Bootstrap: docker\nFrom: ubuntu:24.04\n%post\n    cargo build\n"
        ));
    }

    #[test]
    fn conflicting_rust_build_configuration_is_detected() {
        let rust = CacheProfile::Rust {
            sccache_in_image: true,
        };
        assert_eq!(
            cache_conflict_marker(rust, "curl https://sh.rustup.rs | sh", &[]),
            Some("rustup")
        );
        for var in [
            "CARGO_HOME",
            "RUSTC_WRAPPER",
            "SCCACHE_DIR",
            "SCCACHE_SERVER_PORT",
        ] {
            assert_eq!(
                cache_conflict_marker(rust, &format!("%post\n    export {var}=/opt/custom\n"), &[]),
                Some(var),
                "def file setting {var} must disable the cache"
            );
        }
        assert_eq!(
            cache_conflict_marker(
                rust,
                "%post\n    cargo build --release\n",
                &["--no-setgroups".to_string()]
            ),
            None
        );
    }

    #[test]
    fn python_def_setting_a_uv_cache_variable_gets_no_cache() {
        for var in ["UV_CACHE_DIR", "UV_PYTHON_CACHE_DIR", "UV_LINK_MODE"] {
            let def = format!("{PYTHON_DEF}    export {var}=/opt/custom\n");
            assert_eq!(
                cache_profile_for(PeppygenLanguage::Python, &def, &[]),
                None,
                "def file setting {var} must disable the cache"
            );
            assert_eq!(
                cache_profile_for(
                    PeppygenLanguage::Python,
                    PYTHON_DEF,
                    &[format!("--env={var}=/opt/custom")]
                ),
                None,
                "extra build args setting {var} must disable the cache"
            );
        }
    }

    /// uv's `--link-mode` flag overrides `UV_LINK_MODE=copy`, and a symlink
    /// mode would point the image at the bind-mounted cache.
    #[test]
    fn python_def_choosing_its_own_link_mode_gets_no_cache() {
        for flag in ["--link-mode symlink", "--link-mode=symlink"] {
            let def = format!("{PYTHON_DEF}    uv sync --no-editable {flag}\n");
            assert_eq!(
                cache_profile_for(PeppygenLanguage::Python, &def, &[]),
                None,
                "a def that runs uv with {flag} must disable the cache"
            );
        }
    }

    #[test]
    fn python_def_that_installs_rustup_keeps_the_uv_cache() {
        let def = format!("{PYTHON_DEF}    curl https://sh.rustup.rs | sh -s -- -y\n");
        assert_eq!(
            cache_profile_for(PeppygenLanguage::Python, &def, &[]),
            Some(CacheProfile::Python)
        );
    }

    #[test]
    fn rust_def_that_mentions_a_uv_variable_keeps_the_cargo_cache() {
        let def = format!("{RUST_DEF}    export UV_CACHE_DIR=/opt/custom\n");
        assert_eq!(
            cache_profile_for(PeppygenLanguage::Rust, &def, &[]),
            Some(CacheProfile::Rust {
                sccache_in_image: true
            })
        );
    }

    #[test]
    fn python_def_on_an_image_without_the_mount_point_gets_no_cache() {
        let def = PYTHON_DEF.replace("peppybot/python-uv-base:latest", "python:3.12-slim");
        assert_eq!(cache_profile_for(PeppygenLanguage::Python, &def, &[]), None);
    }

    #[test]
    fn profile_follows_the_language_and_the_image() {
        assert_eq!(
            cache_profile_for(PeppygenLanguage::Python, PYTHON_DEF, &[]),
            Some(CacheProfile::Python)
        );
        assert_eq!(
            cache_profile_for(PeppygenLanguage::Rust, RUST_DEF, &[]),
            Some(CacheProfile::Rust {
                sccache_in_image: true
            })
        );
        assert_eq!(
            cache_profile_for(
                PeppygenLanguage::Rust,
                "From: myorg/custom\n# image ships /peppy-cache\n",
                &[]
            ),
            Some(CacheProfile::Rust {
                sccache_in_image: false
            })
        );
    }

    /// A variable a profile sets that a build could also set must be one of
    /// the profile's markers, or the build's own value and the cache would
    /// collide. `PEPPY_DOWNLOAD_CACHE` is the exception: defs read it by
    /// design and never set it.
    #[test]
    fn every_variable_a_profile_sets_is_one_of_its_markers() {
        for profile in [
            CacheProfile::Rust {
                sccache_in_image: true,
            },
            CacheProfile::Python,
        ] {
            let root = tempfile::tempdir().expect("create temp dir");
            let cache = prepare_in(root.path(), profile).expect("cache prepared");
            for key in env_keys(&cache) {
                assert!(
                    key == DOWNLOAD_CACHE_ENV_VAR || profile.conflict_markers().contains(&key),
                    "{profile:?} sets {key} but does not skip a build that sets it"
                );
            }
        }
    }

    #[test]
    fn rust_base_image_gets_registry_sccache_and_downloads() {
        let root = tempfile::tempdir().expect("create temp dir");
        let cache = prepare_in(
            root.path(),
            CacheProfile::Rust {
                sccache_in_image: true,
            },
        )
        .expect("cache prepared");

        assert!(root.path().join(CARGO_HOME_SUBDIR).is_dir());
        assert!(root.path().join(SCCACHE_CACHE_SUBDIR).is_dir());
        assert!(root.path().join(DOWNLOADS_SUBDIR).is_dir());
        assert!(!root.path().join(UV_CACHE_SUBDIR).exists());
        assert_eq!(
            env_keys(&cache),
            vec![
                "CARGO_HOME",
                "RUSTC_WRAPPER",
                "SCCACHE_DIR",
                "SCCACHE_SERVER_PORT",
                "PEPPY_DOWNLOAD_CACHE"
            ]
        );
        assert_eq!(cache.env[0].1, "/peppy-cache/cargo-home");
        assert_eq!(cache.env[1].1, "sccache");
        assert_eq!(cache.env[2].1, "/peppy-cache/sccache-cache");
        assert_eq!(cache.env[4].1, "/peppy-cache/downloads");
        assert_eq!(
            cache.summary,
            "Container build cache: cargo registry + sccache + downloads"
        );
    }

    #[test]
    fn image_without_sccache_gets_registry_and_downloads() {
        let root = tempfile::tempdir().expect("create temp dir");
        let cache = prepare_in(
            root.path(),
            CacheProfile::Rust {
                sccache_in_image: false,
            },
        )
        .expect("cache prepared");

        assert_eq!(
            cache.env,
            vec![
                ("CARGO_HOME", "/peppy-cache/cargo-home".to_string()),
                ("PEPPY_DOWNLOAD_CACHE", "/peppy-cache/downloads".to_string()),
            ]
        );
        assert!(!root.path().join(SCCACHE_CACHE_SUBDIR).exists());
        assert_eq!(
            cache.summary,
            "Container build cache: cargo registry + downloads"
        );
    }

    #[test]
    fn python_profile_gets_uv_packages_interpreters_and_downloads() {
        let root = tempfile::tempdir().expect("create temp dir");
        let cache = prepare_in(root.path(), CacheProfile::Python).expect("cache prepared");

        assert!(root.path().join(UV_CACHE_SUBDIR).is_dir());
        assert!(root.path().join(UV_PYTHON_SUBDIR).is_dir());
        assert!(root.path().join(DOWNLOADS_SUBDIR).is_dir());
        assert!(!root.path().join(CARGO_HOME_SUBDIR).exists());
        assert!(!root.path().join(SCCACHE_CACHE_SUBDIR).exists());
        assert_eq!(cache.host_dir, root.path());
        assert_eq!(
            cache.env,
            vec![
                ("UV_CACHE_DIR", "/peppy-cache/uv-cache".to_string()),
                ("UV_LINK_MODE", "copy".to_string()),
                ("UV_PYTHON_CACHE_DIR", "/peppy-cache/uv-python".to_string()),
                ("PEPPY_DOWNLOAD_CACHE", "/peppy-cache/downloads".to_string()),
            ]
        );
        assert_eq!(
            cache.summary,
            "Container build cache: uv packages + Python interpreters + downloads"
        );
    }

    #[test]
    fn unusable_downloads_dir_keeps_the_other_caches() {
        let root = tempfile::tempdir().expect("create temp dir");
        std::fs::write(root.path().join(DOWNLOADS_SUBDIR), b"not a directory")
            .expect("block the downloads dir with a file");

        let rust = prepare_in(
            root.path(),
            CacheProfile::Rust {
                sccache_in_image: true,
            },
        )
        .expect("cache prepared");
        assert!(
            !env_keys(&rust).contains(&DOWNLOAD_CACHE_ENV_VAR),
            "a build must not be told about a downloads dir that does not exist"
        );
        assert_eq!(
            rust.summary,
            "Container build cache: cargo registry + sccache"
        );

        let python = prepare_in(root.path(), CacheProfile::Python).expect("cache prepared");
        assert_eq!(
            env_keys(&python),
            vec!["UV_CACHE_DIR", "UV_LINK_MODE", "UV_PYTHON_CACHE_DIR"]
        );
        assert_eq!(
            python.summary,
            "Container build cache: uv packages + Python interpreters"
        );
    }

    #[test]
    fn unusable_uv_python_dir_keeps_the_other_caches() {
        let root = tempfile::tempdir().expect("create temp dir");
        std::fs::write(root.path().join(UV_PYTHON_SUBDIR), b"not a directory")
            .expect("block the uv-python dir with a file");

        let cache = prepare_in(root.path(), CacheProfile::Python).expect("cache prepared");

        assert_eq!(
            env_keys(&cache),
            vec!["UV_CACHE_DIR", "UV_LINK_MODE", "PEPPY_DOWNLOAD_CACHE"],
            "a build must not be told about an interpreter dir that does not exist"
        );
        assert_eq!(
            cache.summary,
            "Container build cache: uv packages + downloads"
        );
    }

    #[test]
    fn unusable_main_dir_gives_no_cache() {
        for (profile, main_subdir) in [
            (
                CacheProfile::Rust {
                    sccache_in_image: true,
                },
                CARGO_HOME_SUBDIR,
            ),
            (CacheProfile::Python, UV_CACHE_SUBDIR),
        ] {
            let root = tempfile::tempdir().expect("create temp dir");
            std::fs::write(root.path().join(main_subdir), b"not a directory")
                .expect("block the main dir with a file");
            assert!(
                prepare_in(root.path(), profile).is_none(),
                "{profile:?} without {main_subdir}/ must get no cache"
            );
        }
    }

    #[test]
    fn cache_path_with_bind_delimiter_disables_caching() {
        let root = tempfile::tempdir().expect("create temp dir");
        let with_colon = root.path().join("odd:dir");
        assert!(
            prepare_in(
                &with_colon,
                CacheProfile::Rust {
                    sccache_in_image: true
                }
            )
            .is_none()
        );
        assert!(prepare_in(&with_colon, CacheProfile::Python).is_none());
    }

    #[test]
    fn server_ports_stay_in_range_and_differ_across_builds() {
        let first = next_server_port();
        let second = next_server_port();
        assert!((24000..26000).contains(&first));
        assert!((24000..26000).contains(&second));
        assert_ne!(first, second);
    }
}
