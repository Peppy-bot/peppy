use crate::{common::AnyType, error::ParsingError};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};
use std::{
    collections::{BTreeMap, HashSet},
    convert::TryFrom,
};

pub use crate::source::{
    DeploymentGitSource, DeploymentLocalSource, DeploymentRepoSource, DeploymentSource,
    DeploymentUrlSource, VariantGitSource, VariantNameSource, VariantSource, VariantUrlSource,
};

/// Schema identifier embedded in `peppy.json5` and `peppy_launcher.json5`
/// documents. The variant tells the daemon which document shape it is reading
/// so the strict deserializer can reject mixed-up files (e.g. a launcher that
/// claims to be a node config).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeppySchema {
    NodeV1,
    LauncherV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeppyLauncher {
    pub peppy_schema: PeppySchema,
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
    pub arguments: BTreeMap<String, AnyType>,
    #[serde(default)]
    pub env_vars: BTreeMap<String, String>,
    #[serde(default)]
    pub framework: FrameworkOverrides,
}

/// Per-instance framework knobs. Distinct from `arguments`: those are
/// declared by the node author and validated against a per-node parameter
/// schema; framework knobs are owned by peppylib, fixed-shape, and applied
/// uniformly to every node. Each field is optional so the daemon can fall
/// through to its own default when the instance omits the override.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameworkOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_sim_time: Option<bool>,
}

fn deserialize_instances<'de, D>(deserializer: D) -> Result<Vec<DeploymentInstance>, D::Error>
where
    D: Deserializer<'de>,
{
    let instances = Vec::<DeploymentInstance>::deserialize(deserializer)?;
    let mut seen = HashSet::with_capacity(instances.len());
    for instance in &instances {
        let id = instance.instance_id.to_string();
        if !seen.insert(id.clone()) {
            let err = crate::error::StructuredError::DuplicateName(id);
            return Err(de::Error::custom(err.json5_message()));
        }
    }
    Ok(instances)
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
        Name::try_from(s).map_err(|err| de::Error::custom(err.to_string()))
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

    /// Verifies that optional fields (`arguments`, `env_vars`, `framework`)
    /// default to empty when omitted, and that partially specified instances
    /// deserialize correctly.
    #[test]
    fn deployment_instance_defaults() {
        let instance: DeploymentInstance =
            serde_json5::from_str("{ instance_id: \"camera_front\" }").unwrap();
        assert_eq!(instance.instance_id, "camera_front");
        assert!(instance.arguments.is_empty());
        assert!(instance.env_vars.is_empty());
        assert_eq!(instance.framework.use_sim_time, None);

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

    /// Per-instance framework overrides parse cleanly and round-trip back
    /// to JSON5. Both the explicit-true and explicit-false cases must be
    /// distinguishable from "absent" so the daemon's precedence (per-instance
    /// > daemon CLI flag > default) has a value to gate on.
    #[test]
    fn deployment_instance_framework_overrides_round_trip() {
        let with_sim: DeploymentInstance = serde_json5::from_str(
            "{ instance_id: \"camera_front\", framework: { use_sim_time: true } }",
        )
        .unwrap();
        assert_eq!(with_sim.framework.use_sim_time, Some(true));

        let with_wall: DeploymentInstance = serde_json5::from_str(
            "{ instance_id: \"camera_front\", framework: { use_sim_time: false } }",
        )
        .unwrap();
        assert_eq!(with_wall.framework.use_sim_time, Some(false));

        let serialized = serde_json5::to_string(&with_sim).unwrap();
        let reparsed: DeploymentInstance = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed.framework.use_sim_time, Some(true));
    }

    /// The launcher rejects unknown framework keys so a typo (e.g.
    /// `use_simulation_time`) does not silently fall through to wall mode.
    #[test]
    fn deployment_instance_framework_rejects_unknown_keys() {
        let err = serde_json5::from_str::<DeploymentInstance>(
            "{ instance_id: \"camera_front\", framework: { unknown_knob: true } }",
        )
        .expect_err("unknown framework key should be rejected");
        assert!(err.to_string().contains("unknown_knob"));
    }
}
