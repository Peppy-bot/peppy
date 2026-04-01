use std::path::{Path, PathBuf};

use config::consts::NODE_CONFIG_FILE;
use config::node::NodeConfigParser;
use core_node::encoding::NodeSource;
use gix_url::Url as GitUrl;

use crate::error::{Error, Result};

pub fn is_probably_remote_source(source: &str) -> bool {
    source.contains("://") || source.starts_with("git@")
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

    // If the config at this path is not a valid root config (e.g. the user
    // ran `node add` from inside a variant directory), walk up the directory
    // tree to locate the root peppy.json5 that contains the manifest.
    let from_dir = if NodeConfigParser::from_path(&peppy_json5).is_err() {
        find_root_node_dir(&from_dir).ok_or_else(|| {
            Error::ExecutionFailed(format!(
                "No root {} with a `manifest` section found at '{}' or any parent directory",
                NODE_CONFIG_FILE,
                from_dir.display(),
            ))
        })?
    } else {
        from_dir
    };

    Ok(NodeSource::Fs(from_dir))
}

/// Parses a variant source string into a [`NodeSource`].
///
/// Unlike [`parse_node_source`], this does **not** canonicalize local paths or
/// check for `peppy.json5`. A plain string (non-URL) is treated as a variant
/// name and wrapped as `NodeSource::Fs`.
///
/// For git sources, an `@ref` suffix on the path portion specifies the git ref:
/// `https://github.com/org/repo.git/path@main`
pub fn parse_variant_source(variant: &str) -> Result<NodeSource> {
    if is_probably_remote_source(variant) {
        if let Ok(url) = url::Url::parse(variant)
            && matches!(url.scheme(), "http" | "https")
            && is_supported_http_archive(&url)
        {
            return Ok(NodeSource::Http { url, sha256: None });
        }

        // Extract @ref from the end of the string before URL parsing.
        // e.g., "https://github.com/org/repo.git/brain@main" → url part + ref="main"
        // Only split on @ that appears after ".git" to avoid splitting on git@ prefixes.
        let (url_part, repo_ref) = if let Some(git_pos) = variant.rfind(".git") {
            let after_git = &variant[git_pos..];
            if let Some(at_pos) = after_git.rfind('@') {
                let split_pos = git_pos + at_pos;
                let after_at = &variant[split_pos + 1..];
                // Only treat '@' as a ref marker if there is no '/' after it;
                // a '/' means this is a scoped-package segment (e.g. @scope/pkg).
                if !after_at.contains('/') {
                    (
                        &variant[..split_pos],
                        if after_at.is_empty() {
                            None
                        } else {
                            Some(after_at.to_string())
                        },
                    )
                } else {
                    (variant, None)
                }
            } else {
                (variant, None)
            }
        } else {
            (variant, None)
        };

        let (repo_url, repo_path) = parse_git_repo_url_and_path(url_part)?;
        return Ok(NodeSource::Git {
            repo_url,
            repo_path,
            repo_ref,
        });
    }

    // Plain string = variant name (looked up in root manifest)
    Ok(NodeSource::Fs(PathBuf::from(variant)))
}

pub fn is_supported_http_archive(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".tar.zst") || path.ends_with(".tar.zstd") || path.ends_with(".tzst")
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

