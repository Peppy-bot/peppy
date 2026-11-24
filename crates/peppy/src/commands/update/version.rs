//! Release version parsing, comparison, and update planning.

use std::fmt;

use url::Url;

/// A Peppy release version of the form `vMAJOR.MINOR.PATCH` with no leading
/// zeros on multi-digit components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReleaseVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl ReleaseVersion {
    /// Parses a release version string such as `v0.39.0` or `0.39.0`.
    pub fn parse(input: &str) -> Result<Self, InvalidReleaseVersion> {
        let trimmed = input.trim();
        let body = trimmed.strip_prefix('v').unwrap_or(trimmed);
        let mut parts = body.split('.');
        let (Some(major_str), Some(minor_str), Some(patch_str), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(InvalidReleaseVersion(input.to_owned()));
        };
        let major =
            parse_component(major_str).ok_or_else(|| InvalidReleaseVersion(input.to_owned()))?;
        let minor =
            parse_component(minor_str).ok_or_else(|| InvalidReleaseVersion(input.to_owned()))?;
        let patch =
            parse_component(patch_str).ok_or_else(|| InvalidReleaseVersion(input.to_owned()))?;
        Ok(Self {
            major,
            minor,
            patch,
        })
    }

    /// Formats the canonical release tag (`vMAJOR.MINOR.PATCH`).
    pub fn tag(self) -> String {
        self.to_string()
    }
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Error returned when a string is not a valid `vMAJOR.MINOR.PATCH` version.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid release version `{0}`: expected a version like `v0.39.0`")]
pub struct InvalidReleaseVersion(String);

fn parse_component(part: &str) -> Option<u64> {
    if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
        return None;
    }
    if !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

/// The version of the currently running `peppy` binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CurrentVersion {
    /// A tagged release build (`PEPPY_GIT_TAG = Some("v0.39.0")`).
    Release(ReleaseVersion),
    /// A tagged non-release build (for example, `dev-5b706c5a0f1e`).
    TaggedOther(String),
    /// An untagged local development build (`PEPPY_GIT_TAG = None`).
    Development,
}

impl CurrentVersion {
    /// Classifies `PEPPY_GIT_TAG` into a typed current version.
    pub(crate) fn from_git_tag(git_tag: Option<&str>) -> Self {
        let Some(tag) = git_tag.map(str::trim).filter(|s| !s.is_empty()) else {
            return Self::Development;
        };
        match ReleaseVersion::parse(tag) {
            Ok(version) => Self::Release(version),
            Err(_) => Self::TaggedOther(tag.to_owned()),
        }
    }
}

impl fmt::Display for CurrentVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Release(version) => write!(f, "{version}"),
            Self::TaggedOther(tag) => f.write_str(tag),
            Self::Development => f.write_str("unknown (development build)"),
        }
    }
}

/// Decision produced by comparing the current binary against the target release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateDecision {
    /// Already running the target release (and `--force` was not passed).
    AlreadyUpToDate { current: ReleaseVersion },
    /// Running a release newer than latest (and neither `--version` nor `--force` was passed).
    NewerThanLatest {
        current: ReleaseVersion,
        latest: ReleaseVersion,
    },
    /// Download and install `target`.
    Install { target: ReleaseVersion },
}

/// Decides whether to install `target` given the current binary version and flags.
pub(crate) fn decide_update(
    current: &CurrentVersion,
    target: ReleaseVersion,
    explicit_version: bool,
    force: bool,
) -> UpdateDecision {
    if force {
        return UpdateDecision::Install { target };
    }
    let CurrentVersion::Release(installed) = *current else {
        return UpdateDecision::Install { target };
    };
    if installed == target {
        return UpdateDecision::AlreadyUpToDate { current: installed };
    }
    if installed > target && !explicit_version {
        return UpdateDecision::NewerThanLatest {
            current: installed,
            latest: target,
        };
    }
    UpdateDecision::Install { target }
}

