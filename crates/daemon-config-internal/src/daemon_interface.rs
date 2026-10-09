//! The daemon interfaces this peppy serves: the registry that the exposure
//! checks, the derived node, the hub check and the server read.
//!
//! A daemon interface is a document in the contract format, compiled into
//! peppy. No node implements it and nothing on the messaging layer answers
//! it: it gives a function of the peppy daemon a public shape for MCP
//! clients, and the server's daemon bridge fills in the rest of the call.
//! peppy serves one tag of each interface. Each entry of the registry holds
//! the interface's document, the type of the scope a launcher gives a target
//! that names it, the narrowing of its published schema by a scope, and the
//! checks a launch runs on a scope.

mod stack_copies;

use crate::internal::contract::{PeppyContract, PeppyContractParser};
use crate::internal::launcher::PreparedLauncher;
use peppy_mcp_catalog::{
    DaemonInterfaceRef, DeclaredMembers, ExposureBundle, McpExposure, ResolvedInterface,
};
use std::sync::LazyLock;

pub use stack_copies::{MAX_DESCRIPTION_CHARS, ScopedOption, StackCopiesScope};

/// One daemon interface this peppy serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DaemonInterface {
    /// `stack_copies`: list the copies of the running launch, add one,
    /// remove one.
    StackCopies,
}

static STACK_COPIES_DOCUMENT: LazyLock<PeppyContract> = LazyLock::new(|| {
    PeppyContractParser::from_content(stack_copies::DOCUMENT)
        .expect("the compiled-in `stack_copies` document parses as a contract")
});

impl DaemonInterface {
    /// Every interface this peppy serves.
    pub const ALL: [Self; 1] = [Self::StackCopies];

    /// The name a target's `daemon` reference names the interface by.
    pub fn name(self) -> &'static str {
        match self {
            Self::StackCopies => "stack_copies",
        }
    }

    /// The one tag of the interface this peppy serves.
    pub fn tag(self) -> &'static str {
        match self {
            Self::StackCopies => "v1",
        }
    }

    /// The interface as messages name it: `name:tag`.
    pub fn label(self) -> String {
        format!("{}:{}", self.name(), self.tag())
    }

    /// The interface's document in the contract format, as compiled in.
    pub fn document_text(self) -> &'static str {
        match self {
            Self::StackCopies => stack_copies::DOCUMENT,
        }
    }

    /// The interface's document, parsed.
    pub fn document(self) -> &'static PeppyContract {
        match self {
            Self::StackCopies => &STACK_COPIES_DOCUMENT,
        }
    }

    /// The interface as the catalog validates an exposure against it: its
    /// identity beside its members.
    pub fn resolved(self) -> ResolvedInterface<'static> {
        let interfaces = &self.document().interfaces;
        ResolvedInterface {
            name: self.name(),
            tag: self.tag(),
            members: DeclaredMembers {
                topics: &interfaces.topics,
                services: &interfaces.services,
                actions: &interfaces.actions,
            },
        }
    }

    /// The interface `reference` names, when this peppy serves it at the
    /// named tag.
    pub fn served(reference: &DaemonInterfaceRef) -> Result<Self, UnservedInterface> {
        let Some(interface) = Self::ALL
            .into_iter()
            .find(|interface| interface.name() == reference.name.as_str())
        else {
            return Err(UnservedInterface::UnknownName {
                interface: reference.to_string(),
                served: served_labels(),
            });
        };
        if interface.tag() != reference.tag {
            return Err(UnservedInterface::UnservedTag {
                interface: reference.to_string(),
                name: interface.name(),
                served: interface.tag(),
            });
        }
        Ok(interface)
    }

    /// Parses a scope a launcher gives a target that names this interface
    /// into the interface's scope type, which holds the scope's rules.
    pub fn parse_scope(self, value: &serde_json::Value) -> Result<DaemonScope, String> {
        match self {
            Self::StackCopies => {
                parse_value::<StackCopiesScope>(value).map(DaemonScope::StackCopies)
            }
        }
    }
}

/// Every interface this peppy serves, as a refusal lists them.
fn served_labels() -> String {
    crate::error::format_quoted_list(
        DaemonInterface::ALL
            .iter()
            .map(|interface| interface.label()),
    )
}

/// Deserializes `value`, naming the field a refusal is about.
fn parse_value<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> Result<T, String> {
    serde_path_to_error::deserialize(value.clone()).map_err(|error| {
        let path = error.path().to_string();
        let inner = error.into_inner();
        if path.is_empty() || path == "." {
            inner.to_string()
        } else {
            format!("{path}: {inner}")
        }
    })
}

/// Why this peppy does not serve the interface a daemon target names.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnservedInterface {
    #[error("daemon interface `{interface}` is not one this peppy serves; it serves {served}")]
    UnknownName { interface: String, served: String },
    #[error(
        "daemon interface `{interface}` is not one this peppy serves; it serves `{name}` at tag \
         `{served}` only, and a document that names a daemon interface ships with the peppy \
         release that serves its tag"
    )]
    UnservedTag {
        interface: String,
        name: &'static str,
        served: &'static str,
    },
}

/// The daemon interface of each daemon target of `exposure`, resolved
/// through the registry, or one refusal per target that names an interface
/// this peppy does not serve.
pub fn served_interfaces(
    exposure: &McpExposure,
) -> Result<Vec<(String, DaemonInterface)>, Vec<String>> {
    let mut served = Vec::new();
    let mut refusals = Vec::new();
    for (target, selection, _) in exposure.surface.targets() {
        let Some(reference) = selection.source.daemon() else {
            continue;
        };
        match DaemonInterface::served(reference) {
            Ok(interface) => served.push((target.clone(), interface)),
            Err(error) => refusals.push(format!("target `{target}`: {error}")),
        }
    }
    if refusals.is_empty() {
        Ok(served)
    } else {
        Err(refusals)
    }
}

/// A scope parsed into its interface's type: what a launcher lets a client
/// of one daemon target do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonScope {
    StackCopies(StackCopiesScope),
}

impl DaemonScope {
    /// The interface this scope belongs to.
    pub fn interface(&self) -> DaemonInterface {
        match self {
            Self::StackCopies(_) => DaemonInterface::StackCopies,
        }
    }

    /// The checks a launch, a join and `peppy stack resolve` run on the
    /// scope against the launcher that gives it: one refusal per problem.
    pub fn check_launch(&self, prepared: &PreparedLauncher) -> Vec<String> {
        match self {
            Self::StackCopies(scope) => scope.check_launch(prepared),
        }
    }

    /// Narrows the published input schemas of the tool and task entries of
    /// `bundle` that daemon target `target` serves to what this scope
    /// allows, so the runtime refuses input outside the scope before a call
    /// reaches the daemon. Entries of other targets keep their schemas.
    pub fn narrow(&self, bundle: &mut ExposureBundle, target: &str) {
        match self {
            Self::StackCopies(scope) => scope.narrow(bundle, target),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
