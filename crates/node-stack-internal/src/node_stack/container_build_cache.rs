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
//!   `nodes/<name>_<tag>/` is the node's own directory, via
//!   `PEPPY_NODE_BUILD_CACHE`: whatever a def keeps there for its next build,
//!   such as a part of the image it builds again only when its inputs change.
//!
//! Caching is best effort and fails open: when the main directory of a
//! profile cannot be set up, the build proceeds exactly as it would without
//! this module, and when an optional part cannot, the build gets the other
//! parts.
//!
//! `uv-cache/` also collects what no later build can use. A build stages the
//! local projects of the node again (the node itself, and
//! `.peppy/libs/peppylib` with its native extension), so uv builds them
//! again and keeps every earlier build of them. Each Python build that uses
//! the cache and succeeds therefore prunes the cache (see
//! [`super::build_steps::prune_uv_cache`]), unless another Python build of
//! this process uses it (see [`UvCacheUse`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use config::node::PeppygenLanguage;
use daemon_config::consts::PeppyDirs;
use parking_lot::Mutex;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
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
const NODE_CACHES_SUBDIR: &str = "nodes";

/// Names the downloads directory inside the build. Its presence is what tells
/// a `%post` that the cache is bind mounted: without the bind, `/peppy-cache`
/// is a plain directory of the image and whatever a build writes there ships
/// in the image.
const DOWNLOAD_CACHE_ENV_VAR: &str = "PEPPY_DOWNLOAD_CACHE";

/// Names the directory of the node being built inside the build, which no
/// build of another node uses. Like [`DOWNLOAD_CACHE_ENV_VAR`], it is set
/// only while the cache is bind mounted.
const NODE_BUILD_CACHE_ENV_VAR: &str = "PEPPY_NODE_BUILD_CACHE";

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
    profile: CacheProfile,
}

impl ContainerBuildCache {
    /// Whether the build fills `uv-cache/`, which the build step then prunes
    /// once the build has written its image.
    pub(super) fn uses_uv_cache(&self) -> bool {
        self.profile == CacheProfile::Python
    }
}

/// Prepares the shared build cache for a container build, or `None` when
/// caching does not apply: user opt-out, an image without the mount point, a
/// build whose configuration conflicts with the cache, or cache directory
/// setup failure.
pub(super) fn prepare(
    peppy_dirs: &PeppyDirs,
    node: &NodeIdentity<'_>,
    language: PeppygenLanguage,
    def_contents: &str,
    apptainer_build_extra_args: &[String],
) -> Option<ContainerBuildCache> {
    if std::env::var_os(NO_CONTAINER_BUILD_CACHE_ENV_VAR).is_some() {
        return None;
    }
    let profile = cache_profile_for(language, def_contents, apptainer_build_extra_args)?;
    prepare_in(&peppy_dirs.container_build_cache_dir(), node, profile)
}

/// The node a container build builds, which names its directory of the
/// cache. The caller has validated the tag (see
/// [`super::build_steps::validate_node_tag`]).
pub(super) struct NodeIdentity<'a> {
    pub name: &'a str,
    pub tag: &'a str,
}

impl NodeIdentity<'_> {
    /// `nodes/<name>_<tag>`, the node's directory under the cache root, named
    /// like its directory of built artifacts.
    fn cache_subdir(&self) -> String {
        format!("{NODE_CACHES_SUBDIR}/{}_{}", self.name, self.tag)
    }
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

/// Testable core of [`prepare`]: lays out `cache_root` for a build of `node`
/// with `profile` and derives the bind plus environment. The main directory
/// of the profile (`cargo-home/` or `uv-cache/`) is required; each other part
/// is optional, and a build that cannot have one gets the rest.
fn prepare_in(
    cache_root: &Path,
    node: &NodeIdentity<'_>,
    profile: CacheProfile,
) -> Option<ContainerBuildCache> {
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

    let mut env = vec![(main_env_var, in_build(main_subdir))];
    let mut parts = vec![main_part];
    match profile {
        CacheProfile::Rust { sccache_in_image } => {
            if sccache_in_image && create_optional_part(cache_root, SCCACHE_CACHE_SUBDIR, "sccache")
            {
                env.push(("RUSTC_WRAPPER", "sccache".to_string()));
                env.push(("SCCACHE_DIR", in_build(SCCACHE_CACHE_SUBDIR)));
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
                env.push(("UV_PYTHON_CACHE_DIR", in_build(UV_PYTHON_SUBDIR)));
                parts.push("Python interpreters");
            }
        }
    }

    if create_optional_part(cache_root, DOWNLOADS_SUBDIR, "download cache") {
        env.push((DOWNLOAD_CACHE_ENV_VAR, in_build(DOWNLOADS_SUBDIR)));
        parts.push("downloads");
    }

    let node_subdir = node.cache_subdir();
    if create_optional_part(cache_root, &node_subdir, "node build cache") {
        env.push((NODE_BUILD_CACHE_ENV_VAR, in_build(&node_subdir)));
        parts.push("node cache");
    }

    Some(ContainerBuildCache {
        host_dir: cache_root.to_path_buf(),
        env,
        summary: cache_line(&parts.join(" + ")),
        profile,
    })
}

