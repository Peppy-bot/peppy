use node_stack::LocalNodesMapper;
use tempfile::TempDir;

#[path = "./helpers/mod.rs"]
mod helpers;

#[test]
fn test_create_node_stack() {
    // Create a local git repo that host 2 different nodes (only uvc_camera will be pulled in this test)
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = helpers::create_git_repo(&git_repo_temp_dir);

    // The directory in which the root_node lives will contain a `.peppy/nodes` folder where cached nodes will be pulled from a local git repo
    let root_temp_dir = TempDir::new().unwrap();
    // Only pull uvc_camera from git
    let root_node = helpers::get_root_node(&root_temp_dir, git_repo_path.to_str().unwrap());

    // Add web_video_stream to the node_stack
    helpers::add_local_web_video_stream(
        root_temp_dir
            .path()
            .join("web_video_stream")
            .join("peppy.json5"),
    );

    let mapper = LocalNodesMapper::from_root_config_file(root_node, None).unwrap();
    let deployment_mapper = mapper.get_local_node_stack().unwrap();

    // Supposed to contain only the root_node and web_video_stream nodes at this stage (uvc_camera is pulled from git)
    assert_eq!(deployment_mapper.node_stack.len(), 2);

    // Now take care of the deployments (git pull etc...)
    let deployment_tree = deployment_mapper.map_deployments_to_nodes();

    // Now check that the tree has been properly created
    let root_map = deployment_tree
        .get(0)
        .expect("deployment tree contains a root node");
    assert_eq!(root_map.deployment().name, "peppy_root");

    // FIXME: The root_node is the only tree at the root
    let children = deployment_tree.children(0);
    assert_eq!(children.len(), 2);

    let uvc_camera_child = deployment_tree
        .get(children[0])
        .expect("uvc_camera deployment exists");
    assert_eq!(uvc_camera_child.deployment().name, "uvc_camera");

    // FIXME: web_video_stream depends on uvc_camera in the tree
    let web_video_stream_child = deployment_tree
        .get(children[1])
        .expect("web_video_stream deployment exists");
    assert_eq!(web_video_stream_child.deployment().name, "web_video_stream");
    dbg!(&git_repo_path);
    let _persisted_path = git_repo_temp_dir.keep();
    let _persisted_path2 = root_temp_dir.keep();
}
