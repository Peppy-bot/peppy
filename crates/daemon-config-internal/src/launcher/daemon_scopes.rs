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
//!
//! The launch and `peppy stack resolve` also hold each scope against the
//! launcher and the stack selection ([`ScopeCheck::Launch`]), which gives the
//! joinable nodes of the launch. A join runs the rules of each instance alone
//! ([`ScopeCheck::Join`]): the launcher and the stack selection of a launch
//! do not change, so a join cannot change that verdict.

use super::compose::{CopyMembership, PreparedLauncher, UnitSelection};
use super::joinable::JoinableNodes;
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

/// Which checks a run holds each scope to.
#[derive(Debug, Clone, Copy)]
pub enum ScopeCheck<'a> {
    /// A launch, or `peppy stack resolve`, of the stack this selection
    /// resolves: the rules of each instance, and each scope against the
    /// launcher and the stack.
    Launch(&'a UnitSelection),
    /// A join: the rules of each instance alone.
    Join,
}

/// What the checks of the daemon scopes of one run gather: every refusal,
/// and the joinable nodes of the scopes that hold.
#[derive(Default)]
struct ScopeVerdicts {
    refusals: Vec<DaemonScopeError>,
    joinable: JoinableNodes,
}

/// Checks the daemon scopes of the flat launcher whose deployments
/// `deployments` lists, each beside the targets its exposures serve (empty
/// for a node deployment). `prepared` is the launcher the stack was composed
/// from, which a scope's own checks read (the options of the axes that run
/// as copies and the copies they compose, for `stack_copies`); `copies`
/// tells which instances belong to a copy. Every refusal is reported at
/// once.
///
/// When the scopes hold, returns the joinable nodes of the launch: the union
/// of the nodes that a join each scope allows can bring. A join runs no check
/// against the launcher, so it gets none.
pub fn check_daemon_scopes<'a>(
    prepared: &PreparedLauncher,
    deployments: impl IntoIterator<Item = (&'a Deployment, &'a DeploymentTargets)>,
    copies: &CopyMembership,
    check: ScopeCheck<'_>,
) -> Result<JoinableNodes, DaemonScopeRefusals> {
    let mut verdicts = ScopeVerdicts::default();
    for (deployment, targets) in deployments {
        for instance in &deployment.instances {
            check_instance(prepared, instance, targets, copies, check, &mut verdicts);
        }
    }
    if verdicts.refusals.is_empty() {
        Ok(verdicts.joinable)
    } else {
        Err(DaemonScopeRefusals(verdicts.refusals))
    }
}

fn check_instance(
    prepared: &PreparedLauncher,
    instance: &super::types::DeploymentInstance,
    targets: &DeploymentTargets,
    copies: &CopyMembership,
    check: ScopeCheck<'_>,
    verdicts: &mut ScopeVerdicts,
) {
    let refusals = &mut verdicts.refusals;
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
            check,
            verdicts,
        );
    }
}

/// One daemon target's scope: present, parsed into its interface's type,
/// and, at a launch, held to the interface's checks against the launcher,
/// whose joinable nodes join those of the other scopes.
fn check_scope(
    prepared: &PreparedLauncher,
    instance: &str,
    daemon_target: &str,
    interface: DaemonInterface,
    scope: Option<&serde_json::Value>,
    check: ScopeCheck<'_>,
    verdicts: &mut ScopeVerdicts,
) {
    let refusals = &mut verdicts.refusals;
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
    let ScopeCheck::Launch(selection) = check else {
        return;
    };
    match parsed.check_launch(prepared, selection) {
        Ok(joinable) => verdicts.joinable.merge(joinable),
        Err(reasons) => refusals.extend(reasons.into_iter().map(|reason| {
            DaemonScopeError::ScopeRefusedByLauncher {
                instance: instance.to_owned(),
                daemon_target: daemon_target.to_owned(),
                interface: interface.label(),
                reason,
            }
        })),
    }
}
