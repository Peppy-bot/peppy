use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};

/// Deployment source: a node reference resolved through the user's repo
/// cache (`~/.peppy/cache/nodes.json5`). Accepts `{ name, tag }` or the
/// combined `{ name: "<name>:<tag>" }` shorthand. Nodes that live outside
/// a configured repository are made resolvable by registering their
/// directory in `repositories.json5` (`peppy repo add <path>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentSource {
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
            "deployment source name cannot be empty",
        ));
    }

    let (name, tag) = if let Some((n, t)) = name_trimmed.split_once(':') {
        if raw_tag.is_some() {
            return Err(invalid_deployment_source::<E>(
                "deployment source cannot combine `name: \"<name>:<tag>\"` with a separate `tag` field",
            ));
        }
        if t.contains(':') {
            return Err(invalid_deployment_source::<E>(
                "deployment source `name` must contain at most one ':' separating name and tag",
            ));
        }
        (n.trim().to_owned(), t.trim().to_owned())
    } else {
        let tag = raw_tag.map(str::trim).unwrap_or("");
        if tag.is_empty() {
            return Err(invalid_deployment_source::<E>(
                "deployment source requires a non-empty `tag` (or the combined `name: \"<name>:<tag>\"` form)",
            ));
        }
        (name_trimmed.to_owned(), tag.to_owned())
    };

    config::repo_node_id::validate_repo_node_name(&name, "deployment source name")
        .map_err(invalid_deployment_source::<E>)?;
    config::repo_node_id::validate_repo_node_tag(&tag, "deployment source tag")
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
            name: Option<String>,
            #[serde(default)]
            tag: Option<String>,
        }

        let raw = RawDeploymentSource::deserialize(deserializer)?;
        let name_raw = raw.name.ok_or_else(|| {
            invalid_deployment_source::<D::Error>(
                "deployment source requires `name` (`{ name, tag }` or `{ name: \"<name>:<tag>\" }`)",
            )
        })?;
        let (name, tag) = split_repo_name_and_tag::<D::Error>(name_raw, raw.tag.as_deref())?;
        Ok(DeploymentSource { name, tag })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ParsingError;

    #[test]
    fn deployment_source_parses_name_and_tag_fields() {
        let src: DeploymentSource =
            serde_json5::from_str("{ name: \"robot_brain\", tag: \"v1\" }").unwrap();
        assert_eq!(src.name, "robot_brain");
        assert_eq!(src.tag, "v1");
    }

    #[test]
    fn deployment_source_parses_combined_name_tag() {
        let src: DeploymentSource = serde_json5::from_str("{ name: \"robot_brain:v1\" }").unwrap();
        assert_eq!(src.name, "robot_brain");
        assert_eq!(src.tag, "v1");
    }

    #[test]
    fn deployment_source_rejects_name_without_tag() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"foo\" }").unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(msg.contains("non-empty `tag`"), "unexpected: {msg}");
    }

    #[test]
    fn deployment_source_rejects_empty_name() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"\", tag: \"v1\" }").unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert_eq!(msg, "deployment source name cannot be empty");
    }

    #[test]
    fn deployment_source_rejects_missing_name() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ tag: \"v1\" }").unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(msg.contains("requires `name`"), "unexpected: {msg}");
    }

    #[test]
    fn deployment_source_rejects_combined_with_separate_tag() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"foo:v1\", tag: \"v1\" }")
                .unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(msg.contains("cannot combine"), "unexpected: {msg}");
    }

    #[test]
    fn deployment_source_rejects_multiple_colons() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"foo:v1:extra\" }").unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(msg.contains("at most one ':'"), "unexpected: {msg}");
    }

    #[test]
    fn deployment_source_rejects_dot_in_tag() {
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
    fn deployment_source_rejects_tag_not_starting_with_letter() {
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
    fn deployment_source_rejects_invalid_name_char() {
        let err: serde_json5::Error =
            serde_json5::from_str::<DeploymentSource>("{ name: \"foo/bar\", tag: \"v1\" }")
                .unwrap_err();
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        assert!(
            msg.contains("invalid deployment source name"),
            "unexpected: {msg}"
        );
    }

    /// A path is not a node reference: the only source shape is
    /// `{ name, tag }`, so a `local` key fails the strict field check.
    #[test]
    fn deployment_source_rejects_path_key() {
        let err = serde_json5::from_str::<DeploymentSource>("{ local: \"./uvc_camera\" }")
            .expect_err("path-shaped source must be rejected");
        assert!(
            err.to_string().contains("local"),
            "error should name the offending key: {err}"
        );
    }

    #[test]
    fn deployment_source_serializes_name_and_tag() {
        let src = DeploymentSource {
            name: "robot_brain".to_owned(),
            tag: "v1".to_owned(),
        };
        let json = serde_json::to_value(&src).unwrap();
        assert_eq!(json, serde_json::json!({ "name": "robot_brain", "tag": "v1" }));
    }
}
