mod add;
mod list;
mod refresh;
mod remove;

use std::sync::Arc;

use clap::Subcommand;
use tracing::info;

use super::Command;
use crate::commands::node::source;
use crate::{context::AppContext, error::Error, error::Result};

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
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        match self.command {
            RepoCommands::List => todo!("repo list"),
            RepoCommands::Refresh => todo!("repo update"),
            RepoCommands::Add { source, git_ref } => add_repo(&source, git_ref),
            RepoCommands::Remove => todo!("repo remove"),
        }
    }
}

fn add_repo(source_str: &str, git_ref: Option<String>) -> Result<()> {
    if !source::is_probably_remote_source(source_str) {
        return Err(Error::ExecutionFailed(format!(
            "'{source_str}' is not a valid repository URL. Expected a git URL \
             (e.g. https://github.com/org/repo.git)."
        )));
    }

    // Plain HTTP archives / URLs are not repository sources.
    if let Ok(url) = url::Url::parse(source_str)
        && matches!(url.scheme(), "http" | "https")
        && !looks_like_git_url(source_str)
    {
        return Err(Error::ExecutionFailed(
            "URL repositories are not supported yet. \
             Please provide a git repository URL (e.g. https://github.com/org/repo.git)."
                .to_string(),
        ));
    }

    let (repo_url, _repo_path) = source::parse_git_repo_url_and_path(source_str)?;
    info!("Adding repository {} (ref: {:?})", repo_url, git_ref);

    todo!("repo add: store repository entry")
}

/// Returns `true` when the URL looks like a git repository
/// (ends with `.git` or uses `git@` / `ssh://` scheme).
fn looks_like_git_url(source: &str) -> bool {
    source.contains(".git") || source.starts_with("git@") || source.starts_with("ssh://")
}
