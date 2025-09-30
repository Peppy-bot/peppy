use node_stack::LocalNodesMapper;
use tempfile::TempDir;

#[path = "./helpers/mod.rs"]
mod helpers;

#[test]
fn test_create_node_stack() {
    let root_node = helpers::get_root_node();
    let temp_dir = TempDir::new().unwrap();
    let repo_path = helpers::create_git_repo(&temp_dir);

    let mapper = LocalNodesMapper::from_root_config_file(root_node, None).unwrap();
    let deployment_mapper = mapper.get_local_node_stack().unwrap();
    let local_node_stack = deployment_mapper.node_stack;

    // Supposed to contain only the root node at this stage (uvc_camera and web_video_stream are pulled from git)
    assert!(local_node_stack.len() == 1);

    dbg!(&repo_path);
    let _persisted_path = temp_dir.keep();
}