/// Extracts a release tag from a redirect URL pointing at `archive_name`.
///
/// Recognizes both GitHub release asset URLs
/// (`.../releases/download/v0.39.0/peppy-x86_64-unknown-linux-gnu.tgz`) and
/// direct versioned URLs (`.../v0.39.0/peppy-x86_64-unknown-linux-gnu.tgz`).
pub(crate) fn extract_release_tag_from_url(
    url: &Url,
    archive_name: &str,
) -> Option<ReleaseVersion> {
    let segments: Vec<&str> = url.path_segments()?.collect();
    if segments.len() < 2 {
        return None;
    }
    let file = *segments.last()?;
    if file != archive_name {
        return None;
    }
    let tag_segment = segments[segments.len() - 2];
    if !tag_segment.starts_with('v') {
        return None;
    }
    ReleaseVersion::parse(tag_segment).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_canonical_and_bare_semver() {
        assert_eq!(
            ReleaseVersion::parse("v0.39.0").unwrap(),
            ReleaseVersion {
                major: 0,
                minor: 39,
                patch: 0,
            }
        );
        assert_eq!(
            ReleaseVersion::parse("0.39.0").unwrap().to_string(),
            "v0.39.0"
        );
        assert_eq!(
            ReleaseVersion::parse("  v1.2.3  ").unwrap().to_string(),
            "v1.2.3"
        );
    }

    #[test]
    fn parse_rejects_malformed_versions() {
        for bad in [
            "",
            "v",
            "v0.39",
            "v0.39.0.1",
            "v0.01.0",
            "v0.39.0-rc1",
            "v0.39.x",
            "latest",
            "dev-5b706c5a0f1e",
            "v-1.0.0",
            "v+1.0.0",
        ] {
            let err = ReleaseVersion::parse(bad).unwrap_err();
            assert!(
                err.to_string().contains(bad),
                "error for `{bad}` should mention the input: {err}"
            );
        }
    }

    #[test]
    fn release_versions_compare_numerically() {
        let v0_9_10 = ReleaseVersion::parse("v0.9.10").unwrap();
        let v0_10_0 = ReleaseVersion::parse("v0.10.0").unwrap();
        let v0_39_0 = ReleaseVersion::parse("v0.39.0").unwrap();
        let v1_0_0 = ReleaseVersion::parse("v1.0.0").unwrap();

        assert!(v0_9_10 < v0_10_0);
        assert!(v0_10_0 < v0_39_0);
        assert!(v0_39_0 < v1_0_0);
    }

    #[test]
    fn current_version_classifies_git_tags() {
        assert_eq!(
            CurrentVersion::from_git_tag(None),
            CurrentVersion::Development
        );
        assert_eq!(
            CurrentVersion::from_git_tag(Some("")),
            CurrentVersion::Development
        );
        assert_eq!(
            CurrentVersion::from_git_tag(Some("v0.39.0")),
            CurrentVersion::Release(ReleaseVersion {
                major: 0,
                minor: 39,
                patch: 0,
            })
        );
        assert_eq!(
            CurrentVersion::from_git_tag(Some("dev-5b706c5a0f1e")),
            CurrentVersion::TaggedOther("dev-5b706c5a0f1e".to_owned())
        );
    }

    #[test]
    fn decide_update_handles_upgrade_up_to_date_downgrade_and_force() {
        let v38 = ReleaseVersion::parse("v0.38.0").unwrap();
        let v39 = ReleaseVersion::parse("v0.39.0").unwrap();
        let v40 = ReleaseVersion::parse("v0.40.0").unwrap();
        let current = CurrentVersion::Release(v39);

        assert_eq!(
            decide_update(&current, v40, false, false),
            UpdateDecision::Install { target: v40 }
        );
        assert_eq!(
            decide_update(&current, v39, false, false),
            UpdateDecision::AlreadyUpToDate { current: v39 }
        );
        assert_eq!(
            decide_update(&current, v39, false, true),
            UpdateDecision::Install { target: v39 }
        );
        assert_eq!(
            decide_update(&current, v38, false, false),
            UpdateDecision::NewerThanLatest {
                current: v39,
                latest: v38,
            }
        );
        assert_eq!(
            decide_update(&current, v38, true, false),
            UpdateDecision::Install { target: v38 }
        );
        assert_eq!(
            decide_update(&current, v38, false, true),
            UpdateDecision::Install { target: v38 }
        );
        assert_eq!(
            decide_update(
                &CurrentVersion::TaggedOther("dev-abc123".to_owned()),
                v39,
                false,
                false
            ),
            UpdateDecision::Install { target: v39 }
        );
    }

    #[test]
    fn extract_release_tag_reads_github_and_mirror_urls() {
        let archive = "peppy-x86_64-unknown-linux-gnu.tgz";
        let gh = Url::parse(
            "https://github.com/Peppy-bot/peppy/releases/download/v0.39.0/peppy-x86_64-unknown-linux-gnu.tgz",
        )
        .unwrap();
        assert_eq!(
            extract_release_tag_from_url(&gh, archive),
            Some(ReleaseVersion::parse("v0.39.0").unwrap())
        );

        let mirror =
            Url::parse("https://peppy.bot/v0.38.4/peppy-x86_64-unknown-linux-gnu.tgz").unwrap();
        assert_eq!(
            extract_release_tag_from_url(&mirror, archive),
            Some(ReleaseVersion::parse("v0.38.4").unwrap())
        );

        let latest =
            Url::parse("https://peppy.bot/latest/peppy-x86_64-unknown-linux-gnu.tgz").unwrap();
        assert_eq!(extract_release_tag_from_url(&latest, archive), None);

        let wrong_file =
            Url::parse("https://github.com/Peppy-bot/peppy/releases/download/v0.39.0/other.tgz")
                .unwrap();
        assert_eq!(extract_release_tag_from_url(&wrong_file, archive), None);

        let bare_version =
            Url::parse("https://example.com/0.39.0/peppy-x86_64-unknown-linux-gnu.tgz").unwrap();
        assert_eq!(extract_release_tag_from_url(&bare_version, archive), None);
    }
}
