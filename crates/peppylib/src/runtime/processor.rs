use std::path::Path;

use crate::error::{Error, MissingStandaloneParameters, ParameterDeserializationError, Result};
use config::{
    NodeArguments,
    consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME},
    node::NodeConfig,
    peppy_config::Name,
    runtime::{NodeInstance, RuntimeConfig},
};

use super::builder::StandaloneConfig;

/// Runtime processor that holds configuration for the node.
#[derive(Clone)]
pub struct Processor {
    runtime_config: RuntimeConfig,
}

impl Processor {
    /// Create processor for daemon mode.
    ///
    /// Reads configuration from PEPPY_RUNTIME_CONFIG env var.
    /// Validates fingerprint matches compiled code.
    pub fn new_daemon(peppy_config: impl AsRef<Path>) -> Result<Self> {
        let launch_config_path = std::env::var(RUNTIME_CONFIG_VAR_NAME).map_err(|source| {
            Error::MissingInstanceIdEnvVar {
                var: RUNTIME_CONFIG_VAR_NAME,
                source,
            }
        })?;

        let runtime_config = Self::load_runtime_config(&launch_config_path)?;

        let codegen_fingerprint = config::fingerprint::read_codegen_fingerprint(
            peppy_config.as_ref(),
            PEPPYGEN_OUTPUT_PATH,
        )
        .map_err(|source| Error::CodegenFingerprintRead {
            path: peppy_config.as_ref().display().to_string(),
            source: std::io::Error::other(source.to_string()),
        })?;
        Self::validate_fingerprint(peppy_config.as_ref(), &codegen_fingerprint)?;

        let node_config: NodeConfig =
            serde_json5::from_str(&std::fs::read_to_string(peppy_config.as_ref())?)?;
        Self::validate_parameter_types(
            &runtime_config.node_instance.arguments,
            &node_config.parameters,
        )?;

        Ok(Self { runtime_config })
    }

    /// Create processor for standalone mode.
    ///
    /// Uses provided configuration or defaults:
    /// - messaging_host: DEFAULT_ZENOH_HOST or user-provided
    /// - messaging_port: DEFAULT_ZENOH_PORT or user-provided
    /// - instance_id: "standalone" or user-provided
    /// - node_name: from peppy.json5 or user-provided
    ///
    /// Skips fingerprint validation for development flexibility.
    pub fn new_standalone(
        peppy_config: impl AsRef<Path>,
        config: &StandaloneConfig,
    ) -> Result<Self> {
        let node_config: NodeConfig =
            serde_json5::from_str(&std::fs::read_to_string(peppy_config.as_ref())?)?;

        let arguments = match &config.parameters {
            Some(params) => serde_json::from_value(params.clone()).map_err(|e| {
                ParameterDeserializationError::single(format!("failed to parse parameters: {}", e))
            })?,
            None => NodeArguments::new(),
        };

        Self::validate_required_parameters(&arguments, &node_config.parameters)?;

        let node_name: String = config
            .node_name
            .clone()
            .unwrap_or_else(|| node_config.manifest.name.clone().into());

        let instance_id = config
            .instance_id
            .clone()
            .unwrap_or_else(|| "standalone".to_string());

        let messaging_host = config.messaging_host_or_default();
        let messaging_port = config.messaging_port_or_default();

        let instance_id_name =
            Name::new(instance_id.clone()).map_err(|e| Error::InvalidNodeName {
                node_name: instance_id,
                reason: e.to_string(),
            })?;

        let runtime_config = RuntimeConfig::new(
            &messaging_host,
            messaging_port,
            NodeInstance {
                instance_id: instance_id_name,
                arguments,
            },
            &node_name,
            "standalone-daemon",
        )?;

        Ok(Self { runtime_config })
    }

    fn load_runtime_config(path: &str) -> Result<RuntimeConfig> {
        let content = std::fs::read_to_string(path).map_err(|source| Error::LaunchConfigRead {
            path: path.to_string(),
            source,
        })?;
        serde_json5::from_str(&content).map_err(|source| Error::LaunchConfigParse {
            path: path.to_string(),
            source,
        })
    }

    fn validate_fingerprint(peppy_config: &Path, expected: &str) -> Result<()> {
        let actual = RuntimeConfig::generate_peppy_config_fingerprint(peppy_config)?;
        if actual == expected {
            Ok(())
        } else {
            Err(Error::PeppyConfigFingerprintMismatch {
                path: peppy_config.display().to_string(),
                expected: expected.to_string(),
                actual,
            })
        }
    }

