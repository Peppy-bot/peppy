//! Decision logic for rebuilding peppylib's embedded native extensions.
//!
//! These are pure functions with no I/O so the policy can be unit tested. A
//! `build.rs` cannot host its own `#[test]` (cargo never compiles a build
//! script as a test target), so the logic lives here and the build script calls
//! it.

/// The cargo build profile, parsed from the `PROFILE` env var that cargo sets
/// for build scripts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuildProfile {
    Debug,
    Release,
}

impl BuildProfile {
    /// Parses the `PROFILE` env var into a profile. Anything other than
    /// "release" (including a missing var) is treated as a debug build.
    pub fn from_env() -> Self {
        match std::env::var("PROFILE").as_deref() {
            Ok("release") => Self::Release,
            _ => Self::Debug,
        }
    }

    /// The tag stored in the build-state marker for artifacts built under this
    /// profile.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Debug => "dev",
            Self::Release => "release",
        }
    }
}

/// Whether the host `.so` (the extension for the build machine's own platform)
/// must be (re)built.
///
/// The host artifact always tracks the current sources: it is rebuilt whenever
/// it is missing or its recorded state is stale, in both debug and release.
/// `current` means the recorded (source hash, profile) matches the build that
/// is about to run. `force` rebuilds unconditionally (the release path uses this
/// as a guarantee against any input the source hash does not cover).
pub fn should_build_host(present: bool, current: bool, force: bool) -> bool {
    force || !present || !current
}

/// Whether a Linux `.so` must be (re)cross-compiled.
///
/// It is built when forced or missing (so the generator can always scaffold
/// container targets), or when stale during a release build. A present-but-stale
/// Linux `.so` is left alone in debug so editing peppylib does not pay the slow
/// zig cross-compile on every `cargo build` / `cargo test`. `force` (the release
/// path, or a developer iterating on container bindings) rebuilds unconditionally.
pub fn should_cross_compile(
    profile: BuildProfile,
    present: bool,
    stale: bool,
    force: bool,
) -> bool {
    if force || !present {
        return true;
    }
    if !stale {
        return false;
    }
    profile == BuildProfile::Release
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_rebuilds_when_missing_in_either_profile() {
        assert!(should_build_host(false, false, false));
        assert!(should_build_host(false, true, false));
    }

    #[test]
    fn host_rebuilds_when_present_but_stale() {
        assert!(should_build_host(true, false, false));
    }

    #[test]
    fn host_is_skipped_when_present_and_current() {
        assert!(!should_build_host(true, true, false));
    }

    #[test]
    fn force_rebuilds_current_host() {
        assert!(should_build_host(true, true, true));
    }

    #[test]
    fn missing_linux_so_always_cross_compiles() {
        for profile in [BuildProfile::Debug, BuildProfile::Release] {
            for stale in [false, true] {
                for force in [false, true] {
                    assert!(
                        should_cross_compile(profile, false, stale, force),
                        "missing target must always build ({profile:?}, stale={stale}, force={force})"
                    );
                }
            }
        }
    }

    #[test]
    fn stale_linux_so_skips_in_debug_but_rebuilds_in_release() {
        assert!(!should_cross_compile(
            BuildProfile::Debug,
            true,
            true,
            false
        ));
        assert!(should_cross_compile(
            BuildProfile::Release,
            true,
            true,
            false
        ));
    }

    #[test]
    fn force_rebuilds_linux_so_unconditionally() {
        // Force overrides both the debug skip and the not-stale skip, in either
        // profile. This is the release guarantee against inputs the source hash
        // does not cover.
        assert!(should_cross_compile(BuildProfile::Debug, true, true, true));
        assert!(should_cross_compile(BuildProfile::Debug, true, false, true));
        assert!(should_cross_compile(
            BuildProfile::Release,
            true,
            false,
            true
        ));
    }

    #[test]
    fn current_linux_so_is_skipped_without_force() {
        assert!(!should_cross_compile(
            BuildProfile::Debug,
            true,
            false,
            false
        ));
        assert!(!should_cross_compile(
            BuildProfile::Release,
            true,
            false,
            false
        ));
    }

    #[test]
    fn profile_tags_are_stable() {
        assert_eq!(BuildProfile::Debug.tag(), "dev");
        assert_eq!(BuildProfile::Release.tag(), "release");
    }
}
