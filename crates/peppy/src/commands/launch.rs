use std::path::PathBuf;

use super::{Command, Error as CommandError};
use crate::AppContext;

pub struct LaunchCommand {
    /// Launch file (mandatory)
    pub launch_file: PathBuf,
}

impl Command for LaunchCommand {
    fn execute(self, ctx: &AppContext) -> Result<(), CommandError> {
        todo!()
    }
}
