use std::fs;
use tempfile::TempDir;

#[test]
fn test_create_command_default_directory() {
    let temp_dir = TempDir::new().unwrap();
    let node_path = temp_dir.path().join("test-node");
    
    let output = std::process::Command::new("cargo")
        .args(&["run", "--", "node", "create", "test-node", "--to-dir", &node_path.to_string_lossy()])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Failed to execute command");
    
    assert!(output.status.success());
    assert!(node_path.exists(), "Node directory should exist");
    assert!(node_path.is_dir(), "Node should be a directory");
    
    assert!(node_path.join("pixi.toml").exists(), "pixi.toml should exist");
    assert!(node_path.join("peppy.star").exists(), "peppy.star should exist");
}

#[test]
fn test_create_command_with_to_dir() {
    let temp_dir = TempDir::new().unwrap();
    let target_path = temp_dir.path().join("my-node");
    
    let output = std::process::Command::new("cargo")
        .args(&["run", "--", "node", "create", "my-node", "--to-dir", &target_path.to_string_lossy()])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Failed to execute command");
    
    assert!(output.status.success());
    assert!(target_path.exists(), "Target directory should exist");
    assert!(target_path.is_dir(), "Target should be a directory");
    
    assert!(target_path.join("pixi.toml").exists(), "pixi.toml should exist");
    assert!(target_path.join("peppy.star").exists(), "peppy.star should exist");
}

#[test]
fn test_pixi_toml_content() {
    let temp_dir = TempDir::new().unwrap();
    let node_path = temp_dir.path().join("test-node");
    
    let output = std::process::Command::new("cargo")
        .args(&["run", "--", "node", "create", "test-node", "--to-dir", &node_path.to_string_lossy()])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Failed to execute command");
    
    assert!(output.status.success());
    
    let pixi_content = fs::read_to_string(node_path.join("pixi.toml")).unwrap();
    assert!(pixi_content.contains("[project]"));
    assert!(pixi_content.contains("name = \"peppy-node\""));
    assert!(pixi_content.contains("channels = [\"conda-forge\"]"));
    assert!(pixi_content.contains("[dependencies]"));
    assert!(pixi_content.contains("[tasks]"));
}

#[test]
fn test_peppy_star_content() {
    let temp_dir = TempDir::new().unwrap();
    let node_path = temp_dir.path().join("test-node");
    
    let output = std::process::Command::new("cargo")
        .args(&["run", "--", "node", "create", "test-node", "--to-dir", &node_path.to_string_lossy()])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Failed to execute command");
    
    assert!(output.status.success());
    
    let peppy_content = fs::read_to_string(node_path.join("peppy.star")).unwrap();
    assert!(peppy_content.contains("# Peppy configuration file"));
    assert!(peppy_content.contains("def main():"));
    assert!(peppy_content.contains("print(\"Hello from peppy!\")"));
}

#[test]
fn test_create_nested_directories() {
    let temp_dir = TempDir::new().unwrap();
    let nested_path = temp_dir.path().join("a").join("b").join("c").join("my-node");
    
    let output = std::process::Command::new("cargo")
        .args(&["run", "--", "node", "create", "my-node", "--to-dir", &nested_path.to_string_lossy()])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Failed to execute command");
    
    assert!(output.status.success());
    assert!(nested_path.exists(), "Nested directory should be created");
    assert!(nested_path.join("pixi.toml").exists());
    assert!(nested_path.join("peppy.star").exists());
}