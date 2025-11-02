macro_rules! assert_rendered {
    ($cond:expr, $rendered:expr, $($arg:tt)+) => {
        if !$cond {
            eprintln!("rendered output:\n{}", $rendered);
            panic!($($arg)+);
        }
    };
}

mod actions;
mod libgen;
mod services;
mod topics;

use super::*;
use std::{fs, path::Path};
use tempfile::TempDir;

const STUB_NODE_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: {
    name: "generated_node",
    tag: "0.1.0",
    launch_cmd: ["cargo", "run", "--release"]
  },
  logging: {
    min_level: "info",
    format: "text"
  }
}
"#;

fn prepare_directories(temp_dir: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let output_dir = temp_dir.path().join(".peppy/libs/peppygen");
    let user_node = temp_dir.path().join("user_node");
    fs::create_dir_all(&output_dir).unwrap();
    fs::create_dir_all(&user_node).unwrap();
    fs::write(user_node.join(PEPPY_NODE_CONFIG_FILE), STUB_NODE_CONFIG).unwrap();
    (output_dir, user_node)
}

fn init_test_env(temp_dir: &TempDir) -> (RustGenerator, std::path::PathBuf, std::path::PathBuf) {
    let (output_dir, user_node) = prepare_directories(temp_dir);
    (RustGenerator::new(), output_dir, user_node)
}

fn copy_config_to_output(user_node: &Path, output_dir: &Path) -> std::path::PathBuf {
    let source = user_node.join(PEPPY_NODE_CONFIG_FILE);
    let destination = output_dir.join(PEPPY_NODE_CONFIG_FILE);
    fs::copy(&source, &destination).unwrap();
    destination
}
