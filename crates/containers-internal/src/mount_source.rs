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

/// Expands a leading `~` in the host-source segment of a
/// `host_path[:container_path[:options]]` mount spec against THIS machine's
/// home directory. The container path and options pass through verbatim: a
/// `~` there belongs to the container, not the host.
///
/// Expansion is deliberately machine-local. A launch coordinator ships mount
/// sources to peer machines as the raw `~/...` token, because the peer's home
/// is not the coordinator's; whichever daemon is about to create, register,
/// or bind a source calls this at that moment. Without it the literal `~` is
/// a relative path, and the auto-create/`--bind` grow a stray `~/` tree in
/// whatever directory the daemon happened to be started from.
///
/// `~user` sources are rejected via [`home_mount_source_rejection`], and an
/// expansion landing in a blocked system directory (a pathological `$HOME`
/// like `/tmp`) is refused with the phrasing the spec validation uses, so the
/// "nothing handed to the auto-create or the runtime is a blocked top-level
/// dir" invariant survives expansion.
pub fn expand_home_in_mount_spec(spec: &str) -> std::result::Result<String, String> {
    expand_home_in_mount_spec_with(spec, dirs::home_dir().as_deref())
}

/// Why `src` cannot name a home-relative mount source, or `None` if it can.
///
/// Only the `~user` form is unsupported: resolving another user's home is a
/// passwd lookup peppy has no business doing on a node's behalf. Shared so
/// the coordinator-side plan validation (which must not expand, only reject)
/// and the machine-local expansion above refuse with the same words.
pub fn home_mount_source_rejection(src: &str) -> Option<String> {
    let is_tilde_user = src.starts_with('~') && src != "~" && !src.starts_with("~/");
    is_tilde_user.then(|| {
        format!(
            "~user paths are not supported in mount path source `{src}`; \
             use an absolute path or ~/..."
        )
    })
}

/// [`expand_home_in_mount_spec`] with the home directory injected, so tests
/// don't depend on the runner's real `$HOME`.
fn expand_home_in_mount_spec_with(
    spec: &str,
    home: Option<&Path>,
) -> std::result::Result<String, String> {
    let src = mount_spec_source(spec);
    if !src.starts_with('~') {
        return Ok(spec.to_owned());
    }
    if let Some(rejection) = home_mount_source_rejection(src) {
        return Err(rejection);
    }
    let home = home.ok_or_else(|| {
        format!("cannot expand `~` in mount path `{spec}`: the home directory is unavailable")
    })?;
    let expanded = if src == "~" {
        home.to_path_buf()
    } else {
        home.join(&src["~/".len()..])
    };
    let expanded_src = expanded.to_str().ok_or_else(|| {
        format!(
            "cannot expand `~` in mount path `{spec}`: the home directory path is not valid UTF-8"
        )
    })?;
    if config::node::is_blocked_mount_source(expanded_src) {
        return Err(format!(
            "mount path `{spec}` expands its source to a blocked system directory \
             `{expanded_src}`; use a subdirectory instead"
        ));
    }
    Ok(format!("{expanded_src}{}", &spec[src.len()..]))
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

    #[test]
    fn expand_home_expands_bare_tilde_source() {
        let expanded = expand_home_in_mount_spec_with("~", Some(Path::new("/home/me")))
            .expect("a bare ~ is a valid source");
        assert_eq!(expanded, "/home/me");
    }

    #[test]
    fn expand_home_expands_tilde_source_and_keeps_dest_and_opts() {
        let expanded = expand_home_in_mount_spec_with(
            "~/.cache/kit:/isaac-sim/kit/cache:rw",
            Some(Path::new("/home/me")),
        )
        .expect("a home-relative source is a valid spec");
        assert_eq!(expanded, "/home/me/.cache/kit:/isaac-sim/kit/cache:rw");
    }

    #[test]
    fn expand_home_rejects_tilde_user_sources() {
        let error = expand_home_in_mount_spec_with("~bob/data:/data", Some(Path::new("/home/me")))
            .expect_err("~user sources are unsupported");
        assert!(
            error.contains("~user paths are not supported"),
            "the rejection must say why, got: {error}"
        );
        assert!(
            error.contains("~bob/data"),
            "the rejection must name the offending source, got: {error}"
        );
    }

    /// Everything without a leading `~` in the SOURCE comes back verbatim: an
    /// absolute spec, a plain relative source (whose cwd anchoring is the
    /// documented Lima behavior, not ours to change here), and a spec whose
    /// only `~` is on the container side.
    #[test]
    fn expand_home_leaves_non_tilde_specs_alone() {
        for spec in [
            "/data/models:/opt/models:ro",
            "robot_assets",
            "/data:~/inside:rw",
        ] {
            let expanded = expand_home_in_mount_spec_with(spec, Some(Path::new("/home/me")))
                .expect("a non-tilde source is always valid");
            assert_eq!(expanded, spec, "{spec} must pass through unchanged");
        }
    }

    #[test]
    fn expand_home_fails_without_a_home_directory() {
        let error = expand_home_in_mount_spec_with("~/.cache/kit", None)
            .expect_err("no home means no expansion");
        assert!(
            error.contains("home directory is unavailable"),
            "the failure must say what is missing, got: {error}"
        );
    }

    /// A pathological home (say the daemon was started with `HOME=/tmp`) must
    /// not let a bare `~` smuggle a blocked top-level directory past the spec
    /// validation, which ran before expansion and saw only the token.
    #[test]
    fn expand_home_rejects_a_blocked_expansion() {
        let error = expand_home_in_mount_spec_with("~", Some(Path::new("/tmp")))
            .expect_err("an expansion into a blocked directory must be refused");
        assert!(
            error.contains("blocked system directory"),
            "the refusal must keep the canonical phrase, got: {error}"
        );
    }

    /// The public wrapper wires the real home lookup to the expansion. Skipped
    /// when the environment has no resolvable home, where the error branch is
    /// already covered above.
    #[test]
    fn expand_home_public_wrapper_uses_the_real_home() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let expanded =
            expand_home_in_mount_spec("~/.cache/kit:/kit:rw").expect("expansion must succeed");
        assert_eq!(
            expanded,
            format!("{}/.cache/kit:/kit:rw", home.to_str().expect("utf-8 home"))
        );
    }
}
