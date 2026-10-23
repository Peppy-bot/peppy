use crate::commands::confirm::TrackedInstances;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::InstanceState;
use core_node_api::encoding::NodeRemoveRequest;
use tracing::info;

use crate::commands::caller_instance_id;
use crate::context::AppContext;
use crate::error::{Error, Result};

use peppylib::core_node::transport::poll;

use super::{daemon_wait, warn_force_killed};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the CLI waits for a `node_remove` to return: the daemon's bound
/// on a removal ([`core_node::node_remove_bound`]) plus a messaging margin,
/// sized from the local daemon's grace as `peppy node stop` sizes its wait.
fn remove_request_timeout(shutdown_grace_secs: u64) -> Duration {
    daemon_wait(core_node::node_remove_bound(Duration::from_secs(
        shutdown_grace_secs,
    )))
}

pub fn remove_node(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    stop_instances: bool,
    force: bool,
) -> Result<()> {
    crate::commands::block_on(remove_node_async(
        ctx,
        node_name,
        tag,
        stop_instances,
        force,
    ))
}

async fn remove_node_async(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    stop_instances: bool,
    force: bool,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let mut stop_instances = stop_instances;
    if force {
        stop_instances = true;
    }
    if !stop_instances {
        let instances =
            super::fetch_tracked_instances(&conn, &node_name, &tag, REQUEST_TIMEOUT).await?;

        if !instances.is_empty() {
            let confirm = confirm_removal(&node_name, &tag, &instances)?;
            if !confirm {
                return Err(Error::ExecutionFailed(
                    "Node removal aborted by user".to_string(),
                ));
            }
            stop_instances = true;
        }
    }

    info!(
        "Calling node_remove for '{}:{}' on daemon '{}' (stop_instances={})...",
        node_name, tag, conn.target_core_node, stop_instances
    );

    let remove_request =
        NodeRemoveRequest::new(&node_name, &tag).with_stop_instances(stop_instances);
    let remove_response = poll(
        &remove_request,
        conn.messenger,
        &conn.core_node_name,
        caller_instance_id(),
        &conn.target_core_node,
        remove_request_timeout(conn.shutdown_grace_secs),
    )
    .await
    .map_err(|e| crate::commands::request_error("Failed to remove the node", e))?;

    for instance_id in &remove_response.force_killed {
        warn_force_killed(instance_id);
    }
    info!("Removed node '{}:{}'", node_name, tag);
    Ok(())
}

fn confirm_removal(
    node_name: &str,
    tag: &str,
    instances: &[(String, InstanceState)],
) -> Result<bool> {
    crate::commands::confirm::confirm_prompt(&removal_question(node_name, tag, instances), None)
}

/// The question a removal of `node_name:tag` asks while it tracks
/// `instances`, naming each with its state and when a start in flight keeps
/// the node.
fn removal_question(node_name: &str, tag: &str, instances: &[(String, InstanceState)]) -> String {
    let tracked = TrackedInstances(instances);
    let starts_keep = if tracked.may_start() {
        ", but an instance starting when the removal reaches it, or running a process that \
         reached `running` after the removal began, keeps the node"
    } else {
        ""
    };
    format!(
        "Are you sure you want to remove `{node_name}:{tag}`? {tracked} {} still tracked; \
         removing stops and clears {}{starts_keep}. [y/n] ",
        tracked.verb(),
        tracked.pronoun()
    )
}

#[cfg(test)]
mod tests {
    use super::super::DAEMON_MESSAGING_MARGIN;
    use super::*;

    /// The CLI outlasts the daemon's bound on a removal, its shutdown
    /// requests and then the stop of what they asked to exit, by the
    /// messaging margin.
    #[test]
    fn the_cli_outlasts_the_daemons_removal() {
        for grace_secs in 1..=600 {
            let daemon = core_node::node_remove_bound(Duration::from_secs(grace_secs));
            assert_eq!(
                remove_request_timeout(grace_secs),
                daemon + DAEMON_MESSAGING_MARGIN
            );
        }
    }

    /// The question names every tracked instance with its state, so a node
    /// whose instances wait for a restart or have exited asks too, and, when
    /// one has a process or waits for one, says which start keeps the node.
    #[test]
    fn the_question_names_each_tracked_instance_and_its_state() {
        let question = |instances: &[(&str, InstanceState)]| {
            let instances: Vec<(String, InstanceState)> = instances
                .iter()
                .map(|(id, state)| ((*id).to_owned(), *state))
                .collect();
            removal_question("cam", "v1", &instances)
        };
        assert_eq!(
            question(&[
                ("cam_1", InstanceState::Finished),
                ("cam_2", InstanceState::Failed)
            ]),
            "Are you sure you want to remove `cam:v1`? 2 instances (\"cam_1\" finished, \"cam_2\" \
             failed) are still tracked; removing stops and clears them. [y/n] "
        );
        assert_eq!(
            question(&[("cam_1", InstanceState::Finished)]),
            "Are you sure you want to remove `cam:v1`? 1 instance (\"cam_1\" finished) is still \
             tracked; removing stops and clears it. [y/n] "
        );
        let starts_keep = ", but an instance starting when the removal reaches it, or running a \
                           process that reached `running` after the removal began, keeps the node";
        assert_eq!(
            question(&[("cam_1", InstanceState::Restarting)]),
            format!(
                "Are you sure you want to remove `cam:v1`? 1 instance (\"cam_1\" restarting) is \
                 still tracked; removing stops and clears it{starts_keep}. [y/n] "
            )
        );
        assert_eq!(
            question(&[("cam_1", InstanceState::Running)]),
            format!(
                "Are you sure you want to remove `cam:v1`? 1 instance (\"cam_1\" running) is \
                 still tracked; removing stops and clears it{starts_keep}. [y/n] "
            )
        );
        assert_eq!(
            question(&[
                ("cam_1", InstanceState::Running),
                ("cam_2", InstanceState::Restarting)
            ]),
            format!(
                "Are you sure you want to remove `cam:v1`? 2 instances (\"cam_1\" running, \
                 \"cam_2\" restarting) are still tracked; removing stops and clears them\
                 {starts_keep}. [y/n] "
            )
        );
        assert_eq!(
            question(&[("cam_1", InstanceState::Starting)]),
            format!(
                "Are you sure you want to remove `cam:v1`? 1 instance (\"cam_1\" starting) is \
                 still tracked; removing stops and clears it{starts_keep}. [y/n] "
            )
        );
    }
}
