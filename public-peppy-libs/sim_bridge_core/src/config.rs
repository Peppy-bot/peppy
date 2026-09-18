use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use crate::types::error::{BridgeError, Result};

const CONFIG_PATH: &str = "config/sim_bridge.json5";
const CONFIG_PRESETS_DIR: &str = "config/presets";
const ENV_PRESET: &str = "PEPPY_BRIDGE_PRESET";
const ENV_SIM_NODE: &str = "SIM_NODE";

#[derive(Debug, Deserialize)]
pub struct PublisherConfig {
    #[serde(rename = "type")]
    pub type_name: String,
    pub topic: String,
    #[serde(default)]
    pub prim: Option<String>,
    #[serde(default)]
    pub robot_name: Option<String>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct SubscriberConfig {
    #[serde(rename = "type")]
    pub type_name: String,
    pub topic: String,
    #[serde(default)]
    pub prim: Option<String>,
    #[serde(default)]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub source_node: Option<String>,
    #[serde(default)]
    pub joint_names: Option<Vec<String>>,
    #[serde(default)]
    pub joint_start: Option<usize>,
    #[serde(default)]
    pub joint_count: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct RobotConfig {
    #[serde(default)]
    pub joint_names: Option<Vec<String>>,
    #[serde(default)]
    pub num_joints: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct BridgeConfig {
    pub sim_node: String,
    #[serde(default)]
    pub robot: Option<RobotConfig>,
    pub publishers: Vec<PublisherConfig>,
    pub subscribers: Vec<SubscriberConfig>,
}

#[derive(Debug, Clone)]
pub struct DaemonState {
    pub core_node_name: String,
    pub messaging_port: u16,
}

pub fn read_bridge_config() -> Result<BridgeConfig> {
    let path = resolve_config_path()?;
    let raw = fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BridgeError::ConfigNotFound {
                path: path.clone(),
                source: e,
            }
        } else {
            BridgeError::Io(e)
        }
    })?;
    let stripped = strip_json5_comments(&raw);
    serde_json::from_str(&stripped).map_err(|e| BridgeError::ConfigParse(e.to_string()))
}

pub fn sim_node_name(config: &BridgeConfig) -> Arc<str> {
    if let Ok(val) = std::env::var(ENV_SIM_NODE) {
        return Arc::from(val.as_str());
    }
    Arc::from(config.sim_node.as_str())
}

pub fn resolve_joint_indices(
    sub: &SubscriberConfig,
    robot_joint_names: Option<&[String]>,
) -> Result<Vec<usize>> {
    if let Some(names) = &sub.joint_names {
        let robot_names = robot_joint_names.ok_or_else(|| {
            BridgeError::JointResolution(
                "subscriber uses joint_names but robot.joint_names is not set in config".into(),
            )
        })?;
        names
            .iter()
            .map(|name| {
                robot_names.iter().position(|n| n == name).ok_or_else(|| {
                    BridgeError::JointResolution(format!(
                        "joint '{name}' not found in robot.joint_names"
                    ))
                })
            })
            .collect()
    } else if sub.joint_start.is_none() && sub.joint_count.is_none() {
        Err(BridgeError::JointResolution(
            "subscriber has no joint_names, joint_start, or joint_count in config".into(),
        ))
    } else {
        let start = sub.joint_start.unwrap_or(0);
        let count = sub.joint_count.unwrap_or(0);
        Ok((start..start + count).collect())
    }
}

fn resolve_config_path() -> Result<PathBuf> {
    if let Ok(preset) = std::env::var(ENV_PRESET) {
        validate_preset_name(&preset)?;
        let path = PathBuf::from(CONFIG_PRESETS_DIR).join(format!("{preset}.json5"));
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => {
                return Err(BridgeError::ConfigParse(format!(
                    "preset path '{}' exists but is not a regular file",
                    path.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BridgeError::ConfigNotFound { path, source: e });
            }
            Err(e) => return Err(BridgeError::Io(e)),
        }
        return Ok(path);
    }
    Ok(PathBuf::from(CONFIG_PATH))
}

fn validate_preset_name(preset: &str) -> Result<()> {
    let valid = preset
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if preset.is_empty() || !valid {
        return Err(BridgeError::InvalidPreset(format!(
            "{preset}: must match [A-Za-z0-9_-]+ (no path separators or dots allowed)"
        )));
    }
    Ok(())
}

fn strip_json5_comments(src: &str) -> String {
    src.lines()
        .map(|l| {
            if l.trim_start().starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_full_line_comments() {
        let input = "{\n  // comment\n  \"key\": \"val://not_a_comment\"\n}";
        let out = strip_json5_comments(input);
        assert!(!out.contains("// comment"));
        assert!(out.contains("val://not_a_comment"));
    }

    #[test]
    fn validate_preset_name_rejects_path_traversal() {
        assert!(validate_preset_name("../etc/passwd").is_err());
        assert!(validate_preset_name("valid_preset-1").is_ok());
        assert!(validate_preset_name("").is_err());
    }

    #[test]
    fn resolve_joint_indices_by_name() {
        let robot_names = vec!["j0".to_string(), "j1".to_string(), "j2".to_string()];
        let sub = SubscriberConfig {
            type_name: "joint_command".into(),
            topic: "t".into(),
            prim: None,
            instance_id: None,
            source_node: None,
            joint_names: Some(vec!["j2".to_string(), "j0".to_string()]),
            joint_start: None,
            joint_count: None,
        };
        let indices = resolve_joint_indices(&sub, Some(&robot_names)).unwrap();
        assert_eq!(indices, vec![2, 0]);
    }
}
