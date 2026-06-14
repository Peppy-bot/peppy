//! Single source of truth for which host paths must not be used as bind-mount
//! sources or Lima guest mountPoints.
//!
//! Lima 2.0+ rejects top-level system directories as guest mountPoints, and using
//! them as Apptainer bind sources is almost always a mistake (mounting an entire
//! system directory into a container). Both `config` (validating user
//! `mount_paths`) and `containers` (mutating the Lima YAML) enforce the same rule.
//! It lives in this dependency-free leaf crate so the two consumers cannot drift,
//! replacing the comment-enforced "keep both in sync" invariant they used before.
#![forbid(unsafe_code)]

/// Top-level system directories that Lima 2.0+ rejects as guest mountPoints.
pub const BLOCKED_MOUNT_PATHS: &[&str] = &[
    "/", "/bin", "/dev", "/etc", "/home", "/opt", "/sbin", "/tmp", "/usr", "/var",
];

/// Format the blocked mount paths as a comma-separated display string, for use in
/// user-facing validation errors.
pub fn blocked_mount_paths_display() -> String {
    BLOCKED_MOUNT_PATHS.join(", ")
}

/// Check whether a path is a blocked top-level system mount.
///
/// Only exact top-level matches are blocked; subdirectories like `/tmp/my_app` are
/// allowed. Also handles macOS `/private/X` equivalents (e.g., `/private/tmp` maps
/// to `/tmp`).
pub fn is_blocked_mount_source(path: &str) -> bool {
    if BLOCKED_MOUNT_PATHS.contains(&path) {
        return true;
    }
    // macOS: /private/tmp -> /tmp, /private/var -> /var
    if let Some(stripped) = path.strip_prefix("/private") {
        return BLOCKED_MOUNT_PATHS.contains(&stripped);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_top_level_system_paths() {
        for path in [
            "/", "/bin", "/dev", "/etc", "/home", "/opt", "/sbin", "/tmp", "/usr", "/var",
        ] {
            assert!(is_blocked_mount_source(path), "{path} should be blocked");
        }
    }

    #[test]
    fn rejects_private_equivalents() {
        assert!(is_blocked_mount_source("/private/tmp"));
        assert!(is_blocked_mount_source("/private/var"));
        assert!(is_blocked_mount_source("/private/etc"));
    }

    #[test]
    fn allows_subdirectories() {
        for path in [
            "/tmp/my_app",
            "/var/log/my_app",
            "/data/shared",
            "/mnt/external",
            "/private/tmp/foo",
        ] {
            assert!(!is_blocked_mount_source(path), "{path} should be allowed");
        }
    }

    #[test]
    fn display_lists_the_blocked_paths() {
        let display = blocked_mount_paths_display();
        for path in BLOCKED_MOUNT_PATHS {
            assert!(
                display.contains(path),
                "display string should list {path}, got: {display}"
            );
        }
    }
}
