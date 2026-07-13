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

/// Whether the caller requested a cross-platform build by setting
/// `PEPPY_CROSS_BUILD` (scripts/build_release.sh and the CI workflow do). Only a
/// cross build produces the Linux container bindings; a plain `cargo build`
/// builds only the host dynamic lib and skips the slow zig cross-compile, so
/// local dev builds stay fast.
///
/// Any non-empty value other than "0" counts as set, matching the
/// `PEPPYLIB_REBUILD` convention.
pub fn cross_build_requested() -> bool {
    std::env::var("PEPPY_CROSS_BUILD").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Whether a Linux `.so` cross-compile must run for a target.
///
/// The build host is macOS, so every Linux target is a cross-compile, and all of
/// them are release-only. A regular `cargo build` therefore produces only the
/// host dynamic lib, exactly like a Linux build (which likewise builds only its
/// own native host `.so`); this keeps the two platforms consistent and a plain
/// build fast. The Linux bindings are built solely in a cross build
/// (`cross_build`, from [`cross_build_requested`], set by
/// `scripts/build_release.sh` and CI), where a target is (re)built when a rebuild
/// is forced, the `.so` is missing, or its recorded state is stale. `force`
/// (`PEPPYLIB_REBUILD`) alone never triggers a Linux build: it refreshes the host
/// artifact, while the Linux bindings stay gated on the cross-build flag.
pub fn should_cross_compile(cross_build: bool, present: bool, stale: bool, force: bool) -> bool {
    cross_build && (force || !present || stale)
}

/// Whether a cached platform `.so` may be embedded into the generator.
///
/// Only a binding whose recorded source hash matches the sources of the build
/// that embeds it may ship: the daemon serializes node configs with the model
/// it was compiled against, and a binding compiled from a different revision
/// of the shared crates deserializes them with a different model. A plain
/// `cargo build` never rebuilds the Linux container bindings (see
/// [`should_cross_compile`]), so after a shared-crate change the cache can
/// hold Linux `.so` from older sources — embedding one puts the schema
/// mismatch inside the container image, where it only surfaces as a baffling
/// parse error when the node starts (e.g. `unknown field \`implements\``).
/// Excluding it instead makes the container build fail up front with the
/// scaffolder's explicit "no embedded native extension" error.
///
/// `None` (no recorded build state) means unknown provenance: never embed.
pub fn should_embed_so(recorded_source_hash: Option<&str>, current_source_hash: &str) -> bool {
    recorded_source_hash == Some(current_source_hash)
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
    fn linux_so_is_release_only() {
        // Without a cross build a Linux .so is never produced, regardless of
        // whether it is missing or stale, and not even under `force` (which
        // refreshes the host build, not a Linux cross-compile). This matches a
        // Linux `cargo build`, which likewise builds only its own host .so.
        for present in [false, true] {
            for stale in [false, true] {
                for force in [false, true] {
                    assert!(
                        !should_cross_compile(false, present, stale, force),
                        "Linux .so must not build without a cross build \
                         (present={present}, stale={stale}, force={force})"
                    );
                }
            }
        }
    }

    #[test]
    fn linux_so_builds_under_cross_build_when_missing_stale_or_forced() {
        assert!(should_cross_compile(true, false, false, false)); // missing
        assert!(should_cross_compile(true, true, true, false)); // stale
        assert!(should_cross_compile(true, true, false, true)); // forced
    }

    #[test]
    fn linux_so_reused_under_cross_build_when_present_and_fresh() {
        assert!(!should_cross_compile(true, true, false, false));
    }

    #[test]
    fn profile_tags_are_stable() {
        assert_eq!(BuildProfile::Debug.tag(), "dev");
        assert_eq!(BuildProfile::Release.tag(), "release");
    }

    #[test]
    fn so_embeds_only_when_built_from_current_sources() {
        assert!(should_embed_so(Some("abc"), "abc"));
    }

    #[test]
    fn stale_so_is_never_embedded() {
        // A binding built from older sources deserializes daemon-serialized
        // configs with a mismatched model; it must be excluded so the container
        // build fails explicitly instead of the node crashing at startup.
        assert!(!should_embed_so(Some("old"), "abc"));
    }

    #[test]
    fn so_without_recorded_state_is_never_embedded() {
        assert!(!should_embed_so(None, "abc"));
    }
}
