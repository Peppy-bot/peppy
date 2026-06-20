use std::path::Path;

use crate::error::{Error, ParameterDeserializationError, Result};
use config::{
    AnyType, NodeArguments,
    consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME},
    launcher::Name,
    node::{NodeConfig, load_standalone_node_config},
    runtime::{NodeInstanceConfig, RuntimeConfig},
    validate_node_arguments,
};
use std::collections::BTreeMap;

use super::builder::StandaloneConfig;

/// Runtime processor that holds configuration for the node.
#[derive(Clone)]
pub struct Processor {
    runtime_config: RuntimeConfig,
    validated_arguments: NodeArguments,
    /// Pre-resolved per-`link_id` [`crate::messaging::ConsumerFilter`].
    /// Computed once at startup from the daemon-supplied `slot_bindings`
    /// plus the manifest's `depends_on`; cached so subscribe / poll /
    /// send_goal call sites return a borrowed reference cheaply.
    consumer_filters: BTreeMap<String, crate::messaging::ConsumerFilter>,
}

impl Processor {
    /// Create processor for daemon mode.
    ///
    /// Resolves the launch-config path from the `PEPPY_RUNTIME_CONFIG` env var
    /// (set by the CLI when it launches the node) and delegates to
    /// [`new_daemon_from_path`](Self::new_daemon_from_path). Validates the
    /// fingerprint matches compiled code.
    pub fn new_daemon(peppy_config: impl AsRef<Path>) -> Result<Self> {
        let launch_config_path = std::env::var(RUNTIME_CONFIG_VAR_NAME).map_err(|source| {
            Error::MissingInstanceIdEnvVar {
                var: RUNTIME_CONFIG_VAR_NAME,
                source,
            }
        })?;

        Self::new_daemon_from_path(peppy_config, &launch_config_path)
    }

    /// Create processor for daemon mode from an explicit launch-config path.
    ///
    /// This is the env-free core that [`new_daemon`](Self::new_daemon) wraps:
    /// the launch-config path is passed in rather than read from the
    /// environment, so callers that already know the path (embedders, tests)
    /// can construct a daemon-mode processor without touching process state.
    /// Validates the fingerprint matches compiled code.
    pub fn new_daemon_from_path(
        peppy_config: impl AsRef<Path>,
        launch_config_path: &str,
    ) -> Result<Self> {
        let runtime_config = Self::load_runtime_config(launch_config_path)?;

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
        let validated_arguments = validate_node_arguments(
            runtime_config.node_instance.arguments.clone(),
            &node_config.execution.parameters,
        )?;

        let consumer_filters = build_consumer_filters(&runtime_config, &node_config);

        Ok(Self {
            runtime_config,
            validated_arguments,
            consumer_filters,
        })
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
        let node_config: NodeConfig = load_standalone_node_config(peppy_config.as_ref())?;

        let arguments: BTreeMap<String, AnyType> = match &config.parameters {
            Some(params) => serde_json::from_value(params.clone()).map_err(|e| {
                ParameterDeserializationError::single(format!("failed to parse parameters: {}", e))
            })?,
            None => BTreeMap::new(),
        };

        let validated_arguments =
            validate_node_arguments(arguments, &node_config.execution.parameters)?;

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
            NodeInstanceConfig::new(instance_id_name),
            &node_name,
            node_config.manifest.tag.as_str(),
            "standalone-core",
        )?;

        let consumer_filters = build_consumer_filters(&runtime_config, &node_config);

        Ok(Self {
            runtime_config,
            validated_arguments,
            consumer_filters,
        })
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

    pub fn bound_instance_id(&self) -> &str {
        self.runtime_config.node_instance.instance_id.as_str()
    }

    pub fn bound_core_node(&self) -> &str {
        self.runtime_config.bound_core_node.as_str()
    }

    pub(crate) fn input_arguments(&self) -> &NodeArguments {
        &self.validated_arguments
    }

