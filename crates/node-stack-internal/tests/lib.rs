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
    // Only pull uvc_camera and lidar_sensor from git
    let peppy_config = helpers::get_peppy_config(
        &root_temp_dir,
        git_repo_path.to_str().unwrap(),
        format!("nodes/{}", helpers::LIDAR_SENSOR_NODE_NAME).as_str(),
        format!("nodes/{}", helpers::UVC_CAMERA_NODE_NAME).as_str(),
    );

    // Add web_video_stream locally to the node_stack
    helpers::add_local_web_video_stream(
        root_temp_dir
            .path()
            .join(helpers::WEB_VIDEO_STREAM_NODE_NAME)
            .join("peppy.json5"),
        helpers::WebStreamVideoStreamNodeTemplate::new(
            helpers::WEB_VIDEO_STREAM_NODE_NAME,
            helpers::UVC_CAMERA_NODE_NAME,
        ),
    );

    // Add brain locally to the node_stack
    helpers::add_local_web_video_stream(
        root_temp_dir
            .path()
            .join(helpers::BRAIN_NODE_NAME)
            .join("peppy.json5"),
        helpers::BrainNodeTemplate::new(
            helpers::BRAIN_NODE_NAME,
            helpers::UVC_CAMERA_NODE_NAME,
            helpers::LIDAR_SENSOR_NODE_NAME,
        ),
    );

    // Add controller locally to the node_stack
    helpers::add_local_web_video_stream(
        root_temp_dir
            .path()
            .join(helpers::CONTROLLER_NODE_NAME)
            .join("peppy.json5"),
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

    let nodes_cache_dir = root_temp_dir.path().join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should exist",
        nodes_cache_dir
    );

    let contains_node_config = |base: &std::path::Path, node_name: &str| {
        let target = std::path::Path::new("nodes")
            .join(node_name)
            .join("peppy.json5");

        fn search(dir: &std::path::Path, target: &std::path::Path) -> bool {
            if dir.join(target).exists() {
                return true;
            }

            match std::fs::read_dir(dir) {
                Ok(entries) => entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .any(|path| search(&path, target)),
                Err(_) => false,
            }
        }

        search(base, &target)
    };

    assert!(
        contains_node_config(&nodes_cache_dir, helpers::UVC_CAMERA_NODE_NAME),
        "uvc_camera should be cached under {:?}",
        nodes_cache_dir
    );
    assert!(
        contains_node_config(&nodes_cache_dir, helpers::LIDAR_SENSOR_NODE_NAME),
        "lidar_sensor should be cached under {:?}",
        nodes_cache_dir
    );
    let _pth = root_temp_dir.path();

    assert!(
        deployment_tree.len() >= 5,
        "deployment graph should contain all nodes"
    );

    let mut deps_by_name: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    for index in deployment_tree.indices() {
        let map = deployment_tree
            .get(index)
            .expect("deployment graph should return node for index");

        assert!(
            map.is_resolved(),
            "deployment {}:{} must resolve",
            map.deployment().name,
            map.deployment().tag
        );

        let dependencies: Vec<String> = deployment_tree
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

        deps_by_name.insert(map.deployment().name.clone(), dependencies);
    }

    let expected_brain = vec![
        helpers::LIDAR_SENSOR_NODE_NAME.to_string(),
        helpers::UVC_CAMERA_NODE_NAME.to_string(),
    ];
    let expected_controller = vec![helpers::BRAIN_NODE_NAME.to_string()];
    let expected_web = vec![helpers::UVC_CAMERA_NODE_NAME.to_string()];

    let mut actual_brain = deps_by_name
        .get(helpers::BRAIN_NODE_NAME)
        .cloned()
        .expect("brain node should be present");
    let mut actual_controller = deps_by_name
        .get(helpers::CONTROLLER_NODE_NAME)
        .cloned()
        .expect("controller node should be present");
    let mut actual_web = deps_by_name
        .get(helpers::WEB_VIDEO_STREAM_NODE_NAME)
        .cloned()
        .expect("web_video_stream node should be present");

    actual_brain.sort();
    actual_controller.sort();
    actual_web.sort();

    let mut expected_brain_sorted = expected_brain.clone();
    let mut expected_controller_sorted = expected_controller.clone();
    let mut expected_web_sorted = expected_web.clone();

    expected_brain_sorted.sort();
    expected_controller_sorted.sort();
    expected_web_sorted.sort();

    assert_eq!(actual_brain, expected_brain_sorted, "brain dependencies");
    assert_eq!(
        actual_controller, expected_controller_sorted,
        "controller dependencies"
    );
    assert_eq!(
        actual_web, expected_web_sorted,
        "web_video_stream dependencies"
    );

    let format_dependencies = |name: &str| -> String {
        deps_by_name
            .get(name)
            .map(|deps| deps.join(" and "))
            .unwrap_or_else(|| "no dependencies".to_string())
    };

    println!(
        "  - `brain` depends on {} (`subscribes_to.topics` property)",
        format_dependencies(helpers::BRAIN_NODE_NAME)
    );
    println!(
        "  - `controller` depends on {} (`subscribes_to.actions` property)",
        format_dependencies(helpers::CONTROLLER_NODE_NAME)
    );
    println!(
        "  - `web_video_stream` depends on {} (`subscribes_to.topics` property)",
        format_dependencies(helpers::WEB_VIDEO_STREAM_NODE_NAME)
    );
}