    fn validate_parameter_types(
        runtime_args: &NodeArguments,
        compiled_params: &NodeArguments,
    ) -> Result<()> {
        for (key, runtime_value) in runtime_args {
            let compiled_type = compiled_params
                .get(key)
                .ok_or_else(|| Error::MissingCompiledParameter { path: key.clone() })?;
            runtime_value.matches_type_spec(compiled_type, key)?;
        }
        Ok(())
    }

    /// Validate that all required parameters defined in peppy.json5 are
    /// provided when running in standalone mode. This catches missing
    /// parameters early — before the Zenoh connection attempt — so the
    /// developer sees a clear error instead of a hanging process.
    fn validate_required_parameters(
        runtime_args: &NodeArguments,
        compiled_params: &NodeArguments,
    ) -> Result<()> {
        let missing: Vec<String> = compiled_params
            .keys()
            .filter(|key| !runtime_args.contains_key(key.as_str()))
            .cloned()
            .collect();

        if !missing.is_empty() {
            return Err(MissingStandaloneParameters {
                parameters: missing,
            }
            .into());
        }

        Ok(())
    }

    pub fn bound_instance_id(&self) -> &str {
        self.runtime_config.node_instance.instance_id.as_str()
    }

    pub fn bound_daemon_node(&self) -> &str {
        self.runtime_config.bound_daemon_node.as_str()
    }

    pub fn input_arguments(&self) -> &NodeArguments {
        &self.runtime_config.node_instance.arguments
    }

    pub fn node_name(&self) -> &str {
        self.runtime_config.node_name.as_str()
    }

    pub fn messaging_host(&self) -> &str {
        &self.runtime_config.messaging_host
    }

    pub fn messaging_port(&self) -> u16 {
        self.runtime_config.messaging_port
    }
}

#[cfg(test)]
mod tests {
    use super::{PEPPYGEN_OUTPUT_PATH, Processor, RUNTIME_CONFIG_VAR_NAME};
    use crate::runtime::builder::StandaloneConfig;
    use config::{AnyType, NodeArguments, runtime::RuntimeConfig};
    use std::{collections::BTreeMap, env, path::Path, sync::Mutex};
    use tempfile::TempDir;

