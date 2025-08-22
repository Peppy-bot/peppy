pub mod init;
pub mod node;
pub mod pixi;
pub mod serve;
pub mod sync;

use crate::{Error, Result};

/// Trait for executable commands
pub trait Command {
    /// Execute the command
    fn execute(self) -> Result<()>;
}
