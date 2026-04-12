use std::sync::Arc;

use clap::Subcommand;

use super::Command;
use crate::{context::AppContext, error::Result};

#[derive(Subcommand)]
pub enum RepoCommands {
    /// List configured repositories
    List,
    /// Update repository indexes
    Update,
    /// Add a new repository
    Add,
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
            RepoCommands::Update => todo!("repo update"),
            RepoCommands::Add => todo!("repo add"),
            RepoCommands::Remove => todo!("repo remove"),
        }
    }
}
