use crate::internal::repository::PinnedItem;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};
use std::fmt;

/// What a deployment runs.
///
/// A node reference is resolved through the user's repo cache
/// (`~/.peppy/cache/nodes.json5`); it is written as `{ name, tag }` or the
/// combined `{ name: "<name>:<tag>" }` shorthand. An exposure list names the
/// `mcp_exposure/v1` documents the built-in MCP server serves, each as
/// `"<name>:<tag>"`, resolved through the exposures cache. Nodes and
/// exposures that live outside a configured repository are made resolvable
/// by registering their directory in `repositories.json5`
/// (`peppy repo add <path>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum DeploymentSource {
    Node { name: String, tag: String },
    Exposures { exposures: Vec<ExposureRef> },
}

impl DeploymentSource {
    /// How refusals and progress lines name the deployment: `camera:v1`
    /// for a node, `exposures [a:v1, b:v1]` for the built-in MCP server.
    pub fn label(&self) -> String {
        match self {
            Self::Node { name, tag } => format!("{name}:{tag}"),
            Self::Exposures { exposures } => format!(
                "exposures [{}]",
                exposures
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// One `<name>:<tag>` exposure reference of an `exposures` deployment,
/// held to the same identity rules as a node reference.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExposureRef {
    pub name: String,
    pub tag: String,
}

impl ExposureRef {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let Some((name, tag)) = raw.trim().split_once(':') else {
            return Err(format!(
                "exposure reference `{raw}` must be written as `<name>:<tag>`"
            ));
        };
        if tag.contains(':') {
            return Err(format!(
                "exposure reference `{raw}` must contain at most one ':' separating name and tag"
            ));
        }
        let (name, tag) = (name.trim(), tag.trim());
        config::repo_node_id::validate_repo_node_name(name, "exposure name")?;
        config::repo_node_id::validate_repo_node_tag(tag, "exposure tag")?;
        Ok(Self {
            name: name.to_owned(),
            tag: tag.to_owned(),
        })
    }
}

impl fmt::Display for ExposureRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.name, self.tag)
    }
}

/// The identity a pin names, as a launcher would list it. A pin's name and
/// tag already passed the identity rules, so no re-validation is needed.
impl From<&PinnedItem> for ExposureRef {
    fn from(pin: &PinnedItem) -> Self {
        Self {
            name: pin.name.as_str().to_owned(),
            tag: pin.tag.as_str().to_owned(),
        }
    }
}

impl Serialize for ExposureRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ExposureRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(de::Error::custom)
    }
}

fn invalid_deployment_source<E>(detail: impl Into<String>) -> E
where
    E: de::Error,
{
    let err = crate::error::StructuredError::InvalidDeploymentSource(detail.into());
    de::Error::custom(err.json5_message())
}

fn split_name_and_tag<E>(raw_name: String, raw_tag: Option<&str>) -> Result<(String, String), E>
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

/// The exposure list of an `exposures` deployment: non-empty, every entry a
/// well-formed reference, no reference listed twice.
fn exposure_list<E>(raw: Vec<String>) -> Result<Vec<ExposureRef>, E>
where
    E: de::Error,
{
    if raw.is_empty() {
        return Err(invalid_deployment_source::<E>(
            "deployment source `exposures` must list at least one exposure",
        ));
    }
    let mut exposures: Vec<ExposureRef> = Vec::with_capacity(raw.len());
    for entry in raw {
        let reference = ExposureRef::parse(&entry).map_err(invalid_deployment_source::<E>)?;
        if exposures.contains(&reference) {
            return Err(invalid_deployment_source::<E>(format!(
                "deployment source lists exposure `{reference}` twice"
            )));
        }
        exposures.push(reference);
    }
    Ok(exposures)
}

