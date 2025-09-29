use tempfile::TempDir;

#[path = "./helpers/mod.rs"]
mod helpers;

#[test]
fn test_create_node_stack() {
    let temp_dir = TempDir::new().unwrap();
    helpers::create_git_repo(&temp_dir);
    dbg!(temp_dir.path());
    let e = 9;
}
