use crate::common::AnyType;
use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::launcher::Name;

/// Resolved per-slot binding for one of this consumer instance's declared
/// `depends_on` entries. The validator translates a launcher / CLI `(KEY,
/// VALUE)` binding map into this slot-keyed view before serializing into
/// `NodeInstanceConfig` so the spawned node does no re-resolution work.
///
/// `Pinned` corresponds to a `depends_on` entry with `from_any: false`;
/// it must be bound (the validator rejects pinned-unbound). `Deferred` is
/// a pinned slot bound via `--bind-deferred` to a target that was not
/// running at launch: it routes identically to `Pinned` (the transport
/// tolerates a producer that appears later), but its conformance and
/// identity were not checked up front — the daemon verifies them when the
/// target appears. `FromAnyBound` is a `from_any: true` slot for which the
/// user supplied one or more bindings via free-form keys. `FromAnyUnbound`
/// is a `from_any: true` slot the user left bindless — the wildcard
/// fallback for producers no sibling slot has claimed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SlotBinding {
    Pinned { producer_instance_id: String },
    Deferred { producer_instance_id: String },
    FromAnyBound { producer_instance_ids: Vec<String> },
    FromAnyUnbound,
}

/// Observability state of a `SlotBinding::Deferred` slot, computed from the
/// live stack rather than stored: a deferred slot's status is a pure
/// function of whether its target is running and, if so, whether the
/// target's node satisfies the slot. Surfaced per `link_id` in
/// `peppy stack list` and the daemon logs.
///
/// - `Pending` — the target instance is not currently running (it has not
///   appeared yet, or its id was a typo, or it has stopped).
/// - `Active` — the target is running and its node satisfies the slot
///   (conforms to the interface, or matches the node identity).
/// - `NonConforming` — the target is running but does not satisfy the slot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeferredStatus {
    Pending,
    Active,
    NonConforming,
}

/// Represents a node instance at runtime. Used by RuntimeConfig to identify the running node and its configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeInstanceConfig {
    pub instance_id: Name,
    #[serde(default)]
    pub arguments: BTreeMap<String, AnyType>,
    #[serde(default)]
    pub framework: ResolvedFramework,
    /// Pre-resolved per-slot bindings for every `link_id` declared in the
    /// consumer manifest's `depends_on`. Built by the validator from the
    /// launcher / CLI raw binding map plus the manifest depends_on (which
    /// distinguishes pinned vs `from_any` slots). Empty when the manifest
    /// has no `depends_on` entries. Read by the generated subscribe /
    /// poll / send_goal call sites via
    /// [`crate::runtime::ConsumerFilter`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slot_bindings: BTreeMap<String, SlotBinding>,
}

impl NodeInstanceConfig {
    /// Builds a config with everything except `instance_id` defaulted:
    /// empty arguments, default framework, empty slot bindings. Use with
    /// struct-update syntax to override a field:
    /// `NodeInstanceConfig { arguments, ..NodeInstanceConfig::new(id) }`.
    pub fn new(instance_id: Name) -> Self {
        Self {
            instance_id,
            arguments: BTreeMap::new(),
            framework: ResolvedFramework::default(),
            slot_bindings: BTreeMap::new(),
        }
    }
}

/// Framework knobs already resolved by the daemon. Distinct from
/// `launcher::FrameworkOverrides` so the type system enforces "resolution
/// happens once": the launcher form carries optional overrides; this form
/// carries concrete values the spawned node reads without further fallback.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedFramework {
    #[serde(default)]
    pub use_sim_time: bool,
}

/// Configuration for the launcher to know how to configure spawned nodes' messaging.
/// This is passed as part of a LauncherRequest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LauncherRuntimeConfig {
    pub messaging_host: String,
    pub messaging_port: u16,
    #[serde(default)]
    pub git_hash: String,
}

impl LauncherRuntimeConfig {
    pub fn new(
        messaging_host: impl Into<String>,
        messaging_port: u16,
        git_hash: impl Into<String>,
    ) -> Self {
        Self {
            messaging_host: messaging_host.into(),
            messaging_port,
            git_hash: git_hash.into(),
        }
    }
}

/// This class is generated by the peppy daemon and then passed to each respective peppy node instances spawned by it
/// through `PEPPY_RUNTIME_CONFIG` env var. It's then deserialized in the process runtime to understand
/// how to communicate with the rest of the system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub messaging_host: String,
    pub messaging_port: u16,
    pub node_name: Name,
    pub node_tag: Name,
    pub bound_core_node: Name,
    pub node_instance: NodeInstanceConfig,
}

