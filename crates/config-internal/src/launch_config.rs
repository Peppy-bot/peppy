use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::config::{Deployment, Name};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceAttributes {
    pub name: Name,
    pub instance_id: Name,
    pub bound_master_node: Name,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfig {
    pub deployment: Deployment,
    pub attributes: InstanceAttributes,
}

impl InstanceAttributes {
    pub fn new(
        name: impl Into<String>,
        instance_id: impl Into<String>,
        bound_master_node: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            name: Name::new(name.into())?,
            instance_id: Name::new(instance_id.into())?,
            bound_master_node: Name::new(bound_master_node.into())?,
        })
    }
}

/// This function is typically invoked by the `peppy` program
/// to persist its launch configuration for `peppylib` or `peppygen` to pick it up.
pub fn save_json5_launch_config(
    deployment: Deployment,
    name: &str,
    instance_id: &str,
    bound_master_node: &str,
    to_path: impl AsRef<Path>,
) -> Result<PathBuf> {
    let path = to_path.as_ref();
    let attributes = InstanceAttributes::new(name, instance_id, bound_master_node)?;
    let config = LaunchConfig {
        deployment,
        attributes,
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let serialized = serde_json5::to_string(&config)
        .map_err(|err| crate::error::Error::Serialize(err.to_string()))?;
    fs::write(path, serialized)?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, ParsingError};
    use tempfile::TempDir;

    fn sample_deployment() -> Deployment {
        let json = r#"{
            name: "camera",
            tag: "0.1.0",
            instances: [
                {
                  instance_id: "camera_front"
                }
            ]
        }"#;

        serde_json5::from_str(json).expect("sample deployment should deserialize")
    }

    fn sample_attributes() -> InstanceAttributes {
        InstanceAttributes::new("camera", "camera_front", "master_node")
            .expect("sample attributes should build")
    }

    #[test]
    fn writes_launch_config_and_creates_parent_dir() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("peppy_launcher.json5");
        let deployment = sample_deployment();
        let attributes = sample_attributes();

        let returned = save_json5_launch_config(
            deployment.clone(),
            attributes.name.as_str(),
            attributes.instance_id.as_str(),
            attributes.bound_master_node.as_str(),
            &path,
        )
        .unwrap();

        let written = fs::read_to_string(&path).expect("launch config should be written to disk");
        let parsed: LaunchConfig =
            serde_json5::from_str(&written).expect("launch config should parse");

        assert_eq!(returned, path);
        assert_eq!(parsed.attributes, attributes);
        assert_eq!(parsed.deployment.name, deployment.name);
        assert_eq!(parsed.deployment.instances.len(), 1);
        assert_eq!(
            parsed.deployment.instances[0].instance_id,
            deployment.instances[0].instance_id
        );
    }

    #[test]
    fn rejects_invalid_instance_id() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("peppy_launcher.json5");
        let deployment = sample_deployment();

        let err = save_json5_launch_config(deployment, "camera", "bad id!", "master_node", &path)
            .unwrap_err();
        assert!(matches!(
            err,
            Error::Parsing(ParsingError::InvalidName(_, _))
        ));
    }
}