    pub fn node_name(&self) -> &str {
        self.runtime_config.node_name.as_str()
    }

    pub fn node_tag(&self) -> &str {
        self.runtime_config.node_tag.as_str()
    }

    pub fn messaging_host(&self) -> &str {
        &self.runtime_config.messaging_host
    }

    pub fn messaging_port(&self) -> u16 {
        self.runtime_config.messaging_port
    }

    /// Peer-discovery settings for this node's messaging session.
    pub(crate) fn discovery(&self) -> &config::runtime::DiscoveryConfig {
        &self.runtime_config.discovery
    }

    /// Grace period this node's daemon-liveness watchdog waits, after the
    /// daemon's heartbeat goes silent, before shutting the node down. Resolved
    /// by the daemon from `peppy_config.json5` and shipped in the runtime
    /// config. Read by [`crate::services::daemon_watchdog`].
    pub fn daemon_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.runtime_config.lifecycle.daemon_grace_secs)
    }

    /// Cooperative-shutdown grace window. The daemon waits this long for a
    /// stopping node to exit before force-killing it, and the node runtime
    /// bounds its registered shutdown hooks
    /// ([`crate::runtime::NodeRunner::on_shutdown`]) by the same window.
    /// Resolved by the daemon from `peppy_config.json5` and shipped in the
    /// runtime config; standalone nodes use the built-in default.
    pub fn shutdown_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.runtime_config.lifecycle.shutdown_grace_secs)
    }

    /// Daemon-resolved framework `use_sim_time` flag for this instance.
    /// Read by [`crate::clock::for_node`] to pick between
    /// the wall-time and sim-time `PeppyClock` implementations.
    pub fn use_sim_time(&self) -> bool {
        self.runtime_config.node_instance.framework.use_sim_time
    }

    /// Resolved [`crate::messaging::ConsumerFilter`] for the consumer
    /// slot declared at `link_id`. The filter is computed once at
    /// startup from the daemon-supplied `slot_bindings` plus the
    /// manifest's `depends_on` (so the pinned-claims-from_any
    /// precedence rule is applied a single time) and cached on the
    /// processor for the lifetime of the node.
    ///
    /// Generated subscribe / poll / send_goal call sites splice
    /// `node_runner.processor().consumer_filter(<link_id>)` at the
    /// consumer-filter argument slot.
    pub fn consumer_filter(&self, link_id: &str) -> &crate::messaging::ConsumerFilter {
        const ANY: crate::messaging::ConsumerFilter = crate::messaging::ConsumerFilter::Any;
        self.consumer_filters.get(link_id).unwrap_or(&ANY)
    }

    /// Convenience for service / action call sites: returns the single
    /// producer this slot pins (`ConsumerFilter::Pin`) as an owned
    /// [`crate::messaging::ProducerRef`], or `None` for every other
    /// variant. The owned form crosses the PyO3 boundary cleanly; native
    /// Rust call sites can either use this or
    /// [`Self::consumer_filter`]`.pinned_target()` (the latter borrows
    /// from the cached filter).
    ///
    /// Deliberately renamed from the pre-`ProducerRef` `pinned_target_for`
    /// so generated Python built against the instance_id-only shape fails
    /// loudly with `AttributeError` instead of silently misaddressing.
    pub fn pinned_producer_for(&self, link_id: &str) -> Option<crate::messaging::ProducerRef> {
        self.consumer_filter(link_id).pinned_target().cloned()
    }
}

