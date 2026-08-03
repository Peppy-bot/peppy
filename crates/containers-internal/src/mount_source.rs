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

/// Makes one bind mount source usable by the container runtime, reporting
/// whether it had to create it.
///
/// An existing path is left untouched whatever it is (file, socket, device,
/// directory); a [host-provided](is_host_provided_mount_source) one is accepted
/// as-is; anything else missing is created with `mkdir -p`.
///
/// `true` means the caller owes the operator a warning. Auto-creating is what
/// makes node-owned scratch and output directories work without ceremony, and
/// it is also what silently turns a typo'd file bind into an empty directory,
/// so every caller announces it in whatever stream it owns. The warning text
/// itself is [`auto_created_warning`], so the three streams say the same thing.
pub fn ensure_bind_source(src: &Path) -> std::io::Result<bool> {
    if src.exists() || is_host_provided_mount_source(src) {
        return Ok(false);
    }
    std::fs::create_dir_all(src).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "bind mount source does not exist: {} (auto-create failed: {e})",
                src.display()
            ),
        )
    })?;
    Ok(true)
}

/// The host path out of a `host_path[:container_path[:options]]` mount spec.
///
/// The source is the only part of a spec that exists outside the container, so
/// it is the only part that gets created, registered with the execution
/// environment, or named in a failure about the host.
pub fn mount_spec_source(spec: &str) -> &str {
    spec.split(':').next().unwrap_or(spec)
}

/// What every caller of [`ensure_bind_source`] says when it returns `true`.
pub fn auto_created_warning(src: &str) -> String {
    format!(
        "auto-created missing bind mount source: {src} \
         (if you intended to bind an existing file, this is a typo)"
    )
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

    #[test]
    fn ensure_bind_source_creates_a_missing_path_and_reports_it() {
        let parent = tempfile::tempdir().expect("tempdir");
        let target = parent.path().join("scratch").join("nested");

        assert!(
            ensure_bind_source(&target).expect("missing dir should be auto-created"),
            "creating the path must be reported so the caller can warn"
        );
        assert!(target.is_dir(), "the whole missing chain must be created");
    }

    /// An existing path is whatever the operator made it. Reporting `false`
    /// keeps the warning for the case it was written for: a path that was not
    /// there at all.
    #[test]
    fn ensure_bind_source_leaves_an_existing_path_alone() {
        let existing = tempfile::tempdir().expect("tempdir");
        assert!(
            !ensure_bind_source(existing.path()).expect("existing dir should be accepted"),
            "an existing path is not an auto-create"
        );
    }

    /// The host owns these trees, so a missing one is accepted rather than
    /// conjured, and there is nothing to warn about.
    #[test]
    fn ensure_bind_source_accepts_host_provided_paths_without_creating() {
        for path in [
            "/dev/does-not-exist-xyz",
            "/proc/missing",
            "/sys/missing",
            // The runtime tmpfs. Sim nodes bind `/run/user` to back
            // `XDG_RUNTIME_DIR`; on a macOS daemon there is no `/run` and `/`
            // is read-only, so any attempt to create it fails with `EROFS`.
            "/run/user",
        ] {
            assert!(
                !ensure_bind_source(Path::new(path)).unwrap_or_else(|error| panic!(
                    "{path} is host-provided and must be accepted: {error}"
                )),
                "{path} must not be reported as created"
            );
        }
        assert!(
            !Path::new("/dev/does-not-exist-xyz").exists(),
            "a host-provided path must be left missing, not created"
        );
    }

    /// The failure names the path and stays recognisable: callers surface this
    /// string to an operator who has to work out which bind is wrong.
    ///
    /// A dangling symlink is the deterministic way to make the create fail: the
    /// path does not exist, so the auto-create is attempted, and `mkdir` then
    /// refuses because the name is already taken. Unlike a read-only parent, no
    /// uid bypasses it, so the test means the same thing wherever it runs.
    #[cfg(unix)]
    #[test]
    fn ensure_bind_source_reports_a_failed_auto_create() {
        let parent = tempfile::tempdir().expect("tempdir");
        let target = parent.path().join("bind_source");
        std::os::unix::fs::symlink(parent.path().join("no_such_target"), &target)
            .expect("plant a dangling symlink");

        let error = ensure_bind_source(&target).expect_err("an uncreatable path must fail");
        let message = error.to_string();
        assert!(
            message.contains("bind mount source does not exist"),
            "error must keep the canonical phrase, got: {message}"
        );
        assert!(
            message.contains(target.to_string_lossy().as_ref()),
            "error must name the offending path, got: {message}"
        );
    }
}
