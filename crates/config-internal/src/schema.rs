use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};
use std::fmt;

/// Schema identifier embedded at the root of node, launcher, and interface
/// `.json5` documents. The schema tag tells the daemon which document shape it
/// is reading so the strict deserializer can reject mixed-up files (e.g. a
/// launcher that claims to be a node config). Node files are always named
/// `peppy.json5`; launcher files conventionally use `peppy_launcher.json5`
/// for standalone projects but may use any `.json5` filename when discovered
/// through a repository. Interface files are filename-agnostic and identified
/// solely by their schema tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeppySchema {
    NodeV1,
    LauncherV1,
    InterfaceV1,
}

impl fmt::Display for PeppySchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PeppySchema::NodeV1 => "node_v1",
            PeppySchema::LauncherV1 => "launcher_v1",
            PeppySchema::InterfaceV1 => "interface_v1",
        };
        f.write_str(s)
    }
}

impl PeppySchema {
    /// Deserialize a `peppy_schema` field and reject any value other
    /// than `expected`. Used as the core of the per-document-shape
    /// `#[serde(deserialize_with = ...)]` guards.
    pub(crate) fn deserialize_expecting<'de, D>(
        deserializer: D,
        expected: Self,
    ) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let schema = Self::deserialize(deserializer)?;
        if schema != expected {
            return Err(de::Error::custom(format!(
                "expected peppy_schema '{expected}', got '{schema}'"
            )));
        }
        Ok(schema)
    }
}
