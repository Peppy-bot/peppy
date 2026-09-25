mod add;
mod exclude;
mod index;
mod init;
mod list;
mod refresh;
mod remove;
mod search;
mod show;

pub use index::{CheckScope, check_index, repo_index};
pub use init::repo_init_with_dirs;
pub use search::search_rendered;
pub use show::show_rendered;

use std::sync::Arc;

use clap::Subcommand;
use core_node_api::encoding::{GitRepoRef, RepoSource};
use daemon_config::consts::peppy_build;
use std::path::PathBuf;

use super::Command;
use crate::{context::AppContext, error::Result};

/// Human-readable label for a repository source as this binary reads it
/// (used in CLI output).
pub(super) fn repo_source_label(source: &RepoSource) -> String {
    source.display_label(&peppy_build())
}

/// Parses `--ref` into the same value a `ref` in `repositories.json5`
/// parses into, so the command line and the file cannot disagree about
/// `@{peppy-release}`.
fn parse_git_ref(raw: &str) -> std::result::Result<GitRepoRef, String> {
    GitRepoRef::parse(raw).map_err(|e| e.to_string())
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
    /// Search the repositories for any indexed item (node, launcher,
    /// contract, pairing, MCP exposure). Every match is listed with its
    /// kind, repository, path, and fingerprint; `repo show` takes the
    /// same query and prints each match's full report instead.
    ///
    /// Each query part is an unanchored regular expression, as `apt
    /// search` reads its patterns: `camera` finds `rgb_camera`,
    /// `^rgb_camera$` only the exact name, `cam(era)?:v[12]` any of four
    /// identities. A missing tag part matches every tag (launchers carry
    /// none), and `@<sha256>` keeps only the copies carrying exactly
    /// those bytes.
    ///
    /// Reads this machine's repository caches, so it reflects the last
    /// `peppy repo refresh`; needs no daemon, and `--core-node` has no
    /// effect on it.
    Search {
        /// The query, as `<name-regex>[:<tag-regex>][@<sha256>]`.
        #[arg(allow_hyphen_values = true)]
        query: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the full report of every indexed item the query matches, as
    /// `apt show` prints a package's record: where each document is
    /// published and, for a contract or pairing, the nodes that
    /// implement, consume, participate in, or observe it, with each pin
    /// checked.
    ///
    /// Takes the same `<name-regex>[:<tag-regex>][@<sha256>]` query as
    /// `repo search` and reports every identity it matches, in the
    /// search's order. A query nothing matches is an error, and so is an
    /// identity one repository publishes twice, since its report has no
    /// answer a launch would accept.
    ///
    /// Reads the same caches as `repo search`; needs no daemon, and
    /// `--core-node` has no effect on it.
    Show {
        /// The query, as `<name-regex>[:<tag-regex>][@<sha256>]`.
        #[arg(allow_hyphen_values = true)]
        query: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Update repository indexes
    ///
    /// A repository that cannot be read does not fail the command: it keeps
    /// the entries it last published, every other repository updates, and
    /// the command names it in a report. With `--strict`, the same report
    /// fails the command, for CI and release checks that must not go on with
    /// a repository they could not read.
    #[clap(alias = "update")]
    Refresh {
        /// Fail when any repository could not be read.
        #[arg(long)]
        strict: bool,
    },
    /// Add a new repository
    Add {
        /// Repository source.
        ///
        /// Supported formats:
        /// - Local path: `/path/to/directory`
        /// - Git URL: `https://github.com/org/repo.git`
        /// - Git URL with ref: `https://github.com/org/repo.git --ref <branch, tag, commit, or @{peppy-release}>`
        /// - Plain URL: `https://example.com/packages`
        source: String,
        /// Git ref to track (git sources only): a branch, a tag, a commit,
        /// or `@{peppy-release}`, which reads the tag
        /// `peppy-release/<version>` of this peppy release (the hub content
        /// the release was tested with; only the Peppy-bot hubs carry these
        /// tags), or `main` in a build without a release version.
        #[arg(long = "ref", value_parser = parse_git_ref)]
        git_ref: Option<GitRepoRef>,
        /// Give the new repo top priority (assigns an id below the current min).
        #[arg(long)]
        top: bool,
        /// Register the repository under this exact id instead of the next
        /// free one. Take ids from the reserved band >= 2000: peppy's
        /// bundled defaults (see its `assets/default_repositories.json5`)
        /// stay below 2000, so a pinned id can never collide with a default
        /// a future peppy release ships — which would silently skip that
        /// default, since defaults are only appended for ids not taken.
        #[arg(long, conflicts_with = "top")]
        id: Option<u64>,
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
        /// - Git URL with ref: `https://github.com/org/repo.git --ref <branch, tag, commit, or @{peppy-release}>`
        /// - Plain URL: `https://example.com/packages`
        source: String,
        /// Git ref of the repository to exclude (git sources only), as it is
        /// written in `repositories.json5`: a branch, a tag, a commit, or
        /// `@{peppy-release}`.
        #[arg(long = "ref", value_parser = parse_git_ref)]
        git_ref: Option<GitRepoRef>,
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
            RepoCommands::Search { query, json } => search::repo_search(&query, json),
            RepoCommands::Show { query, json } => show::repo_show(&query, json),
            RepoCommands::Refresh { strict } => refresh::repo_refresh(ctx, strict),
            RepoCommands::Add {
                source,
                git_ref,
                top,
                id,
            } => add::add_repo(ctx, &source, git_ref, top, id),
            RepoCommands::Remove { id } => remove::remove_repo(ctx, id),
            RepoCommands::Exclude { source, git_ref } => {
                exclude::exclude_repo(ctx, &source, git_ref)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct RepoCli {
        #[command(subcommand)]
        command: RepoCommands,
    }

    fn add_ref(git_ref: &str) -> std::result::Result<Option<GitRepoRef>, clap::Error> {
        RepoCli::try_parse_from([
            "repo",
            "add",
            "https://github.com/org/repo.git",
            "--ref",
            git_ref,
        ])
        .map(|cli| match cli.command {
            RepoCommands::Add { git_ref, .. } => git_ref,
            _ => unreachable!("`repo add` parses into `Add`"),
        })
    }

    fn exclude_ref(git_ref: &str) -> std::result::Result<Option<GitRepoRef>, clap::Error> {
        RepoCli::try_parse_from([
            "repo",
            "exclude",
            "https://github.com/org/repo.git",
            "--ref",
            git_ref,
        ])
        .map(|cli| match cli.command {
            RepoCommands::Exclude { git_ref, .. } => git_ref,
            _ => unreachable!("`repo exclude` parses into `Exclude`"),
        })
    }

    /// `--ref` parses into the value the same `ref` in `repositories.json5`
    /// parses into, for `repo add` and `repo exclude` alike.
    #[test]
    fn ref_on_the_command_line_parses_as_in_the_file() {
        for parse in [add_ref, exclude_ref] {
            for raw in [
                "@{peppy-release}",
                "peppy-release/v0.31.2",
                "peppy-release",
                "main",
            ] {
                assert_eq!(
                    parse(raw).expect(raw),
                    Some(GitRepoRef::parse(raw).expect(raw)),
                    "{raw}"
                );
            }
        }
    }

    #[test]
    fn ref_on_the_command_line_is_refused_as_in_the_file() {
        for parse in [add_ref, exclude_ref] {
            for raw in [
                "@{Peppy-Release}",
                "@{peppy-releases}",
                "@{upstream}",
                "main@{1}",
            ] {
                let error = parse(raw).expect_err(raw).to_string();
                let refusal = GitRepoRef::parse(raw).expect_err(raw).to_string();
                assert!(error.contains(&refusal), "{raw}: {error}");
            }
        }
    }
}
