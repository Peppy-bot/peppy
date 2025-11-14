pub mod install;
pub mod node;
pub mod serve;
pub mod service;

use crate::{AppContext, Error, Result};

/// Trait for executable commands
pub trait Command {
    /// Execute the command
    fn execute(self, ctx: &AppContext) -> Result<()>;
}
