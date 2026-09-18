use askama::Template;
use std::{collections::BTreeMap, fs, path::Path};

pub const WEB_VIDEO_STREAM_NODE_NAME: &str = "web_video_stream";
pub const BRAIN_NODE_NAME: &str = "brain";
pub const CONTROLLER_NODE_NAME: &str = "controller";
pub const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";
pub const LIDAR_SENSOR_NODE_NAME: &str = "lidar_sensor";

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
    pub node_name: &'a str,
    pub uvc_camera_node_name: &'a str,
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
    pub node_name: &'a str,
    pub uvc_camera_node_name: &'a str,
    pub lidar_sensor_node_name: &'a str,
    pub controller_node_name: &'a str,
}

impl<'a> BrainNodeTemplate<'a> {
    pub fn new(
        node_name: &'a str,
        uvc_camera_node_name: &'a str,
        lidar_sensor_node_name: &'a str,
        controller_node_name: &'a str,
    ) -> Self {
        Self {
            node_name,
            uvc_camera_node_name,
            lidar_sensor_node_name,
            controller_node_name,
        }
    }
}

#[derive(Template)]
#[template(path = "nodes/controller/peppy.json5.j2")]
pub struct ControllerNodeTemplate<'a> {
    pub node_name: &'a str,
}

impl<'a> ControllerNodeTemplate<'a> {
    pub fn new(node_name: &'a str) -> Self {
        Self { node_name }
    }
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
    let target = Path::new("nodes")
        .join(node_name)
        .join(config::consts::NODE_CONFIG_FILE);

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
