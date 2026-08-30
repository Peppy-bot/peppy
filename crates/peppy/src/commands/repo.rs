mod add;
mod exclude;
mod index;
mod init;
mod list;
mod refresh;
mod remove;
mod search;

pub use index::{CheckScope, check_index, repo_index};
pub use init::repo_init_with_dirs;
pub use search::search_rendered;

use std::sync::Arc;

use clap::Subcommand;
use core_node_api::encoding::RepoSource;
use std::path::PathBuf;

use super::Command;
use crate::{context::AppContext, error::Result};

#[cfg(test)]
mod tests {
    use super::paint;

    #[test]
    fn paint_is_a_no_op_without_colour() {
        assert_eq!(paint("  (conflict)", "\x1b[31m", false), "  (conflict)");
        assert_eq!(
            paint("  (conflict)", "\x1b[31m", true),
            "\x1b[31m  (conflict)\x1b[0m"
        );
    }
}

/// Human-readable label for a repository source (used in CLI output).
pub(super) fn repo_source_label(source: &RepoSource) -> String {
    source.display_label()
}

/// Orange, for the states that are not errors but are not the plain answer
/// either: entries kept from an earlier read, and an identity a
/// higher-priority repository already answers.
pub(super) const ORANGE: &str = "\x1b[38;5;208m";
/// Red, for what does not resolve at all.
pub(super) const RED: &str = "\x1b[31m";

pub(super) fn paint(text: &str, colour: &str, colorize: bool) -> String {
    if colorize {
        format!("{colour}{text}\x1b[0m")
    } else {
        text.to_owned()
    }
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
    ///
    /// With `--check --validate-mcp-exposures`, also validates every
    /// `mcp_exposure/v1` document the index lists against the contracts it
    /// references, resolved through this machine's repository caches, and
    /// reports every violation of every exposure at once. A contract the
    /// caches cannot resolve is an error naming it, never a pass: register
    /// and refresh the contract repository first (`peppy repo add`,
    /// `peppy repo refresh`), as a hub's CI does.
    Index {
        /// Repository root to index (defaults to the current directory).
        path: Option<PathBuf>,
        /// Verify the committed index instead of writing it.
        #[arg(long)]
        check: bool,
        /// With `--check`: validate the listed `mcp_exposure/v1` documents
        /// against the contracts they reference, through the repository
        /// caches.
        #[arg(long, requires = "check")]
        validate_mcp_exposures: bool,
    },
    /// List configured repositories
    List,
    /// Show who uses a contract or pairing: the nodes that implement,
    /// consume, participate in, or observe it, and whether their pins match
    /// what is published.
    ///
    /// Reads this machine's repository caches, so it reflects the last
    /// `peppy repo refresh`; needs no daemon, and `--core-node` has no
    /// effect on it.
    Search {
        /// The contract or pairing, as `<name>:<tag>`.
        identity: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
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
            RepoCommands::Index {
                path,
                check,
                validate_mcp_exposures,
            } => index::repo_index(
                path,
                match (check, validate_mcp_exposures) {
                    (false, _) => None,
                    (true, false) => Some(CheckScope::Index),
                    (true, true) => Some(CheckScope::IndexAndMcpExposures),
                },
            ),
            RepoCommands::List => list::list_repos(ctx),
            RepoCommands::Search { identity, json } => search::repo_search(&identity, json),
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
