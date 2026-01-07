pub mod node;
pub mod service;
pub mod stack;

use std::sync::Arc;

use crate::{context::AppContext, error::Result};

/// Trait for executable commands
pub trait Command {
    /// Execute the command
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()>;
}