/// A line of build output about the cache. Every such line starts the same
/// way.
fn cache_line(what: &str) -> String {
    format!("Container build cache: {what}")
}

/// The path of the cache directory `subdir` inside the build.
fn in_build(subdir: &str) -> String {
    format!("{BIND_DEST}/{subdir}")
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

/// The gate of the uv cache under `cache_root`, shared by every build of
/// this process that uses that cache: each build holds it shared, and a
/// prune holds it alone. It is keyed by the directory it guards, so builds
/// under other cache roots (another daemon root, another test) never wait
/// for each other.
fn uv_cache_gate(cache_root: &Path) -> Arc<RwLock<()>> {
    static GATES: LazyLock<Mutex<HashMap<PathBuf, Arc<RwLock<()>>>>> =
        LazyLock::new(Mutex::default);
    Arc::clone(GATES.lock().entry(cache_root.to_path_buf()).or_default())
}

/// The use of `uv-cache/` by one Python build of this process, held from
/// before its `apptainer build` starts until the build ends.
///
/// From uv 0.8.20 on, a uv process holds a shared lock on its cache while it
/// uses it, and a prune needs that lock alone, so a prune does not run while
/// such a process uses the cache (see [`uv_cache_prune_command`]). An image
/// can ship an older uv, which uses the cache without that lock: a prune can
/// then delete an archive that its `uv sync` has unpacked but not yet
/// recorded, and so fail its build. This gate keeps the builds of this
/// process clear of each other's prune whatever their uv: a build prunes
/// only when no other build uses the cache, and a build that starts while a
/// prune runs waits for it.
pub(super) struct UvCacheUse {
    cache_root: PathBuf,
    gate: Arc<RwLock<()>>,
    shared: OwnedRwLockReadGuard<()>,
}

impl UvCacheUse {
    /// Starts a use of the uv cache under `cache_root`, once the prune that
    /// runs now, if any, has ended.
    pub(super) async fn begin(cache_root: &Path) -> Self {
        let gate = uv_cache_gate(cache_root);
        let shared = Arc::clone(&gate).read_owned().await;
        Self {
            cache_root: cache_root.to_path_buf(),
            gate,
            shared,
        }
    }

    /// Starts a use of the uv cache under `cache_root` at once, or returns
    /// `None` while a prune runs.
    pub(super) fn try_begin(cache_root: &Path) -> Option<Self> {
        let gate = uv_cache_gate(cache_root);
        let shared = Arc::clone(&gate).try_read_owned().ok()?;
        Some(Self {
            cache_root: cache_root.to_path_buf(),
            gate,
            shared,
        })
    }

    /// Ends this use. Returns the sole use of the cache, which a prune needs,
    /// when no other build of this process uses the cache, and `None` when
    /// one does: the prune is then left to a later build.
    pub(super) fn end(self) -> Option<SoleUvCacheUse> {
        let Self {
            cache_root,
            gate,
            shared,
        } = self;
        drop(shared);
        let sole = gate.try_write_owned().ok()?;
        Some(SoleUvCacheUse {
            cache_root,
            _sole: sole,
        })
    }
}

/// The sole use of the uv cache under `cache_root`, which a prune holds for
/// as long as it runs: no other build of this process uses the cache
/// meanwhile.
pub(super) struct SoleUvCacheUse {
    cache_root: PathBuf,
    _sole: OwnedRwLockWriteGuard<()>,
}

impl SoleUvCacheUse {
    pub(super) fn cache_root(&self) -> &Path {
        &self.cache_root
    }
}

/// The line of build output for a build that waits while another build
/// prunes the uv cache.
pub(super) fn prune_wait_line() -> String {
    cache_line("waiting for another build to prune the uv cache")
}

/// The line of build output for a prune that starts.
pub(super) fn prune_start_line() -> String {
    cache_line("pruning the uv cache")
}

/// What the prune script prints when the image has no uv that it runs.
const NO_UV_IN_IMAGE: &str = "no uv command outside a virtual environment";

/// The command that prunes the uv cache inside the image a build wrote: a
/// `sh` script, as the uv to run is not always the first `uv` on `PATH`.
///
/// * The `%environment` of a def often puts the virtual environment of the
///   node first on `PATH`, which `%post` does not see. When that environment
///   holds a `uv` package, the first `uv` on `PATH` is not the uv of the
///   build, and a uv with another cache layout removes the entries of the
///   build's uv. The script runs the first `uv` file on `PATH` that is not in
///   a virtual environment, which has a `pyvenv.cfg` above its `bin`
///   directory.
/// * `env -i` and `--no-config` keep every `UV_*` variable and uv
///   configuration file of the image out of the prune: with `UV_NO_CACHE`
///   or a `no-cache` setting, for example, uv prunes a temporary directory.
///   The cache directory is an argument. From uv 0.9.16 on,
///   `UV_LOCK_TIMEOUT=0` makes the prune stop at once, instead of waiting,
///   when a uv process outside this daemon holds the lock of the cache; an
///   older uv waits until [`UV_CACHE_PRUNE_TIMEOUT`] stops it.
pub(super) fn uv_cache_prune_command() -> [String; 5] {
    let script = format!(
        r#"set -f
IFS=:
for dir in $PATH; do
    [ -f "$dir/../pyvenv.cfg" ] && continue
    [ -f "$dir/uv" ] && [ -x "$dir/uv" ] || continue
    exec env -i UV_LOCK_TIMEOUT=0 "$dir/uv" --no-config cache prune --cache-dir "$1"
done
echo '{NO_UV_IN_IMAGE}' >&2
exit 127"#
    );
    [
        "sh".to_string(),
        "-c".to_string(),
        script,
        "sh".to_string(),
        in_build(UV_CACHE_SUBDIR),
    ]
}

/// The time a prune has. A prune ends in seconds, and a prune that hangs must
/// not hold up the build: the build idle clock sees no output from it. A
/// prune stopped part way is safe, and the next prune continues it.
pub(super) const UV_CACHE_PRUNE_TIMEOUT: Duration = Duration::from_secs(30);

/// The time a prune has to stop after a SIGTERM, which lets apptainer
/// unmount the image and remove a temporary sandbox before a SIGKILL.
pub(super) const UV_CACHE_PRUNE_STOP_GRACE: Duration = Duration::from_secs(5);

/// What the prune of the uv cache after a Python build did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum UvCachePruneOutcome {
    /// uv pruned the cache. `summary` is the last line it printed, such as
    /// `Removed 15 files (6.8MiB)` or `No unused entries found`, if any.
    Pruned { summary: Option<String> },
    /// Another Python build of this process uses the cache.
    InUseByAnotherBuild,
    /// A uv process outside this daemon holds the lock of the cache, and the
    /// uv of the image (0.9.16 on) does not wait for it.
    LockedByAnotherUv,
    /// The image the build wrote has no `uv` command outside a virtual
    /// environment.
    NoUvInImage,
    /// The prune did not end within [`UV_CACHE_PRUNE_TIMEOUT`].
    TimedOut,
    /// Any other failure, with its reason.
    Failed { reason: String },
}