/// Walks up from `start_dir` looking for a parent directory containing a valid
/// root `peppy.json5` (one that includes a `manifest` section). Returns the
/// first matching directory, or `None` if no root config is found.
///
/// This allows `peppy node add .` to work when invoked from inside a variant
/// subdirectory: the CLI resolves upward to the root node that owns the variant.
fn find_root_node_dir(start_dir: &Path) -> Option<PathBuf> {
    let mut dir = start_dir.parent()?;
    loop {
        let candidate = dir.join(NODE_CONFIG_FILE);
        if candidate.is_file() && NodeConfigParser::from_path(&candidate).is_ok() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_variant_source_name() {
        let source = parse_variant_source("mock").unwrap();
        let NodeSource::Fs(p) = &source else {
            panic!("expected Fs variant, got {:?}", source)
        };
        assert_eq!(p.to_string_lossy(), "mock");
    }

    #[test]
    fn parse_variant_source_git_with_ref() {
        let source =
            parse_variant_source("https://github.com/example/repo.git/brain@main").unwrap();
        let NodeSource::Git {
            repo_path,
            repo_ref,
            ..
        } = &source
        else {
            panic!("expected Git variant, got {:?}", source)
        };
        assert_eq!(repo_path, "brain");
        assert_eq!(repo_ref.as_deref(), Some("main"));
    }

    #[test]
    fn parse_variant_source_git_without_ref() {
        let source = parse_variant_source("https://github.com/example/repo.git/brain").unwrap();
        let NodeSource::Git {
            repo_path,
            repo_ref,
            ..
        } = &source
        else {
            panic!("expected Git variant, got {:?}", source)
        };
        assert_eq!(repo_path, "brain");
        assert_eq!(*repo_ref, None);
    }

    #[test]
    fn parse_variant_source_git_ref_no_path() {
        let source = parse_variant_source("https://github.com/example/repo.git@v1.0").unwrap();
        let NodeSource::Git {
            repo_path,
            repo_ref,
            ..
        } = &source
        else {
            panic!("expected Git variant, got {:?}", source)
        };
        assert!(repo_path.is_empty());
        assert_eq!(repo_ref.as_deref(), Some("v1.0"));
    }

    #[test]
    fn parse_variant_source_git_scoped_package_not_treated_as_ref() {
        let source =
            parse_variant_source("https://github.com/org/repo.git/packages/@scope/node").unwrap();
        let NodeSource::Git {
            repo_path,
            repo_ref,
            ..
        } = &source
        else {
            panic!("expected Git variant, got {:?}", source)
        };
        assert_eq!(repo_path, "packages/@scope/node");
        assert_eq!(*repo_ref, None);
    }

    #[test]
    fn parse_variant_source_http() {
        let source = parse_variant_source("https://example.com/variant.tar.zst").unwrap();
        let NodeSource::Http { url, .. } = &source else {
            panic!("expected Http variant, got {:?}", source)
        };
        assert_eq!(url.as_str(), "https://example.com/variant.tar.zst");
    }

    #[test]
    fn find_root_node_dir_walks_up_from_variant_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("my_node");
        let variant = root.join("variants").join("linux");
        std::fs::create_dir_all(&variant).unwrap();

        // Root has a valid peppy.json5 (with manifest, default variant, no execution)
        std::fs::write(
            root.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "my_node",
                    tag: "0.1.0",
                    variants: [
                        { name: "default", source: { local: "./variants/linux" } }
                    ]
                },
                interfaces: {}
            }"#,
        )
        .unwrap();

        // Variant has a config without manifest
        std::fs::write(
            variant.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: { language: "rust", start_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let found = find_root_node_dir(&variant);
        assert_eq!(found.as_deref(), Some(root.as_path()));
    }

    #[test]
    fn find_root_node_dir_returns_none_when_no_root() {
        let tmp = tempfile::tempdir().unwrap();
        let variant = tmp.path().join("orphan_variant");
        std::fs::create_dir_all(&variant).unwrap();

        std::fs::write(
            variant.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: { language: "rust", start_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        assert!(find_root_node_dir(&variant).is_none());
    }

    #[test]
    fn parse_node_source_resolves_from_variant_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("my_node");
        let variant = root.join("variants").join("linux");
        std::fs::create_dir_all(&variant).unwrap();

        std::fs::write(
            root.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "my_node",
                    tag: "0.1.0",
                    variants: [
                        { name: "default", source: { local: "./variants/linux" } }
                    ]
                },
                interfaces: {}
            }"#,
        )
        .unwrap();

        std::fs::write(
            variant.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: { language: "rust", start_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let source = parse_node_source(variant.to_str().unwrap(), None).unwrap();
        let NodeSource::Fs(resolved) = &source else {
            panic!("expected Fs source, got {:?}", source)
        };
        // Should resolve to the root, not the variant
        assert_eq!(
            resolved.canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }
}
