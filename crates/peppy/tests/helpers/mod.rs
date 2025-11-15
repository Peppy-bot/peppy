use peppy::install::install_peppy_daemon;
use std::path::Path;
use tracing::{error, info};

pub fn setup(current_dir: &Path) {
    info!("Setup function called!");

    match install_peppy_daemon(current_dir) {
        Ok(path) => info!("Initialized peppy.json5 at: {:?}", path),
        Err(e) => error!("Failed to initialize: {}", e),
    }
}
