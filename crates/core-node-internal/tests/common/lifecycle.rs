#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use super::{
    AbortOnDrop, GoalEnd, StartedCoreNode, TEST_NODE_TAG, TIMEOUTS, WAIT, answer_health_probes,
    answer_ready_probe, default_instance_plan, node_run_goal, poll_until_explained,
    send_node_run_goal_and_wait, settle,
};
use config::node::RestartRequest;
use config::runtime::{Name, NodeInstancePlan};
use core_node_api::InstanceState;
use core_node_api::encoding::NodeRunResult;

/// A probe answerer that stops answering when dropped.
pub type Mock = AbortOnDrop<peppylib::PeppyResult<()>>;

/// The ready and health mocks of `instance_id`, which answer for every
/// process started under the id, settled before the run they answer.
pub async fn mocks(started: &StartedCoreNode, name: &str, instance_id: &str) -> (Mock, Mock) {
    let ready = answer_ready_probe(started, name, instance_id).await;
    let health = answer_health_probes(started, name, instance_id).await;
    settle().await;
    (ready, health)
}

/// Runs the instance `plan` names of `name` with `restart`, returning how
/// the run ended; a completed run carries the pid.
pub async fn run_plan(
    started: &StartedCoreNode,
    name: &str,
    plan: NodeInstancePlan,
    restart: RestartRequest,
) -> Result<GoalEnd<NodeRunResult>, String> {
    run_plan_with_env(started, name, plan, restart, Vec::new()).await
}

/// [`run_plan`] with `env_vars` on the goal.
pub async fn run_plan_with_env(
    started: &StartedCoreNode,
    name: &str,
    plan: NodeInstancePlan,
    restart: RestartRequest,
    env_vars: Vec<(String, String)>,
) -> Result<GoalEnd<NodeRunResult>, String> {
    send_node_run_goal_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        node_run_goal(plan, name, TEST_NODE_TAG, &TIMEOUTS)
            .with_env_vars(env_vars)
            .with_restart(restart),
        &TIMEOUTS,
    )
    .await
    .map(|response| response.end)
}

/// Runs `instance_id` of `name` and returns its pid.
pub async fn run(
    started: &StartedCoreNode,
    name: &str,
    instance_id: &str,
    restart: RestartRequest,
) -> u32 {
    let result = run_plan(started, name, default_instance_plan(instance_id), restart)
        .await
        .expect("node_run completes");
    result
        .completed()
        .pid
        .expect("a started instance has a pid")
}

/// `instance_id` as a [`Name`].
pub fn name(instance_id: &str) -> Name {
    Name::new(instance_id).expect("valid instance id")
}

/// The daemon's stack log, empty before its first line.
pub fn stack_log(started: &StartedCoreNode) -> String {
    std::fs::read_to_string(started.peppy_dirs.stack_log_path()).unwrap_or_default()
}

/// The state of `instance_id`, or `None` for an id the stack does not track.
pub fn state_of(started: &StartedCoreNode, instance_id: &str) -> Option<InstanceState> {
    started.node_stack.instance_state(instance_id)
}

/// Waits until `instance_id` is in `state`, reporting the last state and the
/// stack log when it never gets there.
pub async fn wait_for_state(started: &StartedCoreNode, instance_id: &str, state: InstanceState) {
    poll_until_explained(
        WAIT,
        || {
            format!(
                "`{instance_id}` reaches {state}, last seen {:?}; stack log:\n{}",
                state_of(started, instance_id),
                stack_log(started)
            )
        },
        || (state_of(started, instance_id) == Some(state)).then_some(()),
    )
    .await;
}

/// Waits until no background task of `instance_id` is live: its exit
/// watcher, the wait of its restart and its health monitor have ended, so
/// nothing but a request acts on the instance any more.
pub async fn wait_for_supervision_end(started: &StartedCoreNode, instance_id: &str) {
    let tasks = started.node_stack.instance_tasks();
    tokio::time::timeout(WAIT, tasks.ended(&name(instance_id)))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the supervision of `{instance_id}` ends, {} tasks live; stack log:\n{}",
                tasks.live(&name(instance_id)),
                stack_log(started)
            )
        });
}
