use super::types::PeppyLauncher;
use crate::{
    error::{ParsingError, Result},
    parsing::read_non_empty_file,
};
use std::path::Path;

/// Parser responsible for extracting `peppy_launcher.json5` documents
pub struct PeppyLauncherParser;

const PEPPY_LAUNCHER_FILE_NAME: &str = "peppy_launcher.json5";

impl PeppyLauncherParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<PeppyLauncher> {
        let path = file.as_ref();
        let file_name = path.file_name().and_then(|name| name.to_str());
        if file_name != Some(PEPPY_LAUNCHER_FILE_NAME) {
            let found = file_name
                .map(str::to_owned)
                .unwrap_or_else(|| path.display().to_string());
            return Err(ParsingError::InvalidFileName {
                expected: PEPPY_LAUNCHER_FILE_NAME.to_string(),
                found,
            }
            .into());
        }
        let content = read_non_empty_file(path)?;
        Self::from_content(&content)
    }

    /// Takes a JSON5 content as parameter
    pub fn from_content(content: &str) -> Result<PeppyLauncher> {
        // Strict schema validation is handled by serde via #[serde(deny_unknown_fields)]
        crate::error::deserialize_json5_with_path(content)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        error::{Error, ParsingError},
        launcher::DeploymentSource,
    };
    use tempfile::tempdir;

    use super::{PEPPY_LAUNCHER_FILE_NAME, PeppyLauncherParser};

    #[test]
    fn test_parse_peppy_config() {
        let json5 = r#"{
            deployments: [
                {
                    source: {
                        url: "https://example.com/fake_robot_brain.tar.zst",
                        sha256: "33e83da60a54e3bb487a9a3b67705918602143b30f158143b6909acaf017a36a"
                    },
                    instances: [
                        {
                            instance_id: "the_brain",
                            arguments: {}
                        }
                    ]
                },
                {
                    source: {
                        repo: "https://github.com/Peppy-bot/nodes_hub.git",
                        path: "fake_openarm01_controller",
                        ref: "0.1.0"
                    },
                    instances: [
                        {
                            instance_id: "the_nervous_system",
                            arguments: {}
                        }
                    ]
                },
                {
                    source: { local: "./esp32_board" },
                    instances: [
                        {
                            instance_id: "esp32_1",
                            env_vars: {
                                ESP32_DEVICE: "/dev/ttyUSB0"
                            }
                        }
                    ]
                }
            ]
        }"#;

        let cfg = PeppyLauncherParser::from_content(json5).unwrap();
        let deployments = cfg.deployments;
        assert_eq!(deployments.len(), 3);

        // Check first deployment
        let DeploymentSource::Url(url) = &deployments[0].source else {
            panic!("expected url source");
        };
        assert_eq!(url.url, "https://example.com/fake_robot_brain.tar.zst");
        assert_eq!(deployments[0].instances[0].instance_id, "the_brain");
        assert!(deployments[0].instances[0].arguments.is_empty());

        // Check second deployment
        let DeploymentSource::Git(git) = &deployments[1].source else {
            panic!("expected git source");
        };
        assert_eq!(git.ref_, "0.1.0");
        assert_eq!(
            deployments[1].instances[0].instance_id,
            "the_nervous_system"
        );
        assert!(deployments[1].instances[0].arguments.is_empty());

        // Check third deployment
        let DeploymentSource::Local(local) = &deployments[2].source else {
            panic!("expected local source");
        };
        assert_eq!(local.local, std::path::PathBuf::from("esp32_board"));
        assert_eq!(deployments[2].instances.len(), 1);
        assert_eq!(deployments[2].instances[0].instance_id, "esp32_1");
        assert!(deployments[2].instances[0].arguments.is_empty());
        assert_eq!(
            deployments[2].instances[0]
                .env_vars
                .get("ESP32_DEVICE")
                .map(String::as_str),
            Some("/dev/ttyUSB0")
        );
    }

    #[test]
    fn test_from_path_rejects_wrong_file_name() {
        let dir = tempdir().unwrap();
        let wrong_path = dir.path().join(crate::consts::NODE_CONFIG_FILE);
        std::fs::write(&wrong_path, "{}").unwrap();

        let err = PeppyLauncherParser::from_path(&wrong_path).unwrap_err();
        assert!(matches!(
            err,
            Error::Parsing(ParsingError::InvalidFileName { ref expected, ref found })
                if expected == PEPPY_LAUNCHER_FILE_NAME && found == crate::consts::NODE_CONFIG_FILE
        ));
    }

    #[test]
    fn test_from_path_accepts_correct_file_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(PEPPY_LAUNCHER_FILE_NAME);
        let json5 = r#"{
            deployments: []
        }"#;
        std::fs::write(&path, json5).unwrap();

        let cfg = PeppyLauncherParser::from_path(&path).unwrap();
        assert!(cfg.deployments.is_empty());
    }

    #[test]
    fn test_examples_peppy_launcher_parses() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("nodes_example_1")
            .join(PEPPY_LAUNCHER_FILE_NAME);
        let cfg = PeppyLauncherParser::from_path(&path).expect("example launcher should parse");
        assert!(
            !cfg.deployments.is_empty(),
            "example launcher should contain deployments"
        );
    }
}
