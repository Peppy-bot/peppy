use std::{collections::HashMap, path::Path};

use crate::error::{Error, Result};
use config::{NodeParameters, runtime::RuntimeConfig};

const PEPPY_RUNTIME_CONFIG: &str = "PEPPY_RUNTIME_CONFIG";

pub struct RuntimeProcessor {
    launch_config: RuntimeConfig,
}

/// This struct is launched at runtime everytime a new peppy node is launched
impl RuntimeProcessor {
    /// This function takes care of 2 things:
    /// 1. Reads the `PEPPY_RUNTIME_CONFIG` env var passed during runtime from the master node/peppy daemon when the node is started
    /// 2. Checks that the md5 of the peppy_config generated for `peppygen` matches the one we have at runtime as input parameter to this function
    pub fn new_with_peppy_config(peppy_config: impl AsRef<Path>) -> Result<Self> {
        let launch_config_path = std::env::var(PEPPY_RUNTIME_CONFIG).map_err(|source| {
            Error::MissingInstanceIdEnvVar {
                var: PEPPY_RUNTIME_CONFIG,
                source,
            }
        })?;
        let launch_config = RuntimeProcessor::get_peppy_deployment_config(&launch_config_path)?;
        RuntimeProcessor::check_generated_code_matches_runtime_config(
            peppy_config,
            &launch_config.codegen_peppy_config_md5,
        )?;
        Ok(Self { launch_config })
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
        self.launch_config.deployment_instance.instance_id.as_str()
    }

    pub fn bound_master_node(&self) -> &str {
        self.launch_config.bound_master_node.as_str()
    }

    pub fn input_parameters(&self) -> &NodeParameters {
        &self.launch_config.deployment_instance.parameters
    }

    pub fn node_name(&self) -> &str {
        self.launch_config.node_name.as_str()
    }

    pub fn get_instance_ids() -> HashMap<String, String> {
        todo!(
            "Finish. This is a dynamic call to the master node to get the current instances_ids. Don't do that during code generation since the list won't be up to date"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{PEPPY_RUNTIME_CONFIG, RuntimeProcessor};
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

        // Create a peppy config file and compute its MD5
        let peppy_config_path = temp_dir.path().join("peppy_config.json5");
        std::fs::write(&peppy_config_path, "{}").expect("peppy config should be written");
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
            PEPPY_RUNTIME_CONFIG,
            runtime_config_path
                .to_str()
                .expect("runtime config path should be valid UTF-8"),
        );

        let runtime_processor = RuntimeProcessor::new_with_peppy_config(&peppy_config_path)
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
}
