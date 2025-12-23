use crate::{common::NodeArguments, error::ParsingError, node::Logging};
use core::fmt;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
    ser::{self, Serializer},
};
use std::{
    collections::HashSet,
    convert::TryFrom,
    path::{Path, PathBuf},
    str::FromStr,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum BuildSystem {
    #[default]
    Rust, // Defaults to Cargo
    Python, // Defaults to uv
    Cargo,
    Uv,
}

impl fmt::Display for BuildSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildSystem::Python | BuildSystem::Uv => write!(f, "uv"),
            BuildSystem::Rust | BuildSystem::Cargo => write!(f, "cargo"),
        }
    }
}

impl FromStr for BuildSystem {
    type Err = ParsingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "rust" | "cargo" => Ok(BuildSystem::Cargo),
            "python" | "uv" => Ok(BuildSystem::Uv),
            _ => Err(ParsingError::InvalidBuildSystem(s.to_owned())),
        }
    }
}

/// Version identifier embedded in node `peppy.json5` manifests.
/// Using a simple alias keeps serialization straightforward while making the intent explicit.
pub type SchemaVersion = u16;
pub const CURRENT_SCHEMA_VERSION: SchemaVersion = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeppyLauncher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployments: Option<Vec<Deployment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Logging>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub name: Name,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<DeploymentNodeSource>,
    pub tag: String,
    #[serde(default)]
    pub optional: bool,
    #[serde(deserialize_with = "deserialize_instances")]
    pub instances: Vec<DeploymentInstance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentInstance {
    pub instance_id: Name,
    #[serde(default, alias = "parameters")]
    pub arguments: NodeArguments,
}

