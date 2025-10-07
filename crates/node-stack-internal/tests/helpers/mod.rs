use askama::Template;
use git2::{Repository, Signature};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub const WEB_VIDEO_STREAM_NODE_NAME: &str = "web_video_stream";
pub const BRAIN_NODE_NAME: &str = "brain";
pub const CONTROLLER_NODE_NAME: &str = "controller";
pub const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";
pub const LIDAR_SENSOR_NODE_NAME: &str = "lidar_sensor";

#[derive(Template)]
#[template(path = "config_example_1/peppy_config.json5.j2")]
pub struct PeppyConfigTemplateExample1<'a> {
    pub lidar_sensor_node_name: &'a str,
    pub lidar_sensor_github_repo: &'a str,
    // The path to the node inside the repository
    pub lidar_sensor_github_repo_path: &'a str,

    pub uvc_camera_node_name: &'a str,
    pub uvc_camera_github_repo: &'a str,
    pub uvc_camera_github_repo_path: &'a str,

    pub web_video_stream_node_name: &'a str,

    pub brain_node_name: &'a str,

    pub controller_node_name: &'a str,
}

#[derive(Template)]
#[template(path = "config_example_2/peppy_config.json5.j2")]
pub struct PeppyConfigTemplateExample2<'a> {
    pub lidar_sensor_node_name: &'a str,
    pub lidar_sensor_github_repo: &'a str,
    // The path to the node inside the repository
    pub lidar_sensor_github_repo_path: &'a str,
}

#[derive(Template)]
#[template(path = "config_example_3/peppy_config.json5.j2")]
pub struct PeppyConfigTemplateExample3<'a> {
    pub lidar_sensor_node_name: &'a str,
    pub lidar_sensor_url: &'a str,
    pub lidar_sensor_sha256: &'a str,
}

#[derive(Template)]
#[template(path = "config_example_4/peppy_config.json5.j2")]
pub struct PeppyConfigTemplateExample4<'a> {
    pub lidar_sensor_node_name: &'a str,
    pub lidar_sensor_github_repo: &'a str,
    pub lidar_sensor_github_repo_path: &'a str,
}

pub fn render_peppy_config_template<T>(to_path: impl AsRef<Path>, template: T) -> PathBuf
where
    T: Template,
{
    let root_content = template.render().expect("failed to render root template");

    let dir_path = to_path.as_ref();
    let file_path = dir_path.join("peppy_config.json5");

    fs::write(&file_path, root_content).expect("failed to write peppy config content");

    file_path
}

#[derive(Template)]
#[template(path = "nodes/uvc_camera/peppy.json5.j2")]
pub struct UvcCameraNodeTemplate<'a> {
    node_name: &'a str,
}

impl<'a> UvcCameraNodeTemplate<'a> {
    pub fn new(node_name: &'a str) -> Self {
        Self { node_name }
    }
}

#[derive(Template)]
#[template(path = "nodes/lidar_sensor/peppy.json5.j2")]
pub struct LidarSensorNodeTemplate<'a> {
    node_name: &'a str,
    node_tag: &'a str,
}

impl<'a> LidarSensorNodeTemplate<'a> {
    pub fn new(node_name: &'a str, node_tag: &'a str) -> Self {
        Self {
            node_name,
            node_tag,
        }
    }
}

#[derive(Template)]
#[template(path = "nodes/web_video_stream/peppy.json5.j2")]
pub struct WebStreamVideoStreamNodeTemplate<'a> {
    node_name: &'a str,
    uvc_camera_node_name: &'a str,
}

impl<'a> WebStreamVideoStreamNodeTemplate<'a> {
    pub fn new(node_name: &'a str, uvc_camera_node_name: &'a str) -> Self {
        Self {
            node_name,
            uvc_camera_node_name,
        }
    }
}

#[derive(Template)]
#[template(path = "nodes/brain/peppy.json5.j2")]
pub struct BrainNodeTemplate<'a> {
    node_name: &'a str,
    uvc_camera_node_name: &'a str,
    lidar_sensor_node_name: &'a str,
}

impl<'a> BrainNodeTemplate<'a> {
    pub fn new(
        node_name: &'a str,
        uvc_camera_node_name: &'a str,
        lidar_sensor_node_name: &'a str,
    ) -> Self {
        Self {
            node_name,
            uvc_camera_node_name,
            lidar_sensor_node_name,
        }
    }
}

#[derive(Template)]
#[template(path = "nodes/controller/peppy.json5.j2")]
pub struct ControllerNodeTemplate<'a> {
    node_name: &'a str,
    brain_node_name: &'a str,
}

impl<'a> ControllerNodeTemplate<'a> {
    pub fn new(node_name: &'a str, brain_node_name: &'a str) -> Self {
        Self {
            node_name,
            brain_node_name,
        }
    }
}

pub fn create_git_repo(to_path: impl AsRef<Path>) -> PathBuf {
    let base_path = to_path.as_ref();
    let repo_path = base_path.join("peppy_nodes_repo.git");
    fs::create_dir_all(&repo_path).expect("failed to create repo directory");

    let repo = Repository::init(&repo_path).expect("failed to init repository");

    let uvc_content = UvcCameraNodeTemplate::new("uvc_camera")
        .render()
        .expect("failed to render uvc template");
    let lidar_content = LidarSensorNodeTemplate::new(LIDAR_SENSOR_NODE_NAME, "0.1.0")
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
    }
    .render()
    .expect("failed to render brain template");
    let controller_content = ControllerNodeTemplate {
        node_name: CONTROLLER_NODE_NAME,
        brain_node_name: BRAIN_NODE_NAME,
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
    let signature =
        Signature::now("Peppy", "peppy@example.com").expect("failed to create signature");
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
    repo.tag("0.1.0", commit.as_object(), &signature, "0.1.0", false)
        .expect("failed to create tag");

    repo_path
}

pub fn add_local_web_video_stream<T>(to_path: impl AsRef<Path>, template: T)
where
    T: Template,
{
    let to_path = to_path.as_ref();
    let node_root_dir = to_path.parent().unwrap();
    let node_content = template.render().expect("failed to render node template");
    fs::create_dir_all(node_root_dir).expect("failed to create parent directory");
    fs::write(to_path, node_content).expect("failed to write node");
}

pub fn cached_node_exists(base: &Path, node_name: &str) -> bool {
    let target = Path::new("nodes").join(node_name).join("peppy.json5");

    fn walk(dir: &Path, target: &Path) -> bool {
        if dir.join(target).exists() {
            return true;
        }

        match fs::read_dir(dir) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .any(|path| walk(&path, target)),
            Err(_) => false,
        }
    }

    walk(base, &target)
}

pub fn print_dependency_summary(deps: &BTreeMap<String, Vec<String>>) {
    println!("dependency summary:");

    for (node, dependencies) in deps {
        let mut deps_sorted = dependencies.clone();
        deps_sorted.sort();

        if deps_sorted.is_empty() {
            println!("  • `{}` has no dependencies", node);
        } else {
            println!("  • `{}` depends on `{}`", node, deps_sorted.join(" and "));
        }
    }
}