impl UvCachePruneOutcome {
    /// Parses how the command of [`uv_cache_prune_command`] ended inside the
    /// image. uv, and the script when it finds no uv, write all of their
    /// output to stderr, so the tail of stderr holds the summary of a prune
    /// as well as the reason a prune failed. Apptainer writes its own notes
    /// there too, also after uv's last line (see [`is_apptainer_note`]), and
    /// they are neither. A prune of a directory other than the cache of the
    /// build, which uv names in its first line, is a failure.
    pub(super) fn of_run(status: ExitStatus, stderr_tail: &[String]) -> Self {
        let last_line = stderr_tail
            .iter()
            .rev()
            .map(|line| line.trim())
            .find(|line| !line.is_empty() && !is_apptainer_note(line));
        if status.success() {
            let cache_dir = in_build(UV_CACHE_SUBDIR);
            // uv up to 0.4 names the directory relative to the working
            // directory of the prune, `/`.
            let is_cache_dir = |dir: &&str| Path::new("/").join(dir) == Path::new(&cache_dir);
            if let Some(pruned_dir) = pruned_dir(stderr_tail).filter(|dir| !is_cache_dir(dir)) {
                return Self::Failed {
                    reason: format!("uv pruned {pruned_dir}, not {cache_dir}"),
                };
            }
            return Self::Pruned {
                summary: last_line.map(str::to_string),
            };
        }
        let mentions = |text: &str| stderr_tail.iter().any(|line| line.contains(text));
        if mentions("when waiting for lock") {
            return Self::LockedByAnotherUv;
        }
        if mentions(NO_UV_IN_IMAGE) {
            return Self::NoUvInImage;
        }
        Self::Failed {
            reason: last_line.map_or_else(|| status.to_string(), str::to_string),
        }
    }

