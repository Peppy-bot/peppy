//! The launch checks of daemon scopes: every instance that serves a daemon
//! target gives each one a scope that parses into its interface's type and
//! holds against the launcher, runs on the coordinator, and links none of
//! them; no other instance declares a scope.
//!
//! When a launcher is read, a scope is a free value, because only the
//! resolution of the exposure deployments knows which targets are daemon
//! targets. The launch, each join and `peppy stack resolve` run these checks
//! once that resolution is done and before anything starts. They read the
//! flat launcher and the targets of the exposure documents alone, so `peppy
//! stack resolve` runs them with a cold nodes cache too.

use super::compose::{CopyMembership, PreparedLauncher};
use super::types::Deployment;
use crate::internal::daemon_interface::DaemonInterface;
use crate::internal::mcp_deployment::DeploymentTargets;

/// One reason the daemon scopes of a launch do not hold.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DaemonScopeError {
    #[error(
        "instance `{instance}` declares `daemon_scopes`, but it serves no daemon target; a scope \
         belongs to an instance of an exposure deployment whose target names `daemon: {{ name, \
         tag }}`"
    )]
    ScopesWithoutDaemonTarget { instance: String },
    #[error(
        "`daemon_scopes.{key}` of instance `{instance}` names no daemon target of its \
         exposures{}; its daemon targets: {daemon_targets}",
        contract_hint(.key, *.contract_target)
    )]
    UnknownScopeKey {
        instance: String,
        key: String,
        contract_target: bool,
        daemon_targets: String,
    },
    #[error(
        "daemon target `{daemon_target}` ({interface}) of instance `{instance}` has no scope; \
         give it one in the instance's `daemon_scopes`, or with a `set_daemon_scopes` \
         adjustment on `{instance}` in the launcher"
    )]
    MissingScope {
        instance: String,
        daemon_target: String,
        interface: String,
    },
    #[error(
        "the scope of daemon target `{daemon_target}` ({interface}) on instance `{instance}` does \
         not parse: {reason}"
    )]
    InvalidScope {
        instance: String,
        daemon_target: String,
        interface: String,
        reason: String,
    },
    #[error(
        "the scope of daemon target `{daemon_target}` ({interface}) on instance `{instance}` \
         does not hold against the launcher: {reason}"
    )]
    ScopeRefusedByLauncher {
        instance: String,
        daemon_target: String,
        interface: String,
        reason: String,
    },
    #[error(
        "instance `{instance}` serves daemon target `{daemon_target}` and declares `core_node: \
         \"{core_node}\"`; an instance that serves a daemon target runs on the coordinator, the \
         daemon whose stack it reads and changes, so remove `core_node`"
    )]
    PlacedOffCoordinator {
        instance: String,
        daemon_target: String,
        core_node: String,
    },
    #[error(
        "instance `{instance}` of copy `{copy}` serves daemon target `{daemon_target}`; an \
         instance that serves a daemon target runs on the coordinator and belongs to the stack, \
         so deploy it outside the options of an axis that runs as copies"
    )]
    InCopy {
        instance: String,
        copy: String,
        daemon_target: String,
    },
    #[error(
        "`links.{daemon_target}` of instance `{instance}` names daemon target \
         `{daemon_target}`, which takes no link: the daemon that started the server serves it, \
         and the target takes a scope instead, in `daemon_scopes` or with `set_daemon_scopes`"
    )]
    LinkToDaemonTarget {
        instance: String,
        daemon_target: String,
    },
}

fn contract_hint(key: &str, contract_target: bool) -> String {
    if contract_target {
        format!(" (`{key}` is a contract target, which takes `links`)")
    } else {
        String::new()
    }
}

/// Every reason the daemon scopes of a launch do not hold, at once.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the launcher's daemon scopes do not hold:{}",
    crate::error::format_bulleted(.0)
)]
pub struct DaemonScopeRefusals(pub Vec<DaemonScopeError>);

