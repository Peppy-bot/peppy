use super::templates::{
    BRAIN_NODE_NAME, BrainNodeTemplate, CONTROLLER_NODE_NAME, ControllerNodeTemplate,
    LIDAR_SENSOR_NODE_NAME, LidarSensorNodeTemplate, UVC_CAMERA_NODE_NAME, UvcCameraNodeTemplate,
    WEB_VIDEO_STREAM_NODE_NAME, WebStreamVideoStreamNodeTemplate,
};
use askama::Template;
use git2::{Repository, Signature, Time};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn create_nodes_git_repo(to_path: impl AsRef<Path>) -> PathBuf {
    let base_path = to_path.as_ref();
    let repo_path = base_path.join("peppy_nodes_repo.git");
    fs::create_dir_all(&repo_path).expect("failed to create repo directory");

    let repo = Repository::init(&repo_path).expect("failed to init repository");

    let uvc_content = UvcCameraNodeTemplate::new("uvc_camera")
        .render()
        .expect("failed to render uvc template");
    // Keep the node manifest tags aligned with the git ref used for resolution.
    let lidar_content = LidarSensorNodeTemplate::new(LIDAR_SENSOR_NODE_NAME, "v1")
        .render()
        .expect("failed to render lidar template");
    let web_content = WebStreamVideoStreamNodeTemplate {
        node_name: WEB_VIDEO_STREAM_NODE_NAME,
        uvc_camera_node_name: UVC_CAMERA_NODE_NAME,
    }
    .render()
    .expect("failed to render web template");
    let brain_content = BrainNodeTemplate {
        node_name: BRAIN_NODE_NAME,
        uvc_camera_node_name: UVC_CAMERA_NODE_NAME,
        lidar_sensor_node_name: LIDAR_SENSOR_NODE_NAME,
        controller_node_name: CONTROLLER_NODE_NAME,
    }
    .render()
    .expect("failed to render brain template");
    let controller_content = ControllerNodeTemplate {
        node_name: CONTROLLER_NODE_NAME,
    }
    .render()
    .expect("failed to render controller template");

    // Sorted by node name so the repository index below is written in the
    // same deterministic order `peppy repo index` would produce.
    let nodes = [
        (BRAIN_NODE_NAME, brain_content),
        (CONTROLLER_NODE_NAME, controller_content),
        (LIDAR_SENSOR_NODE_NAME, lidar_content),
        (UVC_CAMERA_NODE_NAME, uvc_content),
        (WEB_VIDEO_STREAM_NODE_NAME, web_content),
    ];

    let mut committed_paths: Vec<PathBuf> = Vec::new();
    for (node_name, content) in &nodes {
        let node_path = PathBuf::from(format!("nodes/{node_name}/peppy.json5"));
        let absolute = repo_path.join(&node_path);
        fs::create_dir_all(absolute.parent().expect("node path has a parent"))
            .unwrap_or_else(|e| panic!("failed to create {node_name} directories: {e}"));
        fs::write(&absolute, content)
            .unwrap_or_else(|e| panic!("failed to write {node_name} node: {e}"));
        committed_paths.push(node_path);
    }

    // Every node in this fixture is tagged `v1`, matching the templates and
    // the `v1` git tag the tests resolve against.
    let entries = nodes
        .iter()
        .map(|(node_name, _)| {
            format!("    \"{node_name}\": {{ \"v1\": {{ path: \"nodes/{node_name}/peppy.json5\" }} }},\n")
        })
        .collect::<String>();
    let index_path = PathBuf::from("peppy_repository.json5");
    fs::write(
        repo_path.join(&index_path),
        format!("{{\n  peppy_schema: \"repository/v1\",\n  nodes: {{\n{entries}  }},\n}}\n"),
    )
    .expect("failed to write the repository index");
    committed_paths.push(index_path);

    let mut index = repo.index().expect("failed to open index");
    for path in &committed_paths {
        index
            .add_path(path)
            .unwrap_or_else(|e| panic!("failed to add {}: {e}", path.display()));
    }
    index.write().expect("failed to write index");

    let tree_id = index.write_tree().expect("failed to write tree");
    let tree = repo.find_tree(tree_id).expect("failed to find tree");
    // Use a fixed timestamp (2023-11-14T22:13:20Z, UTC) rather than
    // `Signature::now()` so the fixture is deterministic: identical content
    // yields identical commit/tag SHAs on every run, independent of the wall
    // clock. Refs (`v1`, `v1.0`) are what tests resolve against, but pinning
    // the time keeps the whole repo reproducible.
    let signature = Signature::new("Peppy", "peppy@example.com", &Time::new(1_700_000_000, 0))
        .expect("failed to create signature");
    let commit_id = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "initial commit",
            &tree,
            &[],
        )
        .expect("failed to commit");
    let commit = repo.find_commit(commit_id).expect("failed to find commit");
    // The "correct" ref for nodes in this test repo is `v1` (it matches the nodes' manifest tags).
    repo.tag("v1", commit.as_object(), &signature, "v1", false)
        .expect("failed to create tag");
    // Some config templates use dotted refs (e.g. config example 2 references `v2.0`); include `v1.0`
    // so the repo has a dotted ref too, but note that the node manifest tag remains `v1`.
    repo.tag("v1.0", commit.as_object(), &signature, "v1.0", false)
        .expect("failed to create v1.0 tag");

    repo_path
}
