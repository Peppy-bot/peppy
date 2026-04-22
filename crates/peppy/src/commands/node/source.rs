use std::path::{Path, PathBuf};

use config::consts::NODE_CONFIG_FILE;
use config::node::{NodeConfigParser, find_root_node_dir};
use core_node_api::encoding::{DepVariantOverride, NodeSource};
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

/// Parsed result of a single `--variant <v>` CLI flag.
#[derive(Debug, PartialEq, Eq)]
pub enum VariantArg {
    /// Root-level variant: a name, a Git URL, or an HTTP URL.
    Root(NodeSource),
    /// Dependency-level override: `name:tag@variant_name`.
    Dep(DepVariantOverride),
}

/// Parses one `--variant` argument into either a root-level variant
/// selection or a dep-level override.
///
/// Accepted shapes:
/// - `name:tag@variant_name` → [`VariantArg::Dep`].
/// - Any URL / plain name → [`VariantArg::Root`].
///
/// Returns an error for malformed dep-override shapes such as `foo@bar`
/// (missing `:tag`), `foo:tag@` (empty variant), `:tag@v` (empty name),
/// and `foo:@v` (empty tag).
pub fn parse_variant_arg(arg: &str) -> Result<VariantArg> {
    if let Some((left, variant)) = arg.rsplit_once('@')
        && looks_like_dep_override_shape(left, variant, arg)
    {
        let (name, tag) = left.split_once(':').expect("split_once validated above");
        if name.is_empty() {
            return Err(Error::ExecutionFailed(format!(
                "invalid --variant '{arg}': empty dependency name before ':'"
            )));
        }
        if tag.is_empty() {
            return Err(Error::ExecutionFailed(format!(
                "invalid --variant '{arg}': empty dependency tag between ':' and '@'"
            )));
        }
        if variant.is_empty() {
            return Err(Error::ExecutionFailed(format!(
                "invalid --variant '{arg}': empty variant name after '@'"
            )));
        }
        return Ok(VariantArg::Dep(DepVariantOverride {
            name: name.to_owned(),
            tag: tag.to_owned(),
            variant: variant.to_owned(),
        }));
    }
    parse_variant_source(arg).map(VariantArg::Root)
}

/// Returns `true` when `left@right` (the original arg split on the
/// **last** `@`) is shaped like a dep-override and not an ordinary
/// URL with an `@ref` suffix.
///
/// - `left` must contain a `:` (the name:tag separator).
/// - `left` must not look like a git URL (no `.git`, no scheme) —
///   otherwise the `@` is a ref marker for [`parse_variant_source`].
fn looks_like_dep_override_shape(left: &str, right: &str, original: &str) -> bool {
    // Exactly one `:` (the name:tag separator) — `a:b:c` is malformed.
    if left.matches(':').count() != 1 {
        return false;
    }
    // `rsplit_once('@')` already peeled off the variant; any remaining `@`
    // in `left` means the original had multiple `@`s (e.g. `a:b@c@v`).
    if left.contains('@') {
        return false;
    }
    // A URL (http://…) will contain `://` — and that colon must not be
    // misinterpreted as a name:tag separator.
    if left.contains("://") || original.contains("://") {
        return false;
    }
    if left.contains(".git") {
        return false;
    }
    if left.starts_with("git@") {
        return false;
    }
    // Name portion should not contain path-separator-like characters.
    if left.contains('/') || left.contains('\\') {
        return false;
    }
    // Variant name can't contain whitespace or slashes.
    if right.chars().any(|c| c.is_whitespace() || c == '/') {
        return false;
    }
    true
}

