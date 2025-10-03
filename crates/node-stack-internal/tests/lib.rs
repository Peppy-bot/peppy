use std::collections::{BTreeMap, BTreeSet};

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
fn test_create_node_stack_config_example_1() {
    // Create a local git repo that host 2 different nodes (only uvc_camera and lidar_sensor will be pulled in this test)
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = helpers::create_git_repo(&git_repo_temp_dir);

    // The directory in which the peppy config lives will contain a `.peppy/nodes` folder where cached nodes will be pulled from a local git repo
    let root_temp_dir = TempDir::new().unwrap();
    let root = root_temp_dir.path();
    // Only pull uvc_camera and lidar_sensor from git
    let peppy_config = helpers::get_peppy_config(
        &root_temp_dir,
        git_repo_path.to_str().unwrap(),
        format!("nodes/{}", helpers::LIDAR_SENSOR_NODE_NAME).as_str(),
        format!("nodes/{}", helpers::UVC_CAMERA_NODE_NAME).as_str(),
    );

    let node_path = |name: &str| root.join(name).join("peppy.json5");

    // Add web_video_stream locally to the node_stack
    helpers::add_local_web_video_stream(
        node_path(helpers::WEB_VIDEO_STREAM_NODE_NAME),
        helpers::WebStreamVideoStreamNodeTemplate::new(
            helpers::WEB_VIDEO_STREAM_NODE_NAME,
            helpers::UVC_CAMERA_NODE_NAME,
        ),
    );

    // Add brain locally to the node_stack
    helpers::add_local_web_video_stream(
        node_path(helpers::BRAIN_NODE_NAME),
        helpers::BrainNodeTemplate::new(
            helpers::BRAIN_NODE_NAME,
            helpers::UVC_CAMERA_NODE_NAME,
            helpers::LIDAR_SENSOR_NODE_NAME,
        ),
    );

    // Add controller locally to the node_stack
    helpers::add_local_web_video_stream(
        node_path(helpers::CONTROLLER_NODE_NAME),
        helpers::ControllerNodeTemplate::new(
            helpers::CONTROLLER_NODE_NAME,
            helpers::BRAIN_NODE_NAME,
        ),
    );

    let mapper = LocalNodesMapper::from_root_config_file(peppy_config, None).unwrap();
    let deployment_mapper = mapper.get_local_node_stack().unwrap();

    // Supposed to contain the local nodes stacked in the project directory
    assert_eq!(deployment_mapper.node_stack.len(), 3);

    // Now take care of the deployments (git pull etc...)
    let deployment_tree = deployment_mapper.map_deployments_to_nodes();

    let nodes_cache_dir = root.join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should exist",
        nodes_cache_dir
    );
    assert!(
        helpers::cached_node_exists(&nodes_cache_dir, helpers::UVC_CAMERA_NODE_NAME),
        "uvc_camera should be cached under {:?}",
        nodes_cache_dir
    );
    assert!(
        helpers::cached_node_exists(&nodes_cache_dir, helpers::LIDAR_SENSOR_NODE_NAME),
        "lidar_sensor should be cached under {:?}",
        nodes_cache_dir
    );

    assert!(
        deployment_tree.len() >= 5,
        "deployment graph should contain all nodes"
    );

    let deps_by_name: BTreeMap<String, Vec<String>> = deployment_tree
        .indices()
        .into_iter()
        .map(|index| {
            let map = deployment_tree
                .get(index)
                .expect("deployment graph should return node for index");

            assert!(
                map.is_resolved(),
                "deployment {}:{} must resolve",
                map.deployment().name,
                map.deployment().tag
            );

            let deps = deployment_tree
                .children(index)
                .into_iter()
                .map(|child| {
                    deployment_tree
                        .get(child)
                        .expect("dependency node must exist")
                        .deployment()
                        .name
                        .clone()
                })
                .collect();

            (map.deployment().name.clone(), deps)
        })
        .collect();

    let expected = |names: &[&str]| -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    };
    let actual = |name: &str| -> BTreeSet<String> {
        deps_by_name
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("{} node should be present", name))
            .into_iter()
            .collect()
    };

    assert_eq!(
        actual(helpers::BRAIN_NODE_NAME),
        expected(&[
            helpers::LIDAR_SENSOR_NODE_NAME,
            helpers::UVC_CAMERA_NODE_NAME,
        ]),
        "brain dependencies"
    );
    assert_eq!(
        actual(helpers::CONTROLLER_NODE_NAME),
        expected(&[helpers::BRAIN_NODE_NAME]),
        "controller dependencies"
    );
    assert_eq!(
        actual(helpers::WEB_VIDEO_STREAM_NODE_NAME),
        expected(&[helpers::UVC_CAMERA_NODE_NAME]),
        "web_video_stream dependencies"
    );

    helpers::print_dependency_summary(&deps_by_name);
}
