use std::{
    fs,
    path::{Path, PathBuf},
};

use super::ResolvedNode;
use crate::error::Result;
use config::node::NodeConfigParser;
use config::peppy_config::DeploymentGitSource;
use git2::{AutotagOption, FetchOptions, ObjectType, Repository};

fn node_config_path(path: &str) -> PathBuf {
    let trimmed = path.trim().trim_start_matches("./");
    if trimmed.is_empty() || trimmed == "." {
        return PathBuf::from(config::consts::NODE_CONFIG_FILE);
    }

    let path = Path::new(trimmed);
    if path.extension().and_then(|ext| ext.to_str()) == Some("json5") {
        PathBuf::from(trimmed)
    } else {
        path.join(config::consts::NODE_CONFIG_FILE)
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

fn stable_hash(input: &str) -> u64 {
    // FNV-1a 64-bit
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    input.bytes().fold(OFFSET, |hash, byte| {
        let hash = hash ^ u64::from(byte);
        hash.wrapping_mul(PRIME)
    })
}

fn git_dir_name(remote: &str) -> &str {
    let trimmed = remote.trim_end_matches(['/', '\\']);
    let segment = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    let segment = segment.rsplit(':').next().unwrap_or(segment);
    segment.strip_suffix(".git").unwrap_or(segment)
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
    let hash = stable_hash(remote);
    let dir = git_dir_name(remote);
    let dir = if dir.is_empty() {
        format!("{hash:016x}")
    } else {
        dir.to_owned()
    };
    base.join(format!("{dir}-{hash:016x}"))
}

fn fetch_repository(repo: &Repository) -> std::result::Result<(), git2::Error> {
    if let Ok(mut remote) = repo.find_remote("origin") {
        let mut fo = FetchOptions::new();
        fo.download_tags(AutotagOption::All);
        remote.fetch(
            &[
                "+refs/tags/*:refs/tags/*",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
            Some(&mut fo),
            None,
        )
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
    added_nodes_dir: &Path,
    spec: &DeploymentGitSource,
) -> Result<ResolvedNode> {
    fs::create_dir_all(added_nodes_dir)?;

    let repo_dir = build_repo_cache_path(added_nodes_dir, &spec.repo);
    let repo = ensure_repository(&repo_dir, &spec.repo)?;

    fetch_repository(&repo)?;

    let commit = find_commit_for_tag(&repo, &spec.ref_)?;
    let tree = commit.tree()?;

    let config_path = node_config_path(&spec.path);
    let content = read_blob_from_tree(&repo, &tree, &config_path)?;

    let node = NodeConfigParser::from_content(&content)?;

    let root_path = repo_dir.join(config_path.parent().unwrap_or_else(|| Path::new("")));

    Ok(ResolvedNode {
        config: node,
        root_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::peppy_config::DeploymentGitSource;
    use git2::{ObjectType, Repository, Signature};
    use tempfile::TempDir;

    #[test]
    fn repo_cache_path_uses_git_dir_name() {
        let base = std::path::Path::new("/tmp/cache");
        let remote = "https://github.com/org/repo.git";
        let hash = stable_hash(remote);
        let expected = base.join(format!("repo-{hash:016x}"));
        assert_eq!(build_repo_cache_path(base, remote), expected);
    }

    #[test]
    fn repo_cache_path_handles_scp_style_remote() {
        let base = std::path::Path::new("/tmp/cache");
        let remote = "git@github.com:example.git";
        let hash = stable_hash(remote);
        let expected = base.join(format!("example-{hash:016x}"));
        assert_eq!(build_repo_cache_path(base, remote), expected);
    }

    fn init_local_git_repo(path_within_repo: Option<&str>) -> TempDir {
        let remote_dir = tempfile::tempdir().expect("remote temp dir");
        let repo = Repository::init(remote_dir.path()).expect("init repo");

        let file_path = if let Some(path) = path_within_repo {
            let dir = remote_dir.path().join(path);
            std::fs::create_dir_all(&dir).expect("create nested directory");
            dir.join(config::consts::NODE_CONFIG_FILE)
        } else {
            remote_dir.path().join(config::consts::NODE_CONFIG_FILE)
        };

        std::fs::write(
            &file_path,
            r#"{
                schema_version: 1,
                manifest: {
                    name: "uvc_camera",
                    tag: "0.1.0",
                    language: "rust",
                    start_cmd: ["./target/release/uvc_camera"]
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
        let spec = DeploymentGitSource {
            repo: source.clone(),
            path: ".".to_string(),
            ref_: "0.1.0".to_string(),
        };
        let cache_dir = tempfile::tempdir().expect("cache dir");

        let resolved =
            resolve_remote_git(cache_dir.path(), &spec).expect("remote deployment resolves");
        assert_eq!(resolved.config.manifest.name.as_str(), "uvc_camera");
        assert_eq!(resolved.config.manifest.tag, "0.1.0");
    }

    #[test]
    fn map_deployment_nodes_remote_git_with_path() {
        let remote_repo = init_local_git_repo(Some("nodes/uvc_camera"));
        let spec = DeploymentGitSource {
            repo: remote_repo.path().to_string_lossy().to_string(),
            path: "nodes/uvc_camera".to_string(),
            ref_: "0.1.0".to_string(),
        };
        let cache_dir = tempfile::tempdir().expect("cache dir");

        let resolved =
            resolve_remote_git(cache_dir.path(), &spec).expect("remote deployment resolves");
        assert_eq!(resolved.config.manifest.name.as_str(), "uvc_camera");
        assert_eq!(resolved.config.manifest.tag, "0.1.0");
    }

    #[test]
    fn resolve_remote_git_preserves_git_errors() {
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let spec = DeploymentGitSource {
            repo: "https://example.com/does-not-matter.git".to_string(),
            path: ".".to_string(),
            ref_: "0.1.0".to_string(),
        };

        // Force `ensure_repository` to call `Repository::open` on a non-repository path.
        let repo_dir = build_repo_cache_path(cache_dir.path(), &spec.repo);
        std::fs::create_dir_all(&repo_dir).expect("create cache directory");

        let err = resolve_remote_git(cache_dir.path(), &spec).expect_err("resolution should fail");
        assert!(
            matches!(err, crate::error::Error::Git(_)),
            "expected git error, got: {err}"
        );
    }
}