impl<'de> Deserialize<'de> for DeploymentSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // The retired source keys are still RECOGNIZED, not merely unknown.
        // A launcher written against the old forms is the most likely thing
        // to arrive here, and `unknown field `local`` says only that the key
        // is wrong, not that the form was removed, nor what replaces it. The
        // migration flow is stated here, where the author hits the wall,
        // rather than only in the docs they would have to go find.
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawDeploymentSource {
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            tag: Option<String>,
            #[serde(default)]
            exposures: Option<Vec<String>>,
            #[serde(default)]
            local: Option<serde::de::IgnoredAny>,
            #[serde(default)]
            git: Option<serde::de::IgnoredAny>,
            #[serde(default)]
            url: Option<serde::de::IgnoredAny>,
            #[serde(default)]
            repo: Option<serde::de::IgnoredAny>,
        }

        let raw = RawDeploymentSource::deserialize(deserializer)?;
        if let Some(retired) = [
            ("local", raw.local.is_some()),
            ("git", raw.git.is_some()),
            ("url", raw.url.is_some()),
            ("repo", raw.repo.is_some()),
        ]
        .into_iter()
        .find_map(|(key, present)| present.then_some(key))
        {
            return Err(invalid_deployment_source::<D::Error>(format!(
                "deployment source `{retired}` was removed: a launcher references every node \
                 through the repository index. Register the node's location with \
                 `peppy repo add <path-or-url>`, write its index with `peppy repo index <path>`, \
                 run `peppy repo refresh`, then name it as `{{ name: \"<name>:<tag>\" }}`."
            )));
        }

        if let Some(exposures) = raw.exposures {
            if raw.name.is_some() || raw.tag.is_some() {
                return Err(invalid_deployment_source::<D::Error>(
                    "deployment source names either a node (`name`, `tag`) or `exposures`, not both",
                ));
            }
            return Ok(DeploymentSource::Exposures {
                exposures: exposure_list::<D::Error>(exposures)?,
            });
        }

        let name_raw = raw.name.ok_or_else(|| {
            invalid_deployment_source::<D::Error>(
                "deployment source requires `name` (`{ name, tag }` or `{ name: \"<name>:<tag>\" }`) \
                 or `exposures` (`{ exposures: [\"<name>:<tag>\", ...] }`)",
            )
        })?;
        let (name, tag) = split_name_and_tag::<D::Error>(name_raw, raw.tag.as_deref())?;
        Ok(DeploymentSource::Node { name, tag })
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
        assert_eq!(
            src,
            DeploymentSource::Node {
                name: "robot_brain".to_owned(),
                tag: "v1".to_owned()
            }
        );
    }

    #[test]
    fn deployment_source_parses_combined_name_tag() {
        let src: DeploymentSource = serde_json5::from_str("{ name: \"robot_brain:v1\" }").unwrap();
        assert_eq!(
            src,
            DeploymentSource::Node {
                name: "robot_brain".to_owned(),
                tag: "v1".to_owned()
            }
        );
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
    /// `{ name, tag }`. Each retired key is refused by name and the refusal
    /// carries the migration flow, so an author porting an old launcher is
    /// told what replaces it rather than just that the key is wrong.
    #[test]
    fn deployment_source_rejects_retired_source_keys() {
        for (key, body) in [
            ("local", "{ local: \"./uvc_camera\" }"),
            ("git", "{ git: { url: \"https://example.com/hub.git\" } }"),
            ("url", "{ url: \"https://example.com/node.tar.zst\" }"),
            ("repo", "{ repo: \"uvc_camera:v1\" }"),
        ] {
            let err: serde_json5::Error = serde_json5::from_str::<DeploymentSource>(body)
                .expect_err("a retired source key must be rejected");
            let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
                panic!("expected InvalidDeploymentSource for `{key}`");
            };
            assert!(msg.contains(key), "error should name `{key}`: {msg}");
            assert!(
                msg.contains("peppy repo add") && msg.contains("peppy repo refresh"),
                "error should state the migration flow: {msg}"
            );
        }
    }

    #[test]
    fn deployment_source_serializes_name_and_tag() {
        let src = DeploymentSource::Node {
            name: "robot_brain".to_owned(),
            tag: "v1".to_owned(),
        };
        let json = serde_json::to_value(&src).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "name": "robot_brain", "tag": "v1" })
        );
    }

    fn exposure(name: &str, tag: &str) -> ExposureRef {
        ExposureRef {
            name: name.to_owned(),
            tag: tag.to_owned(),
        }
    }

    #[test]
    fn deployment_source_parses_an_exposure_list_in_declaration_order() {
        let src: DeploymentSource = serde_json5::from_str(
            "{ exposures: [\"camera_and_recording:v1\", \" arm_control:v2 \"] }",
        )
        .unwrap();
        assert_eq!(
            src,
            DeploymentSource::Exposures {
                exposures: vec![
                    exposure("camera_and_recording", "v1"),
                    exposure("arm_control", "v2")
                ]
            }
        );
        assert_eq!(
            src.label(),
            "exposures [camera_and_recording:v1, arm_control:v2]"
        );
    }

    #[test]
    fn deployment_source_exposures_serialize_as_references() {
        let src = DeploymentSource::Exposures {
            exposures: vec![exposure("camera_and_recording", "v1")],
        };
        let json = serde_json::to_value(&src).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "exposures": ["camera_and_recording:v1"] })
        );
        let back: DeploymentSource = serde_json::from_value(json).unwrap();
        assert_eq!(back, src);
    }

    fn exposures_error(body: &str) -> String {
        let err: serde_json5::Error = serde_json5::from_str::<DeploymentSource>(body)
            .expect_err("the exposure list must be refused");
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected InvalidDeploymentSource");
        };
        msg
    }

    #[test]
    fn deployment_source_refuses_an_empty_exposure_list() {
        let msg = exposures_error("{ exposures: [] }");
        assert!(msg.contains("at least one exposure"), "unexpected: {msg}");
    }

    #[test]
    fn deployment_source_refuses_an_exposure_listed_twice() {
        let msg = exposures_error("{ exposures: [\"a:v1\", \"b:v1\", \"a:v1\"] }");
        assert!(
            msg.contains("lists exposure `a:v1` twice"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn deployment_source_refuses_a_malformed_exposure_reference() {
        let msg = exposures_error("{ exposures: [\"camera_and_recording\"] }");
        assert!(msg.contains("`<name>:<tag>`"), "unexpected: {msg}");
        let msg = exposures_error("{ exposures: [\"camera:1v\"] }");
        assert!(msg.contains("exposure tag"), "unexpected: {msg}");
        let msg = exposures_error("{ exposures: [\"cam era:v1\"] }");
        assert!(msg.contains("exposure name"), "unexpected: {msg}");
    }

    #[test]
    fn deployment_source_refuses_exposures_beside_a_node_reference() {
        let msg = exposures_error("{ name: \"camera:v1\", exposures: [\"a:v1\"] }");
        assert!(msg.contains("not both"), "unexpected: {msg}");
    }

    #[test]
    fn exposure_references_sort_by_name_then_tag() {
        let mut refs = vec![
            exposure("camera", "v2"),
            exposure("arm", "v1"),
            exposure("camera", "v1"),
        ];
        refs.sort();
        assert_eq!(
            refs,
            vec![
                exposure("arm", "v1"),
                exposure("camera", "v1"),
                exposure("camera", "v2")
            ]
        );
    }
}
