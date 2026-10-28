//! The joinable nodes of a launch, as the coordinator builds them: every
//! node that a join of an option of a `robot_membership` scope can bring (see
//! [`daemon_config::launcher::JoinableNodes`]).
//!
//! The launch resolves them with its own deployments and mints their doc
//! pins, all before it touches a machine, then adds and builds them on the
//! coordinator in its add-and-build phase, after its own nodes there. It
//! starts no instance of them. The launch record keeps their resolutions
//! with its own, so a later join of a scoped option runs the launch's pins,
//! stages the same trees, and reuses the artifacts this launch built.
//!
//! A launch with `--no-prebuild` does none of it.

use super::feedback::{publish_error, publish_stdout};
use super::{PlannedDeployment, resolve};
use crate::services::stack::action::StackChangeContext;
use config::runtime::{CoreNodeName, Name};
use core_node_api::encoding::LaunchFeedbackStep;
use daemon_config::launcher::{JoinableNode, JoinableNodes, Placements};

/// The refusal of a launch whose joinable nodes do not resolve; the list of
/// the nodes and their reasons follows it.
const UNRESOLVED_JOINABLE_NODES: &str = "the nodes that the launch builds for stack joins do \
     not resolve (`--no-prebuild` launches without them, and a join then resolves and builds \
     them)";

/// One joinable node, as the launch adds and builds it on the coordinator.
#[derive(Debug, Clone)]
pub(in crate::services::stack) struct JoinableBuild {
    /// The node's resolution: the launch's own when the launch runs the
    /// node's source, else the one the launch made for the joins.
    pub(in crate::services::stack) planned: PlannedDeployment,
    /// The scoped options whose copies bring the node.
    options: Vec<Name>,
}

impl JoinableBuild {
    /// The joinable node `planned` resolves, which the copies of `options`
    /// bring.
    pub(in crate::services::stack) fn new(planned: PlannedDeployment, options: Vec<Name>) -> Self {
        Self { planned, options }
    }

    /// What the launch builds the node for, as its add line and its
    /// refusals say it: `for stack joins of `openarm_sim`, `so101_sim``.
    pub(in crate::services::stack) fn purpose(&self) -> String {
        purpose(&self.options)
    }
}

fn purpose(options: &[Name]) -> String {
    format!(
        "for stack joins of {}",
        daemon_config::format_quoted_list(options)
    )
}

/// Resolves the joinable nodes of a launch for the coordinator, after the
/// launch's own deployments and their doc pins: a node whose source the
/// launch runs takes the launch's resolution, and every other node is
/// resolved here, with its doc pins. Each refusal names the node, the scoped
/// options that bring it and `--no-prebuild`, and is published as the
/// launch's error.
///
/// The nodes run on the coordinator alone, so no pin of theirs has to be
/// readable from another machine: a join that places one elsewhere sends
/// its pins there, and that machine refuses a pin it cannot read when the
/// join reserves it.
pub(in crate::services::stack) async fn resolve_joinable_nodes(
    ctx: &StackChangeContext,
    joinable: JoinableNodes,
    planned: &[PlannedDeployment],
    coordinator: &CoreNodeName,
) -> Result<Vec<JoinableBuild>, String> {
    let runs_source = |node: &JoinableNode| {
        planned
            .iter()
            .any(|item| item.deployment.source == node.deployment.source)
    };
    let unresolved: Vec<&JoinableNode> =
        joinable.iter().filter(|node| !runs_source(node)).collect();
    let resolved = resolve_for_joins(ctx, &unresolved, coordinator).await?;
    Ok(joinable
        .into_iter()
        .map(|node| {
            let planned = planned
                .iter()
                .chain(&resolved)
                .find(|item| item.deployment.source == node.deployment.source)
                .expect("the launch or this resolution resolved every joinable node")
                .clone();
            JoinableBuild::new(planned, node.options)
        })
        .collect())
}

/// Resolves `nodes`, which the launch does not run, for the coordinator, and
/// mints their doc pins.
async fn resolve_for_joins(
    ctx: &StackChangeContext,
    nodes: &[&JoinableNode],
    coordinator: &CoreNodeName,
) -> Result<Vec<PlannedDeployment>, String> {
    if nodes.is_empty() {
        return Ok(Vec::new());
    }
    publish_stdout(
        ctx,
        format!("Resolving {} node(s) for stack joins", nodes.len()),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;
    // The trial copies name their instances after themselves, which no
    // placement of this launch names, so they run on the coordinator alone.
    let placements = Placements::all_on(coordinator.clone());
    let node_entries = resolve::load_node_index(ctx).await?;
    let mut resolved = Vec::with_capacity(nodes.len());
    let mut problems = Vec::new();
    for node in nodes {
        let why = purpose(&node.options);
        match resolve::resolve_planned(ctx, node.deployment.clone(), &placements, &node_entries)
            .await
        {
            Ok(item) => {
                publish_stdout(
                    ctx,
                    format!("Node {} resolved {why}", item.key().label()),
                    LaunchFeedbackStep::LauncherStep,
                )
                .await;
                resolved.push(item);
            }
            Err(reason) => problems.push(format!("{reason} ({why})")),
        }
    }
    if problems.is_empty() {
        let minted = resolve::mint_doc_pins_of(ctx, &resolved).await?;
        for ((item, node), minted) in resolved.iter_mut().zip(nodes).zip(minted) {
            match minted {
                Ok(doc_pins) => item.closure_pins.extend(doc_pins),
                Err(reason) => problems.push(format!(
                    "deployment {}: {reason} ({})",
                    item.deployment.source.label(),
                    purpose(&node.options)
                )),
            }
        }
    }
    if problems.is_empty() {
        return Ok(resolved);
    }
    let msg = format!(
        "{UNRESOLVED_JOINABLE_NODES}:{}",
        daemon_config::format_bulleted(&problems)
    );
    publish_error(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
    Err(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(value: &str) -> Name {
        Name::new(value).expect("a name")
    }

    /// The add line and every refusal say what a joinable node is built
    /// for: the joins of each scoped option that brings it.
    #[test]
    fn a_joinable_node_is_built_for_the_joins_of_the_options_that_bring_it() {
        assert_eq!(
            purpose(&[name("so101_sim")]),
            "for stack joins of `so101_sim`"
        );
        assert_eq!(
            purpose(&[name("openarm_sim"), name("so101_sim")]),
            "for stack joins of `openarm_sim`, `so101_sim`"
        );
    }
}
