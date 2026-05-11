use std::path::{Path, PathBuf};

use config::consts::NODE_CONFIG_FILE;
use config::node::NodeConfigParser;
use core_node_api::encoding::NodeSource;
use gix_url::Url as GitUrl;

use crate::error::{Error, Result};

pub fn is_probably_remote_source(source: &str) -> bool {
    source.contains("://") || source.starts_with("git@")
}

/// Heuristic: does this string look like `<name>:<tag>` and NOT like a
/// filesystem path or URL?
///
/// - Must not contain `/` or `\` (those would make it a path).
/// - Must not contain `://` (URLs are caught upstream).
/// - Must not start with `.` or `~` (relative paths).
/// - Must contain exactly one `:` separating a non-empty name from a
///   non-empty tag, neither of which may contain further `:` or `@`.
/// - The arg must not exist on disk (a directory named `foo:bar` wins).
pub fn looks_like_repo_node_ref(source: &str) -> bool {
    if source.contains('/') || source.contains('\\') {
        return false;
    }
    if source.contains("://") {
        return false;
    }
    if source.starts_with('.') || source.starts_with('~') {
        return false;
    }
    let Some((name, tag)) = source.split_once(':') else {
        return false;
    };
    if name.is_empty() || tag.is_empty() {
        return false;
    }
    if name.contains(':') || tag.contains(':') {
        return false;
    }
    if name.contains('@') || tag.contains('@') {
        return false;
    }
    // Don't treat `some:path` as repo-node if a matching file/dir exists
    // on disk.
    if Path::new(source).exists() {
        return false;
    }
    true
}

pub fn parse_node_source(source: &str, git_ref: Option<String>) -> Result<NodeSource> {
    if is_probably_remote_source(source) {
        if let Ok(url) = url::Url::parse(source)
            && matches!(url.scheme(), "http" | "https")
            && is_supported_http_archive(&url)
        {
            if git_ref.is_some() {
                return Err(Error::ExecutionFailed(
                    "`--ref` is only supported for git sources".to_string(),
                ));
            }
            return Ok(NodeSource::Http { url, sha256: None });
        }

        let (repo_url, repo_path) = parse_git_repo_url_and_path(source)?;
        return Ok(NodeSource::Git {
            repo_url,
            repo_path,
            repo_ref: git_ref,
        });
    }

    if looks_like_repo_node_ref(source) {
        if git_ref.is_some() {
            return Err(Error::ExecutionFailed(
                "`--ref` is only supported for git sources".to_string(),
            ));
        }
        let (name, tag) = source
            .split_once(':')
            .expect("looks_like_repo_node_ref guarantees a ':'");
        return NodeSource::repo_node(name, tag).map_err(|e| Error::ExecutionFailed(e.to_string()));
    }

    if git_ref.is_some() {
        return Err(Error::ExecutionFailed(
            "`--ref` is only supported for git sources".to_string(),
        ));
    }

    let source_path = PathBuf::from(source);
    let peppy_json5 = if source_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json5"))
    {
        source_path
    } else {
        source_path.join("peppy.json5")
    };

    let peppy_json5 = peppy_json5.canonicalize().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to resolve path '{}': {}",
            peppy_json5.display(),
            e
        ))
    })?;

    let from_dir = peppy_json5
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let from_dir = resolve_node_root_dir(&from_dir)?;

    Ok(NodeSource::Fs(from_dir))
}

pub fn is_supported_http_archive(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".tar.zst") || path.ends_with(".tar.zstd") || path.ends_with(".tzst")
}

/// Returns `true` when the source string looks like a git repository URL
/// (contains `.git` or uses `git@` / `ssh://` scheme).
pub fn looks_like_git_url(source: &str) -> bool {
    source.ends_with(".git")
        || source.contains(".git/")
        || source.starts_with("git@")
        || source.starts_with("ssh://")
        || source.starts_with("git://")
}

