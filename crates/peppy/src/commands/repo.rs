mod add;
mod exclude;
mod list;
mod refresh;
mod remove;

use std::sync::Arc;

use clap::Subcommand;
use core_node_api::encoding::RepoSource;

use super::Command;
use crate::{context::AppContext, error::Result};

/// Human-readable label for a repository source (used in CLI output).
pub(super) fn repo_source_label(source: &RepoSource) -> String {
    source.display_label()
}

#[derive(Subcommand)]
pub enum RepoCommands {
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
