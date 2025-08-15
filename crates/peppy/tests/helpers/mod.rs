use std::path::Path;

use peppy::commands::init;

pub fn setup(current_dir: &Path) {
    println!("Setup function called!");

    match init::init(&current_dir) {
        Ok(path) => println!("Initialized peppy.star at: {:?}", path),
        Err(e) => eprintln!("Failed to initialize: {}", e),
    }
}
