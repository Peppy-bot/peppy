use peppy::init::init_peppy_config;
use std::path::Path;
use tracing::{error, info};

pub fn setup(current_dir: &Path) {
    info!("Setup function called!");

    match init_peppy_config(current_dir) {
        Ok(path) => info!("Initialized peppy.json5 at: {:?}", path),
        Err(e) => error!("Failed to initialize: {}", e),
    }
}