pub fn parse_git_repo_url_and_path(source: &str) -> Result<(GitUrl, String)> {
    if let Ok(mut parsed) = url::Url::parse(source) {
        parsed.set_query(None);
        parsed.set_fragment(None);

        let segments: Vec<&str> = parsed
            .path_segments()
            .map(|segments| segments.collect())
            .unwrap_or_default();
        let repo_index = segments
            .iter()
            .rposition(|segment| segment.ends_with(".git"));

        let (repo_url_str, repo_path) = if let Some(repo_index) = repo_index {
            let repo_path_segments = segments[..=repo_index].join("/");
            let repo_relative_path = segments[repo_index + 1..].join("/");

            let mut repo_url = parsed.clone();
            repo_url.set_path(&format!("/{}", repo_path_segments));
            (repo_url.to_string(), repo_relative_path)
        } else {
            (parsed.to_string(), String::new())
        };

        let repo_url = GitUrl::try_from(repo_url_str.as_str()).map_err(|e| {
            Error::ExecutionFailed(format!("Invalid git URL '{}': {}", repo_url_str, e))
        })?;
        return Ok((repo_url, repo_path));
    }

    let (repo_url_str, repo_path) = if let Some((before, after)) = source.split_once(".git/") {
        (format!("{before}.git"), after.to_string())
    } else {
        (source.to_string(), String::new())
    };

    let repo_url = GitUrl::try_from(repo_url_str.as_str()).map_err(|e| {
        Error::ExecutionFailed(format!("Invalid git URL '{}': {}", repo_url_str, e))
    })?;
    Ok((repo_url, repo_path))
}

/// Resolves the root node directory from a candidate path. The directory must
/// contain a parseable `peppy.json5`.
pub fn resolve_node_root_dir(dir: &Path) -> Result<PathBuf> {
    let config_path = dir.join(NODE_CONFIG_FILE);
    NodeConfigParser::from_path(&config_path).map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to parse '{}' in directory {}: {}",
            NODE_CONFIG_FILE,
            dir.display(),
            e,
        ))
    })?;
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_git_url_matches_variants() {
        // Ends with .git
        assert!(looks_like_git_url("https://github.com/org/repo.git"));
        // .git followed by subpath
        assert!(looks_like_git_url("https://host/org/repo.git/subpath"));
        // SSH-style schemes
        assert!(looks_like_git_url("git@github.com:org/repo.git"));
        assert!(looks_like_git_url("ssh://git@host/org/repo"));
        assert!(looks_like_git_url("git://host/org/repo"));
        // Plain URLs without .git are not git
        assert!(!looks_like_git_url("https://host/org/repo"));
        assert!(!looks_like_git_url("https://example.com/packages"));
    }

    #[test]
    fn looks_like_repo_node_ref_accepts_name_tag() {
        assert!(looks_like_repo_node_ref("uvc_camera:0.1.0"));
        assert!(looks_like_repo_node_ref("node:1.0.0"));
    }

    #[test]
    fn looks_like_repo_node_ref_rejects_paths() {
        assert!(!looks_like_repo_node_ref("./foo:bar"));
        assert!(!looks_like_repo_node_ref("/abs/foo:bar"));
        assert!(!looks_like_repo_node_ref("~/foo:bar"));
        assert!(!looks_like_repo_node_ref("foo/bar:baz"));
    }

    #[test]
    fn looks_like_repo_node_ref_rejects_urls() {
        assert!(!looks_like_repo_node_ref("https://example.com/foo"));
        assert!(!looks_like_repo_node_ref("git://example.com/foo.git"));
    }

    #[test]
    fn looks_like_repo_node_ref_requires_exactly_one_colon() {
        assert!(!looks_like_repo_node_ref("name"));
        assert!(!looks_like_repo_node_ref("name:"));
        assert!(!looks_like_repo_node_ref(":tag"));
        assert!(!looks_like_repo_node_ref("a:b:c"));
    }

    #[test]
    fn parse_node_source_recognizes_name_tag() {
        let src = parse_node_source("some_node:1.2.3", None).unwrap();
        let NodeSource::RepoNode { name, tag } = &src else {
            panic!("expected RepoNode, got {:?}", src);
        };
        assert_eq!(name, "some_node");
        assert_eq!(tag, "1.2.3");
    }
}
