use peppy::commands::node::create;
use peppy::commands::node::types::{Language, NodeName};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_create_command_default_directory() {
    let temp_dir = TempDir::new().unwrap();
    super::helpers::setup(temp_dir.path());

    let node_name = "test_node";
    let result = create::create(
        temp_dir.path(),
        Some(temp_dir.path()),
        NodeName::new(node_name).unwrap(),
        Language::Rust,
        None,
    );
    let node_path = temp_dir.path().join(node_name);

    assert!(result.is_ok(), "Create should succeed: {:?}", result.err());
    assert!(node_path.exists(), "Node directory should exist");
    assert!(node_path.is_dir(), "Node should be a directory");

    assert!(
        node_path.join("pixi.toml").exists(),
        "pixi.toml should exist"
    );
    assert!(
        node_path.join("peppy.star").exists(),
        "peppy.star should exist"
    );
}

#[test]
fn test_create_command_with_to_dir() {
    let temp_dir = TempDir::new().unwrap();
    super::helpers::setup(temp_dir.path());

    let node_name = "test_node";
    let target_path = temp_dir.path().join(node_name);
    let result = create::create(
        temp_dir.path(),
        Some(temp_dir.path()),
        NodeName::new(node_name).unwrap(),
        Language::Rust,
        None,
    );

    assert!(result.is_ok(), "Create should succeed: {:?}", result.err());
    assert!(target_path.exists(), "Target directory should exist");
    assert!(target_path.is_dir(), "Target should be a directory");

    assert!(
        target_path.join("pixi.toml").exists(),
        "pixi.toml should exist"
    );
    assert!(
        target_path.join("peppy.star").exists(),
        "peppy.star should exist"
    );
}

#[test]
fn test_pixi_toml_content() {
    let temp_dir = TempDir::new().unwrap();
    super::helpers::setup(temp_dir.path());

    let node_name = "peppy-node";
    let node_path = temp_dir.path().join(node_name);
    let result = create::create(
        temp_dir.path(),
        Some(temp_dir.path()),
        NodeName::new(node_name).unwrap(),
        Language::Rust,
        None,
    );

    assert!(result.is_ok(), "Create should succeed: {:?}", result.err());

    let pixi_content = fs::read_to_string(node_path.join("pixi.toml")).unwrap();
    assert!(pixi_content.contains("[project]"));
    assert!(pixi_content.contains("name = \"peppy-node\""));
    assert!(pixi_content.contains("channels = [\"conda-forge\"]"));
    assert!(pixi_content.contains("[dependencies]"));
    assert!(pixi_content.contains("[tasks]"));
}

#[test]
fn test_peppy_yaml_content() {
    let temp_dir = TempDir::new().unwrap();
    super::helpers::setup(temp_dir.path());

    let node_name = "peppy-node";
    let node_path = temp_dir.path().join(node_name);
    let result = create::create(
        temp_dir.path(),
        Some(temp_dir.path()),
        NodeName::new(node_name).unwrap(),
        Language::Rust,
        None,
    );

    assert!(result.is_ok(), "Create should succeed: {:?}", result.err());

    let peppy_content = fs::read_to_string(node_path.join("peppy.star")).unwrap();
    assert!(peppy_content.contains("def create_node():"));
    assert!(peppy_content.contains("exported = struct"));
}
