use super::Command;
use crate::context::AppContext;
use crate::error::Error as CommandError;
use config::peppy_config::PeppyLauncherParser;
use std::path::PathBuf;
use std::sync::Arc;

pub struct LaunchCommand {
    /// Path to the launch file (mandatory)
    pub launcher_config_path: PathBuf,
}

impl Command for LaunchCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        let _peppy_config = PeppyLauncherParser::from_path(&self.launcher_config_path)
            .map_err(CommandError::PeppyConfig)?;
        todo!()
    }
}