    static ENV_VAR_MUTEX: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let _lock = ENV_VAR_MUTEX.lock().expect("env mutex should lock");
            let previous = env::var(key).ok();
            // SAFETY: environment mutation is guarded by a global mutex to avoid races.
            unsafe { env::set_var(key, value) };
            Self {
                key,
                previous,
                _lock,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(ref value) = self.previous {
                // SAFETY: environment mutation is guarded by a global mutex to avoid races.
                unsafe { env::set_var(self.key, value) };
            } else {
                // SAFETY: environment mutation is guarded by a global mutex to avoid races.
                unsafe { env::remove_var(self.key) };
            }
        }
    }

    #[test]
    fn loads_runtime_config_from_env() {
        let bound_daemon_node = "epic-whale-6789";
        let bound_node_name = "uvc_camera";
        let bound_instance_id = "camera_front";

        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Create a peppy config file with type specifications matching runtime parameters
        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                language: "rust",
            },
            build: {
                start_cmd: ["./target/debug/uvc_camera"]
            },
            parameters: {
                exposure: "f32",
                flags: {
                    $type: "array",
                    $items: "string"
                },
                nested: {
                    $type: "object",
                    enabled: "bool",
                    gain: "i64"
                },
                mode: "string"
            }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "$INSTANCE_ID",
                arguments: {
                    exposure: 0.25,
                    flags: ["hdr", "stabilized"],
                    nested: { enabled: true, gain: 10 },
                    mode: "auto"
                }
            },
            node_name: "$NODE_NAME",
            bound_daemon_node: "$DAEMON_NODE"
        }"#;

        let populated_config = json5_config
            .replace("$INSTANCE_ID", bound_instance_id)
            .replace("$NODE_NAME", bound_node_name)
            .replace("$DAEMON_NODE", bound_daemon_node);

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&populated_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let _env_guard = EnvVarGuard::set(
            RUNTIME_CONFIG_VAR_NAME,
            runtime_config_path
                .to_str()
                .expect("runtime config path should be valid UTF-8"),
        );

        let runtime_processor = Processor::new_daemon(&peppy_config_path)
            .expect("runtime processor should load config from env");

        let mut expected_parameters: NodeArguments = NodeArguments::new();
        expected_parameters.insert("exposure".into(), AnyType::Float(0.25));
        expected_parameters.insert(
            "flags".into(),
            AnyType::Array(vec![
                AnyType::String("hdr".into()),
                AnyType::String("stabilized".into()),
            ]),
        );
        expected_parameters.insert(
            "nested".into(),
            AnyType::Object(BTreeMap::from([
                ("enabled".to_string(), AnyType::Bool(true)),
                ("gain".to_string(), AnyType::Int(10)),
            ])),
        );
        expected_parameters.insert("mode".into(), AnyType::String("auto".into()));

        assert_eq!(runtime_processor.bound_instance_id(), bound_instance_id);
        assert_eq!(runtime_processor.bound_daemon_node(), bound_daemon_node);
        assert_eq!(runtime_processor.node_name(), bound_node_name);
        assert_eq!(runtime_processor.input_arguments(), &expected_parameters);
    }

    #[test]
    fn fails_when_codegen_fingerprint_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/test_node"] },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_wrong_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { value: 42 }
            },
            node_name: "test_node",
            bound_daemon_node: "daemon-1234"
        }"#;

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let _env_guard = EnvVarGuard::set(
            RUNTIME_CONFIG_VAR_NAME,
            runtime_config_path.to_str().unwrap(),
        );

        let Err(err) = Processor::new_daemon(&peppy_config_path) else {
            panic!("expected fingerprint mismatch error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("fingerprint mismatch"),
            "expected fingerprint mismatch error, got: {err_string}"
        );
    }

    #[test]
    fn fails_when_runtime_parameter_missing_in_compiled_config() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Compiled config only has 'value' parameter
        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/test_node"] },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        // Runtime config has 'value' AND 'extra_param' - but 'extra_param' is not in compiled
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { value: 42, extra_param: "unexpected" }
            },
            node_name: "test_node",
            bound_daemon_node: "daemon-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let _env_guard = EnvVarGuard::set(
            RUNTIME_CONFIG_VAR_NAME,
            runtime_config_path.to_str().unwrap(),
        );

        let Err(err) = Processor::new_daemon(&peppy_config_path) else {
            panic!("expected missing parameter error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("missing parameter") && err_string.contains("extra_param"),
            "expected missing parameter error for 'extra_param', got: {err_string}"
        );
    }

    #[test]
    fn fails_when_parameter_type_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Compiled config expects 'value' to be i64
        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/test_node"] },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        // Runtime config provides 'value' as a string instead of i64
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { value: "not_an_integer" }
            },
            node_name: "test_node",
            bound_daemon_node: "daemon-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let _env_guard = EnvVarGuard::set(
            RUNTIME_CONFIG_VAR_NAME,
            runtime_config_path.to_str().unwrap(),
        );

        let Err(err) = Processor::new_daemon(&peppy_config_path) else {
            panic!("expected type mismatch error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("type mismatch") && err_string.contains("value"),
            "expected type mismatch error for 'value', got: {err_string}"
        );
    }

    #[test]
    fn fails_when_nested_parameter_type_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Compiled config expects nested object with specific types
        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/test_node"] },
            parameters: {
                config: {
                    $type: "object",
                    enabled: "bool",
                    threshold: "f64"
                }
            }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        // Runtime config provides 'enabled' as string instead of bool
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { config: { enabled: "yes", threshold: 0.5 } }
            },
            node_name: "test_node",
            bound_daemon_node: "daemon-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let _env_guard = EnvVarGuard::set(
            RUNTIME_CONFIG_VAR_NAME,
            runtime_config_path.to_str().unwrap(),
        );

        let Err(err) = Processor::new_daemon(&peppy_config_path) else {
            panic!("expected type mismatch error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("type mismatch") && err_string.contains("config.enabled"),
            "expected type mismatch error for 'config.enabled', got: {err_string}"
        );
    }

    #[test]
    fn fails_when_array_item_type_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Compiled config expects array of strings
        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/test_node"] },
            parameters: {
                tags: {
                    $type: "array",
                    $items: "string"
                }
            }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        // Runtime config provides array with mixed types (string and int)
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { tags: ["valid", 123, "also_valid"] }
            },
            node_name: "test_node",
            bound_daemon_node: "daemon-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let _env_guard = EnvVarGuard::set(
            RUNTIME_CONFIG_VAR_NAME,
            runtime_config_path.to_str().unwrap(),
        );

        let Err(err) = Processor::new_daemon(&peppy_config_path) else {
            panic!("expected type mismatch error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("type mismatch") && err_string.contains("tags[1]"),
            "expected type mismatch error for 'tags[1]', got: {err_string}"
        );
    }

    #[test]
    fn fails_when_codegen_fingerprint_missing() {
        use crate::error::Error;

        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/test_node"] },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        // Note: intentionally NOT creating the fingerprint file

        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { value: 42 }
            },
            node_name: "test_node",
            bound_daemon_node: "daemon-1234"
        }"#;

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let _env_guard = EnvVarGuard::set(
            RUNTIME_CONFIG_VAR_NAME,
            runtime_config_path.to_str().unwrap(),
        );

        let Err(err) = Processor::new_daemon(&peppy_config_path) else {
            panic!("expected codegen fingerprint read error");
        };
        assert!(
            matches!(err, Error::CodegenFingerprintRead { .. }),
            "expected CodegenFingerprintRead error, got: {err:?}"
        );
    }

    #[test]
    fn standalone_mode_uses_manifest_node_name() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "my_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/my_node"] },
            parameters: {}
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new();
        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        assert_eq!(processor.node_name(), "my_node");
        assert_eq!(processor.bound_instance_id(), "standalone");
        assert_eq!(processor.bound_daemon_node(), "standalone-daemon");
        assert_eq!(processor.messaging_host(), "127.0.0.1");
        assert_eq!(processor.messaging_port(), 7448);
    }

    #[test]
    fn standalone_mode_with_custom_config() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "my_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/my_node"] },
            parameters: {}
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new()
            .with_node_name("custom_name")
            .with_instance_id("custom_instance")
            .with_messaging("192.168.1.100", 9999);

        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        assert_eq!(processor.node_name(), "custom_name");
        assert_eq!(processor.bound_instance_id(), "custom_instance");
        assert_eq!(processor.messaging_host(), "192.168.1.100");
        assert_eq!(processor.messaging_port(), 9999);
    }

    #[test]
    fn standalone_mode_with_json5_parameters() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "my_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/my_node"] },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config =
            StandaloneConfig::new().with_parameters_json(serde_json::json!({ "value": 42 }));

        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        let args = processor.input_arguments();
        assert_eq!(args.get("value"), Some(&AnyType::Int(42)));
    }

    #[test]
    fn standalone_mode_with_typed_parameters() {
        use serde::Serialize;

        #[derive(Serialize)]
        struct TestParams {
            threshold: f64,
            enabled: bool,
        }

        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "my_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/my_node"] },
            parameters: { threshold: "f64", enabled: "bool" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let params = TestParams {
            threshold: 0.75,
            enabled: true,
        };
        let config = StandaloneConfig::new().with_parameters(&params);

        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        let args = processor.input_arguments();
        assert_eq!(args.get("threshold"), Some(&AnyType::Float(0.75)));
        assert_eq!(args.get("enabled"), Some(&AnyType::Bool(true)));
    }

    #[test]
    fn standalone_mode_fails_when_required_parameters_missing() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "my_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/my_node"] },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        // No parameters provided — should fail immediately
        let config = StandaloneConfig::new();
        let result = Processor::new_standalone(&peppy_config_path, &config);

        let Err(err) = result else {
            panic!("expected error when required parameters are missing");
        };
        assert!(
            matches!(err, crate::error::Error::MissingStandaloneParameters(_)),
            "expected MissingStandaloneParameters error, got: {err:?}"
        );
        let err_string = err.to_string();
        assert!(
            err_string.contains("value"),
            "error should mention missing parameter 'value', got: {err_string}"
        );
    }

    #[test]
    fn standalone_mode_fails_when_some_parameters_missing() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "my_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./target/debug/my_node"] },
            parameters: { threshold: "f64", enabled: "bool", name: "string" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        // Only provide one of three required parameters
        let config =
            StandaloneConfig::new().with_parameters_json(serde_json::json!({ "threshold": 0.5 }));
        let result = Processor::new_standalone(&peppy_config_path, &config);

        let Err(err) = result else {
            panic!("expected error when some required parameters are missing");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("enabled") && err_string.contains("name"),
            "error should mention missing parameters 'enabled' and 'name', got: {err_string}"
        );
        // The provided parameter should NOT be mentioned
        assert!(
            !err_string.contains("threshold"),
            "error should not mention provided parameter 'threshold', got: {err_string}"
        );
    }
}
