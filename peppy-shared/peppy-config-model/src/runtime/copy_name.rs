use super::Name;
use super::core_node_name::{CoreNodeName, CoreNodeNameError};
use serde::{Deserialize, Serialize};

/// Why a text is not the name of a copy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CopyNameError {
    #[error("`self` names the daemon a launch or join targets; give the copy another name")]
    Reserved,
    #[error("a copy's name is its placement link, so it {0}")]
    Malformed(CoreNodeNameError),
}

/// The name a copy runs under: the prefix of every instance id it mints, and
/// the placement link of the copy's machine. It holds the grammar of a
/// [`CoreNodeName`]: 1 to 63 characters from the allowed set, and not `self`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CopyName(Name);

impl CopyName {
    pub fn new(value: impl Into<String>) -> Result<Self, CopyNameError> {
        let placement = CoreNodeName::new(value).map_err(|error| match error {
            CoreNodeNameError::Reserved => CopyNameError::Reserved,
            CoreNodeNameError::Malformed => CopyNameError::Malformed(error),
        })?;
        Ok(Self(
            Name::new(placement.into_string()).expect("a core node name is a name"),
        ))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The name as the runtime configs and the instance ids carry it.
    pub fn as_name(&self) -> &Name {
        &self.0
    }

    pub fn into_name(self) -> Name {
        self.0
    }
}

impl std::str::FromStr for CopyName {
    type Err = CopyNameError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::new(raw)
    }
}

impl<'de> Deserialize<'de> for CopyName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for CopyName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for CopyName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for CopyName {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for CopyName {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<CopyName> for Name {
    fn from(value: CopyName) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy name holds a core node name's grammar, and each refusal says
    /// why in the words of a copy.
    #[test]
    fn a_copy_name_holds_the_grammar_of_a_core_node_name() {
        for value in ["bravo_2", "Self", &"n".repeat(63)] {
            let name = CopyName::new(value).unwrap();
            assert_eq!(name.as_str(), value);
            assert_eq!(value.parse::<CopyName>().unwrap(), name);
            let encoded = serde_json5::to_string(&name).unwrap();
            assert_eq!(serde_json5::from_str::<CopyName>(&encoded).unwrap(), name);
            assert_eq!(name.as_name().as_str(), value);
        }
        assert_eq!(CopyName::new("self"), Err(CopyNameError::Reserved));
        assert_eq!(
            CopyNameError::Reserved.to_string(),
            "`self` names the daemon a launch or join targets; give the copy another name"
        );
        for malformed in ["", "bad/name", "a@b", &"n".repeat(64)] {
            let refusal = CopyName::new(malformed).unwrap_err();
            assert_eq!(
                refusal,
                CopyNameError::Malformed(CoreNodeNameError::Malformed),
                "{malformed:?}"
            );
            assert!(
                refusal
                    .to_string()
                    .starts_with("a copy's name is its placement link, so it must be non-empty"),
                "{malformed:?}: {refusal}"
            );
            assert!(serde_json5::from_str::<CopyName>(&format!("'{malformed}'")).is_err());
        }
    }
}
