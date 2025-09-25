use super::git::resolve_remote_git;
use super::types::{DeploymentMap, GitRemoteSpec, RemoteSpec};
use super::url::resolve_remote_url;
use crate::error::Result;
use config::Deployment;
use std::path::Path;

pub fn resolve_remote_deployment(
    nodes_cache_dir: &Path,
    deployment: &Deployment,
    remote: &str,
) -> Result<DeploymentMap> {
    match parse_remote_spec(remote) {
        RemoteSpec::Git(spec) => resolve_remote_git(nodes_cache_dir, deployment, spec),
        RemoteSpec::Http(url) => resolve_remote_url(nodes_cache_dir, deployment, url),
    }
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn looks_like_git(value: &str) -> bool {
    value.ends_with(".git")
        || value.contains(".git/")
        || value.contains(".git?")
        || value.starts_with("git@")
        || value.starts_with("ssh://")
        || value.starts_with("git://")
        || value.starts_with("file://")
}

fn parse_remote_spec(value: &str) -> RemoteSpec {
    let (remote, path) = value
        .split_once("::")
        .map(|(remote, path)| (remote.trim(), Some(path.trim())))
        .unwrap_or_else(|| (value.trim(), None));

    let kind = if is_http_url(remote) && !looks_like_git(remote) {
        RemoteSpec::Http(remote.to_owned())
    } else {
        RemoteSpec::Git(GitRemoteSpec {
            repo: remote.to_owned(),
            path: path
                .filter(|p| !p.is_empty())
                .map(|p| p.trim_start_matches('/').to_owned()),
        })
    };

    kind
}
