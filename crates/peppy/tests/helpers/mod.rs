use peppy::init::init_root_node;
use std::path::Path;
use tracing::{error, info};

pub fn setup(current_dir: &Path) {
    info!("Setup function called!");

    match init_root_node(current_dir, "test_root") {
        Ok(path) => info!("Initialized peppy.json5 at: {:?}", path),
        Err(e) => error!("Failed to initialize: {}", e),
    }
}
