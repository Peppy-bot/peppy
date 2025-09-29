use tempfile::TempDir;

#[path = "./helpers/mod.rs"]
mod helpers;

#[test]
fn test_create_node_stack() {
    let temp_dir = TempDir::new().unwrap();
    helpers::setup(temp_dir.path());
}
