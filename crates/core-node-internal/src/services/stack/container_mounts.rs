//! Preparing the host paths a machine's containers will bind.
//!
//! Every daemon that runs a slice of a launch does this once, while its stack
//! is empty: the coordinator as a launch step, a participant when it is told to
//! replace its slice. The reason it cannot be left to the instance starts that
//! need the paths is [`Apptainer::ensure_host_mounts`]: registering a host path
//! the container VM has not seen restarts that VM, and a restart takes every
//! container already running in it. Doing the whole machine's registration
//! before the first instance starts is what keeps that restart free.
//!
//! On Linux there is no VM and `ensure_host_mounts` is a no-op, so what remains
//! is the auto-create of missing sources, which every machine owes its own
//! instances.

use std::path::{Path, PathBuf};

use containers::{Apptainer, is_host_provided_mount_source};

/// Makes this machine's bind sources usable, and reports the ones it created.
///
/// Creation comes first and registration second, on purpose: Lima drops a
/// mount whose host path does not exist, so registering an absent source would
/// buy a VM restart now and another one later when the path appears.
///
/// The returned paths are the ones that did not exist. Each is an operator
/// warning its caller must surface in whatever stream it owns, because an
/// auto-created source is also what a bind meant to name an existing file looks
/// like when the name is misspelled.
pub(crate) async fn prepare_container_mounts(
    mount_sources: &[String],
) -> std::result::Result<Vec<String>, String> {
    let mut auto_created = Vec::new();
    for src in mount_sources {
        if containers::ensure_bind_source(Path::new(src)).map_err(|e| e.to_string())? {
            auto_created.push(src.clone());
        }
    }

    let lima_mount_sources = external_lima_mount_sources(mount_sources);
    if lima_mount_sources.is_empty() {
        return Ok(auto_created);
    }

    // A first-time mount registration restarts the Lima VM, so this runs on the
    // blocking pool rather than holding the async worker for the restart.
    tokio::task::spawn_blocking(move || {
        let mut apptainer =
            Apptainer::new().map_err(|e| format!("Failed to initialize Apptainer: {e}"))?;
        let refs = lima_mount_sources
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        apptainer
            .ensure_host_mounts(&refs)
            .map_err(|e| format!("Failed to prepare container host mounts: {e}"))
    })
    .await
    .map_err(|e| format!("Failed to prepare container host mounts: {e}"))
    .and_then(|result| result)?;

    Ok(auto_created)
}

/// The subset of `mount_sources` a Lima guest cannot already see, absolute.
///
/// Only macOS runs containers in a VM. Home-relative paths arrive through
/// Lima's default `~` mount and host-provided trees are the guest's own, so
/// neither is ours to register; what is left is the paths that need an explicit
/// mount, which is what costs a VM restart.
///
/// Absolute because a registration ends up as a `location:` in the Lima config,
/// which the VM resolves with no notion of the daemon's working directory: a
/// relative source has to be anchored the same way the create above anchored
/// it, or the guest would mount something else, or nothing.
fn external_lima_mount_sources(mount_sources: &[String]) -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    mount_sources
        .iter()
        .filter_map(|src| {
            let src_path = absolute_mount_source(src);
            let guest_already_sees = is_host_provided_mount_source(&src_path)
                || home
                    .as_ref()
                    .is_some_and(|home_path| src_path.starts_with(home_path));
            (!guest_already_sees).then(|| src_path.to_string_lossy().into_owned())
        })
        .collect()
}

