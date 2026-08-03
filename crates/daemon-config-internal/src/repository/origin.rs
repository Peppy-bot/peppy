//! Where a cached or pinned item's bytes live, and which revision they were
//! read at.
//!
//! One vocabulary for two consumers: the repo caches record an origin per
//! entry, and a launch pin carries one to every daemon in the launch. Sharing
//! the type is what keeps "what a cache knows" and "what a coordinator ships"
//! from drifting apart.

use super::types::{GitCommit, RepoRelativePath};
use core_node_api::encoding::RepoSourceKind;
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
        /// The ref the repository is configured to follow, absent when it
        /// follows whatever the remote's default branch is. Kept beside the
        /// commit because a fetch starts from a ref before it can reach a
        /// commit, and because `entry_belongs_to_repo` attributes an entry
        /// to its configured repository by url and ref.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo_ref: Option<String>,
        /// The commit the tree was read at.
        commit: GitCommit,
        /// Path within the repository to the file that declares the item.
        path: RepoRelativePath,
    },
}

impl EntryOrigin {
    pub fn kind(&self) -> RepoSourceKind {
        match self {
            EntryOrigin::Fs { .. } => RepoSourceKind::Fs,
            EntryOrigin::Git { .. } => RepoSourceKind::Git,
        }
    }

    /// The path as the cache spells it: absolute for fs, repository-relative
    /// for git. What error messages quote and what repository attribution
    /// matches against.
    pub fn path_str(&self) -> &str {
        match self {
            // A path that is not valid UTF-8 renders lossily rather than
            // failing the whole cache: this is display and attribution, and
            // a lossy rendering still names the file the user has to look at.
            EntryOrigin::Fs { path } => path.to_str().unwrap_or("<non-UTF-8 path>"),
            EntryOrigin::Git { path, .. } => path.as_str(),
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
            EntryOrigin::Fs { .. } => None,
            EntryOrigin::Git {
                repo_url, commit, ..
            } => Some((repo_url, commit)),
        }
    }

    /// Whether resolving this origin to an on-disk path can block on the
    /// network: a git origin may clone or fetch, while a filesystem origin
    /// already names a file on this machine. Callers inside tokio use it to
    /// decide whether resolution needs [`tokio::task::spawn_blocking`].
    pub fn resolution_may_block(&self) -> bool {
        self.checkout().is_some()
    }

    /// The remote a git origin was read from. `None` for a filesystem
    /// origin, which has no remote.
    pub fn repo_url(&self) -> Option<&str> {
        match self {
            EntryOrigin::Fs { .. } => None,
            EntryOrigin::Git { repo_url, .. } => Some(repo_url),
        }
    }

    /// The ref a git origin is configured to follow. `None` for a
    /// filesystem origin, and for a git origin that follows the remote's
    /// default branch.
    pub fn repo_ref(&self) -> Option<&str> {
        match self {
            EntryOrigin::Fs { .. } => None,
            EntryOrigin::Git { repo_ref, .. } => repo_ref.as_deref(),
        }
    }

    /// The commit a git origin was read at. `None` for a filesystem
    /// origin, which has no revision to record.
    pub fn commit(&self) -> Option<&GitCommit> {
        match self {
            EntryOrigin::Fs { .. } => None,
            EntryOrigin::Git { commit, .. } => Some(commit),
        }
    }
}
