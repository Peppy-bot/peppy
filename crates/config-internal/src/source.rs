use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum DeploymentSource {
    Local(DeploymentLocalSource),
    Git(DeploymentGitSource),
    Url(DeploymentUrlSource),
    Repo(DeploymentRepoSource),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentLocalSource {
    pub local: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentGitSource {
    pub repo: String,
    pub path: String,
    #[serde(rename = "ref")]
    pub ref_: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentUrlSource {
    pub url: String,
    pub sha256: String,
}

/// Deployment source that resolves a node through the user's repo cache
/// (`~/.peppy/cache/nodes.json5`). Accepts `{ name, tag }` or the
/// combined `{ name: "<name>:<tag>" }` shorthand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentRepoSource {
    pub name: String,
    pub tag: String,
}

fn invalid_deployment_source<E>(detail: impl Into<String>) -> E
where
    E: de::Error,
{
    let err = crate::error::StructuredError::InvalidDeploymentSource(detail.into());
    de::Error::custom(err.json5_message())
}

fn trim_non_empty<E>(value: String, empty_error: &'static str) -> Result<String, E>
where
    E: de::Error,
{
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid_deployment_source::<E>(empty_error));
    }
    Ok(trimmed.to_owned())
}

fn normalize_git_path<E>(value: String) -> Result<String, E>
where
    E: de::Error,
{
    let pre_trim = value.trim();
    // Reject absolute paths (including leading-slash form) and parent-dir
    // components *before* stripping leading slashes — otherwise `/foo/bar`
    // would be silently coerced to a valid relative path.
    let original_path = Path::new(pre_trim);
    if original_path.is_absolute()
        || original_path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(invalid_deployment_source::<E>(
            "git path cannot be absolute or contain parent-dir components",
        ));
    }
    let trimmed = pre_trim.trim_start_matches('/');
    if trimmed.is_empty() {
        return Err(invalid_deployment_source::<E>("git path cannot be empty"));
    }
    Ok(trimmed.to_owned())
}

fn normalize_http_url<E>(value: String) -> Result<String, E>
where
    E: de::Error,
{
    let trimmed = trim_non_empty::<E>(value, "url cannot be empty")?;

    if trimmed.contains(' ') {
        return Err(invalid_deployment_source::<E>(
            "url must not contain spaces",
        ));
    }

    let (scheme, rest) = if let Some(rest) = trimmed.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(invalid_deployment_source::<E>(
            "url must start with http:// or https://",
        ));
    };
    _ = scheme;

    // rest must have a non-empty host followed by a non-empty path
    let (host_part, path_part) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => {
            return Err(invalid_deployment_source::<E>(
                "url must have a non-empty path",
            ));
        }
    };

    if host_part.is_empty() {
        return Err(invalid_deployment_source::<E>("url must have a host"));
    }

    // path_part starts with '/', so a path of just "/" means no meaningful path
    if path_part == "/" || path_part.is_empty() {
        return Err(invalid_deployment_source::<E>(
            "url must have a non-empty path",
        ));
    }

    Ok(trimmed)
}

fn normalize_sha256_hex<E>(value: String) -> Result<String, E>
where
    E: de::Error,
{
    let trimmed = trim_non_empty::<E>(value, "sha256 cannot be empty")?;
    if trimmed.len() != 64 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid_deployment_source::<E>(
            "sha256 must be a 64-character hexadecimal string",
        ));
    }
    Ok(trimmed.to_ascii_lowercase())
}

fn split_repo_name_and_tag<E>(
    raw_name: String,
    raw_tag: Option<&str>,
) -> Result<(String, String), E>
where
    E: de::Error,
{
    let name_trimmed = raw_name.trim();
    if name_trimmed.is_empty() {
        return Err(invalid_deployment_source::<E>(
            "repo source name cannot be empty",
        ));
    }

    let (name, tag) = if let Some((n, t)) = name_trimmed.split_once(':') {
        if raw_tag.is_some() {
            return Err(invalid_deployment_source::<E>(
                "repo source cannot combine `name: \"<name>:<tag>\"` with a separate `tag` field",
            ));
        }
        if t.contains(':') {
            return Err(invalid_deployment_source::<E>(
                "repo source `name` must contain at most one ':' separating name and tag",
            ));
        }
        (n.trim().to_owned(), t.trim().to_owned())
    } else {
        let tag = raw_tag.map(str::trim).unwrap_or("");
        if tag.is_empty() {
            return Err(invalid_deployment_source::<E>(
                "repo source requires a non-empty `tag` (or the combined `name: \"<name>:<tag>\"` form)",
            ));
        }
        (name_trimmed.to_owned(), tag.to_owned())
    };

    crate::internal::repo_node_id::validate_repo_node_name(&name, "repo source name")
        .map_err(invalid_deployment_source::<E>)?;
    crate::internal::repo_node_id::validate_repo_node_tag(&tag, "repo source tag")
        .map_err(invalid_deployment_source::<E>)?;

    Ok((name, tag))
}

