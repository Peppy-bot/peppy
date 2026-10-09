mod catalog;
mod serve;

use std::sync::Arc;

use clap::Subcommand;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::Result;

pub use catalog::mcp_catalog_rendered;

#[derive(Subcommand)]
pub enum McpCommands {
    /// Serve the exposures of a launcher deployment.
    ///
    /// Started by the daemon for every `source: { exposures: [...] }`
    /// deployment, which hands the process its runtime configuration and
    /// the pinned exposure and contract documents through the environment.
    /// Serves each exposure at `/<name>/<tag>/mcp` on the deployment's port.
    Serve,
    /// Print the catalog the server derives for an exposure.
    ///
    /// Resolves `<name>:<tag>` and the contracts it references through the
    /// local repository caches (run `peppy repo refresh` first), takes the
    /// daemon interfaces it references from this peppy, and prints the
    /// derived catalog as JSON: what a running endpoint for the exposure
    /// advertises through discovery and the list methods. The schemas of a
    /// daemon target are the interface's own: a running endpoint narrows them
    /// with the scope its launcher gives the target, which belongs to an
    /// instance and not to the exposure.
    Catalog {
        /// The exposure, as `<name>:<tag>`.
        exposure: String,
    },
}

pub struct McpCommand {
    pub command: McpCommands,
}

impl Command for McpCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        match self.command {
            McpCommands::Serve => serve::mcp_serve(),
            McpCommands::Catalog { exposure } => catalog::mcp_catalog(&exposure),
        }
    }
}