fn absolute_mount_source(src: &str) -> PathBuf {
    let path = Path::new(src);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prepare_reports_only_the_sources_it_created() {
        let root = tempfile::tempdir().expect("tempdir");
        let existing = root.path().join("already_there");
        std::fs::create_dir(&existing).expect("mkdir");
        let missing = root.path().join("scratch").join("output");

        let auto_created = prepare_container_mounts(&[
            existing.to_string_lossy().into_owned(),
            missing.to_string_lossy().into_owned(),
        ])
        .await
        .expect("both sources are preparable");

        assert_eq!(
            auto_created,
            vec![missing.to_string_lossy().into_owned()],
            "only the missing source is an operator warning"
        );
        assert!(
            missing.is_dir(),
            "the missing source must have been created"
        );
    }

    /// The whole preparation fails rather than half-registering a machine: a
    /// source that cannot be created is a bind that cannot work, and the launch
    /// that needs it is better stopped here than at the instance start.
    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_fails_when_a_source_cannot_be_created() {
        let root = tempfile::tempdir().expect("tempdir");
        let target = root.path().join("bind_source");
        std::os::unix::fs::symlink(root.path().join("no_such_target"), &target)
            .expect("plant a dangling symlink");

        let error = prepare_container_mounts(&[target.to_string_lossy().into_owned()])
            .await
            .expect_err("an uncreatable source must fail");
        assert!(
            error.contains(target.to_string_lossy().as_ref()),
            "the failure must name the offending path, got: {error}"
        );
    }

    /// A host-provided source must never be handed to Lima as an extra mount.
    /// Registering `/run/user` would mount the macOS side (which does not even
    /// exist) over the guest's own runtime tmpfs, and would restart the VM to
    /// do it. The guest resolves these paths itself.
    ///
    /// macOS-gated like its companions below: off macOS
    /// `external_lima_mount_sources` returns empty before consulting the
    /// filter at all, so an ungated assertion would hold with the filter
    /// deleted and prove nothing. The predicate itself is platform-independent
    /// and covered in `containers::mount_source`.
    #[test]
    #[cfg(target_os = "macos")]
    fn host_provided_sources_are_not_forwarded_to_lima() {
        let forwarded = external_lima_mount_sources(&[
            "/run/user".to_string(),
            "/dev/ttyUSB0".to_string(),
            "/proc/self".to_string(),
            "/sys/class".to_string(),
        ]);
        assert!(
            forwarded.is_empty(),
            "host-provided trees must stay out of the Lima mount list, got: {forwarded:?}"
        );
    }

    /// Lima mounts `$HOME` itself, so a path under it is already visible to the
    /// guest and registering it would buy a VM restart for nothing.
    #[test]
    #[cfg(target_os = "macos")]
    fn home_relative_sources_are_not_forwarded_to_lima() {
        let home = std::env::var("HOME").expect("HOME is set on a macOS test host");
        let forwarded = external_lima_mount_sources(&[format!("{home}/.peppy/built_nodes")]);
        assert!(
            forwarded.is_empty(),
            "a path Lima already mounts must not be registered again, got: {forwarded:?}"
        );
    }

    /// The complement of the tests above: the filter is a carve-out, not a
    /// blanket opt-out. An ordinary path outside `$HOME` still has to reach
    /// Lima or the guest could not see it. Only meaningful on macOS, where
    /// `external_lima_mount_sources` does its work.
    #[test]
    #[cfg(target_os = "macos")]
    fn ordinary_external_sources_are_still_forwarded_to_lima() {
        let forwarded = external_lima_mount_sources(&["/opt/robot_assets".to_string()]);
        assert_eq!(
            forwarded,
            vec!["/opt/robot_assets".to_string()],
            "a non-home path the guest cannot otherwise see must be registered",
        );
    }

    /// A relative source is decided against its absolute form, so it must be
    /// registered in that form too: a `location:` in the Lima config is
    /// resolved by the VM, which has no working directory to relate it to.
    ///
    /// Which branch applies depends on where the test binary runs: a checkout
    /// under `$HOME` makes the path one Lima already mounts, and dropping it is
    /// the same anchoring decision seen from the other side.
    #[test]
    #[cfg(target_os = "macos")]
    fn relative_sources_are_forwarded_to_lima_absolute() {
        let cwd = std::env::current_dir().expect("a working directory");
        let home = std::env::var("HOME").expect("HOME is set on a macOS test host");
        let forwarded = external_lima_mount_sources(&["robot_assets".to_string()]);

        if cwd.starts_with(&home) {
            assert!(
                forwarded.is_empty(),
                "a relative source under $HOME resolves into Lima's own mount, got: {forwarded:?}"
            );
        } else {
            assert_eq!(
                forwarded,
                vec![cwd.join("robot_assets").to_string_lossy().into_owned()],
                "a relative source must reach Lima anchored, not verbatim",
            );
        }
    }
}
