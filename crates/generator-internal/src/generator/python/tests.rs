mod actions;
mod golden;
mod pairings;
mod parameters;
mod services;
mod testing;
mod topics;

use super::*;
use crate::generator::test_helpers::*;
use std::path::Path;

/// Builds the package of `generator` into `output_dir` with the default data
/// root and deploy mode.
fn build_package(generator: PythonGenerator, output_dir: &Path) {
    generator
        .build(
            output_dir,
            &daemon_config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
}
