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

    let uvc_path = Path::new("nodes/uvc_camera/peppy.json5");
    let lidar_path = Path::new("nodes/lidar_sensor/peppy.json5");
    let web_path = Path::new("nodes/web_video_stream/peppy.json5");
    let brain_path = Path::new("nodes/brain/peppy.json5");
    let controller_path = Path::new("nodes/controller/peppy.json5");

    if let Some(parent) = uvc_path.parent() {
        fs::create_dir_all(repo_path.join(parent)).expect("failed to create uvc directories");
    }
    fs::write(repo_path.join(uvc_path), uvc_content).expect("failed to write uvc node");

    if let Some(parent) = lidar_path.parent() {
        fs::create_dir_all(repo_path.join(parent)).expect("failed to create lidar directories");
    }
    fs::write(repo_path.join(lidar_path), lidar_content).expect("failed to write lidar node");

    if let Some(parent) = web_path.parent() {
        fs::create_dir_all(repo_path.join(parent)).expect("failed to create web directories");
    }
    fs::write(repo_path.join(web_path), web_content).expect("failed to write web node");

    if let Some(parent) = brain_path.parent() {
        fs::create_dir_all(repo_path.join(parent)).expect("failed to create brain directories");
    }
    fs::write(repo_path.join(brain_path), brain_content).expect("failed to write brain node");

    if let Some(parent) = controller_path.parent() {
        fs::create_dir_all(repo_path.join(parent))
            .expect("failed to create controller directories");
    }
    fs::write(repo_path.join(controller_path), controller_content)
        .expect("failed to write controller node");

    let mut index = repo.index().expect("failed to open index");
    index.add_path(uvc_path).expect("failed to add uvc node");
    index
        .add_path(lidar_path)
        .expect("failed to add lidar node");
    index.add_path(web_path).expect("failed to add web node");
    index
        .add_path(brain_path)
        .expect("failed to add brain node");
    index
        .add_path(controller_path)
        .expect("failed to add controller node");
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