/// Splits a list of raw `--variant` strings into (root, deps), validating:
/// - at most one root-form `--variant`;
/// - no duplicate dep overrides for the same `name:tag`.
pub fn split_variant_args(raw: &[String]) -> Result<(Option<NodeSource>, Vec<DepVariantOverride>)> {
    let mut root: Option<NodeSource> = None;
    let mut deps: Vec<DepVariantOverride> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    for entry in raw {
        match parse_variant_arg(entry)? {
            VariantArg::Root(src) => {
                if root.is_some() {
                    return Err(Error::ExecutionFailed(
                        "only one root --variant may be given per invocation; use `name:tag@variant` for dependency overrides".to_owned(),
                    ));
                }
                root = Some(src);
            }
            VariantArg::Dep(ov) => {
                let key = (ov.name.clone(), ov.tag.clone());
                if !seen.insert(key) {
                    return Err(Error::ExecutionFailed(format!(
                        "duplicate --variant override for {}:{}",
                        ov.name, ov.tag
                    )));
                }
                deps.push(ov);
            }
        }
    }

    Ok((root, deps))
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

/// Resolves the root node directory from a candidate path.
///
/// If `dir` contains a valid root `peppy.json5` whose manifest declares
/// variants, returns `dir` as-is. If the config has a manifest but no
/// variants (ambiguous: could be a standalone root or a variant config that
/// carries a manifest), walks up looking for a parent root with variants and
/// falls back to `dir` when none is found. If the config is a variant config
/// (missing `manifest`), walks up the directory tree to locate the root node.
/// Returns an error when no root config can be found or when the config
/// contains parse errors.
pub fn resolve_node_root_dir(dir: &Path) -> Result<PathBuf> {
    let config_path = dir.join(NODE_CONFIG_FILE);
    match NodeConfigParser::from_path(&config_path) {
        Ok(cfg) if cfg.has_variants() => Ok(dir.to_path_buf()),
        Ok(_) => {
            // Config has manifest + execution but no variants. Could be a
            // standalone root or a variant config that carries a manifest.
            // Walk up looking for a parent root with variants; if none is
            // found, treat this directory as the root itself.
            match find_root_node_dir(dir).map_err(config_parse_error(dir))? {
                Some(root) => Ok(root),
                None => Ok(dir.to_path_buf()),
            }
        }
        Err(config::ConfigError::Parsing(ref e)) if e.is_missing_manifest() => {
            match find_root_node_dir(dir).map_err(config_parse_error(dir))? {
                Some(root) => Ok(root),
                None => Err(Error::ExecutionFailed(format!(
                    "No root {} with a `manifest` section found at '{}' or any parent directory",
                    NODE_CONFIG_FILE,
                    dir.display(),
                ))),
            }
        }
        Err(other) => Err(Error::ExecutionFailed(format!(
            "Failed to parse '{}' in directory {}: {}",
            NODE_CONFIG_FILE,
            dir.display(),
            other,
        ))),
    }
}

/// Adapts a [`config::ConfigError`] raised while walking up ancestor directories
/// into the CLI's [`Error`] type.
fn config_parse_error(dir: &Path) -> impl Fn(config::ConfigError) -> Error + '_ {
    move |err| {
        Error::ExecutionFailed(format!(
            "Failed to parse ancestor `{}` while resolving node root from {}: {}",
            NODE_CONFIG_FILE,
            dir.display(),
            err,
        ))
    }
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
    fn looks_like_repo_node_ref_rejects_arg_with_at() {
        // `foo:bar@variant` is a dep-variant override, not a source.
        assert!(!looks_like_repo_node_ref("foo:bar@x"));
    }

    #[test]
    fn parse_node_source_recognizes_name_tag() {
        let src = parse_node_source("some_node:1.2.3", None).unwrap();
        let NodeSource::RepoNode { name, tag, .. } = &src else {
            panic!("expected RepoNode, got {:?}", src);
        };
        assert_eq!(name, "some_node");
        assert_eq!(tag, "1.2.3");
    }

    #[test]
    fn parse_variant_arg_plain_name_is_root() {
        let got = parse_variant_arg("mock-python").unwrap();
        assert_eq!(
            got,
            VariantArg::Root(NodeSource::Fs(PathBuf::from("mock-python")))
        );
    }

    #[test]
    fn parse_variant_arg_http_url_is_root() {
        let got = parse_variant_arg("https://example.com/variant.tar.zst").unwrap();
        assert!(matches!(got, VariantArg::Root(NodeSource::Http { .. })));
    }

    #[test]
    fn parse_variant_arg_git_url_is_root() {
        let got = parse_variant_arg("https://github.com/foo/bar.git/x").unwrap();
        assert!(matches!(got, VariantArg::Root(NodeSource::Git { .. })));
    }

    #[test]
    fn parse_variant_arg_dep_override_parsed() {
        let got = parse_variant_arg("uvc_camera:0.1.0@mock-python").unwrap();
        match got {
            VariantArg::Dep(ov) => {
                assert_eq!(ov.name, "uvc_camera");
                assert_eq!(ov.tag, "0.1.0");
                assert_eq!(ov.variant, "mock-python");
            }
            other => panic!("expected dep override, got {:?}", other),
        }
    }

    #[test]
    fn parse_variant_arg_rejects_malformed_dep() {
        // Missing :tag before @
        assert!(parse_variant_arg("foo@bar").is_ok()); // `foo@bar` => root variant name "foo@bar"
        // Empty tag between ':' and '@'
        assert!(parse_variant_arg("foo:@bar").is_err());
        // Empty variant after '@'
        assert!(parse_variant_arg("foo:tag@").is_err());
        // Empty name before ':'
        assert!(parse_variant_arg(":tag@variant").is_err());
    }

    #[test]
    fn parse_variant_arg_rejects_extra_colons_in_dep_shape() {
        // `a:b:c@v` has two `:` in the left — it is not a well-formed
        // dep override and must not produce `VariantArg::Dep`. It should
        // fall through to `parse_variant_source` and be treated as a
        // (malformed but non-dep) plain-name root variant.
        let got = parse_variant_arg("a:b:c@v").unwrap();
        assert!(
            !matches!(got, VariantArg::Dep(_)),
            "expected non-Dep parse for 'a:b:c@v', got {:?}",
            got
        );
    }

    #[test]
    fn parse_variant_arg_rejects_extra_at_in_dep_shape() {
        // `a:b@c@v` — after peeling the last `@`, `left = "a:b@c"` still
        // contains `@`, which means the string has too many `@` markers
        // for a dep override.
        let got = parse_variant_arg("a:b@c@v").unwrap();
        assert!(
            !matches!(got, VariantArg::Dep(_)),
            "expected non-Dep parse for 'a:b@c@v', got {:?}",
            got
        );
    }

    #[test]
    fn split_variant_args_accepts_root_only() {
        let (root, deps) = split_variant_args(&["mock-python".to_owned()]).unwrap();
        assert!(root.is_some());
        assert!(deps.is_empty());
    }

    #[test]
    fn split_variant_args_accepts_deps_only() {
        let (root, deps) =
            split_variant_args(&["a:1.0@v1".to_owned(), "b:2.0@v2".to_owned()]).unwrap();
        assert!(root.is_none());
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn split_variant_args_accepts_root_plus_deps() {
        let (root, deps) =
            split_variant_args(&["mock-python".to_owned(), "a:1.0@v1".to_owned()]).unwrap();
        assert!(root.is_some());
        assert_eq!(deps.len(), 1);
    }

    #[test]
    fn split_variant_args_rejects_two_root_forms() {
        let err = split_variant_args(&["mock-python".to_owned(), "alt".to_owned()]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("only one root --variant"),
            "expected multi-root rejection, got: {msg}"
        );
    }

    #[test]
    fn split_variant_args_rejects_duplicate_dep_names() {
        let err = split_variant_args(&["a:1.0@v1".to_owned(), "a:1.0@v2".to_owned()]).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

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
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let found = find_root_node_dir(&variant).unwrap();
        assert_eq!(found, Some(root));
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
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let found = find_root_node_dir(&variant).unwrap();
        assert!(found.is_none(), "expected None when no root exists");
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
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
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
        assert_eq!(found, Some(root));
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
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
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
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
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
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
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
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
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
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let err = find_root_node_dir(&variant).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Cannot parse configuration") || msg.contains("Failed to parse"),
            "expected parse error from parent to be surfaced, got: {msg}"
        );
    }

    #[test]
    fn resolve_node_root_dir_walks_up_from_variant_with_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("my_node");
        let variant = root.join("variants").join("linux");
        std::fs::create_dir_all(&variant).unwrap();

        // Root has a valid peppy.json5 with variants
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

        // Variant carries a manifest with different name/tag (should NOT be treated as root)
        std::fs::write(
            variant.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "linux-variant",
                    tag: "9.9.9",
                },
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let resolved = resolve_node_root_dir(&variant).unwrap();
        assert_eq!(resolved, root);

        // Verify the resolved root has the root manifest, not the variant's
        let root_config = NodeConfigParser::from_path(resolved.join(NODE_CONFIG_FILE)).unwrap();
        assert_eq!(root_config.manifest_name(), "my_node");
        assert_eq!(root_config.manifest_tag(), "0.1.0");
    }

    #[test]
    fn find_root_node_dir_walks_up_past_variant_with_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("my_node");
        let platform = root.join("variants").join("platform");
        let variant = platform.join("linux");
        std::fs::create_dir_all(&variant).unwrap();

        // Root has variants
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

        // Intermediate directory has a manifest-bearing config with different name/tag
        std::fs::write(
            platform.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "platform-variant",
                    tag: "9.9.9",
                },
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let found = find_root_node_dir(&variant).unwrap();
        assert_eq!(found, Some(root.clone()));

        // Verify the found root has the root manifest, not the intermediate variant's
        let root_config =
            NodeConfigParser::from_path(found.unwrap().join(NODE_CONFIG_FILE)).unwrap();
        assert_eq!(root_config.manifest_name(), "my_node");
        assert_eq!(root_config.manifest_tag(), "0.1.0");
    }

    #[test]
    fn resolve_node_root_dir_returns_standalone_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("simple_node");
        std::fs::create_dir_all(&root).unwrap();

        // Simple root: manifest + execution, no variants
        std::fs::write(
            root.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "simple_node",
                    tag: "0.1.0",
                },
                execution: { language: "rust", run_cmd: ["./bin"] }
            }"#,
        )
        .unwrap();

        let resolved = resolve_node_root_dir(&root).unwrap();
        assert_eq!(resolved, root);
    }
}
