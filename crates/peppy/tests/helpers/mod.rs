use std::path::Path;
use tracing::{error, info};

use peppy::commands::init;

pub fn setup(current_dir: &Path) {
    info!("Setup function called!");

    match init::init(current_dir) {
        Ok(path) => info!("Initialized peppy.star at: {:?}", path),
        Err(e) => error!("Failed to initialize: {}", e),
    }
}
