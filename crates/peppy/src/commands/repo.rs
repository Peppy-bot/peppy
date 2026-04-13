mod add;
mod exclude;
mod list;
mod refresh;
mod remove;

use std::sync::Arc;

use clap::Subcommand;
use core_node::encoding::RepoSource;

use super::Command;
use crate::{context::AppContext, error::Result};

/// Human-readable label for a repository source (used in CLI output).
pub(super) fn repo_source_label(source: &RepoSource) -> String {
    match source {
        RepoSource::Fs(path) => path.to_string_lossy().into_owned(),
        RepoSource::Git { repo_url, repo_ref } => match repo_ref {
            Some(r) => format!("{repo_url} (ref: {r})"),
            None => repo_url.clone(),
        },
        RepoSource::Url(url) => url.clone(),
    }
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
    },
    /// Remove a repository
    Remove {
        /// Repository ID to remove (shown by `peppy repo list`)
        id: u32,
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
            RepoCommands::Add { source, git_ref } => add::add_repo(ctx, &source, git_ref),
            RepoCommands::Remove { id } => remove::remove_repo(ctx, id),
            RepoCommands::Exclude { source, git_ref } => {
                exclude::exclude_repo(ctx, &source, git_ref)
            }
        }
    }
}
