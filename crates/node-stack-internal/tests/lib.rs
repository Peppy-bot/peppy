use node_stack::LocalNodesMapper;
use tempfile::TempDir;

#[path = "./helpers/mod.rs"]
mod helpers;

/// Uses the following nodes:
/// - brain
/// - controller
/// - lidar_sensor
/// - uvc_camera
/// - web_video_stream
/// With the following dependencies:
/// - `brain` depends on `lidar_sensor` and `uvc_camera` (`subscribes_to.topics` property)
/// - `controller` depends on `brain` (`subscribes_to.actions` property)
/// - `web_video_stream` depends on `uvc_camera` (`subscribes_to.topics` property)
#[test]
fn test_create_node_stack() {
    let web_video_stream_node_name = "web_video_stream";
    let brain_node_name = "brain";
    let uvc_camera_node_name = "uvc_camera";
    let lidar_sensor_node_name = "lidar_sensor";

    // Create a local git repo that host 2 different nodes (only uvc_camera will be pulled in this test)
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = helpers::create_git_repo(&git_repo_temp_dir);

    // The directory in which the root_node lives will contain a `.peppy/nodes` folder where cached nodes will be pulled from a local git repo
    let root_temp_dir = TempDir::new().unwrap();
    // Only pull uvc_camera from git
    let root_node = helpers::get_root_node(&root_temp_dir, git_repo_path.to_str().unwrap());

    // Add web_video_stream locally to the node_stack
    helpers::add_local_web_video_stream(
        root_temp_dir
            .path()
            .join(web_video_stream_node_name)
            .join("peppy.json5"),
        helpers::WebStreamVideoStreamNodeTemplate::new(
            web_video_stream_node_name,
            uvc_camera_node_name,
        ),
    );

    // Add brain locally to the node_stack
    helpers::add_local_web_video_stream(
        root_temp_dir
            .path()
            .join(brain_node_name)
            .join("peppy.json5"),
        helpers::BrainNodeTemplate::new(
            brain_node_name,
            uvc_camera_node_name,
            lidar_sensor_node_name,
        ),
    );

    let mapper = LocalNodesMapper::from_root_config_file(root_node, None).unwrap();
    let deployment_mapper = mapper.get_local_node_stack().unwrap();

    // Supposed to contain only `web_video_stream` and `brain` nodes at this stage (`uvc_camera`, `lidar_sensor` and `controller` are pulled from git)
    assert_eq!(deployment_mapper.node_stack.len(), 2);

    // Now take care of the deployments (git pull etc...)
    let deployment_tree = deployment_mapper.map_deployments_to_nodes();

    // Now check that the graph has been properly created
    let root_index = deployment_tree.root_index();
    let root_map = deployment_tree
        .get(root_index)
        .expect("deployment tree contains a root node");
    assert_eq!(root_map.deployment().name, "peppy_root");

    assert!(deployment_tree.children(root_index).is_empty());
    assert!(deployment_tree.parents(root_index).is_empty());

    let mut uvc_index = None;
    let mut web_index = None;

    for index in deployment_tree.indices() {
        let Some(map) = deployment_tree.get(index) else {
            continue;
        };
        match map.deployment().name.as_str() {
            "uvc_camera" => uvc_index = Some(index),
            "web_video_stream" => web_index = Some(index),
            _ => {}
        }
    }

    let uvc_index = uvc_index.expect("uvc_camera deployment exists in the graph");
    let web_index = web_index.expect("web_video_stream deployment exists in the graph");

    assert!(deployment_tree.parents(uvc_index).is_empty());
    let uvc_children = deployment_tree.children(uvc_index);
    assert_eq!(uvc_children.len(), 1);
    assert_eq!(uvc_children[0], web_index);

    let web_video_stream_child = deployment_tree
        .get(web_index)
        .expect("web_video_stream deployment exists");
    assert_eq!(web_video_stream_child.deployment().name, "web_video_stream");
    let parents = deployment_tree.parents(web_index);
    assert_eq!(parents.len(), 1);
    assert_eq!(parents[0], uvc_index);

    helpers::print_graph(&deployment_tree, &|map| map.deployment().name.clone());
    let _persisted_path = git_repo_temp_dir.keep();
    let _persisted_path2 = root_temp_dir.keep();
}
