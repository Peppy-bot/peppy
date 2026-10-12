use super::core_node_name::{CoreNodeName, CoreNodeNameError, SELF_CORE_NODE};

/// Where a copy, a part of a copy or a `--place` link runs: on the
/// coordinator, the daemon a launch or a join is sent to, or on one named
/// machine. Its text is `self` or a core node name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CoreNodeTarget {
    Coordinator,
    Named(CoreNodeName),
}

impl CoreNodeTarget {
    /// The target `raw` names: `self` is the coordinator, any other text is
    /// held to a core node name's grammar.
    pub fn parse(raw: &str) -> Result<Self, CoreNodeNameError> {
        if raw == SELF_CORE_NODE {
            return Ok(Self::Coordinator);
        }
        CoreNodeName::new(raw).map(Self::Named)
    }

    /// The concrete machine, given the coordinator's name.
    pub fn resolve(&self, coordinator: &CoreNodeName) -> CoreNodeName {
        match self {
            Self::Coordinator => coordinator.clone(),
            Self::Named(name) => name.clone(),
        }
    }

    /// The machine the target names, or `None` for the coordinator.
    pub fn named(&self) -> Option<&CoreNodeName> {
        match self {
            Self::Coordinator => None,
            Self::Named(name) => Some(name),
        }
    }
}

impl std::str::FromStr for CoreNodeTarget {
    type Err = CoreNodeNameError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl std::fmt::Display for CoreNodeTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Coordinator => f.write_str(SELF_CORE_NODE),
            Self::Named(name) => f.write_str(name.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_the_coordinator_or_a_named_machine() {
        let coordinator = CoreNodeName::new("cn-desk").unwrap();
        let target = CoreNodeTarget::parse("self").unwrap();
        assert_eq!(target, CoreNodeTarget::Coordinator);
        assert_eq!(target.to_string(), "self");
        assert_eq!(target.resolve(&coordinator), coordinator);
        assert_eq!(target.named(), None);

        let gpu_box = CoreNodeName::new("cn-gpu-box").unwrap();
        let target: CoreNodeTarget = "cn-gpu-box".parse().unwrap();
        assert_eq!(target, CoreNodeTarget::Named(gpu_box.clone()));
        assert_eq!(target.to_string(), "cn-gpu-box");
        assert_eq!(target.resolve(&coordinator), gpu_box);
        assert_eq!(target.named(), Some(&gpu_box));

        for malformed in ["", "bad/name", "a@b", &"n".repeat(64)] {
            assert_eq!(
                CoreNodeTarget::parse(malformed),
                Err(CoreNodeNameError::Malformed),
                "{malformed:?}"
            );
        }
    }
}
