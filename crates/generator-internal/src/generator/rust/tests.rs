mod actions;
mod parameters;
mod services;
mod topics;

use super::*;
use crate::generator::test_helpers::*;
use config::consts::NODE_CONFIG_FILE;
use config::consts::PEPPYGEN_OUTPUT_PATH;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const STUB_NODE_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: {
    name: "generated_node",
    tag: "0.1.0",
    language: "rust",
    start_cmd: ["cargo", "run", "--release"]
  }
}
"#;

fn prepare_directories(
    temp_dir: &TempDir,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let output_dir = temp_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    let user_node = temp_dir.path().join("user_node");
    let peppy_node_config = user_node.join(NODE_CONFIG_FILE);
    fs::create_dir_all(&output_dir).unwrap();
    fs::create_dir_all(&user_node).unwrap();
    fs::write(&peppy_node_config, STUB_NODE_CONFIG).unwrap();
    (output_dir, user_node, peppy_node_config)
}

fn init_test_env(
    temp_dir: &TempDir,
) -> (
    RustGenerator,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let (output_dir, user_node, peppy_node_config_path) = prepare_directories(temp_dir);
    (
        RustGenerator::new(),
        output_dir,
        user_node,
        peppy_node_config_path,
    )
}

fn copy_config_to_output(user_node: &Path, output_dir: &Path) -> std::path::PathBuf {
    let source = user_node.join(NODE_CONFIG_FILE);
    let destination = output_dir.join(NODE_CONFIG_FILE);
    fs::copy(&source, &destination).unwrap();
    destination
}
