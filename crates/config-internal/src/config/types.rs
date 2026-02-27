use crate::{common::NodeArguments, error::ParsingError};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};
use std::{
    collections::{BTreeMap, HashSet},
    convert::TryFrom,
    path::PathBuf,
};

/// Version identifier embedded in node `peppy.json5` manifests.
/// Using a simple alias keeps serialization straightforward while making the intent explicit.
pub type SchemaVersion = u16;
pub const CURRENT_SCHEMA_VERSION: SchemaVersion = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeppyLauncher {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<Deployment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub source: DeploymentSource,
    #[serde(deserialize_with = "deserialize_instances")]
    pub instances: Vec<DeploymentInstance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentInstance {
    pub instance_id: Name,
    #[serde(default)]
    pub arguments: NodeArguments,
    #[serde(default)]
    pub env_vars: BTreeMap<String, String>,
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
            let err = crate::error::StructuredError::DuplicateName(id.to_owned());
            let msg =
                serde_json5::to_string(&err).unwrap_or_else(|_| "serialization error".to_string());
            return Err(de::Error::custom(msg));
        }
    }
    Ok(instances)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum DeploymentSource {
    Local(DeploymentLocalSource),
    Git(DeploymentGitSource),
    Url(DeploymentUrlSource),
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

fn invalid_deployment_source<E>(detail: impl Into<String>) -> E
where
    E: de::Error,
{
    let err = crate::error::StructuredError::InvalidDeploymentSource(detail.into());
    let msg = serde_json5::to_string(&err).unwrap_or_else(|_| "serialization error".to_string());
    de::Error::custom(msg)
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
    let trimmed = value.trim().trim_start_matches('/');
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
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(invalid_deployment_source::<E>(
            "url must start with http:// or https://",
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
        }

        let raw = RawDeploymentSource::deserialize(deserializer)?;
        let has_local = raw.local.is_some();
        let has_git = raw.repo.is_some() || raw.path.is_some() || raw.ref_.is_some();
        let has_url = raw.url.is_some() || raw.sha256.is_some();

        match (has_local, has_git, has_url) {
            (true, false, false) => {
                let local = trim_non_empty::<D::Error>(
                    raw.local.expect("local is present"),
                    "local path cannot be empty",
                )?;
                Ok(DeploymentSource::Local(DeploymentLocalSource {
                    local: PathBuf::from(local),
                }))
            }
            (false, true, false) => {
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
            (false, false, true) => {
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
            _ => Err(invalid_deployment_source::<D::Error>(
                "source must be one of: { local }, { repo, path, ref }, { url, sha256 }",
            )),
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
    fn deployment_source_parses_all_variants() {
        let local: DeploymentSource = serde_json5::from_str("{ local: \"./uvc_camera\" }").unwrap();
        let DeploymentSource::Local(local) = local else {
            panic!("expected local source");
        };
        assert_eq!(local.local, PathBuf::from("./uvc_camera"));

        let git: DeploymentSource = serde_json5::from_str(
            "{ repo: \"https://github.com/Peppy-bot/example_nodes.git\", path: \"fake_openarm01_controller\", ref: \"0.1.0\" }",
        )
        .unwrap();
        let DeploymentSource::Git(git) = git else {
            panic!("expected git source");
        };
        assert_eq!(git.repo, "https://github.com/Peppy-bot/example_nodes.git");
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
    fn duplicate_instance_ids_are_rejected() {
        let duplicate_instances = r#"{
            source: { local: "./uvc_camera" },
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
    fn deployment_instance_defaults() {
        let instance: DeploymentInstance =
            serde_json5::from_str("{ instance_id: \"camera_front\" }").unwrap();
        assert_eq!(instance.instance_id, "camera_front");
        assert!(instance.arguments.is_empty());
        assert!(instance.env_vars.is_empty());

        let with_env: DeploymentInstance = serde_json5::from_str(
            "{ instance_id: \"esp32_1\", env_vars: { ESP32_DEVICE: \"/dev/ttyUSB0\" } }",
        )
        .unwrap();
        assert_eq!(with_env.instance_id, "esp32_1");
        assert_eq!(
            with_env.env_vars.get("ESP32_DEVICE").map(String::as_str),
            Some("/dev/ttyUSB0")
        );
    }
}