impl RuntimeConfig {
    pub fn new(
        messaging_host: &str,
        messaging_port: u16,
        node_instance: NodeInstanceConfig,
        node_name: impl Into<String>,
        node_tag: impl Into<String>,
        bound_core_node: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            messaging_host: messaging_host.to_owned(),
            messaging_port,
            node_instance,
            node_name: Name::new(node_name.into())?,
            node_tag: Name::new(node_tag.into())?,
            bound_core_node: Name::new(bound_core_node.into())?,
        })
    }

    /// This function is typically invoked by the `peppy` program
    /// to persist its launch configuration for `peppylib` or `peppygen` to pick it up.
    pub fn save_json5_launch_config(&self, to_path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = to_path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let serialized = serde_json5::to_string(self)
            .map_err(|err| crate::error::Error::Serialize(err.to_string()))?;
        fs::write(path, serialized)?;
        Ok(path.to_path_buf())
    }

    pub fn generate_peppy_config_fingerprint(peppy_config: impl AsRef<Path>) -> Result<String> {
        use sha2::{Digest, Sha256};
        let config_path = peppy_config.as_ref();
        let content = std::fs::read(config_path)?;
        let hash = Sha256::digest(&content);
        Ok(hash
            .iter()
            .fold(String::with_capacity(hash.len() * 2), |mut acc, b| {
                use std::fmt::Write;
                let _ = write!(acc, "{:02x}", b);
                acc
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, ParsingError};
    use tempfile::TempDir;

    fn runtime_config_from_json(instance_id: &str) -> Result<RuntimeConfig> {
        let json = r#"{
            messaging_host: "$MESSAGING_HOST",
            messaging_port: $MESSAGING_PORT,
            node_instance: {
                instance_id: "$INSTANCE_ID"
            },
            node_name: "camera",
            node_tag: "v1",
            bound_core_node: "core_node"
        }"#;

        let populated = json
            .replace("$INSTANCE_ID", instance_id)
            .replace("$MESSAGING_HOST", "127.0.0.1")
            .replace("$MESSAGING_PORT", "7448");
        serde_json5::from_str(&populated).map_err(|err| Error::Parsing(err.into()))
    }

    /// `use_sim_time` round-trips through serialize/deserialize, and a
    /// runtime config written before this field existed (no `framework` key)
    /// still parses cleanly with `use_sim_time = false`.
    #[test]
    fn resolved_framework_round_trip_and_back_compat() {
        let with_sim: RuntimeConfig = serde_json5::from_str(
            r#"{
                messaging_host: "127.0.0.1",
                messaging_port: 7448,
                node_instance: {
                    instance_id: "camera_front",
                    framework: { use_sim_time: true }
                },
                node_name: "camera",
                node_tag: "v1",
                bound_core_node: "core_node"
            }"#,
        )
        .unwrap();
        assert!(with_sim.node_instance.framework.use_sim_time);

        let serialized = serde_json5::to_string(&with_sim).unwrap();
        let reparsed: RuntimeConfig = serde_json5::from_str(&serialized).unwrap();
        assert!(reparsed.node_instance.framework.use_sim_time);

        let legacy = runtime_config_from_json("camera_front").unwrap();
        assert!(!legacy.node_instance.framework.use_sim_time);
    }

    #[test]
    fn writes_launch_config_and_creates_parent_dir() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("peppy_launcher.json5");

        let config = runtime_config_from_json("camera_front").expect("runtime config should parse");
        let returned = config
            .save_json5_launch_config(&path)
            .expect("runtime config should write");

        let written = fs::read_to_string(&path).expect("launch config should be written to disk");
        let parsed: RuntimeConfig =
            serde_json5::from_str(&written).expect("launch config should parse");

        assert_eq!(returned, path);
        assert_eq!(parsed.node_name, "camera");
        assert_eq!(parsed.node_instance.instance_id, "camera_front");
        assert_eq!(parsed.bound_core_node, "core_node");
        assert_eq!(
            parsed.node_instance.instance_id,
            config.node_instance.instance_id
        );
        assert!(parsed.node_instance.arguments.is_empty());
    }

    #[test]
    fn rejects_invalid_instance_id() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("peppy_launcher.json5");

        let err = runtime_config_from_json("bad id!")
            .and_then(|config| config.save_json5_launch_config(&path))
            .unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref msg)) if msg.contains("Invalid name"))
                || matches!(err, Error::Parsing(ParsingError::InvalidName(_, _))),
            "expected parsing error about invalid name, got: {err}"
        );
    }
}
