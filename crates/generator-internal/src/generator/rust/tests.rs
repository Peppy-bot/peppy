mod actions;
mod golden;
mod pairings;
mod parameters;
mod services;
mod testing;
mod topics;

use super::*;
use crate::generator::test_helpers::*;
use config::consts::NODE_CONFIG_FILE;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Builds the crate of `generator` into `output_dir` with the default data
/// root and deploy mode.
fn build_crate(generator: RustGenerator, output_dir: &Path) {
    generator
        .build(
            output_dir,
            &daemon_config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
}

/// [`build_crate`] with a copy of the stub node config of `user_node` in
/// `output_dir` during the build. The copy is removed afterwards, so the built
/// crate holds no node config.
fn build_crate_with_node_config(generator: RustGenerator, output_dir: &Path, user_node: &Path) {
    let output_config = copy_config_to_output(user_node, output_dir);
    build_crate(generator, output_dir);
    fs::remove_file(output_config).unwrap();
}
