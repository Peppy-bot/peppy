use std::path::Path;

use crate::error::{Error, Result};
use config::{
    NodeArguments,
    consts::{NODE_CONFIG_FINGERPRINT_FILE, PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME},
    node::NodeConfig,
    runtime::RuntimeConfig,
};

/// This struct is launched at runtime everytime a new peppy node is launched
pub struct Processor {
    runtime_config: RuntimeConfig,
}

impl Processor {
    /// This function takes care of 2 things:
    /// 1. Reads the `PEPPY_RUNTIME_CONFIG` env var passed during runtime from the master node/peppy daemon when the node is started
    /// 2. Checks that the fingerprint of the peppy_config generated for `peppygen` matches the one we have at runtime as input parameter to this function
    pub fn new_with_peppy_config(peppy_config: impl AsRef<Path>) -> Result<Self> {
        let launch_config_path = std::env::var(RUNTIME_CONFIG_VAR_NAME).map_err(|source| {
            Error::MissingInstanceIdEnvVar {
                var: RUNTIME_CONFIG_VAR_NAME,
                source,
            }
        })?;
        let runtime_config = Processor::get_peppy_deployment_config(&launch_config_path)?;
        let codegen_peppy_config_fingerprint =
            Processor::read_codegen_fingerprint(peppy_config.as_ref())?;
        let node_config: NodeConfig =
            serde_json5::from_str(&std::fs::read_to_string(peppy_config.as_ref())?)?;
        Processor::check_generated_code_matches_runtime_config(
            peppy_config,
            &codegen_peppy_config_fingerprint,
        )?;
        Processor::check_node_config_parameters_types(
            &runtime_config.deployment_instance.arguments,
            &node_config.parameters,
        )?;

        Ok(Self { runtime_config })
    }

    fn check_generated_code_matches_runtime_config(
        peppy_config: impl AsRef<Path>,
        codegen_peppy_config_fingerprint: &str,
    ) -> Result<()> {
        let hash = RuntimeConfig::generate_peppy_config_fingerprint(&peppy_config)?;

        if hash == codegen_peppy_config_fingerprint {
            return Ok(());
        }

        Err(Error::PeppyConfigFingerprintMismatch {
            path: peppy_config.as_ref().display().to_string(),
            expected: codegen_peppy_config_fingerprint.to_string(),
            actual: hash,
        })
    }

    fn check_node_config_parameters_types(
        runtime_arguments: &NodeArguments,
        compiled_node_parameters: &NodeArguments,
    ) -> Result<()> {
        for (key, runtime_value) in runtime_arguments {
            let compiled_type = compiled_node_parameters
                .get(key)
                .ok_or_else(|| Error::MissingCompiledParameter { path: key.clone() })?;
            runtime_value.matches_type_spec(compiled_type, key)?;
        }
        Ok(())
    }

    fn get_peppy_deployment_config(launch_config_path: &str) -> Result<RuntimeConfig> {
        let content = std::fs::read_to_string(launch_config_path).map_err(|source| {
            Error::LaunchConfigRead {
                path: launch_config_path.to_string(),
                source,
            }
        })?;
        serde_json5::from_str(&content).map_err(|source| Error::LaunchConfigParse {
            path: launch_config_path.to_string(),
            source,
        })
    }

    fn read_codegen_fingerprint(peppy_config: &Path) -> Result<String> {
        let peppy_config_dir = peppy_config.parent().unwrap_or_else(|| Path::new("."));
        let fingerprint_path = peppy_config_dir
            .join(PEPPYGEN_OUTPUT_PATH)
            .join(NODE_CONFIG_FINGERPRINT_FILE);
        std::fs::read_to_string(&fingerprint_path)
            .map(|s| s.trim().to_string())
            .map_err(|source| Error::CodegenFingerprintRead {
                path: fingerprint_path.display().to_string(),
                source,
            })
    }

    pub fn bound_instance_id(&self) -> &str {
        self.runtime_config.deployment_instance.instance_id.as_str()
    }

    pub fn bound_master_node(&self) -> &str {
        self.runtime_config.bound_master_node.as_str()
    }