    /// The line of build output that reports this outcome.
    pub(super) fn line(&self) -> String {
        let what = match self {
            Self::Pruned { summary: None } => "uv cache pruned".to_string(),
            Self::Pruned {
                summary: Some(summary),
            } => format!("uv cache pruned: {summary}"),
            Self::InUseByAnotherBuild => {
                "uv cache not pruned, because another build uses it".to_string()
            }
            Self::LockedByAnotherUv => {
                "uv cache not pruned, because another uv process uses it".to_string()
            }
            Self::NoUvInImage => {
                format!("uv cache not pruned, because the image has {NO_UV_IN_IMAGE}")
            }
            Self::TimedOut => format!(
                "uv cache not pruned: the prune did not end in {} s",
                UV_CACHE_PRUNE_TIMEOUT.as_secs()
            ),
            Self::Failed { reason } => format!("uv cache not pruned: {reason}"),
        };
        cache_line(&what)
    }
}

/// The directory that uv says it prunes: `Pruning cache at: <dir>`, or
/// `No cache found at: <dir>` when the directory does not exist.
fn pruned_dir(stderr_tail: &[String]) -> Option<&str> {
    stderr_tail.iter().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("Pruning cache at: ")
            .or_else(|| line.strip_prefix("No cache found at: "))
    })
}

