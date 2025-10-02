use serde::{Deserialize, Serialize};

// A flexible value to hold arbitrary JSON5 content (runtime values only)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(untagged)]
pub enum AnyType {
    #[default]
    Null,
    Bool(bool),
    /// A plain string value
    String(String),
    Array(Vec<AnyType>),
    Object(std::collections::BTreeMap<String, AnyType>),

    // Numeric values: prefer signed, then unsigned, then float
    Int(i64),
    UInt(u64),
    Float(f64),
}

// Node parameters with open-ended structure
pub type NodeParameters = std::collections::BTreeMap<String, AnyType>;