    pub fn input_arguments(&self) -> &NodeArguments {
        &self.runtime_config.deployment_instance.arguments
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
    use super::{
        NODE_CONFIG_FINGERPRINT_FILE, PEPPYGEN_OUTPUT_PATH, Processor, RUNTIME_CONFIG_VAR_NAME,
    };
    use config::{AnyType, NodeArguments, runtime::RuntimeConfig};
    use std::{collections::BTreeMap, env, fs, path::Path, sync::Mutex};
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

    /// Creates the fingerprint file at the expected location for runtime checks.
    fn create_codegen_fingerprint(peppy_config_path: &Path) {
        let peppy_config_dir = peppy_config_path.parent().unwrap_or(Path::new("."));
        let fingerprint_dir = peppy_config_dir.join(PEPPYGEN_OUTPUT_PATH);
        fs::create_dir_all(&fingerprint_dir).expect("fingerprint dir should be created");
        let fingerprint_path = fingerprint_dir.join(NODE_CONFIG_FINGERPRINT_FILE);
        let fingerprint = RuntimeConfig::generate_peppy_config_fingerprint(peppy_config_path)
            .expect("fingerprint should be generated");
        fs::write(&fingerprint_path, format!("{fingerprint}\n"))
            .expect("fingerprint should be written");
    }

    /// Creates a fingerprint file with incorrect content to test mismatch errors.
    fn create_wrong_codegen_fingerprint(peppy_config_path: &Path) {
        let peppy_config_dir = peppy_config_path.parent().unwrap_or(Path::new("."));
        let fingerprint_dir = peppy_config_dir.join(PEPPYGEN_OUTPUT_PATH);
        fs::create_dir_all(&fingerprint_dir).expect("fingerprint dir should be created");
        let fingerprint_path = fingerprint_dir.join(NODE_CONFIG_FINGERPRINT_FILE);
        fs::write(&fingerprint_path, "wrong_fingerprint_value\n")
            .expect("fingerprint should be written");
    }

    #[test]
    fn loads_runtime_config_from_env() {
        let bound_master_node = "epic-whale-6789";
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
                launch_cmd: ["cargo", "run"]
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
        create_codegen_fingerprint(&peppy_config_path);

        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            deployment_instance: {
                instance_id: "$INSTANCE_ID",
                parameters: {
                    exposure: 0.25,
                    flags: ["hdr", "stabilized"],
                    nested: { enabled: true, gain: 10 },
                    mode: "auto"
                }
            },
            node_name: "$NODE_NAME",
            bound_master_node: "$MASTER_NODE"
        }"#;

        let populated_config = json5_config
            .replace("$INSTANCE_ID", bound_instance_id)
            .replace("$NODE_NAME", bound_node_name)
            .replace("$MASTER_NODE", bound_master_node);

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

        let runtime_processor = Processor::new_with_peppy_config(&peppy_config_path)
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
        assert_eq!(runtime_processor.bound_master_node(), bound_master_node);
        assert_eq!(runtime_processor.node_name(), bound_node_name);
        assert_eq!(runtime_processor.input_arguments(), &expected_parameters);
    }

    #[test]
    fn fails_when_codegen_fingerprint_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0", launch_cmd: ["cargo", "run"] },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        create_wrong_codegen_fingerprint(&peppy_config_path);

        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            deployment_instance: {
                instance_id: "test_instance",
                parameters: { value: 42 }
            },
            node_name: "test_node",
            bound_master_node: "master-1234"
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

        let Err(err) = Processor::new_with_peppy_config(&peppy_config_path) else {
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
            manifest: { name: "test_node", tag: "0.1.0", launch_cmd: ["cargo", "run"] },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        create_codegen_fingerprint(&peppy_config_path);

        // Runtime config has 'value' AND 'extra_param' - but 'extra_param' is not in compiled
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            deployment_instance: {
                instance_id: "test_instance",
                parameters: { value: 42, extra_param: "unexpected" }
            },
            node_name: "test_node",
            bound_master_node: "master-1234"
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

        let Err(err) = Processor::new_with_peppy_config(&peppy_config_path) else {
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
            manifest: { name: "test_node", tag: "0.1.0", launch_cmd: ["cargo", "run"] },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        create_codegen_fingerprint(&peppy_config_path);

        // Runtime config provides 'value' as a string instead of i64
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            deployment_instance: {
                instance_id: "test_instance",
                parameters: { value: "not_an_integer" }
            },
            node_name: "test_node",
            bound_master_node: "master-1234"
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

        let Err(err) = Processor::new_with_peppy_config(&peppy_config_path) else {
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
            manifest: { name: "test_node", tag: "0.1.0", launch_cmd: ["cargo", "run"] },
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
        create_codegen_fingerprint(&peppy_config_path);

        // Runtime config provides 'enabled' as string instead of bool
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            deployment_instance: {
                instance_id: "test_instance",
                parameters: { config: { enabled: "yes", threshold: 0.5 } }
            },
            node_name: "test_node",
            bound_master_node: "master-1234"
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

        let Err(err) = Processor::new_with_peppy_config(&peppy_config_path) else {
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
            manifest: { name: "test_node", tag: "0.1.0", launch_cmd: ["cargo", "run"] },
            parameters: {
                tags: {
                    $type: "array",
                    $items: "string"
                }
            }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        create_codegen_fingerprint(&peppy_config_path);

        // Runtime config provides array with mixed types (string and int)
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            deployment_instance: {
                instance_id: "test_instance",
                parameters: { tags: ["valid", 123, "also_valid"] }
            },
            node_name: "test_node",
            bound_master_node: "master-1234"
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

        let Err(err) = Processor::new_with_peppy_config(&peppy_config_path) else {
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
            manifest: { name: "test_node", tag: "0.1.0", launch_cmd: ["cargo", "run"] },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        // Note: intentionally NOT creating the fingerprint file

        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            deployment_instance: {
                instance_id: "test_instance",
                parameters: { value: 42 }
            },
            node_name: "test_node",
            bound_master_node: "master-1234"
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

        let Err(err) = Processor::new_with_peppy_config(&peppy_config_path) else {
            panic!("expected codegen fingerprint read error");
        };
        assert!(
            matches!(err, Error::CodegenFingerprintRead { .. }),
            "expected CodegenFingerprintRead error, got: {err:?}"
        );
    }
}
