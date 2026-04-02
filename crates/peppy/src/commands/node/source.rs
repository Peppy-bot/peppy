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

    let from_dir = resolve_node_root_dir(&from_dir)?;

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

/// Resolves the root node directory from a candidate path.
///
/// If `dir` contains a valid root `peppy.json5` (with a `manifest` section),
/// returns `dir` as-is. If the config is a variant config (missing `manifest`),
/// walks up the directory tree to locate the root node. Returns an error when
/// no root config can be found or when the config contains parse errors.
pub fn resolve_node_root_dir(dir: &Path) -> Result<PathBuf> {
    let config_path = dir.join(NODE_CONFIG_FILE);
    match NodeConfigParser::from_path(&config_path) {
        Ok(_) => Ok(dir.to_path_buf()),
        Err(config::ConfigError::Parsing(ref e)) if e.is_missing_manifest() => {
            find_root_node_dir(dir)
        }
        Err(other) => Err(Error::ExecutionFailed(format!(
            "Failed to parse '{}' in directory {}: {}",
            NODE_CONFIG_FILE,
            dir.display(),
            other,
        ))),
    }
}

/// Walks up from `start_dir` looking for a parent directory containing a valid
/// root `peppy.json5` (one that includes a `manifest` section). Returns the
/// first matching directory, or an error if no root config is found or a
/// config file contains invalid syntax.
///
/// This allows `peppy node add .` to work when invoked from inside a variant
/// subdirectory: the CLI resolves upward to the root node that owns the variant.
pub fn find_root_node_dir(start_dir: &Path) -> Result<PathBuf> {
    let mut dir = start_dir.parent().ok_or_else(|| {
        Error::ExecutionFailed(format!(
            "No root {} with a `manifest` section found at '{}' or any parent directory",
            NODE_CONFIG_FILE,
            start_dir.display(),
        ))
    })?;
    loop {
        let candidate = dir.join(NODE_CONFIG_FILE);
        if candidate.is_file() {
            match NodeConfigParser::from_path(&candidate) {
                Ok(_) => return Ok(dir.to_path_buf()),
                Err(config::ConfigError::Parsing(ref e)) if e.is_missing_manifest() => {}
                Err(other) => {
                    return Err(Error::ExecutionFailed(format!(
                        "Failed to parse '{}': {}",
                        candidate.display(),
                        other,
                    )));
                }
            }
        }
        dir = dir.parent().ok_or_else(|| {
            Error::ExecutionFailed(format!(
                "No root {} with a `manifest` section found at '{}' or any parent directory",
                NODE_CONFIG_FILE,
                start_dir.display(),
            ))
        })?;
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

        let found = find_root_node_dir(&variant).unwrap();
        assert_eq!(found, root);
    }

    #[test]
    fn find_root_node_dir_returns_err_when_no_root() {
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

        let err = find_root_node_dir(&variant).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("No root peppy.json5 with a `manifest` section found"),
            "expected missing-manifest error, got: {msg}"
        );
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

    #[test]
    fn find_root_node_dir_walks_up_multiple_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace").join("nodes").join("my_node");
        // Deeply nested: root/variants/mock_rust/src/impl
        let deep = root
            .join("variants")
            .join("mock_rust")
            .join("src")
            .join("impl");
        std::fs::create_dir_all(&deep).unwrap();

        std::fs::write(
            root.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "my_node",
                    tag: "0.1.0",
                    variants: [
                        { name: "default", source: { local: "./variants/mock_rust" } }
                    ]
                },
                interfaces: {}
            }"#,
        )
        .unwrap();

        // No peppy.json5 in any intermediate directories — only at root
        let found = find_root_node_dir(&deep).unwrap();
        assert_eq!(found, root);
    }

    #[test]
    fn resolve_node_root_dir_walks_up_multiple_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("my_node");
        let variant = root.join("variants").join("mock_rust");
        let deep = variant.join("src").join("impl");
        std::fs::create_dir_all(&deep).unwrap();

        std::fs::write(
            root.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "my_node",
                    tag: "0.1.0",
                    variants: [
                        { name: "default", source: { local: "./variants/mock_rust" } }
                    ]
                },
                interfaces: {}
            }"#,
        )
        .unwrap();

        // Variant dir has a config without manifest
        std::fs::write(
            variant.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: { language: "rust", start_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let resolved = resolve_node_root_dir(&variant).unwrap();
        assert_eq!(resolved, root);
    }

    #[test]
    fn parse_node_source_resolves_from_deeply_nested_variant_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("my_node");
        let variant = root.join("variants").join("platform").join("linux");
        std::fs::create_dir_all(&variant).unwrap();

        std::fs::write(
            root.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "my_node",
                    tag: "0.1.0",
                    variants: [
                        { name: "default", source: { local: "./variants/platform/linux" } }
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
        assert_eq!(
            resolved.canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn parse_node_source_errors_when_no_root_found() {
        let tmp = tempfile::tempdir().unwrap();
        let orphan = tmp.path().join("orphan_variant");
        std::fs::create_dir_all(&orphan).unwrap();

        std::fs::write(
            orphan.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: { language: "rust", start_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let err = parse_node_source(orphan.to_str().unwrap(), None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("No root peppy.json5 with a `manifest` section found"),
            "expected missing-manifest error, got: {msg}"
        );
    }

    #[test]
    fn resolve_node_root_dir_errors_when_no_root_found() {
        let tmp = tempfile::tempdir().unwrap();
        let orphan = tmp.path().join("orphan_variant");
        std::fs::create_dir_all(&orphan).unwrap();

        std::fs::write(
            orphan.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: { language: "rust", start_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let err = resolve_node_root_dir(&orphan).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("No root peppy.json5 with a `manifest` section found"),
            "expected missing-manifest error, got: {msg}"
        );
    }

    #[test]
    fn resolve_node_root_dir_surfaces_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bad_node");
        std::fs::create_dir_all(&dir).unwrap();

        // Write malformed JSON5 (unclosed brace, invalid syntax)
        std::fs::write(dir.join(NODE_CONFIG_FILE), r#"{ manifest: [unclosed"#).unwrap();

        let err = resolve_node_root_dir(&dir).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Failed to parse"),
            "expected parse error to be surfaced, got: {msg}"
        );
    }

    #[test]
    fn find_root_node_dir_surfaces_parse_error_from_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("my_node");
        let variant = root.join("variants").join("linux");
        std::fs::create_dir_all(&variant).unwrap();

        // Root has a malformed peppy.json5
        std::fs::write(
            root.join(NODE_CONFIG_FILE),
            r#"{ this is totally broken json5 {{{"#,
        )
        .unwrap();

        // Variant has a valid variant config (missing manifest)
        std::fs::write(
            variant.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: { language: "rust", start_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let err = find_root_node_dir(&variant).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Failed to parse"),
            "expected parse error from parent to be surfaced, got: {msg}"
        );
    }
}
