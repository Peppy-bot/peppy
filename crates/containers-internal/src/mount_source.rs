//! Which bind mount sources the *host* is responsible for materializing.
//!
//! Complements `config::node::is_blocked_mount_source`, which answers a
//! different question: that list rejects whole top-level system directories
//! (`/dev`, `/tmp`, `/usr`, …) as bind sources or Lima mountPoints outright.
//! This one accepts the *subpaths* under a handful of those trees and says
//! only that peppy must not conjure them — `/dev/video0` is a legitimate bind
//! source, `/dev` is not.

use std::path::Path;

/// Whether a bind mount source lives under a tree the *host* materializes,
/// which peppy must therefore never create.
///
/// `/dev`, `/proc` and `/sys` are kernel virtual filesystems; `/run` is the
/// runtime tmpfs whose contents (`/run/user/$UID`, session sockets) are owned
/// by init and the login stack. In every case the entry either already exists
/// on the host that will run the container, or it is not ours to conjure: a
/// `mkdir -p` would at best be a no-op and at worst mask a real "this host
/// isn't the one you want" mismatch behind an empty directory.
///
/// The distinction matters most off-Linux. A macOS daemon runs its containers
/// inside the Lima guest, and macOS has neither `/run` nor a writable `/`
/// (the sealed system volume), so auto-creating these paths fails outright
/// with `EROFS` — see the sim nodes that bind `/run/user` to back
/// `XDG_RUNTIME_DIR`. Skipping them here leaves resolution to the guest,
/// where the path really lives.
pub fn is_host_provided_mount_source(path: &Path) -> bool {
    path.starts_with("/dev")
        || path.starts_with("/proc")
        || path.starts_with("/run")
        || path.starts_with("/sys")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The carve-out is a prefix test over whole trees, not an exact-path
    /// match: a node binding `/run/user/1000/pipewire-0` must be covered just
    /// like the tree root. Everything outside those roots stays an ordinary
    /// bind source, which is what keeps a typo'd bind loud.
    #[test]
    fn host_provided_mount_sources_cover_whole_trees_only() {
        for path in [
            "/dev",
            "/dev/can0",
            "/proc",
            "/proc/self",
            "/run",
            "/run/user",
            "/run/user/1000/pipewire-0",
            "/sys",
            "/sys/class",
        ] {
            assert!(
                is_host_provided_mount_source(Path::new(path)),
                "{path} is host-provided and must never be auto-created"
            );
        }
        // `/runtime` guards the boundary: `Path::starts_with` matches whole
        // components, so a longer name sharing the `/run` prefix must not be
        // swept in.
        for path in ["/", "/runtime", "/home/peppy/run", "/opt/dev"] {
            assert!(
                !is_host_provided_mount_source(Path::new(path)),
                "{path} is an ordinary bind source and must keep auto-create + warning"
            );
        }
    }
}
