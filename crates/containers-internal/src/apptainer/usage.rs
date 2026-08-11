//! Backend-aware disk-usage probe for observing container build progress.
//!
//! `apptainer build` is mostly silent off-TTY while it downloads a docker base
//! image and assembles the SIF: one "Copying blob …" line per blob, then
//! nothing until the phase completes. The bytes it moves do land on disk,
//! though — in its cache directory, its `--tmpdir` scratch, and the output
//! image — so sampling the total footprint of those write surfaces
//! distinguishes "slow but downloading" from "wedged". The probe deliberately
//! samples whole directory roots rather than any cache-internal layout:
//! apptainer's cache structure varies across versions, and a whole-root sum is
//! layout-agnostic.

use super::facade::{Apptainer, Backend};
use super::lima;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Apptainer's env spelling of its cache-directory override.
const APPTAINER_CACHEDIR_ENV: &str = "APPTAINER_CACHEDIR";

/// The host-side directory apptainer caches OCI blobs and converted layers in:
/// `$APPTAINER_CACHEDIR` when set in this process's environment, else
/// `~/.apptainer/cache` (apptainer's own default).
///
/// peppy deliberately reads this rather than setting or relocating
/// `APPTAINER_CACHEDIR`: pointing an existing installation at a fresh cache
/// directory would cold-start multi-GB re-downloads for exactly the
/// slow-connection users the progress probe serves.
fn effective_host_cache_dir() -> Option<PathBuf> {
    effective_host_cache_dir_from(
        std::env::var_os(APPTAINER_CACHEDIR_ENV),
        std::env::var_os("HOME"),
    )
}

/// Env-injectable core of [`effective_host_cache_dir`], split out so the
/// resolution order is testable without racing other tests over the process
/// environment.
pub(crate) fn effective_host_cache_dir_from(
    cachedir_env: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(dir) = cachedir_env.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".apptainer/cache"))
}

/// Samples the total on-disk footprint of every surface an `apptainer build`
/// writes to. Self-contained (owns every path it needs, borrows nothing from
/// the facade) so callers can hold it across facade moves and hand it to
/// long-lived monitor tasks; obtained via [`Apptainer::cache_usage_probe`].
#[derive(Clone, Debug)]
pub struct CacheUsageProbe {
    /// Host-side roots, each summed recursively without following symlinks.
    pub(super) host_roots: Vec<PathBuf>,
    /// Guest-side cache sampling for the Lima backend (macOS), where the
    /// apptainer cache lives inside the VM.
    pub(super) guest: Option<GuestUsageProbe>,
}

/// The `limactl shell` plumbing needed to `du` the guest-side apptainer cache.
#[derive(Clone, Debug)]
pub(super) struct GuestUsageProbe {
    pub(super) limactl_path: PathBuf,
    pub(super) lima_home: PathBuf,
}

impl Apptainer {
    /// Builds a [`CacheUsageProbe`] over every surface a build on this backend
    /// writes to, plus `extra_host_roots` supplied by the caller (typically
    /// the output SIF path and the container build cache bind directory).
    ///
    /// Native (Linux): the host cache dir and the facade's `APPTAINER_TMPDIR`
    /// scratch, plus the extras. Lima (macOS): the guest cache is sampled via
    /// `du` inside the VM; only the extras are sampled host-side (the build's
    /// working dir is host-mounted, so they remain visible).
    pub fn cache_usage_probe(&self, extra_host_roots: Vec<PathBuf>) -> CacheUsageProbe {
        let mut host_roots = extra_host_roots;
        match &self.backend {
            Backend::Native { tmp_dir, .. } => {
                host_roots.extend(effective_host_cache_dir());
                host_roots.push(tmp_dir.clone());
                CacheUsageProbe {
                    host_roots,
                    guest: None,
                }
            }
            Backend::Lima {
                limactl_path,
                lima_home,
                ..
            } => CacheUsageProbe {
                host_roots,
                guest: Some(GuestUsageProbe {
                    limactl_path: limactl_path.clone(),
                    lima_home: lima_home.clone(),
                }),
            },
        }
    }
}

impl CacheUsageProbe {
    /// Total bytes currently on disk across every sampled root.
    ///
    /// Blocking (filesystem walks; a `limactl shell du` subprocess under
    /// Lima), so call it from a blocking context. Missing roots count 0 and
    /// per-root errors are skipped, so a partial sum still detects growth; a
    /// persistently failing probe reads as a flat 0, which emits no progress
    /// and leaves timeout behavior exactly as it is today — the probe can
    /// defer an idle timeout only while bytes are really landing, never
    /// neuter it.
    pub fn usage_bytes(&self) -> u64 {
        let host: u64 = self
            .host_roots
            .iter()
            .fold(0, |sum, root| sum.saturating_add(dir_size_bytes(root)));
        let guest = self.guest.as_ref().map_or(0, GuestUsageProbe::usage_bytes);
        host.saturating_add(guest)
    }
}

impl GuestUsageProbe {
    /// `du -sb` of the guest-side apptainer cache, through the same
    /// `limactl shell` plumbing every other guest command uses. The guest
    /// shell resolves the cache dir with the same override-then-default rule
    /// as [`effective_host_cache_dir`], so the two spellings cannot fork. Any
    /// failure (VM unreachable, `du` missing, unparseable output) reads as 0.
    fn usage_bytes(&self) -> u64 {
        let output = lima::lima_shell_cmd(&self.limactl_path, &self.lima_home, lima::LIMA_INSTANCE)
            .args([
                "sh",
                "-c",
                r#"du -sb "${APPTAINER_CACHEDIR:-$HOME/.apptainer/cache}" 2>/dev/null | cut -f1"#,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or(0),
            _ => 0,
        }
    }
}

/// Recursive file-size sum of `root`, never following symlinks (a symlink's
/// own metadata is counted, not its target's). Missing paths and unreadable
/// entries contribute 0 so one bad subtree cannot zero out the whole sample.
fn dir_size_bytes(root: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(_) => return 0,
    };
    if meta.is_dir() {
        dir_tree_size(root)
    } else {
        meta.len()
    }
}

/// Sums the entries of a directory known to exist. Leans on `read_dir`'s own
/// per-entry handles — `file_type` (free on most filesystems) to pick the
/// recursion, `metadata` (lstat, no path building) for sizes — because this
/// runs over the whole apptainer cache every sample tick.
fn dir_tree_size(dir: &Path) -> u64 {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    entries
        .filter_map(|entry| entry.ok())
        .fold(0, |sum, entry| {
            let size = match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => dir_tree_size(&entry.path()),
                Ok(_) => entry.metadata().map_or(0, |meta| meta.len()),
                Err(_) => 0,
            };
            sum.saturating_add(size)
        })
}
