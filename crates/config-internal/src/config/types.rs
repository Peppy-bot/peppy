use crate::{common::NodeParameters, error::ParsingError, node::Logging};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
    ser::{self, Serializer},
};
use std::{
    convert::TryFrom,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeppyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployments: Option<Vec<Deployment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Logging>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<DeploymentNodeSource>,
    pub tag: String,
    pub instances: Vec<DeploymentInstance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentInstance {
    pub namespace: Namespace,
    #[serde(default)]
    pub parameters: NodeParameters,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentNodeSource {
    Local(PathBuf),
    Git(GitRemoteSpec),
    Http(String),
}

impl DeploymentNodeSource {
    const FILE_SCHEME: &'static str = "file://";

    pub fn is_local(&self) -> bool {
        matches!(self, DeploymentNodeSource::Local(_))
    }

    pub fn as_local_path(&self) -> Option<&Path> {
        match self {
            DeploymentNodeSource::Local(path) => Some(path.as_path()),
            _ => None,
        }
    }

    pub fn git(&self) -> Option<&GitRemoteSpec> {
        match self {
            DeploymentNodeSource::Git(spec) => Some(spec),
            _ => None,
        }
    }

    pub fn http(&self) -> Option<&str> {
        match self {
            DeploymentNodeSource::Http(url) => Some(url.as_str()),
            _ => None,
        }
    }

    pub fn from_str(value: &str) -> Result<Self, ParsingError> {
        Self::from_string(value.to_owned())
    }

    fn from_string(value: String) -> Result<Self, ParsingError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(ParsingError::InvalidDeploymentSource(
                "source cannot be empty".to_string(),
            ));
        }

        if let Some(rest) = trimmed.strip_prefix(Self::FILE_SCHEME) {
            if rest.is_empty() {
                return Err(ParsingError::InvalidDeploymentSource(
                    "file path cannot be empty".to_string(),
                ));
            }
            return Ok(DeploymentNodeSource::Local(PathBuf::from(rest)));
        }

        if Self::is_http_url(trimmed) && !Self::looks_like_git(trimmed) {
            return Ok(DeploymentNodeSource::Http(trimmed.to_owned()));
        }

        let spec = Self::parse_git_spec(trimmed)?;
        Ok(DeploymentNodeSource::Git(spec))
    }

    fn from_git_fields(repo: String, path: Option<String>) -> Result<Self, ParsingError> {
        if repo.trim().is_empty() {
            return Err(ParsingError::InvalidDeploymentSource(
                "git repo cannot be empty".to_string(),
            ));
        }

        Ok(DeploymentNodeSource::Git(GitRemoteSpec {
            repo,
            path: Self::normalize_git_path(path),
        }))
    }

    fn normalize_git_path(path: Option<String>) -> Option<String> {
        path.and_then(|segment| {
            let trimmed = segment.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.trim_start_matches('/').to_owned())
            }
        })
    }

    fn parse_git_spec(value: &str) -> Result<GitRemoteSpec, ParsingError> {
        let (repo_raw, path_raw) = value
            .split_once("::")
            .map(|(repo, path)| (repo.trim(), Some(path.trim())))
            .unwrap_or_else(|| (value.trim(), None));

        if repo_raw.is_empty() {
            return Err(ParsingError::InvalidDeploymentSource(
                "git repo cannot be empty".to_string(),
            ));
        }

        let path = Self::normalize_git_path(path_raw.map(|segment| segment.to_owned()));

        Ok(GitRemoteSpec {
            repo: repo_raw.to_owned(),
            path,
        })
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
    }
}

impl Serialize for DeploymentNodeSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            DeploymentNodeSource::Local(path) => {
                let path_str = path
                    .to_str()
                    .ok_or_else(|| ser::Error::custom("local path is not valid UTF-8"))?;
                serializer.serialize_str(&format!("{}{}", Self::FILE_SCHEME, path_str))
            }
            DeploymentNodeSource::Git(spec) => {
                if let Some(path) = spec.path.as_deref() {
                    #[derive(Serialize)]
                    struct GitSource<'a> {
                        repo: &'a str,
                        #[serde(skip_serializing_if = "Option::is_none")]
                        path: Option<&'a str>,
                    }

                    let helper = GitSource {
                        repo: &spec.repo,
                        path: Some(path),
                    };
                    helper.serialize(serializer)
                } else {
                    serializer.serialize_str(&spec.repo)
                }
            }
            DeploymentNodeSource::Http(url) => serializer.serialize_str(url),
        }
    }
}

impl<'de> Deserialize<'de> for DeploymentNodeSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawNodeSource {
            String(String),
            InlineGit(RawGitSpec),
            Git { git: RawGitSpec },
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawGitSpec {
            repo: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            path: Option<String>,
        }

