use askama::Template;
use git2::{Repository, Signature};
use node_stack::{DeploymentGraph, DeploymentMap};
use petgraph::stable_graph::NodeIndex;
use std::{
    collections::{HashSet, VecDeque},
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
struct PeppyConfigTemplate1<'a> {
    lidar_sensor_node_name: &'a str,
    lidar_sensor_github_repo: &'a str,
    // The path to the node inside the repository
    lidar_sensor_github_repo_path: &'a str,

    uvc_camera_node_name: &'a str,
    uvc_camera_github_repo: &'a str,
    uvc_camera_github_repo_path: &'a str,

    web_video_stream_node_name: &'a str,

    brain_node_name: &'a str,

    controller_node_name: &'a str,
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

pub fn get_peppy_config(
    to_path: impl AsRef<Path>,
    github_repo: &str,
    lidar_sensor_github_repo_path: &str,
    uvc_camera_github_repo_path: &str,
) -> PathBuf {
    let root_content = PeppyConfigTemplate1 {
        lidar_sensor_node_name: LIDAR_SENSOR_NODE_NAME,
        lidar_sensor_github_repo: github_repo,
        lidar_sensor_github_repo_path: lidar_sensor_github_repo_path,

        uvc_camera_node_name: UVC_CAMERA_NODE_NAME,
        uvc_camera_github_repo: github_repo,
        uvc_camera_github_repo_path: uvc_camera_github_repo_path,

        web_video_stream_node_name: WEB_VIDEO_STREAM_NODE_NAME,
        brain_node_name: BRAIN_NODE_NAME,
        controller_node_name: CONTROLLER_NODE_NAME,
    }
    .render()
    .expect("failed to render root template");

    let dir_path = to_path.as_ref();
    let file_path = dir_path.join("peppy_config.json5");

    fs::write(&file_path, root_content).expect("failed to write peppy config content");

    file_path
}

pub fn create_git_repo(to_path: impl AsRef<Path>) -> PathBuf {
    let base_path = to_path.as_ref();
    let repo_path = base_path.join("uvc_camera_repo.git");
    fs::create_dir_all(&repo_path).expect("failed to create repo directory");

    let repo = Repository::init(&repo_path).expect("failed to init repository");

    let uvc_content = UvcCameraNodeTemplate {
        node_name: "uvc_camera",
    }
    .render()
    .expect("failed to render uvc template");
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
    let web_path = Path::new("nodes/web_video_stream/peppy.json5");
    let brain_path = Path::new("nodes/brain/peppy.json5");
    let controller_path = Path::new("nodes/controller/peppy.json5");

    if let Some(parent) = uvc_path.parent() {
        fs::create_dir_all(repo_path.join(parent)).expect("failed to create uvc directories");
    }
    fs::write(repo_path.join(uvc_path), uvc_content).expect("failed to write uvc node");

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
    fs::create_dir_all(&node_root_dir).expect("failed to create parent directory");
    fs::write(to_path, node_content).expect("failed to write node");
}

pub fn print_graph<F>(graph: &DeploymentGraph, label: &F)
where
    F: Fn(&DeploymentMap) -> String,
{
    if graph.is_empty() {
        println!("<empty graph>");
        return;
    }

    let format_node = |index: NodeIndex| -> String {
        graph
            .get(index)
            .map(|node| format!("{} [{}]", label(node), index.index()))
            .unwrap_or_else(|| format!("<missing node> [{}]", index.index()))
    };

    println!(
        "graph adjacency (root = {}):",
        format_node(graph.root_index())
    );

    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(graph.root_index());

    println!("-> lists the outgoing edges for that node (everything it points to)");
    println!("<- lists the incoming edges (everything that points to it).");
    while let Some(index) = queue.pop_front() {
        if !visited.insert(index) {
            continue;
        }

        let node_repr = format_node(index);
        println!("{}", node_repr);

        let children = graph.children(index);
        if children.is_empty() {
            println!("  -> []");
        } else {
            let child_entries: Vec<_> = children.iter().map(|child| format_node(*child)).collect();
            println!("  -> {}", child_entries.join(", "));
        }

        let parents = graph.parents(index);
        if parents.is_empty() {
            println!("  <- []");
        } else {
            let parent_entries: Vec<_> =
                parents.iter().map(|parent| format_node(*parent)).collect();
            println!("  <- {}", parent_entries.join(", "));
        }

        for child in children {
            queue.push_back(child);
        }
    }
}