/// Whether `line` is a note of apptainer itself, such as the
/// `INFO:    Cleaning up image...` it prints after the command when it runs
/// the image from a temporary sandbox. Its errors (`ERROR:`, `FATAL:`) are
/// not notes: they can be the reason a prune failed.
fn is_apptainer_note(line: &str) -> bool {
    ["INFO:", "WARNING:", "VERBOSE:", "DEBUG:"]
        .iter()
        .any(|level| line.starts_with(level))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PYTHON_DEF: &str = "Bootstrap: docker\nFrom: peppybot/python-uv-base:latest\n\
        %post\n    export UV_PYTHON_INSTALL_DIR=/opt/uv-python\n    uv python install\n    \
        uv sync --no-editable --no-dev\n";

    const RUST_DEF: &str = "Bootstrap: docker\nFrom: peppybot/rust-cargo-base:latest\n\
        %post\n    cargo build --release\n";

    const NODE: NodeIdentity<'static> = NodeIdentity {
        name: "node",
        tag: "v1",
    };

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
    /// collide. `PEPPY_DOWNLOAD_CACHE` and `PEPPY_NODE_BUILD_CACHE` are the
    /// exceptions: defs read them by design and never set them.
    #[test]
    fn every_variable_a_profile_sets_is_one_of_its_markers() {
        for profile in [
            CacheProfile::Rust {
                sccache_in_image: true,
            },
            CacheProfile::Python,
        ] {
            let root = tempfile::tempdir().expect("create temp dir");
            let cache = prepare_in(root.path(), &NODE, profile).expect("cache prepared");
            for key in env_keys(&cache) {
                assert!(
                    key == DOWNLOAD_CACHE_ENV_VAR
                        || key == NODE_BUILD_CACHE_ENV_VAR
                        || profile.conflict_markers().contains(&key),
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
            &NODE,
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
                "PEPPY_DOWNLOAD_CACHE",
                "PEPPY_NODE_BUILD_CACHE",
            ]
        );
        assert_eq!(cache.env[0].1, "/peppy-cache/cargo-home");
        assert_eq!(cache.env[1].1, "sccache");
        assert_eq!(cache.env[2].1, "/peppy-cache/sccache-cache");
        assert_eq!(cache.env[4].1, "/peppy-cache/downloads");
        assert_eq!(cache.env[5].1, "/peppy-cache/nodes/node_v1");
        assert!(root.path().join("nodes/node_v1").is_dir());
        assert_eq!(
            cache.summary,
            "Container build cache: cargo registry + sccache + downloads + node cache"
        );
    }

    #[test]
    fn image_without_sccache_gets_registry_and_downloads() {
        let root = tempfile::tempdir().expect("create temp dir");
        let cache = prepare_in(
            root.path(),
            &NODE,
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
                (
                    "PEPPY_NODE_BUILD_CACHE",
                    "/peppy-cache/nodes/node_v1".to_string()
                ),
            ]
        );
        assert!(!root.path().join(SCCACHE_CACHE_SUBDIR).exists());
        assert_eq!(
            cache.summary,
            "Container build cache: cargo registry + downloads + node cache"
        );
    }

    #[test]
    fn python_profile_gets_uv_packages_interpreters_and_downloads() {
        let root = tempfile::tempdir().expect("create temp dir");
        let cache = prepare_in(root.path(), &NODE, CacheProfile::Python).expect("cache prepared");

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
                (
                    "PEPPY_NODE_BUILD_CACHE",
                    "/peppy-cache/nodes/node_v1".to_string()
                ),
            ]
        );
        assert_eq!(
            cache.summary,
            "Container build cache: uv packages + Python interpreters + downloads + node cache"
        );
    }

    #[test]
    fn unusable_downloads_dir_keeps_the_other_caches() {
        let root = tempfile::tempdir().expect("create temp dir");
        std::fs::write(root.path().join(DOWNLOADS_SUBDIR), b"not a directory")
            .expect("block the downloads dir with a file");

        let rust = prepare_in(
            root.path(),
            &NODE,
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
            "Container build cache: cargo registry + sccache + node cache"
        );

        let python = prepare_in(root.path(), &NODE, CacheProfile::Python).expect("cache prepared");
        assert_eq!(
            env_keys(&python),
            vec![
                "UV_CACHE_DIR",
                "UV_LINK_MODE",
                "UV_PYTHON_CACHE_DIR",
                "PEPPY_NODE_BUILD_CACHE"
            ]
        );
        assert_eq!(
            python.summary,
            "Container build cache: uv packages + Python interpreters + node cache"
        );
    }

    #[test]
    fn unusable_uv_python_dir_keeps_the_other_caches() {
        let root = tempfile::tempdir().expect("create temp dir");
        std::fs::write(root.path().join(UV_PYTHON_SUBDIR), b"not a directory")
            .expect("block the uv-python dir with a file");

        let cache = prepare_in(root.path(), &NODE, CacheProfile::Python).expect("cache prepared");

        assert_eq!(
            env_keys(&cache),
            vec![
                "UV_CACHE_DIR",
                "UV_LINK_MODE",
                "PEPPY_DOWNLOAD_CACHE",
                "PEPPY_NODE_BUILD_CACHE"
            ],
            "a build must not be told about an interpreter dir that does not exist"
        );
        assert_eq!(
            cache.summary,
            "Container build cache: uv packages + downloads + node cache"
        );
    }

    #[test]
    fn unusable_node_dir_keeps_the_other_caches() {
        let root = tempfile::tempdir().expect("create temp dir");
        std::fs::write(root.path().join(NODE_CACHES_SUBDIR), b"not a directory")
            .expect("block the node caches dir with a file");

        let cache = prepare_in(
            root.path(),
            &NODE,
            CacheProfile::Rust {
                sccache_in_image: true,
            },
        )
        .expect("cache prepared");

        assert!(
            !env_keys(&cache).contains(&NODE_BUILD_CACHE_ENV_VAR),
            "a build must not be told about a node dir that does not exist"
        );
        assert_eq!(
            cache.summary,
            "Container build cache: cargo registry + sccache + downloads"
        );
    }

    #[test]
    fn each_node_identity_gets_its_own_directory() {
        let root = tempfile::tempdir().expect("create temp dir");
        let node_dir_of = |name, tag| {
            let cache = prepare_in(
                root.path(),
                &NodeIdentity { name, tag },
                CacheProfile::Python,
            )
            .expect("cache prepared");
            cache
                .env
                .into_iter()
                .find(|(key, _)| *key == NODE_BUILD_CACHE_ENV_VAR)
                .map(|(_, dir)| dir)
                .expect("the node dir is set")
        };

        assert_eq!(node_dir_of("waldo", "v1"), "/peppy-cache/nodes/waldo_v1");
        assert_eq!(node_dir_of("waldo", "v2"), "/peppy-cache/nodes/waldo_v2");
        assert_eq!(node_dir_of("camera", "v1"), "/peppy-cache/nodes/camera_v1");
        assert!(root.path().join("nodes/waldo_v1").is_dir());
        assert!(root.path().join("nodes/waldo_v2").is_dir());
        assert!(root.path().join("nodes/camera_v1").is_dir());
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
                prepare_in(root.path(), &NODE, profile).is_none(),
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
                &NODE,
                CacheProfile::Rust {
                    sccache_in_image: true
                }
            )
            .is_none()
        );
        assert!(prepare_in(&with_colon, &NODE, CacheProfile::Python).is_none());
    }

    #[test]
    fn server_ports_stay_in_range_and_differ_across_builds() {
        let first = next_server_port();
        let second = next_server_port();
        assert!((24000..26000).contains(&first));
        assert!((24000..26000).contains(&second));
        assert_ne!(first, second);
    }

    #[test]
    fn only_a_python_build_uses_the_uv_cache() {
        for (profile, uses_uv_cache) in [
            (CacheProfile::Python, true),
            (
                CacheProfile::Rust {
                    sccache_in_image: true,
                },
                false,
            ),
            (
                CacheProfile::Rust {
                    sccache_in_image: false,
                },
                false,
            ),
        ] {
            let root = tempfile::tempdir().expect("create temp dir");
            let cache = prepare_in(root.path(), &NODE, profile).expect("cache prepared");
            assert_eq!(cache.uses_uv_cache(), uses_uv_cache, "{profile:?}");
        }
    }

    /// The prune must act on the directory the build pointed uv at.
    #[test]
    fn the_prune_command_names_the_uv_cache_of_the_build() {
        let root = tempfile::tempdir().expect("create temp dir");
        let cache = prepare_in(root.path(), &NODE, CacheProfile::Python).expect("cache prepared");
        let uv_cache_dir = &cache
            .env
            .iter()
            .find(|(key, _)| *key == "UV_CACHE_DIR")
            .expect("the Python profile sets UV_CACHE_DIR")
            .1;
        let command = uv_cache_prune_command();
        assert_eq!(command[0], "sh");
        assert_eq!(&command[4], uv_cache_dir);
    }

    /// Runs the prune command of the uv cache outside any container, with
    /// `path` as its whole `PATH` and `env` as its other variables. Returns
    /// its exit code, stdout and stderr.
    fn run_prune_command(path: &[&Path], env: &[(&str, &str)]) -> (Option<i32>, String, String) {
        let command = uv_cache_prune_command();
        let output = std::process::Command::new("/bin/sh")
            .args(&command[1..])
            .env_clear()
            .env("PATH", std::env::join_paths(path).expect("join the PATH"))
            .envs(env.iter().copied())
            .output()
            .expect("run the prune command");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Directories for [`run_prune_command`]: the `bin` directory of a
    /// virtual environment and a plain directory, each with a `uv`, a
    /// directory whose `uv` is a directory, and a directory with the `env`
    /// the command runs uv through. Each `uv` file is a
    /// link to a file of the repository: a test that runs a file it has just
    /// written can fail with `ETXTBSY` (see
    /// `run_build_cmd_resolves_the_program_via_the_child_path`).
    struct PruneCommandDirs {
        _root: tempfile::TempDir,
        venv_bin: PathBuf,
        uv_is_a_dir: PathBuf,
        plain: PathBuf,
        tools: PathBuf,
    }

    fn prune_command_dirs() -> PruneCommandDirs {
        use std::os::unix::fs::symlink;

        let fake_uv = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("fake_uv.sh");
        let root = tempfile::tempdir().expect("create temp dir");
        let venv_bin = root.path().join("venv").join("bin");
        let uv_is_a_dir = root.path().join("uv-is-a-dir");
        let plain = root.path().join("plain");
        let tools = root.path().join("tools");
        for dir in [&venv_bin, &uv_is_a_dir.join("uv"), &plain, &tools] {
            std::fs::create_dir_all(dir).expect("create a PATH directory");
        }
        std::fs::write(
            root.path().join("venv").join("pyvenv.cfg"),
            "home = /usr/bin\n",
        )
        .expect("mark the virtual environment");
        symlink(&fake_uv, venv_bin.join("uv")).expect("link the uv of the venv");
        symlink(&fake_uv, plain.join("uv")).expect("link the plain uv");
        symlink("/usr/bin/env", tools.join("env")).expect("link env");
        PruneCommandDirs {
            _root: root,
            venv_bin,
            uv_is_a_dir,
            plain,
            tools,
        }
    }

    /// A venv of the node first on `PATH` (as a `%environment` puts it), a
    /// directory named `uv`, and uv variables of the image must not change
    /// which uv prunes, or what it prunes.
    #[test]
    fn the_prune_runs_the_first_uv_outside_a_virtual_environment_with_no_uv_variable() {
        let dirs = prune_command_dirs();
        let (code, stdout, stderr) = run_prune_command(
            &[&dirs.venv_bin, &dirs.uv_is_a_dir, &dirs.plain, &dirs.tools],
            &[
                ("UV_NO_CACHE", "1"),
                ("UV_CONFIG_FILE", "/etc/uv/uv.toml"),
                ("UV_LOCK_TIMEOUT", "300"),
            ],
        );
        assert_eq!(code, Some(0), "stderr: {stderr}");
        assert_eq!(
            stdout,
            format!(
                "ran: {}/uv --no-config cache prune --cache-dir /peppy-cache/uv-cache\n\
                 UV_LOCK_TIMEOUT=0 UV_NO_CACHE=unset UV_CONFIG_FILE=unset\n",
                dirs.plain.display()
            )
        );
    }

    #[test]
    fn the_prune_reports_an_image_with_no_uv_outside_a_virtual_environment() {
        let dirs = prune_command_dirs();
        let (code, stdout, stderr) = run_prune_command(&[&dirs.venv_bin, &dirs.tools], &[]);
        assert_eq!(code, Some(127));
        assert_eq!(stdout, "", "no uv may run");
        let stderr_tail = lines(&stderr.lines().collect::<Vec<_>>());
        assert_eq!(
            UvCachePruneOutcome::of_run(exit_status(127), &stderr_tail),
            UvCachePruneOutcome::NoUvInImage
        );
    }

    /// Whether `future` is still pending when polled once.
    fn is_pending<F: Future>(future: std::pin::Pin<&mut F>) -> bool {
        future
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    }

    #[tokio::test]
    async fn a_build_alone_gets_the_sole_use_of_the_uv_cache_when_it_ends() {
        let root = tempfile::tempdir().expect("create temp dir");
        let build = UvCacheUse::begin(root.path()).await;
        let sole = build.end().expect("a build alone gets the sole use");
        assert_eq!(sole.cache_root(), root.path());
    }

    #[tokio::test]
    async fn trying_to_use_the_uv_cache_fails_only_while_a_prune_runs() {
        let root = tempfile::tempdir().expect("create temp dir");
        let first = UvCacheUse::try_begin(root.path()).expect("no prune runs");
        let second = UvCacheUse::try_begin(root.path()).expect("builds share the cache");
        drop(second);
        let prune = first.end().expect("a build alone gets the sole use");
        assert!(
            UvCacheUse::try_begin(root.path()).is_none(),
            "a build must not start to use the cache while a prune runs"
        );
        drop(prune);
        assert!(UvCacheUse::try_begin(root.path()).is_some());
    }

    #[tokio::test]
    async fn a_build_leaves_the_prune_to_another_build_that_uses_the_uv_cache() {
        let root = tempfile::tempdir().expect("create temp dir");
        let first = UvCacheUse::begin(root.path()).await;
        let second = UvCacheUse::begin(root.path()).await;
        assert!(
            first.end().is_none(),
            "a build must not prune while another build uses the cache"
        );
        assert!(
            second.end().is_some(),
            "the last build to end must get the sole use"
        );
    }

    #[tokio::test]
    async fn a_build_starts_to_use_the_uv_cache_only_after_the_running_prune() {
        let root = tempfile::tempdir().expect("create temp dir");
        let prune = UvCacheUse::begin(root.path())
            .await
            .end()
            .expect("a build alone gets the sole use");

        let mut next = std::pin::pin!(UvCacheUse::begin(root.path()));
        assert!(
            is_pending(next.as_mut()),
            "a build must wait while a prune runs"
        );
        drop(prune);
        let next = next.await;
        assert!(next.end().is_some());
    }

    #[tokio::test]
    async fn builds_under_another_cache_root_do_not_hold_the_prune_off() {
        let root = tempfile::tempdir().expect("create temp dir");
        let other_root = tempfile::tempdir().expect("create temp dir");
        let _other = UvCacheUse::begin(other_root.path()).await;
        assert!(UvCacheUse::begin(root.path()).await.end().is_some());
    }

    fn exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    fn lines(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    /// The stderr shapes are those of uv 0.12.1 and apptainer 1.5.2.
    #[test]
    fn prune_outcome_follows_how_the_prune_ended() {
        let cases = [
            (
                0,
                lines(&[
                    "Pruning cache at: /peppy-cache/uv-cache",
                    "Removed 15 files (6.8MiB)",
                ]),
                UvCachePruneOutcome::Pruned {
                    summary: Some("Removed 15 files (6.8MiB)".to_string()),
                },
            ),
            (
                0,
                lines(&[
                    "Pruning cache at: /peppy-cache/uv-cache",
                    "No unused entries found",
                ]),
                UvCachePruneOutcome::Pruned {
                    summary: Some("No unused entries found".to_string()),
                },
            ),
            (0, Vec::new(), UvCachePruneOutcome::Pruned { summary: None }),
            // Apptainer runs the image from a temporary sandbox when it has
            // no FUSE driver for the image, or when `--unsquash` or
            // `APPTAINER_UNSQUASH` asks for it.
            (
                0,
                lines(&[
                    "INFO:    Converting SIF file to temporary sandbox...",
                    "Pruning cache at: /peppy-cache/uv-cache",
                    "Removed 1 file",
                    "INFO:    Cleaning up image...",
                ]),
                UvCachePruneOutcome::Pruned {
                    summary: Some("Removed 1 file".to_string()),
                },
            ),
            (
                2,
                lines(&[
                    "INFO:    Converting SIF file to temporary sandbox...",
                    "error: Permission denied (os error 13)",
                    "INFO:    Cleaning up image...",
                ]),
                UvCachePruneOutcome::Failed {
                    reason: "error: Permission denied (os error 13)".to_string(),
                },
            ),
            (
                255,
                lines(&["FATAL:   could not open image /work/node.sif: no such file"]),
                UvCachePruneOutcome::Failed {
                    reason: "FATAL:   could not open image /work/node.sif: no such file"
                        .to_string(),
                },
            ),
            (
                2,
                lines(&[
                    "Cache is currently in-use, waiting for other uv processes to finish \
                     (use `--force` to override)",
                    "error: Timeout (0s) when waiting for lock on `/peppy-cache/uv-cache` at \
                     `/peppy-cache/uv-cache/.lock`, is another uv process running? You can set \
                     `UV_LOCK_TIMEOUT` to increase the timeout.",
                ]),
                UvCachePruneOutcome::LockedByAnotherUv,
            ),
            // A setting of the image that sends uv elsewhere.
            (
                0,
                lines(&[
                    "Pruning cache at: /tmp/.tmpCk8DbE",
                    "No unused entries found",
                ]),
                UvCachePruneOutcome::Failed {
                    reason: "uv pruned /tmp/.tmpCk8DbE, not /peppy-cache/uv-cache".to_string(),
                },
            ),
            // uv up to 0.4 names the directory relative to `/`.
            (
                0,
                lines(&[
                    "Pruning cache at: peppy-cache/uv-cache",
                    "Removed 1 directory",
                ]),
                UvCachePruneOutcome::Pruned {
                    summary: Some("Removed 1 directory".to_string()),
                },
            ),
            (
                0,
                lines(&["No cache found at: /peppy-cache/uv-cache"]),
                UvCachePruneOutcome::Pruned {
                    summary: Some("No cache found at: /peppy-cache/uv-cache".to_string()),
                },
            ),
            (
                127,
                lines(&[NO_UV_IN_IMAGE]),
                UvCachePruneOutcome::NoUvInImage,
            ),
            (
                2,
                lines(&[
                    "Pruning cache at: /peppy-cache/uv-cache",
                    "error: Permission denied (os error 13)",
                    "",
                ]),
                UvCachePruneOutcome::Failed {
                    reason: "error: Permission denied (os error 13)".to_string(),
                },
            ),
            (
                1,
                Vec::new(),
                UvCachePruneOutcome::Failed {
                    reason: "exit status: 1".to_string(),
                },
            ),
        ];
        for (code, stderr_tail, expected) in cases {
            assert_eq!(
                UvCachePruneOutcome::of_run(exit_status(code), &stderr_tail),
                expected,
                "exit code {code}, stderr {stderr_tail:?}"
            );
        }
    }

    #[test]
    fn each_prune_outcome_is_one_line_of_the_cache() {
        let cases = [
            (
                UvCachePruneOutcome::Pruned {
                    summary: Some("Removed 15 files (6.8MiB)".to_string()),
                },
                "Container build cache: uv cache pruned: Removed 15 files (6.8MiB)",
            ),
            (
                UvCachePruneOutcome::Pruned { summary: None },
                "Container build cache: uv cache pruned",
            ),
            (
                UvCachePruneOutcome::InUseByAnotherBuild,
                "Container build cache: uv cache not pruned, because another build uses it",
            ),
            (
                UvCachePruneOutcome::LockedByAnotherUv,
                "Container build cache: uv cache not pruned, because another uv process uses it",
            ),
            (
                UvCachePruneOutcome::NoUvInImage,
                "Container build cache: uv cache not pruned, because the image has no uv command \
                 outside a virtual environment",
            ),
            (
                UvCachePruneOutcome::TimedOut,
                "Container build cache: uv cache not pruned: the prune did not end in 30 s",
            ),
            (
                UvCachePruneOutcome::Failed {
                    reason: "exit status: 1".to_string(),
                },
                "Container build cache: uv cache not pruned: exit status: 1",
            ),
        ];
        for (outcome, line) in cases {
            assert_eq!(outcome.line(), line);
        }
    }
}