        match RawNodeSource::deserialize(deserializer)? {
            RawNodeSource::String(value) => {
                DeploymentNodeSource::from_string(value).map_err(de::Error::custom)
            }
            RawNodeSource::InlineGit(git) => {
                DeploymentNodeSource::from_git_fields(git.repo, git.path).map_err(de::Error::custom)
            }
            RawNodeSource::Git { git } => {
                DeploymentNodeSource::from_git_fields(git.repo, git.path).map_err(de::Error::custom)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitRemoteSpec {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl GitRemoteSpec {
    pub fn as_remote(&self) -> String {
        match &self.path {
            Some(path) if !path.is_empty() => format!("{}::{}", self.repo, path),
            _ => self.repo.clone(),
        }
    }
}

/// Validated namespace. Same as Name but allows '/'.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Namespace(String);

impl Namespace {
    pub fn new<S: Into<String>>(s: S) -> Result<Self, ParsingError> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid_char(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '/'
    }
}

impl TryFrom<String> for Namespace {
    type Error = ParsingError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(ParsingError::InvalidNamespace(
                "Namespace cannot be empty".to_string(),
            ));
        }
        if value.chars().all(Namespace::is_valid_char) {
            return Ok(Namespace(value));
        }
        Err(ParsingError::InvalidNamespace(value))
    }
}

impl From<Namespace> for String {
    fn from(v: Namespace) -> Self {
        v.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_validation() {
        assert!(Namespace::new("/").is_ok());
        assert!(Namespace::new("/robot").is_ok());
        assert!(Namespace::new("/robot/camera_v1").is_ok());

        assert!(Namespace::new("").is_err()); // empty not permitted
        assert!(Namespace::new("/Robot").is_err()); // capital
        assert!(Namespace::new("/robot$cam").is_err()); // special
    }

    #[test]
    fn node_source_validation() {
        let local: DeploymentNodeSource = serde_json5::from_str("\"file:///tmp/node\"").unwrap();
        let DeploymentNodeSource::Local(local_path) = local else {
            panic!("expected local node source");
        };
        assert_eq!(local_path.as_path(), Path::new("/tmp/node"));

        let http: DeploymentNodeSource =
            serde_json5::from_str("\"https://nodes.peppy.bot/nodes/camera\"").unwrap();
        let DeploymentNodeSource::Http(http_url) = http else {
            panic!("expected http node source");
        };
        assert_eq!(http_url, "https://nodes.peppy.bot/nodes/camera");

        let git_inline: DeploymentNodeSource = serde_json5::from_str(
            "{ repo: \"https://github.com/Peppy/nodes.git\", path: \"uvc_camera\" }",
        )
        .unwrap();
        let DeploymentNodeSource::Git(GitRemoteSpec {
            repo: inline_repo,
            path: inline_path,
        }) = git_inline
        else {
            panic!("expected git node source for inline format");
        };
        assert_eq!(inline_repo, "https://github.com/Peppy/nodes.git");
        assert_eq!(inline_path.as_deref(), Some("uvc_camera"));

        let git_full: DeploymentNodeSource = serde_json5::from_str(
            "{ git: { repo: \"https://github.com/Peppy/uvc_camera.git\", path: \"configs/camera\" } }",
        )
        .unwrap();
        let DeploymentNodeSource::Git(GitRemoteSpec { repo, path }) = git_full else {
            panic!("expected git node source for full format");
        };
        assert_eq!(repo, "https://github.com/Peppy/uvc_camera.git");
        assert_eq!(path.as_deref(), Some("configs/camera"));

        let git_string: DeploymentNodeSource =
            serde_json5::from_str("\"https://github.com/Peppy/uvc_camera.git\"").unwrap();
        let DeploymentNodeSource::Git(GitRemoteSpec {
            repo: string_repo,
            path: string_path,
        }) = git_string
        else {
            panic!("expected git node source for string format");
        };
        assert_eq!(string_repo, "https://github.com/Peppy/uvc_camera.git");
        assert!(string_path.is_none());

        let defaulted: Deployment = serde_json5::from_str(
            r#"{
                name: "controller",
                tag: "0.1.0",
                instances: [{ namespace: "/" }]
            }"#,
        )
        .unwrap();
        assert!(defaulted.source.is_none());

        let empty: Result<DeploymentNodeSource, _> = serde_json5::from_str("\"\"");
        let err = empty.expect_err("deserializing an empty node source should fail");
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected invalid deployment source error");
        };
        assert_eq!(msg, "source cannot be empty");
    }
}