fn deserialize_instances<'de, D>(deserializer: D) -> Result<Vec<DeploymentInstance>, D::Error>
where
    D: Deserializer<'de>,
{
    let instances = Vec::<DeploymentInstance>::deserialize(deserializer)?;
    let mut seen = HashSet::new();
    for instance in &instances {
        let id = instance.instance_id.as_str();
        if !seen.insert(id.to_owned()) {
            if !seen.insert(id.to_owned()) {
                let err = crate::error::StructuredError::DuplicateName(id.to_owned());
                let msg = serde_json5::to_string(&err)
                    .unwrap_or_else(|_| "serialization error".to_string());
                return Err(de::Error::custom(msg));
            }
        }
    }
    Ok(instances)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentNodeSource {
    Local(PathBuf),
    Git(GitRemoteSpec),
    Http(HttpRemoteSpec),
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

    pub fn http(&self) -> Option<&HttpRemoteSpec> {
        match self {
            DeploymentNodeSource::Http(spec) => Some(spec),
            _ => None,
        }
    }

    fn from_string(value: String) -> Result<Self, ParsingError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            if trimmed.is_empty() {
                let err = crate::error::StructuredError::InvalidDeploymentSource(
                    "source cannot be empty".to_string(),
                );
                let msg = serde_json5::to_string(&err)
                    .unwrap_or_else(|_| "serialization error".to_string());
                return Err(ParsingError::Structured(msg));
            }
        }

        if let Some(rest) = trimmed.strip_prefix(Self::FILE_SCHEME) {
            if rest.is_empty() {
                let err = crate::error::StructuredError::InvalidDeploymentSource(
                    "file path cannot be empty".to_string(),
                );
                let msg = serde_json5::to_string(&err)
                    .unwrap_or_else(|_| "serialization error".to_string());
                return Err(ParsingError::Structured(msg));
            }
            return Ok(DeploymentNodeSource::Local(PathBuf::from(rest)));
        }

        if Self::is_http_url(trimmed) && !Self::looks_like_git(trimmed) {
            let spec = HttpRemoteSpec::new(trimmed.to_owned(), None)?;
            return Ok(DeploymentNodeSource::Http(spec));
        }

        let spec = Self::parse_git_spec(trimmed)?;
        Ok(DeploymentNodeSource::Git(spec))
    }

    fn from_git_fields(repo: String, path: Option<String>) -> Result<Self, ParsingError> {
        if repo.trim().is_empty() {
            let err = crate::error::StructuredError::InvalidDeploymentSource(
                "git repo cannot be empty".to_string(),
            );
            let msg =
                serde_json5::to_string(&err).unwrap_or_else(|_| "serialization error".to_string());
            return Err(ParsingError::Structured(msg));
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
            let err = crate::error::StructuredError::InvalidDeploymentSource(
                "git repo cannot be empty".to_string(),
            );
            let msg =
                serde_json5::to_string(&err).unwrap_or_else(|_| "serialization error".to_string());
            return Err(ParsingError::Structured(msg));
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

impl FromStr for DeploymentNodeSource {
    type Err = ParsingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_string(value.to_owned())
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
            DeploymentNodeSource::Http(spec) => {
                if spec.checksum.is_none() {
                    serializer.serialize_str(&spec.bundle_url)
                } else {
                    spec.serialize(serializer)
                }
            }
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
            Http(RawHttpSpec),
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawGitSpec {
            repo: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            path: Option<String>,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawHttpSpec {
            bundle_url: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            checksum: Option<String>,
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
            RawNodeSource::Http(http) => HttpRemoteSpec::new(http.bundle_url, http.checksum)
                .map(DeploymentNodeSource::Http)
                .map_err(de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpRemoteSpec {
    pub bundle_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

impl HttpRemoteSpec {
    pub fn new(bundle_url: String, checksum: Option<String>) -> Result<Self, ParsingError> {
        let trimmed = bundle_url.trim();
        if trimmed.is_empty() {
            let err = crate::error::StructuredError::InvalidDeploymentSource(
                "http bundle url cannot be empty".to_string(),
            );
            let msg =
                serde_json5::to_string(&err).unwrap_or_else(|_| "serialization error".to_string());
            return Err(ParsingError::Structured(msg));
        }

        if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
            let err = crate::error::StructuredError::InvalidDeploymentSource(
                "http bundle url must start with http:// or https://".to_string(),
            );
            let msg =
                serde_json5::to_string(&err).unwrap_or_else(|_| "serialization error".to_string());
            return Err(ParsingError::Structured(msg));
        }

        Ok(Self {
            bundle_url: trimmed.to_owned(),
            checksum,
        })
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(into = "String")]
pub struct Name(String);

use crate::consts::ALLOWED_CONFIG_CHARS;

impl Name {
    pub fn new<S: Into<String>>(s: S) -> Result<Self, ParsingError> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid_char(c: char) -> bool {
        ALLOWED_CONFIG_CHARS.contains(c)
    }
}

impl TryFrom<String> for Name {
    type Error = ParsingError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(ParsingError::EmptyName);
        }
        if value.chars().all(Name::is_valid_char) {
            return Ok(Name(value));
        }
        Err(ParsingError::InvalidName(
            value,
            ALLOWED_CONFIG_CHARS.to_string(),
        ))
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Name::try_from(s).map_err(|err| {
            let structured = match err {
                ParsingError::EmptyName => crate::error::StructuredError::EmptyName,
                ParsingError::InvalidName(name, allowed) => {
                    crate::error::StructuredError::InvalidName { name, allowed }
                }
                _ => return de::Error::custom(err.to_string()),
            };
            let msg = serde_json5::to_string(&structured)
                .unwrap_or_else(|_| "serialization error".to_string());
            de::Error::custom(msg)
        })
    }
}

impl From<Name> for String {
    fn from(v: Name) -> Self {
        v.0
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<Name> for &str {
    fn eq(&self, other: &Name) -> bool {
        *self == other.0
    }
}

impl PartialEq<String> for Name {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl PartialEq<Name> for String {
    fn eq(&self, other: &Name) -> bool {
        *self == other.0
    }
}

impl PartialOrd for Name {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Name {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation() {
        assert!(Name::new("robot").is_ok());
        assert!(Name::new("camera_v1").is_ok());

        assert!(Name::new("").is_err()); // empty not permitted
        assert!(Name::new("/").is_err()); // slash not permitted
        assert!(Name::new("/robot").is_err()); // slash not permitted
        assert!(Name::new("Robot").is_ok()); // capital now allowed
        assert!(Name::new("robot$cam").is_err()); // special
    }

    #[test]
    fn name_error_message() {
        let err = Name::new("Invalid!").unwrap_err();
        if let ParsingError::InvalidName(_, msg) = err {
            assert_eq!(msg, crate::consts::ALLOWED_CONFIG_CHARS);
        } else {
            panic!("Expected InvalidName error");
        }
    }

    #[test]
    fn node_source_validation() {
        let local: DeploymentNodeSource = serde_json5::from_str("\"file:///tmp/node\"").unwrap();
        let DeploymentNodeSource::Local(local_path) = local else {
            panic!("expected local node source");
        };
        assert_eq!(local_path.as_path(), Path::new("/tmp/node"));

        let http: DeploymentNodeSource =
            serde_json5::from_str("\"https://nodes.peppy.bot/nodes/camera.tar.zst\"").unwrap();
        let DeploymentNodeSource::Http(http_spec) = http else {
            panic!("expected http node source");
        };
        assert_eq!(
            http_spec.bundle_url,
            "https://nodes.peppy.bot/nodes/camera.tar.zst"
        );
        assert!(http_spec.checksum.is_none());

        let http_with_checksum: DeploymentNodeSource = serde_json5::from_str(
            "{ bundle_url: \"https://nodes.peppy.bot/nodes/camera.tar.zst\", checksum: \"sha256:deadbeef\" }",
        )
        .unwrap();
        let DeploymentNodeSource::Http(http_spec) = http_with_checksum else {
            panic!("expected http node source with checksum");
        };
        assert_eq!(
            http_spec.bundle_url,
            "https://nodes.peppy.bot/nodes/camera.tar.zst"
        );
        assert_eq!(http_spec.checksum.as_deref(), Some("sha256:deadbeef"));

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
                instances: []
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

    #[test]
    fn http_source_with_checksum_round_trip() {
        let json = "{ bundle_url: \"https://example.com/nodes/uvc_camera.tar.zst\", checksum: \"sha256:0011aa\" }";
        let source: DeploymentNodeSource =
            serde_json5::from_str(json).expect("parse http source with checksum");

        let DeploymentNodeSource::Http(spec) = &source else {
            panic!("expected http deployment source");
        };
        assert_eq!(
            spec.bundle_url,
            "https://example.com/nodes/uvc_camera.tar.zst"
        );
        assert_eq!(spec.checksum.as_deref(), Some("sha256:0011aa"));

        let serialized = serde_json5::to_string(&source).expect("serialize http deployment source");
        let round_trip: DeploymentNodeSource =
            serde_json5::from_str(&serialized).expect("re-parse serialized http deployment source");
        assert_eq!(round_trip, source);
    }

    #[test]
    fn duplicate_instance_ids_are_rejected() {
        let duplicate_instances = r#"{
            name: "uvc_camera",
            tag: "0.1.0",
            instances: [
                { instance_id: "camera_front" },
                { instance_id: "camera_front" }
            ]
        }"#;

        let err = serde_json5::from_str::<Deployment>(duplicate_instances)
            .expect_err("expected duplicate instance_id rejection");
        let ParsingError::DuplicateName(duplicate) = ParsingError::from(err) else {
            panic!("expected duplicate instance id error");
        };
        assert_eq!(duplicate, "camera_front");
    }

    #[test]
    fn deployment_name_validation() {
        let valid = r#"{
            name: "valid_name-1",
            tag: "0.1.0",
            instances: []
        }"#;
        assert!(serde_json5::from_str::<Deployment>(valid).is_ok());

        let invalid_char = r#"{
            name: "invalid!",
            tag: "0.1.0",
            instances: []
        }"#;
        let err = serde_json5::from_str::<Deployment>(invalid_char).expect_err("should fail");
        println!("Error: {:?}", err);
        let ParsingError::InvalidName(_, msg) = ParsingError::from(err) else {
            panic!("expected InvalidName error");
        };
        assert_eq!(msg, crate::consts::ALLOWED_CONFIG_CHARS);

        let empty = r#"{
            name: "",
            tag: "0.1.0",
            instances: []
        }"#;
        let err = serde_json5::from_str::<Deployment>(empty).expect_err("should fail");
        let ParsingError::EmptyName = ParsingError::from(err) else {
            panic!("expected EmptyName error");
        };
    }
}