/// Pre-resolve a [`ConsumerFilter`] for every `link_id` declared in
/// the consumer manifest's `depends_on`. Called once during
/// [`Processor::new_daemon`] / [`Processor::new_standalone`] so the
/// per-link_id accessor is a borrow into a stable cache.
fn build_consumer_filters(
    runtime_config: &RuntimeConfig,
    node_config: &NodeConfig,
) -> BTreeMap<String, crate::messaging::ConsumerFilter> {
    let depends_on = node_config.manifest.depends_on.as_ref();
    let mut out = BTreeMap::new();
    if let Some(deps) = depends_on {
        for dep in &deps.nodes {
            let filter = crate::messaging::resolve_consumer_filter(
                dep.link_id.as_str(),
                &runtime_config.node_instance.slot_bindings,
                depends_on,
            );
            out.insert(dep.link_id.clone(), filter);
        }
        for dep in &deps.interfaces {
            let filter = crate::messaging::resolve_consumer_filter(
                dep.link_id.as_str(),
                &runtime_config.node_instance.slot_bindings,
                depends_on,
            );
            out.insert(dep.link_id.clone(), filter);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{PEPPYGEN_OUTPUT_PATH, Processor};
    use crate::runtime::builder::StandaloneConfig;
    use config::node::TypeToken;
    use config::{
        AnyType, ParameterSchema, ParameterSpec, runtime::RuntimeConfig, validate_node_arguments,
    };
    use std::{collections::BTreeMap, path::Path};
    use tempfile::TempDir;

    #[test]
    fn loads_runtime_config_from_path() {
        let bound_core_node = "epic-whale-6789";
        let bound_node_name = "uvc_camera";
        let bound_instance_id = "camera_front";

        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Create a peppy config file with type specifications matching runtime parameters
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "uvc_camera",
                tag: "v1",
            },
            execution: {
                language: "rust",
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
                },
                run_cmd: ["./target/debug/uvc_camera"]
            },
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
            node_tag: "v1",
            bound_core_node: "$CORE_NODE"
        }"#;

        let populated_config = json5_config
            .replace("$INSTANCE_ID", bound_instance_id)
            .replace("$NODE_NAME", bound_node_name)
            .replace("$CORE_NODE", bound_core_node);

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&populated_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let runtime_processor = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path
                .to_str()
                .expect("runtime config path should be valid UTF-8"),
        )
        .expect("runtime processor should load config from path");

        let mut expected_parameters: BTreeMap<String, AnyType> = BTreeMap::new();
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
        assert_eq!(runtime_processor.bound_core_node(), bound_core_node);
        assert_eq!(runtime_processor.node_name(), bound_node_name);
        assert_eq!(
            serde_json::to_value(runtime_processor.input_arguments()).unwrap(),
            serde_json::to_value(&expected_parameters).unwrap(),
        );
    }

    #[test]
    fn fails_when_codegen_fingerprint_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/test_node"] },
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
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#;

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
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
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/test_node"] },
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
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
            panic!("expected missing parameter error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("unknown parameter") && err_string.contains("extra_param"),
            "expected unknown parameter error for 'extra_param', got: {err_string}"
        );
    }

    #[test]
    fn fails_when_parameter_type_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Compiled config expects 'value' to be i64
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/test_node"] },
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
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
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
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: {
                language: "rust",
                parameters: {
                    config: {
                        $type: "object",
                        enabled: "bool",
                        threshold: "f64"
                    }
                },
                run_cmd: ["./target/debug/test_node"]
            },
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
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
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
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: {
                language: "rust",
                parameters: {
                    tags: {
                        $type: "array",
                        $items: "string"
                    }
                },
                run_cmd: ["./target/debug/test_node"]
            },
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
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
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

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/test_node"] },
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
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#;

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
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
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new();
        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        assert_eq!(processor.node_name(), "my_node");
        assert_eq!(processor.bound_instance_id(), "standalone");
        assert_eq!(processor.bound_core_node(), "standalone-core");
        assert_eq!(processor.messaging_host(), "127.0.0.1");
        assert_eq!(processor.messaging_port(), 7448);
    }

    #[test]
    fn standalone_mode_with_custom_config() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
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
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config =
            StandaloneConfig::new().with_parameters_json(serde_json::json!({ "value": 42 }));

        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        let args_json = serde_json::to_value(processor.input_arguments()).unwrap();
        assert_eq!(args_json.get("value"), Some(&serde_json::json!(42)));
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
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", parameters: { threshold: "f64", enabled: "bool" }, run_cmd: ["./target/debug/my_node"] },
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

        let args_json = serde_json::to_value(processor.input_arguments()).unwrap();
        assert_eq!(args_json.get("threshold"), Some(&serde_json::json!(0.75)));
        assert_eq!(args_json.get("enabled"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn standalone_mode_fails_when_required_parameters_missing() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/my_node"] },
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
            matches!(err, crate::error::Error::NodeArgumentsValidation(_)),
            "expected NodeArgumentsValidation error, got: {err:?}"
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
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", parameters: { threshold: "f64", enabled: "bool", name: "string" }, run_cmd: ["./target/debug/my_node"] },
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

    #[test]
    fn standalone_mode_fills_defaults_for_omitted_parameters() {
        // Partial config: user omits `frame_rate`, runtime fills it from $default.
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: {
                language: "rust",
                parameters: {
                    name: "string",
                    frame_rate: { $type: "u16", $default: 30 }
                },
                run_cmd: ["./target/debug/my_node"]
            },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config =
            StandaloneConfig::new().with_parameters_json(serde_json::json!({ "name": "front" }));
        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("standalone with partial parameters should succeed");

        let args_json = serde_json::to_value(processor.input_arguments()).unwrap();
        assert_eq!(args_json.get("name"), Some(&serde_json::json!("front")));
        assert_eq!(args_json.get("frame_rate"), Some(&serde_json::json!(30)));
    }

    #[test]
    fn standalone_mode_synthesizes_fully_defaulted_group() {
        // The whole `device` group is omitted; every leaf has a default,
        // so the runtime synthesizes the group from defaults.
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: {
                language: "rust",
                parameters: {
                    device: {
                        path: { $type: "string", $default: "/dev/video0" },
                        auto_detect: { $type: "bool", $default: true }
                    }
                },
                run_cmd: ["./target/debug/my_node"]
            },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new();
        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("fully-defaulted group should be synthesized");

        let args_json = serde_json::to_value(processor.input_arguments()).unwrap();
        assert_eq!(
            args_json.get("device"),
            Some(&serde_json::json!({ "path": "/dev/video0", "auto_detect": true }))
        );
    }

    #[test]
    fn standalone_mode_partial_default_group_reports_missing_leaf() {
        // `device.serial` has no default (USB serials vary per unit); the
        // error must name that leaf by full dot-path so users know what to
        // supply.
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: {
                language: "rust",
                parameters: {
                    device: {
                        path: { $type: "string", $default: "/dev/video0" },
                        serial: "string"
                    }
                },
                run_cmd: ["./target/debug/my_node"]
            },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new();
        let result = Processor::new_standalone(&peppy_config_path, &config);
        let err = result.err().expect("missing required leaf should error");
        let msg = err.to_string();
        assert!(
            msg.contains("device.serial"),
            "error should name the missing leaf path, got: {msg}"
        );
    }

    #[test]
    fn validated_arguments_cannot_be_serialized_back_to_raw() {
        // NodeArguments derives Serialize but does not expose the inner
        // data — the only way to consume it is through
        // deserialize_parameters, which parses into a typed struct.
        let arguments = BTreeMap::from([("x".to_string(), AnyType::Int(1))]);
        let schema = ParameterSchema::from([(
            "x".to_string(),
            ParameterSpec::Primitive {
                kind: TypeToken::I64,
                default: None,
            },
        )]);
        let validated =
            validate_node_arguments(arguments, &schema).expect("validation should pass");

        // We can serialize (for deserialize_parameters) but cannot access
        // the inner map directly — this is a compile-time guarantee.
        let json = serde_json::to_value(&validated).expect("should serialize");
        assert_eq!(json.get("x"), Some(&serde_json::json!(1)));
    }
}
