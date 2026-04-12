mod add;
mod list;
mod refresh;
mod remove;

use std::sync::Arc;

use clap::Subcommand;

use super::Command;
use crate::{context::AppContext, error::Result};

#[derive(Subcommand)]
pub enum RepoCommands {
    /// List configured repositories
    List,
    /// Update repository indexes
    #[clap(alias = "update")]
    Refresh,
    /// Add a new repository
    Add {
        /// Repository source (git URL).
        ///
        /// Supported formats:
        /// - Git URL: `https://github.com/org/repo.git`
        /// - Git URL with ref: `https://github.com/org/repo.git --ref tag-or-branch`
        source: String,
        /// Git ref (tag/branch/commit) to track (git sources only).
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
    /// Remove a repository
    Remove,
}

pub struct RepoCommand {
    pub command: RepoCommands,
}

impl Command for RepoCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        match self.command {
            RepoCommands::List => todo!("repo list"),
            RepoCommands::Refresh => todo!("repo update"),
            RepoCommands::Add { source, git_ref } => add::add_repo(ctx, &source, git_ref),
            RepoCommands::Remove => todo!("repo remove"),
        }
    }
}
