use peppy::commands::node::create;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_create_command_default_directory() {
    super::helpers::setup();

    let temp_dir = TempDir::new().unwrap();
    let node_path = temp_dir.path().join("test-node");

    let result = create::create("test-node", "rust", Some(node_path.clone()));

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
    super::helpers::setup();

    let temp_dir = TempDir::new().unwrap();
    let target_path = temp_dir.path().join("my-node");

    let result = create::create("my-node", "rust", Some(target_path.clone()));

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
    super::helpers::setup();

    let temp_dir = TempDir::new().unwrap();
    let node_path = temp_dir.path().join("test-node");

    let result = create::create("peppy-node", "rust", Some(node_path.clone()));

    assert!(result.is_ok(), "Create should succeed: {:?}", result.err());

    let pixi_content = fs::read_to_string(node_path.join("pixi.toml")).unwrap();
    assert!(pixi_content.contains("[project]"));
    assert!(pixi_content.contains("name = \"peppy-node\""));
    assert!(pixi_content.contains("channels = [\"conda-forge\"]"));
    assert!(pixi_content.contains("[dependencies]"));
    assert!(pixi_content.contains("[tasks]"));
}

#[test]
fn test_peppy_star_content() {
    super::helpers::setup();

    let temp_dir = TempDir::new().unwrap();
    let node_path = temp_dir.path().join("test-node");

    let result = create::create("test-node", "rust", Some(node_path.clone()));

    assert!(result.is_ok(), "Create should succeed: {:?}", result.err());

    let peppy_content = fs::read_to_string(node_path.join("peppy.star")).unwrap();
    assert!(peppy_content.contains("def create_node():"));
    assert!(peppy_content.contains("exported = struct"));
}
