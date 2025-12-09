use std::path::PathBuf;
use std::sync::Arc;

use config::peppy_config::PeppyLauncherParser;

use super::{Command, Error as CommandError};
use crate::AppContext;

pub struct LaunchCommand {
    /// Path to the launch file (mandatory)
    pub launcher_config_path: PathBuf,
}

impl Command for LaunchCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        let _peppy_config = PeppyLauncherParser::from_path(&self.launcher_config_path)
            .map_err(crate::Error::PeppyConfig)?;
        todo!()
    }
}
