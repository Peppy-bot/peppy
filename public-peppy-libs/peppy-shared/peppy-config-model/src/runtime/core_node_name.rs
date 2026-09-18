use crate::consts::ALLOWED_CONFIG_CHARS;

/// Maximum `core_node_name` length in characters. The name is embedded in
/// every zenoh key expression that addresses a daemon
/// (`service/node/{name}/core/...`), so the DNS-label cap keeps those keys
/// bounded.
pub const MAX_CORE_NODE_NAME_LEN: usize = 63;
/// The literal a command writes for "the daemon this command targets".
/// Reserved: no machine may be named it, so the word always means the target.
pub const SELF_CORE_NODE: &str = "self";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreNodeNameError {
    #[error(
        "`{SELF_CORE_NODE}` is reserved: it means \"the daemon this command targets\", so no \
         core node may be named it. Rename the daemon (its `core_node_name`, or `peppy service \
         serve --core-node-name`) and restart; it re-registers under the new name on its own."
    )]
    Reserved,
    #[error(
        "must be non-empty, at most {MAX_CORE_NODE_NAME_LEN} characters, and use only \
         characters from \"{ALLOWED_CONFIG_CHARS}\""
    )]
    Malformed,
}

/// The rule every constructor applies, over a borrowed name so construction
/// from an owned string costs no allocation.
fn validate(value: &str) -> Result<(), CoreNodeNameError> {
    if value == SELF_CORE_NODE {
        return Err(CoreNodeNameError::Reserved);
    }
    if value.is_empty()
        || value.len() > MAX_CORE_NODE_NAME_LEN
        || !value.chars().all(|c| ALLOWED_CONFIG_CHARS.contains(c))
    {
        return Err(CoreNodeNameError::Malformed);
    }
    Ok(())
}

crate::validated_identity!(
    /// A concrete machine name: valid identifier characters, bounded length, and not `self`.
    CoreNodeName,
    CoreNodeNameError,
    |raw: &str| validate(raw).map(|()| raw.to_owned())
);

impl CoreNodeName {
    /// [`Self::parse`] for a caller that already owns the string.
    pub fn new(value: impl Into<String>) -> Result<Self, CoreNodeNameError> {
        let value = value.into();
        validate(&value)?;
        Ok(Self(value))
    }

    pub fn is_self_keyword(value: &str) -> bool {
        value == SELF_CORE_NODE
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for CoreNodeName {
    type Error = CoreNodeNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CoreNodeName> for String {
    fn from(value: CoreNodeName) -> Self {
        value.0
    }
}

impl AsRef<str> for CoreNodeName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_names_are_checked_at_construction_and_deserialization() {
        for value in ["", "self", "has space", "has/slash", &"n".repeat(64)] {
            assert!(CoreNodeName::new(value).is_err(), "{value}");
            assert!(CoreNodeName::parse(value).is_err(), "{value}");
            assert!(serde_json5::from_str::<CoreNodeName>(&format!("'{value}'")).is_err());
        }
        for value in ["cn-robot-7", "Self", &"n".repeat(63)] {
            let name = CoreNodeName::new(value).unwrap();
            let encoded = serde_json5::to_string(&name).unwrap();
            assert_eq!(
                serde_json5::from_str::<CoreNodeName>(&encoded).unwrap(),
                name
            );
            assert_eq!(CoreNodeName::parse(value).unwrap(), name);
            assert_eq!(name.as_str(), value);
            assert_eq!(name, value);
        }
        assert_eq!(CoreNodeName::new("self"), Err(CoreNodeNameError::Reserved));
        let reserved = CoreNodeNameError::Reserved.to_string();
        for expected in ["reserved", "core_node_name", "restart"] {
            assert!(reserved.contains(expected), "{reserved}");
        }
        assert!(CoreNodeName::is_self_keyword("self"));
        assert!(!CoreNodeName::is_self_keyword("selfish"));
    }
}
