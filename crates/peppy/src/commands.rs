pub mod error;
pub mod init;
pub mod node;
pub mod pixi;
pub mod serve;
pub mod sync;

use error::CommandError;

/// Trait for executable commands
pub trait Command {
    /// Execute the command
    fn execute(self) -> Result<(), CommandError>;
}
