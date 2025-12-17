use std::path::Path;

use crate::error::{Error, Result};
use config::{
    NodeParameters, consts::RUNTIME_CONFIG_VAR_NAME, node::NodeConfig, runtime::RuntimeConfig,
};
use node_stack::NodeStack;

/// This struct is launched at runtime everytime a new peppy node is launched
pub struct Processor {
    runtime_config: RuntimeConfig,
}

impl Processor {
    /// This function takes care of 2 things:
    /// 1. Reads the `PEPPY_RUNTIME_CONFIG` env var passed during runtime from the master node/peppy daemon when the node is started
    /// 2. Checks that the md5 of the peppy_config generated for `peppygen` matches the one we have at runtime as input parameter to this function
    pub fn new_with_peppy_config(peppy_config: impl AsRef<Path>) -> Result<Self> {
        let launch_config_path = std::env::var(RUNTIME_CONFIG_VAR_NAME).map_err(|source| {
            Error::MissingInstanceIdEnvVar {
                var: RUNTIME_CONFIG_VAR_NAME,
                source,
            }
        })?;
        let launch_config = Processor::get_peppy_deployment_config(&launch_config_path)?;
        let node_config: NodeConfig =
            serde_json5::from_str(&std::fs::read_to_string(peppy_config.as_ref())?)?;
        Processor::check_generated_code_matches_runtime_config(
            peppy_config,
            &launch_config.codegen_peppy_config_md5,
        )?;
        Processor::check_node_config_parameters_types(
            &launch_config.deployment_instance.parameters,
            &node_config.parameters,
        )?;

        Ok(Self {
            runtime_config: launch_config,
        })
    }

    fn check_generated_code_matches_runtime_config(
        peppy_config: impl AsRef<Path>,
        codegen_peppy_config_md5: &str,
    ) -> Result<()> {
        let md5 = RuntimeConfig::generate_peppy_config_md5(&peppy_config)?;

        if md5 == codegen_peppy_config_md5 {
            return Ok(());
        }

        Err(Error::PeppyConfigMd5Mismatch {
            path: peppy_config.as_ref().display().to_string(),
            expected: codegen_peppy_config_md5.to_string(),
            actual: md5,
        })
    }

    fn check_node_config_parameters_types(
        runtime_parameters: &NodeParameters,
        compiled_node_parameters: &NodeParameters,
    ) -> Result<()> {
        for (key, runtime_value) in runtime_parameters {
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

    pub fn bound_instance_id(&self) -> &str {
        self.runtime_config.deployment_instance.instance_id.as_str()
    }

    pub fn bound_master_node(&self) -> &str {
        self.runtime_config.bound_master_node.as_str()
    }

    pub fn input_parameters(&self) -> &NodeParameters {
        &self.runtime_config.deployment_instance.parameters
    }

    pub fn node_name(&self) -> &str {
        self.runtime_config.node_name.as_str()
    }

    pub fn get_node_stack() -> NodeStack {
        todo!(
            "Call the master node endpoint that returns the node stack. Don't do that during code generation since the list won't be up to date"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Processor, RUNTIME_CONFIG_VAR_NAME};
    use config::{AnyType, NodeParameters, runtime::RuntimeConfig};
    use std::{collections::BTreeMap, env, sync::Mutex};
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
                tag: "0.1.0"
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
        let codegen_md5 = RuntimeConfig::generate_peppy_config_md5(&peppy_config_path)
            .expect("peppy config md5 should be generated");

        let json5_config = r#"{
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
            bound_master_node: "$MASTER_NODE",
            codegen_peppy_config_md5: "$CODEGEN_MD5"
        }"#;

        let populated_config = json5_config
            .replace("$INSTANCE_ID", bound_instance_id)
            .replace("$NODE_NAME", bound_node_name)
            .replace("$MASTER_NODE", bound_master_node)
            .replace("$CODEGEN_MD5", &codegen_md5);

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

        let mut expected_parameters: NodeParameters = NodeParameters::new();
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
        assert_eq!(runtime_processor.input_parameters(), &expected_parameters);
    }

    #[test]
    fn fails_when_codegen_md5_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0" },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let json5_config = r#"{
            deployment_instance: {
                instance_id: "test_instance",
                parameters: { value: 42 }
            },
            node_name: "test_node",
            bound_master_node: "master-1234",
            codegen_peppy_config_md5: "invalid_md5_that_does_not_match"
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
            panic!("expected md5 mismatch error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("md5 mismatch"),
            "expected md5 mismatch error, got: {err_string}"
        );
    }

    #[test]
    fn fails_when_runtime_parameter_missing_in_compiled_config() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Compiled config only has 'value' parameter
        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        let peppy_config_content = r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0" },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let codegen_md5 = RuntimeConfig::generate_peppy_config_md5(&peppy_config_path)
            .expect("peppy config md5 should be generated");

        // Runtime config has 'value' AND 'extra_param' - but 'extra_param' is not in compiled
        let json5_config = format!(
            r#"{{
            deployment_instance: {{
                instance_id: "test_instance",
                parameters: {{ value: 42, extra_param: "unexpected" }}
            }},
            node_name: "test_node",
            bound_master_node: "master-1234",
            codegen_peppy_config_md5: "{codegen_md5}"
        }}"#
        );

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
            manifest: { name: "test_node", tag: "0.1.0" },
            parameters: { value: "i64" }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let codegen_md5 = RuntimeConfig::generate_peppy_config_md5(&peppy_config_path)
            .expect("peppy config md5 should be generated");

        // Runtime config provides 'value' as a string instead of i64
        let json5_config = format!(
            r#"{{
            deployment_instance: {{
                instance_id: "test_instance",
                parameters: {{ value: "not_an_integer" }}
            }},
            node_name: "test_node",
            bound_master_node: "master-1234",
            codegen_peppy_config_md5: "{codegen_md5}"
        }}"#
        );

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
            manifest: { name: "test_node", tag: "0.1.0" },
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

        let codegen_md5 = RuntimeConfig::generate_peppy_config_md5(&peppy_config_path)
            .expect("peppy config md5 should be generated");

        // Runtime config provides 'enabled' as string instead of bool
        let json5_config = format!(
            r#"{{
            deployment_instance: {{
                instance_id: "test_instance",
                parameters: {{ config: {{ enabled: "yes", threshold: 0.5 }} }}
            }},
            node_name: "test_node",
            bound_master_node: "master-1234",
            codegen_peppy_config_md5: "{codegen_md5}"
        }}"#
        );

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
            manifest: { name: "test_node", tag: "0.1.0" },
            parameters: {
                tags: {
                    $type: "array",
                    $items: "string"
                }
            }
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let codegen_md5 = RuntimeConfig::generate_peppy_config_md5(&peppy_config_path)
            .expect("peppy config md5 should be generated");

        // Runtime config provides array with mixed types (string and int)
        let json5_config = format!(
            r#"{{
            deployment_instance: {{
                instance_id: "test_instance",
                parameters: {{ tags: ["valid", 123, "also_valid"] }}
            }},
            node_name: "test_node",
            bound_master_node: "master-1234",
            codegen_peppy_config_md5: "{codegen_md5}"
        }}"#
        );

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
}
