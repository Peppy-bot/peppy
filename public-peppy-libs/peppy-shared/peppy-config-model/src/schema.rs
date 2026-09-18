use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};
use std::fmt;

/// Schema identifier embedded at the root of every Peppy `.json5` document.
/// The schema tag tells the daemon which document shape it is reading so the
/// strict deserializer can reject mixed-up files (e.g. a launcher that claims
/// to be a node config). Node files are always named `peppy.json5`; launcher
/// files conventionally use `peppy_launcher.json5` for standalone projects but
/// may use any `.json5` filename when listed in a repository index. Contract,
/// pairing, and MCP exposure files are filename-agnostic and identified solely
/// by their schema tag. `launcher_fragment/v1` tags a fragment file: a piece
/// of a composed launcher (deployments, adjustments, core node links) that a
/// launcher's `components` reference by path and that never launches on its
/// own. `repository/v1` is the odd one out: it tags a repository's
/// `peppy_repository.json5` index, which declares no item of its own and
/// instead states which identities the repository publishes and where each is
/// declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeppySchema {
    #[serde(rename = "node/v1")]
    NodeV1,
    #[serde(rename = "launcher/v1")]
    LauncherV1,
    #[serde(rename = "launcher_fragment/v1")]
    LauncherFragmentV1,
    #[serde(rename = "contract/v1")]
    ContractV1,
    #[serde(rename = "pairing/v1")]
    PairingV1,
    #[serde(rename = "mcp_exposure/v1")]
    McpExposureV1,
    #[serde(rename = "repository/v1")]
    RepositoryV1,
}

impl fmt::Display for PeppySchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PeppySchema::NodeV1 => "node/v1",
            PeppySchema::LauncherV1 => "launcher/v1",
            PeppySchema::LauncherFragmentV1 => "launcher_fragment/v1",
            PeppySchema::ContractV1 => "contract/v1",
            PeppySchema::PairingV1 => "pairing/v1",
            PeppySchema::McpExposureV1 => "mcp_exposure/v1",
            PeppySchema::RepositoryV1 => "repository/v1",
        };
        f.write_str(s)
    }
}

impl PeppySchema {
    /// Deserialize a `peppy_schema` field and reject any value other
    /// than `expected`. Used as the core of the strict per-document-shape
    /// `#[serde(deserialize_with = ...)]` guards, both here and in
    /// daemon-side document parsers (peppy `daemon-config`); public so
    /// every parser shares this one guard and its error text.
    pub fn deserialize_expecting<'de, D>(deserializer: D, expected: Self) -> Result<Self, D::Error>
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

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_SCHEMA: &[PeppySchema] = &[
        PeppySchema::NodeV1,
        PeppySchema::LauncherV1,
        PeppySchema::LauncherFragmentV1,
        PeppySchema::ContractV1,
        PeppySchema::PairingV1,
        PeppySchema::McpExposureV1,
        PeppySchema::RepositoryV1,
    ];

    /// `Display` and the serde rename are written out separately, so this
    /// pins them to each other: a variant added to one and not the other
    /// would make a document's tag and its error text disagree.
    #[test]
    fn display_matches_the_wire_tag_for_every_variant() {
        for schema in EVERY_SCHEMA {
            let wire = serde_json::to_string(schema).expect("serializes");
            assert_eq!(wire, format!("\"{schema}\""));
            let parsed: PeppySchema = serde_json::from_str(&wire).expect("round trips");
            assert_eq!(parsed, *schema);
        }
    }

    #[test]
    fn rejects_an_unknown_schema_tag() {
        assert!(serde_json::from_str::<PeppySchema>("\"repository/v2\"").is_err());
    }
}
