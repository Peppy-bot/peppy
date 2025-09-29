use std::{
    fs,
    path::{Path, PathBuf},
};

use super::types::{DeploymentMap, GitRemoteSpec, ResolvedNodeSource};
use crate::error::{Error, Result};
use config::{Deployment, NodeConfigParser};
use git2::{AutotagOption, FetchOptions, ObjectType, Repository};

fn node_config_path(spec: &GitRemoteSpec) -> PathBuf {
    let path = spec.path.as_deref().map(Path::new);
    match path {
        Some(path) if path.extension().and_then(|ext| ext.to_str()) == Some("json5") => {
            PathBuf::from(path)
        }
        Some(path) => path.join("peppy.json5"),
        None => PathBuf::from("peppy.json5"),
    }
}

fn find_commit_for_tag<'repo>(
    repo: &'repo Repository,
    tag: &str,
) -> std::result::Result<git2::Commit<'repo>, git2::Error> {
    let reference_name = format!("refs/tags/{tag}");
    let object = repo
        .revparse_single(&reference_name)
        .or_else(|_| repo.revparse_single(tag))?;
    let peeled = object.peel(ObjectType::Commit)?;
    peeled
        .into_commit()
        .map_err(|_| git2::Error::from_str("tag does not point to a commit"))
}

fn sanitize_remote(remote: &str) -> String {
    let mut out = remote
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>();
    const MAX_LEN: usize = 64;
    if out.len() > MAX_LEN {
        out.truncate(MAX_LEN);
    }
    out
}

fn stable_hash(input: &str) -> u64 {
    // FNV-1a 64-bit
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    input.bytes().fold(OFFSET, |hash, byte| {
        let hash = hash ^ u64::from(byte);
        hash.wrapping_mul(PRIME)
    })
}

fn read_blob_from_tree(
    repo: &Repository,
    tree: &git2::Tree,
    path: &Path,
) -> std::result::Result<String, git2::Error> {
    let entry = tree.get_path(path)?;
    let blob = repo.find_blob(entry.id())?;
    let content = std::str::from_utf8(blob.content())
        .map_err(|_| git2::Error::from_str("invalid utf-8 in node config"))?;
    Ok(content.to_owned())
}

fn build_repo_cache_path(base: &Path, remote: &str) -> PathBuf {
    let sanitized = sanitize_remote(remote);
    let hash = stable_hash(remote);
    base.join(format!("{sanitized}-{hash:016x}"))
}

fn fetch_repository(repo: &Repository) -> std::result::Result<(), git2::Error> {
    if let Ok(mut remote) = repo.find_remote("origin") {
        let mut fo = FetchOptions::new();
        fo.download_tags(AutotagOption::All);
        remote.fetch(&[] as &[&str], Some(&mut fo), None)
    } else {
        Ok(())
    }
}

fn ensure_repository(
    repo_dir: &Path,
    remote: &str,
) -> std::result::Result<Repository, git2::Error> {
    if repo_dir.exists() {
        Repository::open(repo_dir)
    } else {
        Repository::clone(remote, repo_dir)
    }
}

