use tempfile::TempDir;

use peppy::node::{Language, NodeName, create};

mod helpers;

#[test]
fn test_create_command_default_directory() {
    let temp_dir = TempDir::new().unwrap();
    helpers::setup(temp_dir.path());

    let node_name = "test_node";
    let result = create::create(
        temp_dir.path(),
        NodeName::new(node_name).unwrap(),
        Language::Rust,
        None,
        false,
    );
    let node_path = temp_dir.path().join(node_name);

    assert!(result.is_ok(), "Create should succeed: {:?}", result.err());
    assert!(node_path.exists(), "Node directory should exist");
    assert!(node_path.is_dir(), "Node should be a directory");

    assert!(
        node_path.join("peppy.json5").exists(),
        "peppy.json5 should exist"
    );
}

#[test]
fn test_create_command_with_to_dir() {
    let temp_dir = TempDir::new().unwrap();
    helpers::setup(temp_dir.path());

    let node_name = "test_node";
    let target_path = temp_dir.path().join(node_name);
    let result = create::create(
        temp_dir.path(),
        NodeName::new(node_name).unwrap(),
        Language::Rust,
        None,
        false,
    );

    assert!(result.is_ok(), "Create should succeed: {:?}", result.err());
    assert!(target_path.exists(), "Target directory should exist");
    assert!(target_path.is_dir(), "Target should be a directory");

    assert!(
        target_path.join("peppy.json5").exists(),
        "peppy.json5 should exist"
    );
}
