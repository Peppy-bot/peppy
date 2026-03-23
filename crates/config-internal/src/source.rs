use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};
use std::path::PathBuf;

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
}
