use super::types::PeppyLauncher;
use crate::{error::Result, parsing::read_non_empty_file};
use std::path::Path;

/// Parser responsible for extracting launcher documents.
///
/// Launcher files have no fixed name — any `.json5` file whose body
/// declares `peppy_schema: "launcher_v1"` is a launcher. Schema and
/// shape validation are handled by serde
/// (`#[serde(deny_unknown_fields)]` + the typed `PeppySchema` enum), so
/// callers that walk a repository can just attempt to parse and treat
/// failures as "not a launcher."
pub struct PeppyLauncherParser;

impl PeppyLauncherParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<PeppyLauncher> {
        let path = file.as_ref();
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
    use crate::launcher::{DeploymentSource, PeppySchema, VariantSource};
    use tempfile::tempdir;

    use super::PeppyLauncherParser;

    #[test]
    fn test_parse_peppy_config() {
        let json5 = r#"{
            peppy_schema: "launcher_v1",
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

    /// Launcher files have no fixed name — any path with valid launcher
    /// content parses regardless of basename.
    #[test]
    fn test_from_path_accepts_arbitrary_file_name() {
        let dir = tempdir().unwrap();
        let json5 = r#"{
            peppy_schema: "launcher_v1",
            deployments: []
        }"#;

        for name in ["openarm01_sim_teleop.json5", "demo.json5", "anything.json5"] {
            let path = dir.path().join(name);
            std::fs::write(&path, json5).unwrap();
            let cfg = PeppyLauncherParser::from_path(&path)
                .unwrap_or_else(|e| panic!("{name} should parse as launcher: {e}"));
            assert_eq!(cfg.peppy_schema, PeppySchema::LauncherV1);
            assert!(cfg.deployments.is_empty());
        }
    }

    /// A file declaring `peppy_schema: "node_v1"` is not a launcher.
    /// The strict deserializer either rejects unexpected node fields or
    /// the caller can gate on `peppy_schema` after parsing — either way
    /// node configs do not slip through `from_path`.
    #[test]
    fn test_from_path_rejects_node_schema() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("anything.json5");
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "n", tag: "0.1.0" },
            interfaces: {},
            execution: { language: "rust", build_cmd: ["true"], run_cmd: ["true"] }
        }"#;
        std::fs::write(&path, json5).unwrap();

        let err = PeppyLauncherParser::from_path(&path)
            .expect_err("node config must not parse as a launcher");
        // The unknown `manifest` key surfaces in the error message.
        assert!(
            err.to_string().contains("manifest"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_examples_peppy_launcher_parses() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("nodes_example_1")
            .join("peppy_launcher.json5");
        let cfg = PeppyLauncherParser::from_path(&path).expect("example launcher should parse");
        assert!(
            !cfg.deployments.is_empty(),
            "example launcher should contain deployments"
        );
    }

    #[test]
    fn test_parse_peppy_config_with_variants() {
        let valid_sha = "33e83da60a54e3bb487a9a3b67705918602143b30f158143b6909acaf017a36a";
        let json5 = r#"{
            peppy_schema: "launcher_v1",
            deployments: [
                {
                    source: {
                        repo: "https://github.com/Peppy-bot/nodes_hub.git",
                        path: "robot_brain",
                        ref: "main",
                        variant: { name: "mock-rust" }
                    },
                    instances: [{ instance_id: "the_brain" }]
                },
                {
                    source: {
                        url: "https://example.com/node.tar.zst",
                        sha256: "VALID_SHA",
                        variant: {
                            url: "https://example.com/variant.tar.zst"
                        }
                    },
                    instances: [{ instance_id: "node_1" }]
                },
                {
                    source: {
                        local: "./my_node",
                        variant: {
                            repo: "https://github.com/Peppy-bot/variants.git",
                            path: "mock_node",
                            ref: "v2"
                        }
                    },
                    instances: [{ instance_id: "node_2" }]
                },
                {
                    source: { local: "./no_variant_node" },
                    instances: [{ instance_id: "node_3" }]
                }
            ]
        }"#
        .replace("VALID_SHA", valid_sha);

        let cfg = PeppyLauncherParser::from_content(&json5).unwrap();
        assert_eq!(cfg.deployments.len(), 4);

        // Git source with name variant
        let d0 = &cfg.deployments[0];
        let DeploymentSource::Git(git) = &d0.source else {
            panic!("expected git source");
        };
        let Some(VariantSource::Name(v)) = &git.variant else {
            panic!("expected name variant");
        };
        assert_eq!(v.name, "mock-rust");

        // Url source with url variant (no sha256)
        let d1 = &cfg.deployments[1];
        let DeploymentSource::Url(url_src) = &d1.source else {
            panic!("expected url source");
        };
        let Some(VariantSource::Url(v)) = &url_src.variant else {
            panic!("expected url variant");
        };
        assert_eq!(v.url, "https://example.com/variant.tar.zst");
        assert_eq!(v.sha256, None);

        // Local source with git variant
        let d2 = &cfg.deployments[2];
        let DeploymentSource::Local(local) = &d2.source else {
            panic!("expected local source");
        };
        let Some(VariantSource::Git(v)) = &local.variant else {
            panic!("expected git variant");
        };
        assert_eq!(v.repo, "https://github.com/Peppy-bot/variants.git");
        assert_eq!(v.path.as_deref(), Some("mock_node"));
        assert_eq!(v.ref_.as_deref(), Some("v2"));

        // Source without variant
        let d3 = &cfg.deployments[3];
        assert!(d3.source.variant().is_none());
    }
}
