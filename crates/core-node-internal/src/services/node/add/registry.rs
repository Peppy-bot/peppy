//! The add of a node of an oci origin.
//!
//! The node has no sources on this machine: the add does not stage or
//! generate it. It pulls the `.sif` of the platform of this machine, by
//! digest, and registers the node ready to start, with the `peppy.json5` of
//! the artifact and the documents its manifest names. A failed pull fails
//! the add: there is no fall back to a build from sources.

use super::{
    AddStepFailure, NodeAddActionContext, ProcessNodeAddContext, add_documents,
    check_dependencies_present, push_failure_kind, record_add_failure, shutdown_existing_instances,
};
use crate::services::node::pull::{ImageToPull, pull_node_image};
use config::failure_kind::PeppyFailureKind;
use config::node::NodeConfig;
use core_node_api::encoding::{NodeAddGoal, NodeAddResult};
use daemon_config::consts::HOST_ARTIFACT_PLATFORM;
use daemon_config::repository::{EntryOrigin, PinnedItem};
use futures::FutureExt as _;
use node_stack::{Announcer, PulledArtifact};
use peppylib::messaging::Failure;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;

/// Adds the node `pin` names, whose origin is an oci repository. `config` is
/// its parsed `peppy.json5`, stored at `config_path`. `goal` carries the
/// document pins of the batch, and for the root of a batch its environment
/// and `--force`.
pub(crate) async fn run_registry_node_add(
    goal: NodeAddGoal,
    pin: &PinnedItem,
    config: NodeConfig,
    config_path: PathBuf,
    action_context: NodeAddActionContext,
    announcer: Announcer,
) -> Result<NodeAddResult, Failure> {
    let log = announcer.log().clone();
    announcer
        .log()
        .name_node(config.manifest.name.as_str(), &config.manifest.tag);
    let ctx = ProcessNodeAddContext {
        action: action_context,
        announcer,
    };
    let added = AssertUnwindSafe(add_registry_node(goal, pin, config, config_path, &ctx))
        .catch_unwind()
        .await;
    match added {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(failure)) => Err(record_add_failure(
            &ctx.announcer,
            Failure::new(failure.kind, failure.message),
        )),
        Err(panic_payload) => {
            let message = format!(
                "node_add task panicked: {}",
                crate::services::node::panic_message(&*panic_payload)
            );
            tracing::error!("{message}");
            log.error(&message);
            Err(Failure::new(PeppyFailureKind::AttemptFailed, message))
        }
    }
}

async fn add_registry_node(
    goal: NodeAddGoal,
    pin: &PinnedItem,
    config: NodeConfig,
    config_path: PathBuf,
    ctx: &ProcessNodeAddContext,
) -> Result<NodeAddResult, AddStepFailure> {
    super::super::validate_goal_env_vars(&goal.env_vars)
        .map_err(|e| AddStepFailure::new(PeppyFailureKind::Rejected, e.to_string()))?;
    let node_name = config.manifest.name.as_str().to_owned();
    let node_tag = config.manifest.tag.clone();
    let EntryOrigin::Oci { url, platforms, .. } = &pin.origin else {
        return Err(AddStepFailure::new(
            PeppyFailureKind::AttemptFailed,
            format!(
                "internal error: {} reached the add of an oci node with a {} origin",
                pin.label(),
                pin.origin.kind()
            ),
        ));
    };
    let platform = HOST_ARTIFACT_PLATFORM;
    let Some(artifact) = platforms.get(&platform) else {
        return Err(AddStepFailure::new(
            PeppyFailureKind::PreconditionFailed,
            missing_platform_message(&node_name, &node_tag, url, platform, &ctx.action),
        ));
    };
    if config.execution.container.is_none() {
        return Err(AddStepFailure::new(
            PeppyFailureKind::Rejected,
            format!(
                "`{node_name}:{node_tag}` comes from the oci repository {url}, and its \
                 peppy.json5 declares a process node: only a container node runs from an image"
            ),
        ));
    }
    check_dependencies_present(&config, &ctx.action.node_stack)?;
    let documents = add_documents(&goal, &config, &ctx.action.peppy_dirs, &ctx.announcer).await?;

    let sif_path = pull_node_image(
        &ctx.action.peppy_dirs,
        ImageToPull {
            node_name: &node_name,
            node_tag: &node_tag,
            repository: url,
            artifact,
            platform,
        },
        &ctx.announcer,
    )
    .await
    .map_err(|message| AddStepFailure::new(PeppyFailureKind::AttemptFailed, message))?;

    // The same precondition as the add of a sourced node: `push_pulled`
    // refuses to replace a node that still has live instances.
    shutdown_existing_instances(&node_name, &node_tag, ctx)
        .await
        .map_err(|refusal| {
            AddStepFailure::new(
                PeppyFailureKind::PreconditionFailed,
                format!("Failed to shutdown existing node instances: {refusal}"),
            )
        })?;
    ctx.action
        .node_stack
        .push_pulled(
            config,
            &config_path,
            PulledArtifact {
                sif_path,
                repository: url.clone(),
            },
            documents.into_documents(),
        )
        .map_err(|error| {
            AddStepFailure::new(
                push_failure_kind(&error),
                format!("Failed to add node config: {error}"),
            )
        })?;
    ctx.action
        .node_stack
        .set_add_log_path(&node_name, &node_tag, ctx.announcer.log().path());
    Ok(NodeAddResult::new(node_name, node_tag))
}

/// Why a machine cannot run a node of an oci origin that has no artifact for
/// its platform, with the command that adds one.
fn missing_platform_message(
    node_name: &str,
    node_tag: &str,
    repository: &core_node_api::encoding::OciRepository,
    platform: core_node_api::encoding::ArtifactPlatform,
    action: &NodeAddActionContext,
) -> String {
    format!(
        "`{node_name}:{node_tag}` comes from the oci repository {repository}, which has no \
         artifact of it for {platform}, the platform of the core node `{}`. Add one with \
         `peppy node publish {node_name}:{node_tag} --platforms {platform}` on a machine that \
         holds its sources",
        action.bound_core_node
    )
}
