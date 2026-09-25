//! Which ref of a git repository peppy reads: the `ref` of a git entry in
//! `repositories.json5`, and the `--ref` of `peppy repo add` and
//! `peppy repo exclude`, both parsed by [`GitRepoRef::parse`].

use std::fmt;

/// The `ref` that reads the hub content released with the running peppy.
///
/// Git refuses every ref name that contains `@{`, so no branch or tag can
/// carry this name, and peppy can follow every real branch or tag. The
/// spelling follows git's own syntax for a ref that git works out, such as
/// `@{upstream}`; here peppy works it out (see [`GitRepoRef::read_ref`]) and
/// git never sees the keyword.
pub const PEPPY_RELEASE_REF: &str = "@{peppy-release}";

/// The prefix of the tag the Peppy release puts in each hub it tested: a
/// release build of `v0.31.2` reads the tag `peppy-release/v0.31.2`.
pub const PEPPY_RELEASE_TAG_PREFIX: &str = "peppy-release/";

/// The branch `@{peppy-release}` reads in a build that is not a release:
/// the hub content the next release will carry.
pub const UNRELEASED_HUB_BRANCH: &str = "main";

/// The ref a git repository entry is configured to read.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GitRepoRef {
    /// No `ref`: the branch the remote's HEAD names.
    RemoteHead,
    /// A branch, a tag or a commit, passed to git as written.
    Named(String),
    /// [`PEPPY_RELEASE_REF`]: the hub content released with the running
    /// peppy.
    PeppyRelease,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitRepoRefError {
    #[error(
        "`{0}` is not a ref peppy can read: a ref that contains `@{{` must be exactly \
         `@{{peppy-release}}`, which reads the hub content released with the running peppy"
    )]
    NotThePeppyReleaseKeyword(String),
}

impl GitRepoRef {
    /// Parses a ref as written.
    ///
    /// Empty (or only whitespace) is [`GitRepoRef::RemoteHead`], exactly
    /// [`PEPPY_RELEASE_REF`] is [`GitRepoRef::PeppyRelease`] (the match is
    /// case-sensitive), and any other value that contains `@{` is refused:
    /// git's revision expressions such as `main@{1}` or `@{upstream}` name
    /// no fixed ref in a new clone. Every other value is a git name.
    pub fn parse(raw: &str) -> Result<Self, GitRepoRefError> {
        let value = raw.trim();
        if value.is_empty() {
            return Ok(Self::RemoteHead);
        }
        if value == PEPPY_RELEASE_REF {
            return Ok(Self::PeppyRelease);
        }
        if value.contains("@{") {
            return Err(GitRepoRefError::NotThePeppyReleaseKeyword(value.to_owned()));
        }
        Ok(Self::Named(value.to_owned()))
    }

    /// The `ref` as it is written in `repositories.json5` and on the wire:
    /// `None` for [`GitRepoRef::RemoteHead`].
    pub fn configured(&self) -> Option<&str> {
        match self {
            Self::RemoteHead => None,
            Self::Named(name) => Some(name),
            Self::PeppyRelease => Some(PEPPY_RELEASE_REF),
        }
    }

    /// The git ref `build` reads for this entry, `None` for
    /// [`GitRepoRef::RemoteHead`].
    ///
    /// [`GitRepoRef::PeppyRelease`] reads the tag
    /// `peppy-release/<version>` in a release build, and the branch
    /// [`UNRELEASED_HUB_BRANCH`] in any other build.
    pub fn read_ref(&self, build: &PeppyBuild) -> Option<ReadRef> {
        match (self, build) {
            (Self::RemoteHead, _) => None,
            (Self::Named(name), _) => Some(ReadRef::Named(name.clone())),
            (Self::PeppyRelease, PeppyBuild::Release(version)) => {
                Some(ReadRef::ReleaseTag(version.clone()))
            }
            (Self::PeppyRelease, PeppyBuild::Unreleased) => {
                Some(ReadRef::Named(UNRELEASED_HUB_BRANCH.to_owned()))
            }
        }
    }
}

/// The `ref` as it is written on the wire: the configured ref, and the
/// empty text for [`GitRepoRef::RemoteHead`].
impl fmt::Display for GitRepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.configured().unwrap_or(""))
    }
}

/// Decodes the `repoRef` text of a git source, as [`GitRepoRef`]'s
/// `Display` writes it: the empty text is [`GitRepoRef::RemoteHead`], and a
/// ref peppy cannot read fails the decode.
pub(crate) fn decode_git_repo_ref(text: &str) -> crate::Result<GitRepoRef> {
    GitRepoRef::parse(text).map_err(|e| crate::Error::Decoding(e.to_string()))
}