pub fn resolve_remote_git(
    nodes_cache_dir: &Path,
    deployment: &Deployment,
    spec: GitRemoteSpec,
) -> Result<DeploymentMap> {
    fs::create_dir_all(nodes_cache_dir)?;

    let repo_dir = build_repo_cache_path(nodes_cache_dir, &spec.repo);
    let repo = ensure_repository(&repo_dir, &spec.repo)
        .map_err(|_| Error::NodeNotFound(deployment.name.clone()))?;

    fetch_repository(&repo).map_err(|_| Error::NodeNotFound(deployment.name.clone()))?;

    let commit = find_commit_for_tag(&repo, &deployment.tag)
        .map_err(|_| Error::NodeNotFound(deployment.name.clone()))?;
    let tree = commit
        .tree()
        .map_err(|_| Error::NodeNotFound(deployment.name.clone()))?;

    let config_path = node_config_path(&spec);
    let content = read_blob_from_tree(&repo, &tree, &config_path)
        .map_err(|_| Error::NodeNotFound(deployment.name.clone()))?;

    let node = NodeConfigParser::from_content(&content)?;

    if node.manifest.name.as_str() != deployment.name || node.manifest.tag != deployment.tag {
        return Err(Error::NoMatchingNode(
            deployment.name.clone(),
            deployment.tag.clone(),
        ));
    }

    let node_source = ResolvedNodeSource::new(deployment.source.clone(), node);
    Ok(DeploymentMap::new(deployment.clone(), node_source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use config::NodeSource as ConfigNodeSource;
    use git2::{ObjectType, Repository, Signature};
    use tempfile::TempDir;

    fn sample_remote_deployment(source: ConfigNodeSource) -> Deployment {
        Deployment {
            name: "uvc_camera".to_string(),
            source: Some(source),
            tag: "0.1.0".to_string(),
            optional: false,
            instances: vec![],
        }
    }

    fn init_local_git_repo(path_within_repo: Option<&str>) -> TempDir {
        let remote_dir = tempfile::tempdir().expect("remote temp dir");
        let repo = Repository::init(remote_dir.path()).expect("init repo");

        let file_path = if let Some(path) = path_within_repo {
            let dir = remote_dir.path().join(path);
            std::fs::create_dir_all(&dir).expect("create nested directory");
            dir.join("peppy.json5")
        } else {
            remote_dir.path().join("peppy.json5")
        };

        std::fs::write(
            &file_path,
            r#"{
                manifest: {
                    name: "uvc_camera",
                    tag: "0.1.0",
                    launch_cmd: ["cargo", "run", "--release"]
                }
            }"#,
        )
        .expect("write node config");

        let rel_path = file_path
            .strip_prefix(remote_dir.path())
            .expect("relative path");

        let mut index = repo.index().expect("repository index");
        index.add_path(rel_path).expect("add file to index");
        index.write().expect("write index");

        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let signature = Signature::now("Peppy", "peppy@example.com").expect("signature");
        let commit_id = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .expect("create commit");

        let commit_obj = repo
            .find_object(commit_id, Some(ObjectType::Commit))
            .expect("find commit object");
        repo.tag("0.1.0", &commit_obj, &signature, "tag", false)
            .expect("create tag");

        remote_dir
    }

    #[test]
    fn map_deployment_nodes_remote_git_root() {
        let remote_repo = init_local_git_repo(None);
        let source = remote_repo.path().to_string_lossy().to_string();
        let spec = GitRemoteSpec {
            repo: source.clone(),
            path: None,
        };
        let deployment = sample_remote_deployment(ConfigNodeSource::Git(spec.clone()));
        let cache_dir = tempfile::tempdir().expect("cache dir");

        let map = resolve_remote_git(cache_dir.path(), &deployment, spec)
            .expect("remote deployment resolves");

        let node = map.node_source().node();
        assert_eq!(node.manifest.name.as_str(), deployment.name);
        assert_eq!(node.manifest.tag, deployment.tag);
    }

    #[test]
    fn map_deployment_nodes_remote_git_with_path() {
        let remote_repo = init_local_git_repo(Some("nodes/uvc_camera"));
        let spec = GitRemoteSpec {
            repo: remote_repo.path().to_string_lossy().to_string(),
            path: Some("nodes/uvc_camera".to_string()),
        };
        let deployment = sample_remote_deployment(ConfigNodeSource::Git(spec.clone()));
        let cache_dir = tempfile::tempdir().expect("cache dir");

        let map = resolve_remote_git(cache_dir.path(), &deployment, spec)
            .expect("remote deployment resolves");

        let node = map.node_source().node();
        assert_eq!(node.manifest.name.as_str(), deployment.name);
        assert_eq!(node.manifest.tag, deployment.tag);
    }

    #[test]
    fn map_deployment_nodes_remote_git_with_repo_folder() {
        let remote_repo = init_local_git_repo(Some("nodes/uvc_camera"));
        let repo_url = remote_repo.path().to_string_lossy().to_string();
        let spec = GitRemoteSpec {
            repo: repo_url.clone(),
            path: Some("nodes/uvc_camera".to_string()),
        };
        let full_git_source = spec.as_remote();
        let deployment_source = ConfigNodeSource::from_str(&full_git_source)
            .expect("parses <repo>::<path> git deployment source");
        let deployment = sample_remote_deployment(deployment_source);
        match deployment.source.as_ref() {
            Some(ConfigNodeSource::Git(git_source)) => {
                assert_eq!(git_source.repo, repo_url);
                assert_eq!(git_source.path.as_deref(), Some("nodes/uvc_camera"));
            }
            _ => panic!("deployment must use git source"),
        }
        let cache_dir = tempfile::tempdir().expect("cache dir");

        let map = resolve_remote_git(cache_dir.path(), &deployment, spec)
            .expect("remote deployment resolves");

        let node = map.node_source().node();
        assert_eq!(node.manifest.name.as_str(), deployment.name);
        assert_eq!(node.manifest.tag, deployment.tag);
    }

    #[test]
    fn map_deployment_nodes_remote_http_returns_node_not_found() {
        let remote = "https://nodes.peppy.bot/uvc_camera";
        let deployment = sample_remote_deployment(ConfigNodeSource::Http(remote.to_string()));

        let cache_dir = tempfile::tempdir().expect("cache dir");

        let spec = GitRemoteSpec {
            repo: remote.to_string(),
            path: None,
        };

        let err = resolve_remote_git(cache_dir.path(), &deployment, spec)
            .expect_err("http remote should fail");

        match err {
            Error::NodeNotFound(name) => assert_eq!(name, deployment.name),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
