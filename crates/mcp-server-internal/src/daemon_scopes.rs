//! The scope of each daemon target of the server.
//!
//! A daemon target names an interface of the peppy daemon, which no node
//! implements, and the daemon binds the daemon that serves it to its slot.
//! Each daemon target has its own scope, which the launch gave the
//! instance. The server parses the scopes at start ([`DaemonScopes::parse`])
//! and narrows the published schemas of each target with its scope
//! ([`DaemonScopes::narrow`]), so the runtime refuses input outside the
//! scope before a call reaches the daemon, which checks the scope of each
//! call too. A vacant target is served to no client. A server whose scope
//! is missing or does not parse does not start.

#[cfg(test)]
mod tests;

use crate::serve::ServeError;
use daemon_config::daemon_interface::{DaemonInterface, TargetScope};
use peppy_mcp_catalog::ExposureBundle;
use serde_json::Value;
use std::collections::BTreeMap;

/// One reason the server cannot serve a daemon target with the scopes the
/// launch gave its instance.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
enum ScopeProblem {
    #[error(
        "daemon target `{target}` ({interface}) has no scope; give it one in the `daemon_scopes` \
         of instance `{instance}`, or with a `set_daemon_scopes` adjustment on `{instance}` in \
         the launcher"
    )]
    Missing {
        target: String,
        interface: String,
        instance: String,
    },
    #[error(
        "the scope of daemon target `{target}` ({interface}) does not parse: {reason}; correct it \
         in the `daemon_scopes` of instance `{instance}`, or in the `set_daemon_scopes` \
         adjustment on `{instance}` in the launcher"
    )]
    Invalid {
        target: String,
        interface: String,
        instance: String,
        reason: String,
    },
    #[error(
        "`daemon_scopes.{key}` of instance `{instance}` names no daemon target of this server; \
         its daemon targets: {targets}"
    )]
    UnknownKey {
        key: String,
        instance: String,
        targets: String,
    },
}

/// The scope of each daemon target of the server, parsed into the type of
/// its interface.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DaemonScopes {
    by_target: BTreeMap<String, TargetScope>,
}

impl DaemonScopes {
    /// Parses `scopes`, the scopes the launch gave instance `instance`, for
    /// `targets`, the daemon targets of the server with the interface each
    /// names. Every daemon target takes a scope that parses into its
    /// interface's type, and every scope names a daemon target; each problem
    /// is reported at once. A launch and a join check the scopes before they
    /// start the server, so the server finds a problem only when it starts
    /// some other way, for example with `peppy node run`.
    pub(crate) fn parse(
        instance: &str,
        targets: &BTreeMap<String, DaemonInterface>,
        scopes: &BTreeMap<String, Value>,
    ) -> Result<Self, ServeError> {
        let mut problems = Vec::new();
        let mut by_target = BTreeMap::new();
        for (target, interface) in targets {
            let Some(value) = scopes.get(target) else {
                problems.push(ScopeProblem::Missing {
                    target: target.clone(),
                    interface: interface.label(),
                    instance: instance.to_owned(),
                });
                continue;
            };
            match interface.parse_target_scope(value) {
                Ok(scope) => {
                    by_target.insert(target.clone(), scope);
                }
                Err(reason) => problems.push(ScopeProblem::Invalid {
                    target: target.clone(),
                    interface: interface.label(),
                    instance: instance.to_owned(),
                    reason,
                }),
            }
        }
        for key in scopes.keys().filter(|key| !targets.contains_key(*key)) {
            problems.push(ScopeProblem::UnknownKey {
                key: key.clone(),
                instance: instance.to_owned(),
                targets: daemon_config::format_quoted_list(targets.keys()),
            });
        }
        if !problems.is_empty() {
            return Err(ServeError::DaemonScopes {
                problems: daemon_config::format_bulleted(problems),
            });
        }
        Ok(Self { by_target })
    }

    /// Narrows the published input schemas of every daemon target of
    /// `bundle` to its scope, so the runtime refuses input outside the scope
    /// before a call reaches the daemon. The entries of other targets keep
    /// their schemas.
    pub(crate) fn narrow(&self, bundle: &mut ExposureBundle) {
        for (target, scope) in &self.by_target {
            match scope {
                TargetScope::Scoped(scope) => scope.narrow(bundle, target),
                TargetScope::Vacant(_) => {
                    bundle.tools.retain(|tool| tool.target != *target);
                    bundle.tasks.retain(|task| task.target != *target);
                }
            }
        }
    }
}