/// The namespace of every tag in a git repository.
const TAGS_NAMESPACE: &str = "refs/tags/";

/// A git ref a repository entry reads, with [`PEPPY_RELEASE_REF`] resolved.
///
/// Stored as its [`ReadRef::git_name`] and read back with
/// [`ReadRef::from_git_name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadRef {
    /// A branch, a tag or a commit, passed to git as written.
    Named(String),
    /// The tag [`PEPPY_RELEASE_TAG_PREFIX`]`<version>` of a release build.
    ReleaseTag(String),
}

impl ReadRef {
    /// The name a clone is positioned on. A release tag goes by its full
    /// name `refs/tags/peppy-release/<version>`: a clone holds every branch
    /// head, and a branch with the same short name must never stand in for
    /// the tag.
    pub fn git_name(&self) -> String {
        match self {
            Self::Named(name) => name.clone(),
            Self::ReleaseTag(_) => format!("{TAGS_NAMESPACE}{}", self.short_name()),
        }
    }

    /// The read ref whose [`ReadRef::git_name`] is `git_name`: the full name
    /// of a release tag, `refs/tags/peppy-release/v<MAJOR>.<MINOR>.<PATCH>`,
    /// is a [`ReadRef::ReleaseTag`], and any other name is a
    /// [`ReadRef::Named`].
    pub fn from_git_name(git_name: &str) -> Self {
        let release_version = git_name
            .strip_prefix(TAGS_NAMESPACE)
            .and_then(|tag| tag.strip_prefix(PEPPY_RELEASE_TAG_PREFIX))
            .filter(|version| is_release_version(version));
        match release_version {
            Some(version) => Self::ReleaseTag(version.to_owned()),
            None => Self::Named(git_name.to_owned()),
        }
    }

    /// The name people read: `peppy-release/<version>`, or the ref as
    /// configured.
    pub fn short_name(&self) -> String {
        match self {
            Self::Named(name) => name.clone(),
            Self::ReleaseTag(version) => format!("{PEPPY_RELEASE_TAG_PREFIX}{version}"),
        }
    }
}

impl serde::Serialize for ReadRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.git_name())
    }
}

impl<'de> serde::Deserialize<'de> for ReadRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let git_name = <String as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::from_git_name(&git_name))
    }
}

/// The build of the running peppy, as far as [`PEPPY_RELEASE_REF`] goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeppyBuild {
    /// A release build, with its version `v<MAJOR>.<MINOR>.<PATCH>`.
    Release(String),
    /// Any other build: a CI dev build, a local test build, a plain
    /// `cargo build`.
    Unreleased,
}

impl PeppyBuild {
    /// The build a binary built with `git_tag` as its `PEPPY_GIT_TAG` is.
    ///
    /// Only `v<MAJOR>.<MINOR>.<PATCH>`, with digits only in each part, is a
    /// release: that is the one form of tag the release publishes. Any other
    /// tag (`v0.31.2-rc1`, `dev-5b706c5a0f1e`, `test`) or no tag at all is
    /// [`PeppyBuild::Unreleased`].
    pub fn from_git_tag(git_tag: Option<&str>) -> Self {
        match git_tag {
            Some(tag) if is_release_version(tag) => Self::Release(tag.to_owned()),
            _ => Self::Unreleased,
        }
    }
}