impl<'de> Deserialize<'de> for DeploymentSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawDeploymentSource {
            #[serde(default)]
            local: Option<String>,
            #[serde(default)]
            repo: Option<String>,
            #[serde(default)]
            path: Option<String>,
            #[serde(rename = "ref", default)]
            ref_: Option<String>,
            #[serde(default)]
            url: Option<String>,
            #[serde(default)]
            sha256: Option<String>,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            tag: Option<String>,
        }

        let raw = RawDeploymentSource::deserialize(deserializer)?;
        let has_local = raw.local.is_some();
        let has_git = raw.repo.is_some() || raw.path.is_some() || raw.ref_.is_some();
        let has_url = raw.url.is_some() || raw.sha256.is_some();
        let has_repo = raw.name.is_some() || raw.tag.is_some();

        match (has_local, has_git, has_url, has_repo) {
            (false, false, false, true) => {
                let name_raw = raw.name.ok_or_else(|| {
                    invalid_deployment_source::<D::Error>("repo source requires `name`")
                })?;
                let (name, tag) =
                    split_repo_name_and_tag::<D::Error>(name_raw, raw.tag.as_deref())?;
                Ok(DeploymentSource::Repo(DeploymentRepoSource { name, tag }))
            }
            (true, false, false, false) => {
                let local = trim_non_empty::<D::Error>(
                    raw.local.expect("local is present"),
                    "local path cannot be empty",
                )?;
                let local: PathBuf = Path::new(&local)
                    .components()
                    .filter(|c| !matches!(c, Component::CurDir))
                    .collect();
                Ok(DeploymentSource::Local(DeploymentLocalSource { local }))
            }
            (false, true, false, false) => {
                let repo = raw.repo.ok_or_else(|| {
                    invalid_deployment_source::<D::Error>("git source requires `repo`")
                })?;
                let path = raw.path.ok_or_else(|| {
                    invalid_deployment_source::<D::Error>("git source requires `path`")
                })?;
                let ref_ = raw.ref_.ok_or_else(|| {
                    invalid_deployment_source::<D::Error>("git source requires `ref`")
                })?;

                let repo = trim_non_empty::<D::Error>(repo, "git repo cannot be empty")?;
                let path = normalize_git_path::<D::Error>(path)?;
                let ref_ = trim_non_empty::<D::Error>(ref_, "git ref cannot be empty")?;

                Ok(DeploymentSource::Git(DeploymentGitSource {
                    repo,
                    path,
                    ref_,
                }))
            }
            (false, false, true, false) => {
                let url = raw.url.ok_or_else(|| {
                    invalid_deployment_source::<D::Error>("url source requires `url`")
                })?;
                let sha256 = raw.sha256.ok_or_else(|| {
                    invalid_deployment_source::<D::Error>("url source requires `sha256`")
                })?;

                let url = normalize_http_url::<D::Error>(url)?;
                let sha256 = normalize_sha256_hex::<D::Error>(sha256)?;

                Ok(DeploymentSource::Url(DeploymentUrlSource { url, sha256 }))
            }
            _ => {
                if has_git && has_repo {
                    Err(invalid_deployment_source::<D::Error>(
                        "cannot mix git fields (`repo`, `path`, `ref`) with repo-source fields (`name`, `tag`); use one source type per deployment",
                    ))
                } else {
                    Err(invalid_deployment_source::<D::Error>(
                        "source must be one of: { local }, { repo, path, ref }, { url, sha256 }, { name, tag }",
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ParsingError;

    #[test]
    fn deployment_source_parses_all_variants() {
        let local: DeploymentSource = serde_json5::from_str("{ local: \"./uvc_camera\" }").unwrap();
        let DeploymentSource::Local(local) = local else {
            panic!("expected local source");
        };
        assert_eq!(local.local, PathBuf::from("uvc_camera"));

        let git: DeploymentSource = serde_json5::from_str(
            "{ repo: \"https://github.com/Peppy-bot/nodes_hub.git\", path: \"fake_openarm01_controller\", ref: \"0.1.0\" }",
        )
        .unwrap();
        let DeploymentSource::Git(git) = git else {
            panic!("expected git source");
        };
        assert_eq!(git.repo, "https://github.com/Peppy-bot/nodes_hub.git");
        assert_eq!(git.path, "fake_openarm01_controller");
        assert_eq!(git.ref_, "0.1.0");

        let url: DeploymentSource = serde_json5::from_str(
            "{ url: \"https://example.com/fake_robot_brain.tar.zst\", sha256: \"33e83da60a54e3bb487a9a3b67705918602143b30f158143b6909acaf017a36a\" }",
        )
        .unwrap();
        let DeploymentSource::Url(url) = url else {
            panic!("expected url source");
        };
        assert_eq!(url.url, "https://example.com/fake_robot_brain.tar.zst");
        assert_eq!(
            url.sha256,
            "33e83da60a54e3bb487a9a3b67705918602143b30f158143b6909acaf017a36a"
        );
    }

    #[test]
    fn deployment_source_validation_errors_are_structured() {
        let empty_local: Result<DeploymentSource, _> = serde_json5::from_str("{ local: \"\" }");
        let err = empty_local.expect_err("empty local should fail");
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected invalid deployment source error");
        };
        assert_eq!(msg, "local path cannot be empty");

        let bad_url: Result<DeploymentSource, _> = serde_json5::from_str(
            "{ url: \"ftp://example.com/node.tar.zst\", sha256: \"33e83da60a54e3bb487a9a3b67705918602143b30f158143b6909acaf017a36a\" }",
        );
        let err = bad_url.expect_err("non-http url should fail");
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected invalid deployment source error");
        };
        assert_eq!(msg, "url must start with http:// or https://");

        let bad_sha: Result<DeploymentSource, _> = serde_json5::from_str(
            "{ url: \"https://example.com/node.tar.zst\", sha256: \"not-a-sha\" }",
        );
        let err = bad_sha.expect_err("bad sha256 should fail");
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected invalid deployment source error");
        };
        assert_eq!(msg, "sha256 must be a 64-character hexadecimal string");
    }

    #[test]
    fn deployment_source_rejects_malformed_urls() {
        let valid_sha = "33e83da60a54e3bb487a9a3b67705918602143b30f158143b6909acaf017a36a";

        let cases = [
            (
                format!("{{ url: \"https://\", sha256: \"{valid_sha}\" }}"),
                "url must have a non-empty path",
            ),
            (
                format!("{{ url: \"https:// /node.tar.zst\", sha256: \"{valid_sha}\" }}"),
                "url must not contain spaces",
            ),
            (
                format!("{{ url: \"https://example.com\", sha256: \"{valid_sha}\" }}"),
                "url must have a non-empty path",
            ),
            (
                format!("{{ url: \"https://example.com/\", sha256: \"{valid_sha}\" }}"),
                "url must have a non-empty path",
            ),
        ];

        for (input, expected_msg) in cases {
            let result: Result<DeploymentSource, _> = serde_json5::from_str(&input);
            let err = result.expect_err(&format!("expected failure for: {input}"));
            let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
                panic!("expected InvalidDeploymentSource for: {input}");
            };
            assert_eq!(msg, expected_msg, "wrong message for: {input}");
        }
    }

    // -- DeploymentRepoSource tests --

    #[test]
    fn repo_source_parses_name_and_tag_fields() {
        let src: DeploymentSource =
            serde_json5::from_str("{ name: \"robot_brain\", tag: \"v1\" }").unwrap();
        let DeploymentSource::Repo(repo) = src else {
            panic!("expected repo source");
        };
        assert_eq!(repo.name, "robot_brain");
        assert_eq!(repo.tag, "v1");
    }

    #[test]
    fn repo_source_parses_combined_name_tag() {
        let src: DeploymentSource = serde_json5::from_str("{ name: \"robot_brain:v1\" }").unwrap();
        let DeploymentSource::Repo(repo) = src else {
            panic!("expected repo source");
        };
        assert_eq!(repo.name, "robot_brain");
        assert_eq!(repo.tag, "v1");
    }

    #[test]
    fn repo_source_rejects_name_without_tag() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"foo\" }").unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(msg.contains("non-empty `tag`"), "unexpected: {msg}");
    }

    #[test]
    fn repo_source_rejects_empty_name() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"\", tag: \"v1\" }").unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert_eq!(msg, "repo source name cannot be empty");
    }

    #[test]
    fn repo_source_rejects_combined_with_separate_tag() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"foo:v1\", tag: \"v1\" }")
                .unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(msg.contains("cannot combine"), "unexpected: {msg}");
    }

    #[test]
    fn repo_source_rejects_multiple_colons() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"foo:v1:extra\" }").unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(msg.contains("at most one ':'"), "unexpected: {msg}");
    }

    #[test]
    fn repo_source_rejects_dot_in_tag() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"foo\", tag: \"v1.2\" }")
                .unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(
            msg.contains("disallowed character") && msg.contains("'.'"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn repo_source_rejects_tag_not_starting_with_letter() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"foo\", tag: \"0.1.0\" }")
                .unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(
            msg.contains("must start with an ASCII letter"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn repo_source_rejects_invalid_name_char() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"foo/bar\", tag: \"v1\" }")
                .unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(
            msg.contains("invalid repo source name"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn repo_source_rejects_mixed_with_local() {
        let err: serde_json5::Error = serde_json5::from_str::<DeploymentSource>(
            "{ name: \"foo\", tag: \"v1\", local: \"./x\" }",
        )
        .unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(msg.contains("source must be one of"), "unexpected: {msg}");
    }

    #[test]
    fn repo_source_rejects_mixed_with_git_fields() {
        let err: serde_json5::Error = serde_json5::from_str::<DeploymentSource>(
            "{ repo: \"https://github.com/org/repo.git\", name: \"foo\", tag: \"v1\" }",
        )
        .unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(msg.contains("cannot mix git fields"), "unexpected: {msg}");
    }
}
