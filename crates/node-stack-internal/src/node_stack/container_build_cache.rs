//! Persistent build caches for Rust container nodes.
//!
//! `apptainer build` compiles every Rust container node from scratch: the
//! `%post` scriptlet runs in an ephemeral sandbox, so the cargo registry and
//! all compiled artifacts are discarded with it. This module provisions a
//! host-side cache directory that is bind mounted into the build at
//! [`BIND_DEST`] and activated through `APPTAINERENV_*` variables, which
//! apptainer injects into the `%post` environment:
//!
//! * `cargo-home/` persists the crates.io registry between builds via
//!   `CARGO_HOME`. Toolchain discovery is unaffected: rustup binaries are
//!   found via `PATH` and `RUSTUP_HOME`, not `CARGO_HOME`.
//! * `sccache-cache/` persists compiled artifacts via `RUSTC_WRAPPER` and
//!   `SCCACHE_DIR`. The sccache executable ships inside the peppy Rust base
//!   image, so the wrapper is only activated for defs that bootstrap from it.
//!
//! Caching is best effort and fails open: when anything is missing or goes
//! wrong during setup, the build proceeds exactly as it would without this
//! module.

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
/// caching does not apply: non-Rust node, user opt-out, an image without the
/// mount point, a build whose configuration conflicts with the cache, or
/// cache directory setup failure.
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
    if !def_provides_cache_mount(def_contents) {
        tracing::debug!(
            "container build cache disabled: the def file's base image is not \
             known to contain {BIND_DEST} (create it in the image to opt in)"
        );
        return None;
    }
    if let Some(marker) = cache_conflict_marker(def_contents, apptainer_build_extra_args) {
        warn!(
            "container build cache disabled: the def file or \
             apptainer_build_extra_args mention `{marker}`, which the cache \
             environment overrides would interfere with"
        );
        return None;
    }
    prepare_in(
        &peppy_dirs.container_build_cache_dir(),
        def_contents.contains(SCCACHE_IMAGE),
    )
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

/// Returns the first marker showing the build's own configuration would
/// collide with the cache: a rustup install that the `CARGO_HOME` override
/// would misplace, or one of the environment variables the cache manages.
/// Plain substring matching errs toward skipping the cache.
fn cache_conflict_marker(
    def_contents: &str,
    apptainer_build_extra_args: &[String],
) -> Option<&'static str> {
    const CONFLICT_MARKERS: [&str; 5] = [
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
/// plus environment. `sccache_in_image` is whether the build's base image
/// ships the sccache executable.
fn prepare_in(cache_root: &Path, sccache_in_image: bool) -> Option<ContainerBuildCache> {
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

    if sccache_in_image {
        match std::fs::create_dir_all(cache_root.join(SCCACHE_CACHE_SUBDIR)) {
            Ok(()) => {
                env.push(("RUSTC_WRAPPER", "sccache".to_string()));
                env.push(("SCCACHE_DIR", format!("{BIND_DEST}/{SCCACHE_CACHE_SUBDIR}")));
                // Concurrent builds share the host network namespace. A unique
                // port per build keeps each sccache server paired with the
                // build that started it; servers die with the build's PID
                // namespace, so ports free up immediately.
                env.push(("SCCACHE_SERVER_PORT", next_server_port().to_string()));
                parts.push("sccache");
            }
            Err(e) => warn!("sccache disabled for this build: {e}"),
        }
    }

    Some(ContainerBuildCache {
        host_dir: cache_root.to_path_buf(),
        env,
        summary: format!("Container build cache: {}", parts.join(" + ")),
    })
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

    #[test]
    fn cache_mount_detection_requires_a_known_image_or_explicit_mention() {
        assert!(def_provides_cache_mount(
            "Bootstrap: docker\nFrom: peppybot/rust-cargo-base:latest\n"
        ));
        assert!(def_provides_cache_mount(
            "Bootstrap: docker\nFrom: peppybot/python-uv-base:latest\n"
        ));
        assert!(def_provides_cache_mount(
            "From: myorg/custom\n# image ships /peppy-cache\n"
        ));
        assert!(!def_provides_cache_mount(
            "Bootstrap: docker\nFrom: ubuntu:24.04\n%post\n    cargo build\n"
        ));
    }

    #[test]
    fn conflicting_build_configuration_is_detected() {
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
    fn rust_base_image_gets_registry_and_sccache() {
        let root = tempfile::tempdir().expect("create temp dir");
        let cache = prepare_in(root.path(), true).expect("cache prepared");

        assert!(root.path().join(CARGO_HOME_SUBDIR).is_dir());
        assert!(root.path().join(SCCACHE_CACHE_SUBDIR).is_dir());
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
        assert_eq!(cache.env[1].1, "sccache");
        assert_eq!(
            cache.summary,
            "Container build cache: cargo registry + sccache"
        );
    }

    #[test]
    fn image_without_sccache_gets_registry_only() {
        let root = tempfile::tempdir().expect("create temp dir");
        let cache = prepare_in(root.path(), false).expect("cache prepared");

        assert_eq!(
            cache.env,
            vec![("CARGO_HOME", format!("{BIND_DEST}/{CARGO_HOME_SUBDIR}"))]
        );
        assert_eq!(cache.summary, "Container build cache: cargo registry");
    }

    #[test]
    fn cache_path_with_bind_delimiter_disables_caching() {
        let root = tempfile::tempdir().expect("create temp dir");
        let with_colon = root.path().join("odd:dir");
        assert!(prepare_in(&with_colon, true).is_none());
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
