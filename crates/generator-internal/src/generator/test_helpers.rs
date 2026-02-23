macro_rules! assert_rendered {
    ($cond:expr, $rendered:expr, $($arg:tt)+) => {
        if !$cond {
            eprintln!("rendered output:\n{}", $rendered);
            panic!($($arg)+);
        }
    };
}

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

use super::types::InterfaceArtifact;

pub const STUB_NODE_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: {
    name: "generated_node",
    tag: "0.1.0",
    language: "rust",
    start_cmd: ["./target/release/generated_node"]
  }
}
"#;

pub fn prepare_directories(
    temp_dir: &TempDir,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let user_node = temp_dir.path().join("user_node");
    let output_dir = user_node.join(PEPPYGEN_OUTPUT_PATH);
    let peppy_node_config = user_node.join(NODE_CONFIG_FILE);
    fs::create_dir_all(&output_dir).unwrap();
    fs::create_dir_all(&user_node).unwrap();
    fs::write(&peppy_node_config, STUB_NODE_CONFIG).unwrap();
    (output_dir, user_node, peppy_node_config)
}

pub fn init_test_env<G: Default>(
    temp_dir: &TempDir,
) -> (
    G,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let (output_dir, user_node, peppy_node_config_path) = prepare_directories(temp_dir);
    (G::default(), output_dir, user_node, peppy_node_config_path)
}

pub fn copy_config_to_output(user_node: &Path, output_dir: &Path) -> std::path::PathBuf {
    let source = user_node.join(NODE_CONFIG_FILE);
    let destination = output_dir.join(NODE_CONFIG_FILE);
    fs::copy(&source, &destination).unwrap();
    destination
}

/// Converts artifacts into a vector of generated code strings.
pub fn render_artifacts(artifacts: Vec<InterfaceArtifact>) -> Vec<String> {
    artifacts
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect()
}

/// Asserts that all given patterns are present in the rendered output.
pub fn assert_contains_all(rendered: &str, patterns: &[&str]) {
    for pattern in patterns {
        assert_rendered!(
            rendered.contains(pattern),
            rendered,
            "expected to find: {:?}",
            pattern
        );
    }
}

/// Asserts that at least one artifact contains the given pattern.
pub fn assert_artifact_contains(artifacts: &[String], pattern: &str) {
    let rendered = artifacts.join("\n");
    assert_rendered!(
        artifacts.iter().any(|artifact| artifact.contains(pattern)),
        &rendered,
        "expected an artifact containing pattern: {:?}",
        pattern
    );
}
