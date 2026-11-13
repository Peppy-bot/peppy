//! Where a cached or pinned item's bytes live, and which revision they were
//! read at.
//!
//! One vocabulary for two consumers: the repo caches record an origin per
//! entry, and a launch pin carries one to every daemon in the launch. Sharing
//! the type is what keeps "what a cache knows" and "what a coordinator ships"
//! from drifting apart.

use super::types::{GitCommit, RepoRelativePath};
use core_node_api::encoding::{
    ArtifactPlatform, ArtifactTag, OciDigest, OciRepository, ReadRef, RepoSourceKind,
};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Where an item's bytes live, and which revision they were read at.
///
/// A branch name is not a revision: two machines following `main` a week
/// apart hold different trees while recording the same string. The commit
/// is what makes an entry mean one set of bytes rather than "whatever that
/// branch pointed at when this machine last looked".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "source_type", rename_all = "lowercase")]
pub enum EntryOrigin {
    /// A tree on this machine. Nothing off this machine can read it, which
    /// is why a deployment placed on another core node cannot be backed by
    /// one.
    Fs {
        /// Absolute path to the file that declares the item.
        path: PathBuf,
    },
    Git {
        repo_url: String,
        /// The ref the repository is configured to follow, as written in
        /// `repositories.json5` (`@{peppy-release}` included), absent when it
        /// follows whatever the remote's default branch is. It is what
        /// `RepoOwners` attributes an entry to its configured repository by,
        /// together with the url.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo_ref: Option<String>,
        /// The git ref the tree was read at: the configured ref, or the ref
        /// `@{peppy-release}` resolved to in the peppy that read it. Absent
        /// when the repository follows the remote's default branch. Kept
        /// beside the commit because a fetch starts from a ref before it can
        /// reach a commit, and `@{peppy-release}` is no ref git can fetch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        read_ref: Option<ReadRef>,
        /// The commit the tree was read at.
        commit: GitCommit,
        /// Path within the repository to the file that declares the item.
        path: RepoRelativePath,
    },
    /// Prebuilt `.sif` artifacts of a container node in an OCI repository,
    /// one for each platform the repository holds for the peppy version
    /// that read it. Only nodes have this origin.
    Oci {
        url: OciRepository,
        /// The revision of the node that every artifact was built from: the
        /// sha256 of its staged tree without the vendored peppylib. The
        /// artifacts of all platforms share it.
        source_digest: OciDigest,
        /// The artifact of each platform. A machine runs the one of its own
        /// platform.
        platforms: BTreeMap<ArtifactPlatform, OciArtifact>,
    },
}

/// One artifact of a node in an OCI repository: its tag, the digest of its
/// manifest, and the digest of the `.sif` layer that the manifest holds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OciArtifact {
    pub tag: ArtifactTag,
    pub manifest_digest: OciDigest,
    pub sif_digest: OciDigest,
}

impl EntryOrigin {
    pub fn kind(&self) -> RepoSourceKind {
        match self {
            Self::Fs { .. } => RepoSourceKind::Fs,
            Self::Git { .. } => RepoSourceKind::Git,
            Self::Oci { .. } => RepoSourceKind::Oci,
        }
    }

    /// Where the item is within its repository, as the cache spells it: the
    /// absolute path of the declaring file for fs, its repository-relative
    /// path for git, and the artifact tags for oci. What error messages,
    /// listings, search results and refresh reports quote.
    pub fn path_str(&self) -> Cow<'_, str> {
        match self {
            // A path that is not valid UTF-8 renders as the fixed text
            // `<non-UTF-8 path>` rather than failing the whole cache: the
            // value is only shown to the user, and repository attribution
            // matches the `PathBuf` itself.
            Self::Fs { path } => Cow::Borrowed(path.to_str().unwrap_or("<non-UTF-8 path>")),
            Self::Git { path, .. } => Cow::Borrowed(path.as_str()),
            Self::Oci { platforms, .. } => Cow::Owned(
                platforms
                    .values()
                    .map(|artifact| artifact.tag.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        }
    }

    /// The `(repo_url, commit)` this origin resolves through the checkout
    /// cache, and `None` for a filesystem origin, which resolves in place.
    ///
    /// The one statement of what keeps a cached checkout reachable, so
    /// pruning them cannot go looking in a different place than resolution
    /// does.
    pub fn checkout(&self) -> Option<(&str, &GitCommit)> {
        match self {
            Self::Fs { .. } | Self::Oci { .. } => None,
            Self::Git {
                repo_url, commit, ..
            } => Some((repo_url, commit)),
        }
    }

    /// Whether resolving this origin to bytes on disk can block on the
    /// network: a git origin may clone or fetch, and an oci origin may read
    /// a blob from its registry, while a filesystem origin already names a
    /// file on this machine. Callers inside tokio use it to decide whether
    /// resolution needs [`tokio::task::spawn_blocking`].
    pub fn resolution_may_block(&self) -> bool {
        match self {
            Self::Fs { .. } => false,
            Self::Git { .. } | Self::Oci { .. } => true,
        }
    }

    /// The remote a git origin was read from. `None` for a filesystem or an
    /// oci origin, which [`EntryOrigin::oci_repository`] names.
    pub fn repo_url(&self) -> Option<&str> {
        match self {
            Self::Fs { .. } | Self::Oci { .. } => None,
            Self::Git { repo_url, .. } => Some(repo_url),
        }
    }

    /// The ref a git origin is configured to follow. `None` for a
    /// filesystem or an oci origin, and for a git origin that follows the
    /// remote's default branch.
    pub fn repo_ref(&self) -> Option<&str> {
        match self {
            Self::Fs { .. } | Self::Oci { .. } => None,
            Self::Git { repo_ref, .. } => repo_ref.as_deref(),
        }
    }

    /// The commit a git origin was read at. `None` for a filesystem origin,
    /// which has no revision to record, and for an oci origin, whose
    /// revision is its source digest.
    pub fn commit(&self) -> Option<&GitCommit> {
        match self {
            Self::Fs { .. } | Self::Oci { .. } => None,
            Self::Git { commit, .. } => Some(commit),
        }
    }

    /// The OCI repository an oci origin holds its artifacts in. `None` for
    /// a filesystem or a git origin.
    pub fn oci_repository(&self) -> Option<&OciRepository> {
        match self {
            Self::Oci { url, .. } => Some(url),
            Self::Fs { .. } | Self::Git { .. } => None,
        }
    }
}