/// Checks the daemon scopes of the flat launcher whose deployments
/// `deployments` lists, each beside the targets its exposures serve (empty
/// for a node deployment). `prepared` is the launcher the stack was composed
/// from, which a scope's own checks read (the options of the axes that run
/// as copies, for `stack_copies`); `copies` tells which instances belong to
/// a copy. Every refusal is reported at once.
pub fn check_daemon_scopes<'a>(
    prepared: &PreparedLauncher,
    deployments: impl IntoIterator<Item = (&'a Deployment, &'a DeploymentTargets)>,
    copies: &CopyMembership,
) -> Result<(), DaemonScopeRefusals> {
    let mut refusals = Vec::new();
    for (deployment, targets) in deployments {
        for instance in &deployment.instances {
            check_instance(prepared, instance, targets, copies, &mut refusals);
        }
    }
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(DaemonScopeRefusals(refusals))
    }
}

fn check_instance(
    prepared: &PreparedLauncher,
    instance: &super::types::DeploymentInstance,
    targets: &DeploymentTargets,
    copies: &CopyMembership,
    refusals: &mut Vec<DaemonScopeError>,
) {
    let id = instance.instance_id.as_str();
    let Some(first_target) = targets.daemon.keys().next() else {
        if !instance.daemon_scopes.is_empty() {
            refusals.push(DaemonScopeError::ScopesWithoutDaemonTarget {
                instance: id.to_owned(),
            });
        }
        return;
    };

    if let Some(copy) = copies.copy_of(id) {
        refusals.push(DaemonScopeError::InCopy {
            instance: id.to_owned(),
            copy: copy.to_string(),
            daemon_target: first_target.clone(),
        });
    } else if let Some(core_node) = &instance.core_node {
        refusals.push(DaemonScopeError::PlacedOffCoordinator {
            instance: id.to_owned(),
            daemon_target: first_target.clone(),
            core_node: core_node.clone(),
        });
    }

    for daemon_target in instance
        .links
        .keys()
        .filter(|key| targets.daemon.contains_key(key.as_str()))
    {
        refusals.push(DaemonScopeError::LinkToDaemonTarget {
            instance: id.to_owned(),
            daemon_target: daemon_target.clone(),
        });
    }

    for key in instance
        .daemon_scopes
        .keys()
        .filter(|key| !targets.daemon.contains_key(key.as_str()))
    {
        refusals.push(DaemonScopeError::UnknownScopeKey {
            instance: id.to_owned(),
            key: key.clone(),
            contract_target: targets.contract.contains(key),
            daemon_targets: crate::error::format_quoted_list(targets.daemon.keys()),
        });
    }

    for (daemon_target, interface) in &targets.daemon {
        check_scope(
            prepared,
            id,
            daemon_target,
            *interface,
            instance.daemon_scopes.get(daemon_target),
            refusals,
        );
    }
}

/// One daemon target's scope: present, parsed into its interface's type,
/// and held to the interface's checks against the launcher.
fn check_scope(
    prepared: &PreparedLauncher,
    instance: &str,
    daemon_target: &str,
    interface: DaemonInterface,
    scope: Option<&serde_json::Value>,
    refusals: &mut Vec<DaemonScopeError>,
) {
    let Some(scope) = scope else {
        refusals.push(DaemonScopeError::MissingScope {
            instance: instance.to_owned(),
            daemon_target: daemon_target.to_owned(),
            interface: interface.label(),
        });
        return;
    };
    let parsed = match interface.parse_scope(scope) {
        Ok(parsed) => parsed,
        Err(reason) => {
            refusals.push(DaemonScopeError::InvalidScope {
                instance: instance.to_owned(),
                daemon_target: daemon_target.to_owned(),
                interface: interface.label(),
                reason,
            });
            return;
        }
    };
    refusals.extend(parsed.check_launch(prepared).into_iter().map(|reason| {
        DaemonScopeError::ScopeRefusedByLauncher {
            instance: instance.to_owned(),
            daemon_target: daemon_target.to_owned(),
            interface: interface.label(),
            reason,
        }
    }));
}
