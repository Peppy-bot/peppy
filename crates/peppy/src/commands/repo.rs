mod add;
mod exclude;
mod index;
mod init;
mod list;
mod refresh;
mod remove;

pub use index::repo_index;
pub use init::repo_init_with_dirs;

use std::sync::Arc;

use clap::Subcommand;
use core_node_api::encoding::RepoSource;
use std::path::PathBuf;

use super::Command;
use crate::{context::AppContext, error::Result};

/// Human-readable label for a repository source (used in CLI output).
pub(super) fn repo_source_label(source: &RepoSource) -> String {
    source.display_label()
}

#[derive(Subcommand)]
pub enum RepoCommands {
    /// Sync the local `repositories.json5` with the bundled defaults.
    ///
    /// Creates the file if it does not exist, otherwise appends any default
    /// entries that are missing without touching existing user entries.
    /// Operates on the local config file directly; no daemon required.
    Init,
    /// Write a repository's `peppy_repository.json5` index, or verify it.
    ///
    /// Walks the repository, then records every identity it publishes and
    /// the file that declares each one. Refuses, naming both files, when one
    /// identity is claimed twice.
    ///
    /// With `--check`, verifies the committed index instead of writing it:
    /// every item in the tree must be listed, and every listed path must be
    /// a file inside the repository declaring exactly what it is filed
    /// under. Run it in CI so a repository cannot merge an index that has
    /// drifted from its contents. Needs no daemon.
    Index {
        /// Repository root to index (defaults to the current directory).
        path: Option<PathBuf>,
        /// Verify the committed index instead of writing it.
        #[arg(long)]
        check: bool,
    },
    /// List configured repositories
    List,
    /// Update repository indexes
    #[clap(alias = "update")]
    Refresh,
    /// Add a new repository
    Add {
        /// Repository source.
        ///
        /// Supported formats:
        /// - Local path: `/path/to/directory`
        /// - Git URL: `https://github.com/org/repo.git`
        /// - Git URL with ref: `https://github.com/org/repo.git --ref tag-or-branch`
        /// - Plain URL: `https://example.com/packages`
        source: String,
        /// Git ref (tag/branch/commit) to track (git sources only).
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Give the new repo top priority (assigns an id below the current min).
        #[arg(long)]
        top: bool,
    },
    /// Remove a repository
    Remove {
        /// Repository ID to remove (shown by `peppy repo list`)
        id: u64,
    },
    /// Exclude a repository
    Exclude {
        /// Repository source.
        ///
        /// Supported formats:
        /// - Local path: `/path/to/directory`
        /// - Git URL: `https://github.com/org/repo.git`
        /// - Git URL with ref: `https://github.com/org/repo.git --ref tag-or-branch`
        /// - Plain URL: `https://example.com/packages`
        source: String,
        /// Git ref (tag/branch/commit) to track (git sources only).
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
}

pub struct RepoCommand {
    pub command: RepoCommands,
}

impl Command for RepoCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        match self.command {
            RepoCommands::Init => init::repo_init(ctx),
            RepoCommands::Index { path, check } => index::repo_index(path, check),
            RepoCommands::List => list::list_repos(ctx),
            RepoCommands::Refresh => refresh::repo_refresh(ctx),
            RepoCommands::Add {
                source,
                git_ref,
                top,
            } => add::add_repo(ctx, &source, git_ref, top),
            RepoCommands::Remove { id } => remove::remove_repo(ctx, id),
            RepoCommands::Exclude { source, git_ref } => {
                exclude::exclude_repo(ctx, &source, git_ref)
            }
        }
    }
}
