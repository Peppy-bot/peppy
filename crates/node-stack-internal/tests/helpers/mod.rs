use askama::Template;
use git2::{Repository, Signature};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Template)]
#[template(path = "root_example_1/peppy.json5.j2")]
struct RootNodeTemplate1<'a> {
    node_name: &'a str,
    uvc_camera_github_repo: &'a str,
    // The path to the node inside the repository
    uvc_camera_github_repo_path: &'a str,
}

#[derive(Template)]
#[template(path = "nodes/uvc_camera/peppy.json5.j2")]
struct UvcCameraNodeTemplate<'a> {
    node_name: &'a str,
}

#[derive(Template)]
#[template(path = "nodes/web_video_stream/peppy.json5.j2")]
struct WebStreamVideoStreamNodeTemplate<'a> {
    node_name: &'a str,
}

pub fn get_root_node(to_path: impl AsRef<Path>, uvc_camera_github_repo: &str) -> PathBuf {
    let root_content = RootNodeTemplate1 {
        node_name: "peppy_root",
        uvc_camera_github_repo: uvc_camera_github_repo,
        uvc_camera_github_repo_path: "nodes/uvc_camera",
    }
    .render()
    .expect("failed to render root template");

    let dir_path = to_path.as_ref();
    let file_path = dir_path.join("peppy.json5");

    fs::write(&file_path, root_content).expect("failed to write root node content");

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
        node_name: "web_video_stream",
    }
    .render()
    .expect("failed to render web template");

    let uvc_path = Path::new("nodes/uvc_camera/peppy.json5");
    let web_path = Path::new("nodes/web_video_stream/peppy.json5");

    if let Some(parent) = uvc_path.parent() {
        fs::create_dir_all(repo_path.join(parent)).expect("failed to create uvc directories");
    }
    fs::write(repo_path.join(uvc_path), uvc_content).expect("failed to write uvc node");

    if let Some(parent) = web_path.parent() {
        fs::create_dir_all(repo_path.join(parent)).expect("failed to create web directories");
    }
    fs::write(repo_path.join(web_path), web_content).expect("failed to write web node");

    let mut index = repo.index().expect("failed to open index");
    index.add_path(uvc_path).expect("failed to add uvc node");
    index.add_path(web_path).expect("failed to add web node");
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

pub fn add_local_web_video_stream(to_path: impl AsRef<Path>) {
    let to_path = to_path.as_ref();
    let node_root_dir = to_path.parent().unwrap();
    let web_content = WebStreamVideoStreamNodeTemplate {
        node_name: "web_video_stream",
    }
    .render()
    .expect("failed to render web template");
    fs::create_dir_all(&node_root_dir).expect("failed to create parent directory");
    fs::write(to_path, web_content).expect("failed to write web_video_stream node");
}
