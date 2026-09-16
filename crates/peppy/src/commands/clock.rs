//! `peppy clock list`: the clock domains this federation is running.

pub mod list;

use super::Command;
use crate::{context::AppContext, error::Error as CommandError};
use clap::Subcommand;
use std::sync::Arc;

#[derive(Subcommand)]
pub enum ClockCommands {
    /// List the clock domains running across this federation: who supplies
    /// each one, whether it has started, and what reads it.
    List {
        /// Emit the listing as JSON.
        #[arg(long)]
        json: bool,
    },
}

pub struct ClockCommand {
    pub command: ClockCommands,
}

impl Command for ClockCommand {
    fn execute(self, app_ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            ClockCommands::List { json } => list::list_clocks(app_ctx, json),
        }
    }
}
