use std::{fs, path::Path};

use git2::{ObjectType, Repository, Signature};
use tempfile::{TempDir, tempdir};

pub fn create_simple_git_repo(manifest_content: &str, tag: &str) -> TempDir {
    let remote_dir = tempdir().expect("remote temp dir");
    let repo = Repository::init(remote_dir.path()).expect("init git repo");

    let file_path = remote_dir.path().join(config::consts::NODE_CONFIG_FILE);
    fs::write(&file_path, manifest_content).expect("write manifest");

    let rel_path = file_path
        .strip_prefix(remote_dir.path())
        .expect("relative manifest path");

    let mut index = repo.index().expect("repository index");
    index.add_path(rel_path).expect("add manifest to index");
    index.write().expect("write index");

    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("signature");
    let commit_id = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "initial commit",
            &tree,
            &[],
        )
        .expect("create commit");

    let commit = repo
        .find_object(commit_id, Some(ObjectType::Commit))
        .expect("find commit object");
    repo.tag(tag, &commit, &signature, "tag", false)
        .expect("create tag");

    remote_dir
}

pub fn push_git_commit(repo_path: &Path, files: &[(&str, &str)], message: &str) -> git2::Oid {
    let repo = Repository::open(repo_path).expect("open git repo");

    for (relative_path, contents) in files {
        let full_path = repo_path.join(relative_path);
        if let Some(parent) = Path::new(relative_path).parent() {
            fs::create_dir_all(repo_path.join(parent)).expect("create directories for file");
        }
        fs::write(&full_path, contents).expect("write file contents");
    }

    let mut index = repo.index().expect("repo index");
    for (relative_path, _) in files {
        index
            .add_path(Path::new(relative_path))
            .expect("add file to index");
    }
    index.write().expect("write index");

    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("signature");

    let parents: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|head| head.target())
        .and_then(|oid| repo.find_commit(oid).ok())
        .into_iter()
        .collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parent_refs,
    )
    .expect("create commit")
}