fn is_release_version(tag: &str) -> bool {
    let Some(version) = tag.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(version: &str) -> PeppyBuild {
        PeppyBuild::Release(version.to_owned())
    }

    #[test]
    fn an_absent_or_empty_ref_follows_the_remote_head() {
        assert_eq!(GitRepoRef::parse(""), Ok(GitRepoRef::RemoteHead));
        assert_eq!(GitRepoRef::parse("   "), Ok(GitRepoRef::RemoteHead));
        assert_eq!(GitRepoRef::RemoteHead.configured(), None);
    }

    #[test]
    fn the_keyword_is_the_peppy_release() {
        assert_eq!(
            GitRepoRef::parse("@{peppy-release}"),
            Ok(GitRepoRef::PeppyRelease)
        );
        assert_eq!(
            GitRepoRef::PeppyRelease.configured(),
            Some("@{peppy-release}")
        );
    }

    #[test]
    fn every_git_name_is_a_named_ref_passed_as_written() {
        for name in [
            "main",
            "feat/gripper-force",
            "v2.0",
            "peppy-release",
            "peppy-release/v0.31.2",
            "0123456789abcdef0123456789abcdef01234567",
        ] {
            assert_eq!(
                GitRepoRef::parse(name),
                Ok(GitRepoRef::Named(name.to_owned())),
                "{name}"
            );
        }
    }

    #[test]
    fn any_other_ref_with_an_at_brace_is_refused_naming_the_keyword() {
        for raw in [
            "@{Peppy-Release}",
            "@{peppy-releases}",
            "@{upstream}",
            "main@{1}",
        ] {
            let error = GitRepoRef::parse(raw).expect_err(raw);
            assert_eq!(
                error,
                GitRepoRefError::NotThePeppyReleaseKeyword(raw.to_owned())
            );
            assert!(error.to_string().contains("`@{peppy-release}`"), "{error}");
        }
    }

    #[test]
    fn only_a_plain_release_version_is_a_release_build() {
        assert_eq!(
            PeppyBuild::from_git_tag(Some("v0.31.2")),
            release("v0.31.2")
        );
        for tag in [
            "v0.31.2-rc1",
            "0.31.2",
            "v0.31",
            "v0.31.2.1",
            "v0..2",
            "dev-5b706c5a0f1e",
            "test",
            "",
        ] {
            assert_eq!(
                PeppyBuild::from_git_tag(Some(tag)),
                PeppyBuild::Unreleased,
                "{tag}"
            );
        }
        assert_eq!(PeppyBuild::from_git_tag(None), PeppyBuild::Unreleased);
    }

    #[test]
    fn a_release_build_reads_its_own_release_tag_by_its_full_name() {
        let read = GitRepoRef::PeppyRelease
            .read_ref(&release("v0.31.2"))
            .expect("the keyword names a ref");
        assert_eq!(read.short_name(), "peppy-release/v0.31.2");
        assert_eq!(read.git_name(), "refs/tags/peppy-release/v0.31.2");
    }

    #[test]
    fn any_other_build_reads_main() {
        let read = GitRepoRef::PeppyRelease
            .read_ref(&PeppyBuild::Unreleased)
            .expect("the keyword names a ref");
        assert_eq!(read, ReadRef::Named("main".to_owned()));
        assert_eq!(read.git_name(), "main");
    }

    #[test]
    fn a_read_ref_is_stored_as_its_git_name_and_read_back() {
        for read in [
            ReadRef::ReleaseTag("v0.31.2".to_owned()),
            ReadRef::Named("main".to_owned()),
            ReadRef::Named("peppy-release/v0.31.2".to_owned()),
            ReadRef::Named("refs/tags/peppy-release/next".to_owned()),
            ReadRef::Named("0123456789abcdef0123456789abcdef01234567".to_owned()),
        ] {
            let stored = serde_json::to_string(&read).expect("serialize");
            assert_eq!(stored, format!("\"{}\"", read.git_name()));
            let read_back: ReadRef = serde_json::from_str(&stored).expect("deserialize");
            assert_eq!(read_back, read, "{stored}");
        }
    }

    /// The short name of a release tag is a branch or a tag the user can
    /// name: only the full name `refs/tags/peppy-release/<version>` reads as
    /// the release tag.
    #[test]
    fn only_the_full_name_of_a_release_tag_reads_back_as_the_release_tag() {
        assert_eq!(
            ReadRef::from_git_name("refs/tags/peppy-release/v0.31.2"),
            ReadRef::ReleaseTag("v0.31.2".to_owned())
        );
        assert_eq!(
            ReadRef::from_git_name("peppy-release/v0.31.2"),
            ReadRef::Named("peppy-release/v0.31.2".to_owned())
        );
    }

    #[test]
    fn the_wire_text_of_a_ref_decodes_back_to_it() {
        for repo_ref in [
            GitRepoRef::RemoteHead,
            GitRepoRef::PeppyRelease,
            GitRepoRef::Named("feat/gripper-force".to_owned()),
        ] {
            assert_eq!(
                decode_git_repo_ref(&repo_ref.to_string()).expect("decode"),
                repo_ref
            );
        }
        assert!(decode_git_repo_ref("@{upstream}").is_err());
    }

    #[test]
    fn a_named_ref_reads_itself_in_every_build() {
        let named = GitRepoRef::Named("peppy-release/v0.30.0".to_owned());
        for build in [release("v0.31.2"), PeppyBuild::Unreleased] {
            let read = named.read_ref(&build).expect("a named ref");
            assert_eq!(read.git_name(), "peppy-release/v0.30.0");
            assert_eq!(read.short_name(), "peppy-release/v0.30.0");
        }
        assert_eq!(GitRepoRef::RemoteHead.read_ref(&release("v0.31.2")), None);
    }
}
