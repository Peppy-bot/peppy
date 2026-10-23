//! The restarts of an instance whose policy starts it again: the respawn
//! under the same id, the backoff and its cap, the stop paths during the
//! wait, the admission behind a stack change, the health kill, the pairs,
//! observations and watchers a restart takes again with the peer daemons it
//! asks, and the stops and removals of instances beside a restart.

mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, GoalEnd, Mock, StartedCoreNode, TEST_NODE_TAG, TIMEOUTS, WAIT,
    always, assert_failure, fast_health_monitor, fast_health_monitor_with_kill, mocks, name, never,
    node_service_mock, on_failure, peer_daemon_service, poll_until, poll_until_explained, run,
    run_plan, run_plan_with_env, send_node_add_and_wait_with_force, send_node_add_then_build,
    settle, stack_log, start_core_node_with_supervision,
    start_core_node_with_supervision_and_grace, state_of, wait_for_state, write_peppy_json5,
};
use config::failure_kind::PeppyFailureKind;
use config::node::{Cardinality, RestartDeclaration, RestartOrigin, RestartPolicy, RestartRequest};
use config::runtime::{
    ClockBinding, ClockDomainId, ClockIncarnation, CoreNodeName, Name, NodeInstancePlan,
    ProducerRef,
};
use core_node::test_support::{
    EXIT_CLEANLY_ON_TERM, RESET_WINDOW, dump_runtime_config, framework_entry,
    instant_restart_backoff, parked_restart_backoff, poll_file, resetting_restart_backoff,
    signal_process,
};
use core_node::{HealthMonitorPolicy, ReserveOutcome, RestartBackoff};
use core_node_api::encoding::{
    FederationAck, NodeAddResult, NodeInfoRequest, NodeRemoveRequest, NodeRemoveResponse,
    NodeStopRequest, NodeStopResponse, StackResetRequest,
};
use core_node_api::{ExitCause, InstanceState, SerializedInstance};
use node_stack::action_log::restart_separator;
use node_stack::action_log::test_support::{lines_without_time, nth_newest_run_log, time_of};
use peppylib::PeppyError;
use peppylib::core_node::transport::{poll, poll_node_stop};
use peppylib::messaging::{Failure, MessengerHandle, ServiceAnswer};
use peppylib::services::shutdown::listen_for_shutdown;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// How long a test watches for something that must not happen.
const QUIET: Duration = Duration::from_millis(1500);
/// The messaging headroom a test waits beyond what the daemon takes for a
/// request.
const REQUEST_MARGIN: Duration = Duration::from_secs(5);
/// The misses in a row that turn an instance unhealthy under the test
/// monitor.
const UNHEALTHY_AFTER: u32 = 3;
/// One miss cycle of the test monitor: its 200 ms interval and 100 ms
/// timeout.
const MISS_CYCLE: Duration = Duration::from_millis(300);
/// The seconds without an answered health check the killing test monitor
/// kills after, as its lines print them.
const KILL_AFTER_SECS: u64 = 2;
/// [`KILL_AFTER_SECS`] as the monitor takes it. An instance turns unhealthy
/// about 900 ms into its silence ([`UNHEALTHY_AFTER`] of [`MISS_CYCLE`])
/// and is killed at the first miss past two seconds, 2.1 s in and 1.2 s
/// after the edge; two monitors silenced together reach the kill within two
/// cycles of each other.
const KILL_AFTER: Duration = Duration::from_secs(KILL_AFTER_SECS);
/// How long a test watches past the unhealthy edge for a kill that must not
/// come: [`KILL_AFTER`] and two cycles of the test monitor.
const PAST_THE_KILL: Duration = Duration::from_millis(2600);
/// A process that runs until it is killed.
const SLEEPER: &str = r#"["sleep", "300"]"#;
/// A node whose output a process outside its group holds open, so a
/// cancelled start's abort waits out its readers' bound before its slot
/// leaves.
const OUTPUT_HELD_OUTSIDE_THE_GROUP: &str = r#"["sh", "-c", "setsid sleep 20 & exec sleep 300"]"#;
/// A node whose process ignores SIGTERM.
const IGNORES_TERM: &str = r#"["sh", "-c", "trap '' TERM; while true; do sleep 0.2; done"]"#;
/// The shortest grace the daemon takes (`lifecycle.shutdown_grace_secs` is
/// at least 1), which the tests that wait out a force-kill deadline run under
/// to keep it short.
const SHORT_GRACE_SECS: u64 = 1;
const PUBLISHER: &str = "sim_clock";
const PUBLISHER_ID: &str = "sim_clock_inst";
const READER: &str = "clocked";
const READER_ID: &str = "clocked_inst";
/// How long the slow instance of a removal takes to answer its shutdown
/// request: inside the removal's 5 s shutdown poll, and past
/// [`HELD_RESTART_DELAY`], so a restart due falls inside the removal.
const SLOW_SHUTDOWN_ANSWER: Duration = Duration::from_secs(2);
/// The wait of a restart a removal holds off: the removal starts well
/// within it and lasts past it.
const HELD_RESTART_DELAY: Duration = Duration::from_secs(1);
/// The launch this daemon takes part in for the watcher tests.
const LAUNCH: &str = "watched-launch";
/// The daemon that watches the instances of the watcher tests.
const WATCHER: &str = "cloud";
/// The slot the nodes of the pairing tests play `joint_link`'s follower on.
const FOLLOWER_SLOT: &str = "controller";
/// The slot [`LEADER`] pairs on, and a test leader node's.
const LEADER_SLOT: &str = "arm";
/// The instance on [`WATCHER`]'s daemon the pairing tests pair with.
const LEADER: &str = "leader_inst";

/// The budget of a reset, a participant request, a pair commit or a
/// lifecycle notice: a reset's own bound at the default grace, which outlasts
/// the others these tests send, plus [`REQUEST_MARGIN`].
fn request_budget() -> Duration {
    core_node::stack_reset_timeout(config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS)
        + REQUEST_MARGIN
}

/// How a federation request ends for its sender: acknowledged, or refused
/// with the failure of its error reply.
type Verdict = Result<FederationAck, Failure>;

/// The verdict of a request its receiver refused: `precondition_failed`.
fn refused(reason: impl Into<String>) -> Verdict {
    Err(Failure::new(PeppyFailureKind::PreconditionFailed, reason))
}

/// The verdict of a pair its receiver could not pin: `resource_unavailable`.
fn undelivered(reason: impl Into<String>) -> Verdict {
    Err(Failure::new(PeppyFailureKind::ResourceUnavailable, reason))
}

/// The message of `verdict` when it is a failure of `kind`, else `None`.
fn failure_message_of(verdict: &Verdict, kind: PeppyFailureKind) -> Option<&str> {
    match verdict {
        Err(failure) if failure.kind().as_str() == kind.name() => Some(failure.message()),
        _ => None,
    }
}

/// The answer of a peer daemon's mock that gives `verdict`.
fn answer_with(verdict: &Verdict) -> ServiceAnswer {
    verdict
        .clone()
        .map(|ack| ack.encode().expect("an acknowledgement encodes"))
}

/// The failure of the error reply that `error` carries. Panics on an error
/// without a reply: the `request` did not complete.
#[track_caller]
fn error_reply(error: PeppyError, request: &str) -> Failure {
    error
        .failure()
        .cloned()
        .unwrap_or_else(|| panic!("{request} did not complete: {error}"))
}

/// A process that exits cleanly on SIGTERM.
fn clean_on_term() -> String {
    format!(r#"["sh", "-c", "{EXIT_CLEANLY_ON_TERM}"]"#)
}

/// A daemon with the case's backoff and the test health monitor, which
/// probes every 200 ms and turns an instance unhealthy at the
/// [`UNHEALTHY_AFTER`]th miss.
async fn daemon(backoff: RestartBackoff) -> StartedCoreNode {
    start_core_node_with_supervision(fast_health_monitor(UNHEALTHY_AFTER), backoff).await
}

/// The test monitor that kills an instance unanswered for [`KILL_AFTER`].
fn kill_monitor() -> HealthMonitorPolicy {
    fast_health_monitor_with_kill(UNHEALTHY_AFTER, KILL_AFTER)
}

/// The `failed` line of an instance the test monitor killed.
fn health_kill_line() -> String {
    format!("failed: killed after {KILL_AFTER_SECS} s without an answered health check")
}

/// The line the test monitor writes as it kills an instance.
fn kill_line() -> String {
    format!("answered no health check for {KILL_AFTER_SECS} s; killed")
}

/// The line the monitor writes as an instance turns unhealthy.
fn unhealthy_line() -> String {
    format!("became unhealthy: {UNHEALTHY_AFTER} consecutive")
}

/// Adds and builds the process node `name` running `run_cmd`, with
/// `framework` as its manifest's block when given.
async fn add_node(
    started: &StartedCoreNode,
    name: &str,
    run_cmd: &str,
    framework: Option<&str>,
) -> TempDir {
    let framework = framework_entry(framework);
    add_node_with_manifest(
        started,
        &format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{ name: "{name}", tag: "{TEST_NODE_TAG}" }},
                execution: {{ language: "rust", run_cmd: {run_cmd} }},
                {framework}
            }}"#
        ),
    )
    .await
}

/// Adds and builds the process node `peppy_json5` declares.
async fn add_node_with_manifest(started: &StartedCoreNode, peppy_json5: &str) -> TempDir {
    let dir = TempDir::new().expect("temp node dir");
    write_peppy_json5(dir.path(), peppy_json5);
    add_and_build(started, dir.path()).await;
    dir
}

/// Adds and builds the node whose manifest is in `dir`.
async fn add_and_build(started: &StartedCoreNode, dir: &std::path::Path) {
    send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        dir,
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("node_add and node_build complete");
}

/// The `run_cmd` of a node whose process runs the shell `body`.
fn shell(body: &str) -> String {
    serde_json::to_string(&["sh", "-c", body]).expect("a run command serializes")
}

fn capped(max_restarts: u32) -> RestartRequest {
    RestartRequest::Flag(
        RestartDeclaration::new(RestartPolicy::OnFailure, NonZeroU32::new(max_restarts))
            .expect("a cap on a restarting policy"),
    )
}

/// The slot of `instance_id`, in any state, or `None` for an id the stack
/// does not track.
fn slot_of(
    started: &StartedCoreNode,
    instance_id: &str,
) -> Option<node_stack::TrackedNodeInstance> {
    started
        .node_stack
        .find_instance_and_entity_any_state(&name(instance_id))
        .map(|(slot, _)| slot)
}

/// The run log of `instance_id`'s current process, the newest of its files.
fn run_log(started: &StartedCoreNode, instance_id: &str) -> String {
    nth_run_log(started, instance_id, 0)
}

/// The run log of the process before `instance_id`'s current one.
fn previous_run_log(started: &StartedCoreNode, instance_id: &str) -> String {
    nth_run_log(started, instance_id, 1)
}

fn nth_run_log(started: &StartedCoreNode, instance_id: &str, n: usize) -> String {
    std::fs::read_to_string(nth_run_log_path(started, instance_id, n))
        .expect("the run log is readable")
}

/// Every run log of `instance_id`, the newest first.
fn every_run_log(started: &StartedCoreNode, instance_id: &str) -> Vec<String> {
    (0..)
        .map_while(|n| nth_newest_run_log(&started.peppy_dirs.logs_dir_run(), instance_id, n))
        .map(|path| std::fs::read_to_string(path).expect("the run log is readable"))
        .collect()
}

/// The path of the run log `n` back from `instance_id`'s current one.
fn nth_run_log_path(started: &StartedCoreNode, instance_id: &str, n: usize) -> PathBuf {
    nth_newest_run_log(&started.peppy_dirs.logs_dir_run(), instance_id, n)
        .unwrap_or_else(|| panic!("`{instance_id}` has a run log {n} back"))
}

/// The run logs `peppy node info` lists for the node `name`.
async fn run_log_paths(started: &StartedCoreNode, name: &str) -> Vec<PathBuf> {
    common::poll_node_info(started, &NodeInfoRequest::new(name, TEST_NODE_TAG), WAIT)
        .await
        .run_log_paths
}

/// The instance as `stack list` reports it.
fn serialized(started: &StartedCoreNode, instance_id: &str) -> SerializedInstance {
    started
        .node_stack
        .to_serialized_graph()
        .nodes
        .into_iter()
        .flat_map(|node| node.instances)
        .find(|instance| instance.instance_id == instance_id)
        .unwrap_or_else(|| panic!("`{instance_id}` is in the stack"))
}

/// Waits until `instance_id` is in `state` with a process and returns its
/// pid: a `starting` instance has one once it forked.
async fn wait_for_process(
    started: &StartedCoreNode,
    instance_id: &str,
    state: InstanceState,
) -> u32 {
    poll_until_explained(
        WAIT,
        || {
            format!(
                "`{instance_id}` is {state} with a process; stack log:\n{}",
                stack_log(started)
            )
        },
        || {
            let slot = slot_of(started, instance_id)?;
            slot.pid().filter(|_| slot.state() == state)
        },
    )
    .await
}

/// Waits until the process group `pid` led is empty.
async fn wait_for_group_gone(pid: u32, what: &str) {
    poll_until(
        WAIT,
        &format!("{what}: the group of {pid} is empty"),
        || (!node_stack::process_group_exists(pid)).then_some(()),
    )
    .await;
}

/// Waits until the instance has restarted `count` times and is `running`.
async fn wait_for_restarts(started: &StartedCoreNode, instance_id: &str, count: u32) {
    wait_for_restarts_within(started, instance_id, count, WAIT).await;
}

/// [`wait_for_restarts`] for up to `limit`.
async fn wait_for_restarts_within(
    started: &StartedCoreNode,
    instance_id: &str,
    count: u32,
    limit: Duration,
) {
    poll_until_explained(
        limit,
        || {
            format!(
                "`{instance_id}` runs again with {count} restarts; stack log:\n{}",
                stack_log(started)
            )
        },
        || {
            let instance = serialized(started, instance_id);
            (instance.state == InstanceState::Running
                && instance.restart_status.restart_count == count)
                .then_some(())
        },
    )
    .await;
}

async fn wait_for_line(started: &StartedCoreNode, needle: &str) {
    poll_until_explained(
        WAIT,
        || {
            format!(
                "the stack log holds `{needle}`; stack log:\n{}",
                stack_log(started)
            )
        },
        || stack_log(started).contains(needle).then_some(()),
    )
    .await;
}

/// When the last stack log line of `instance_id` holding `text` was
/// written.
fn logged_at(started: &StartedCoreNode, instance_id: &str, text: &str) -> chrono::NaiveDateTime {
    let log = stack_log(started);
    log.lines()
        .rev()
        .find(|line| line.contains(&format!("'{instance_id}'")) && line.contains(text))
        .and_then(time_of)
        .unwrap_or_else(|| panic!("a timed line `{text}` of `{instance_id}`; stack log:\n{log}"))
}

/// The lines of the stack log that name `instance_id`, without their time.
fn lines_for(started: &StartedCoreNode, instance_id: &str) -> Vec<String> {
    lines_without_time(&stack_log(started))
        .into_iter()
        .filter(|line| line.contains(&format!("'{instance_id}'")))
        .collect()
}

/// Whether a stack log line of `instance_id` holds `needle`.
fn logged(started: &StartedCoreNode, instance_id: &str, needle: &str) -> bool {
    lines_for(started, instance_id)
        .iter()
        .any(|line| line.contains(needle))
}

/// `peppy node remove` of `node`, clearing its instances when
/// `stop_instances`: the response, or the failure of the error reply.
async fn remove(
    started: &StartedCoreNode,
    node: &str,
    stop_instances: bool,
) -> Result<NodeRemoveResponse, Failure> {
    poll(
        &NodeRemoveRequest::new(node, TEST_NODE_TAG).with_stop_instances(stop_instances),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        core_node::node_remove_bound(started.node_stack.shutdown_grace()) + REQUEST_MARGIN,
    )
    .await
    .map_err(|error| error_reply(error, "node_remove"))
}

/// [`remove`] of `node`, which must succeed.
async fn remove_ok(
    started: &StartedCoreNode,
    node: &str,
    stop_instances: bool,
) -> NodeRemoveResponse {
    remove(started, node, stop_instances)
        .await
        .unwrap_or_else(|failure| panic!("node_remove failed: {failure}"))
}

/// `peppy node add --force` of the node in `dir`, which overwrites it.
async fn overwrite(started: &StartedCoreNode, dir: &std::path::Path) -> GoalEnd<NodeAddResult> {
    send_node_add_and_wait_with_force(
        &started.caller_handle,
        &started.core_node_name,
        dir,
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add completes")
}

/// `peppy node stop` of `instance_id`: the response, or the failure of the
/// error reply.
async fn stop(started: &StartedCoreNode, instance_id: &str) -> Result<NodeStopResponse, Failure> {
    poll_node_stop(
        &NodeStopRequest::new(instance_id),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        &started.core_node_name,
        core_node::node_stop_bound(started.node_stack.shutdown_grace()) + REQUEST_MARGIN,
    )
    .await
    .map_err(|error| error_reply(error, "node_stop"))
}

/// [`stop`] of `instance_id`, which must succeed.
async fn stop_ok(started: &StartedCoreNode, instance_id: &str) -> NodeStopResponse {
    stop(started, instance_id)
        .await
        .unwrap_or_else(|failure| panic!("node_stop failed: {failure}"))
}

/// Resets the stack and waits for the reset to succeed.
async fn reset(started: &StartedCoreNode) {
    poll(
        &StackResetRequest::new(),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        request_budget(),
    )
    .await
    .unwrap_or_else(|error| panic!("stack_reset failed: {error}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_under_on_failure_respawns_under_the_same_id() {
    const NODE: &str = "crasher";
    const ID: &str = "crasher_inst";
    let started = daemon(instant_restart_backoff()).await;
    // Each process copies its runtime config and its environment probe under
    // its pid.
    let dumps = TempDir::new().expect("dumps dir");
    let run_cmd = shell(&format!(
        "{}; echo \"$RESTART_PROBE\" > '{dir}/'$$.env; exec sleep 300",
        dump_runtime_config(dumps.path()),
        dir = dumps.path().display()
    ));
    let _dir = add_node_with_manifest(
        &started,
        &format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{ name: "{NODE}", tag: "{TEST_NODE_TAG}" }},
                execution: {{
                    language: "rust",
                    parameters: {{ speed: {{ $type: "u32", $default: 1 }} }},
                    run_cmd: {run_cmd},
                }},
            }}"#
        ),
    )
    .await;
    let (_ready, health) = mocks(&started, NODE, ID).await;
    let plan = common::instance_plan(
        ID,
        std::collections::BTreeMap::from([("speed".to_owned(), config::AnyType::UInt(3))]),
    );
    let result = run_plan_with_env(
        &started,
        NODE,
        plan,
        on_failure(),
        vec![("RESTART_PROBE".to_owned(), "steady".to_owned())],
    )
    .await
    .expect("node_run completes");
    let first_pid = result.completed().pid.expect("a pid");
    let first_slot = slot_of(&started, ID).expect("tracked");
    let first_config = first_slot
        .runtime_config_path()
        .expect("a running instance has a runtime config")
        .to_path_buf();

    // The first process dies unhealthy, so only the flip gives the next one
    // a healthy flag until its own monitor's first probe writes one.
    drop(health);
    wait_for_line(
        &started,
        &format!("'{ID}' of node '{NODE}:{TEST_NODE_TAG}' became unhealthy"),
    )
    .await;
    signal_process(first_pid, "-9");
    let second_health = common::answer_health_probes(&started, NODE, ID).await;
    wait_for_restarts(&started, ID, 1).await;

    let instance = serialized(&started, ID);
    assert_eq!(
        instance.restart_status.restart.policy(),
        RestartPolicy::OnFailure
    );
    assert_eq!(instance.restart_status.restart_origin, RestartOrigin::Flag);
    assert_eq!(instance.restart_status.attempt, 1);
    assert_eq!(
        instance.restart_status.last_exit,
        Some(ExitCause::signal(9, false))
    );
    assert_eq!(instance.restart_status.next_start_ms, None);
    let second_slot = slot_of(&started, ID).expect("tracked");
    let second_pid = second_slot.pid().expect("the new process has a pid");
    assert_ne!(second_pid, first_pid, "a new process");
    assert!(second_slot.healthy(), "a fresh health flag");
    assert_ne!(
        second_slot.runtime_config_path(),
        Some(first_config.as_path()),
        "a new runtime config file"
    );
    assert!(
        !first_config.exists(),
        "the first process's config is removed"
    );

    let lines = lines_for(&started, ID);
    let expected = [
        "failed: signal 9 (SIGKILL)",
        "restarting in 0 s (attempt 1)",
        "restarted (attempt 1)",
    ];
    for needle in expected {
        assert!(
            lines.iter().any(|line| line.contains(needle)),
            "`{needle}` in {lines:?}"
        );
    }
    let positions: Vec<usize> = expected
        .iter()
        .map(|needle| lines.iter().position(|line| line.contains(needle)).unwrap())
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{lines:?}");

    let (first_log, second_log) = (previous_run_log(&started, ID), run_log(&started, ID));
    let opening = format!(
        "{}; the previous log is {}",
        restart_separator(1),
        nth_run_log_path(&started, ID, 1).display()
    );
    assert!(
        second_log
            .lines()
            .next()
            .is_some_and(|line| line.ends_with(&opening)),
        "the restart's run log opens with its attempt and the first process's file:\n{second_log}"
    );
    assert_eq!(
        run_log_paths(&started, NODE).await,
        [nth_run_log_path(&started, ID, 0)],
        "`node info` names the current process's run log"
    );
    let command_line = |log: &str| {
        log.lines()
            .find(|line| line.contains("Executing"))
            .map(|line| line[line.find("Executing").unwrap()..].to_owned())
    };
    let (first, second) = (command_line(&first_log), command_line(&second_log));
    assert!(
        first.is_some() && first == second,
        "both processes run the same command:\n{first_log}\n{second_log}"
    );
    // Both processes start from the same runtime config, arguments, slots
    // and clock alike, and the same environment.
    let dump_path = |pid: u32, extension: &str| dumps.path().join(format!("{pid}.{extension}"));
    let config_of = |pid: u32| -> serde_json::Value {
        poll_file(&dump_path(pid, "json5"), |text| {
            serde_json5::from_str(text).ok()
        })
    };
    // `echo` ends the probe with a newline.
    let env_of = |pid: u32| -> String {
        poll_file(&dump_path(pid, "env"), |text| {
            text.ends_with('\n').then(|| text.trim().to_owned())
        })
    };
    let first_config = config_of(first_pid);
    assert_eq!(
        first_config["node_instance"]["arguments"]["speed"],
        serde_json::json!(3),
        "{first_config}"
    );
    assert_eq!(config_of(second_pid), first_config);
    assert_eq!(env_of(first_pid), "steady");
    assert_eq!(env_of(second_pid), env_of(first_pid));

    // The new process is probed: dropping its health mock turns the slot's
    // fresh health flag, which only the new process's monitor holds.
    drop(second_health);
    poll_until_explained(
        WAIT,
        || {
            format!(
                "the new process turns unhealthy; stack log:\n{}",
                stack_log(&started)
            )
        },
        || {
            let unhealthy_lines = lines_for(&started, ID)
                .iter()
                .filter(|line| line.contains("became unhealthy"))
                .count();
            (unhealthy_lines == 2).then_some(())
        },
    )
    .await;
    assert!(!second_slot.healthy());
    signal_process(second_pid, "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_exit_under_on_failure_stays_finished() {
    const NODE: &str = "one_shot";
    const ID: &str = "one_shot_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, &clean_on_term(), None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    signal_process(pid, "-TERM");
    wait_for_state(&started, ID, InstanceState::Finished).await;
    wait_for_line(&started, "finished: process exited cleanly").await;
    let instance = serialized(&started, ID);
    assert_eq!(instance.restart_status.attempt, 0);
    assert_eq!(instance.restart_status.restart_count, 0);
    assert_eq!(instance.restart_status.next_start_ms, None);
    assert_eq!(instance.restart_status.last_exit, Some(ExitCause::Code(0)));
    assert!(!stack_log(&started).contains("restarting"));

    // `peppy node stop` clears a finished instance and frees the id.
    stop_ok(&started, ID).await;
    assert_eq!(state_of(&started, ID), None);
    let again = run(&started, NODE, ID, never()).await;
    signal_process(again, "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_respawns_a_clean_exit() {
    const NODE: &str = "looper";
    const ID: &str = "looper_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, &clean_on_term(), None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, always()).await;
    signal_process(pid, "-TERM");
    wait_for_restarts(&started, ID, 1).await;
    let instance = serialized(&started, ID);
    assert_eq!(instance.restart_status.last_exit, Some(ExitCause::Code(0)));
    for needle in ["finished: process exited cleanly", "restarted (attempt 1)"] {
        assert!(
            logged(&started, ID, needle),
            "`{needle}` in {:?}",
            lines_for(&started, ID)
        );
    }
    signal_process(serialized_pid(&started, ID), "-9");
}

fn serialized_pid(started: &StartedCoreNode, instance_id: &str) -> u32 {
    slot_of(started, instance_id)
        .expect("tracked")
        .pid()
        .expect("a running instance has a pid")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_leaves_a_crash_failed_and_node_stop_frees_the_id() {
    const NODE: &str = "fragile";
    const ID: &str = "fragile_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, never()).await;
    signal_process(pid, "-9");
    wait_for_state(&started, ID, InstanceState::Failed).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(state_of(&started, ID), Some(InstanceState::Failed));
    let instance = serialized(&started, ID);
    assert_eq!(
        instance.restart_status.restart.policy(),
        RestartPolicy::Never
    );
    assert_eq!(instance.restart_status.restart_origin, RestartOrigin::Flag);
    assert_eq!(instance.restart_status.attempt, 0);
    assert_eq!(instance.restart_status.next_start_ms, None);
    assert!(!stack_log(&started).contains("restarting"));

    let (_shutdown, mut shutdown_asked) =
        common::install_shutdown_listener_for_instance(&started, NODE, TEST_NODE_TAG, ID).await;
    let response = stop_ok(&started, ID).await;
    assert!(!response.force_killed);
    assert!(
        shutdown_asked.try_recv().is_err(),
        "an instance without a process is cleared with no shutdown request"
    );
    assert_eq!(state_of(&started, ID), None);
    signal_process(run(&started, NODE, ID, never()).await, "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manifest_policy_restarts_without_a_flag() {
    const NODE: &str = "declared";
    const ID: &str = "declared_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(
        &started,
        NODE,
        SLEEPER,
        Some(r#"{ restart: "on_failure" }"#),
    )
    .await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, RestartRequest::Unset).await;
    assert_eq!(
        serialized(&started, ID).restart_status.restart_origin,
        RestartOrigin::Node
    );
    signal_process(pid, "-9");
    wait_for_restarts(&started, ID, 1).await;

    // The flag wins over the manifest.
    const SECOND: &str = "declared_never";
    let _second_mocks = mocks(&started, NODE, SECOND).await;
    let second_pid = run(&started, NODE, SECOND, never()).await;
    assert_eq!(
        serialized(&started, SECOND).restart_status.restart_origin,
        RestartOrigin::Flag
    );
    signal_process(second_pid, "-9");
    wait_for_state(&started, SECOND, InstanceState::Failed).await;
    signal_process(serialized_pid(&started, ID), "-9");
}

/// With nothing declared on the flag or in the manifest, an instance
/// restarts after a crash: `on_failure` is the default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_restarts_under_the_default_policy() {
    const NODE: &str = "undeclared";
    const ID: &str = "undeclared_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, RestartRequest::Unset).await;
    let status = serialized(&started, ID).restart_status;
    assert_eq!(status.restart_origin, RestartOrigin::Default);
    assert_eq!(
        status.restart,
        RestartDeclaration::policy_only(RestartPolicy::OnFailure)
    );
    signal_process(pid, "-9");
    wait_for_restarts(&started, ID, 1).await;
    signal_process(serialized_pid(&started, ID), "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_respawn_that_dies_during_startup_schedules_the_next_attempt() {
    const NODE: &str = "flaky";
    const ID: &str = "flaky_inst";
    let marker = TempDir::new().expect("marker dir");
    let marker_path = marker.path().join("fail");
    let run_cmd = format!(
        r#"["sh", "-c", "if [ -e {} ]; then echo boom >&2; exit 3; fi; exec sleep 300"]"#,
        marker_path.display()
    );
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, &run_cmd, None).await;
    let ready = common::answer_ready_probe(&started, NODE, ID).await;
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    settle().await;
    let pid = run(&started, NODE, ID, on_failure()).await;

    // With no ready answer, the respawn sits in its ready wait and sees the
    // process exit there.
    drop(ready);
    std::fs::write(&marker_path, "").expect("marker");
    signal_process(pid, "-9");
    wait_for_line(&started, "restarting in 0 s (attempt 2)").await;
    let log = stack_log(&started);
    assert!(
        log.lines().any(|line| line.ends_with(
            "start failed: node process exited during startup (exit code 3); its run log says why"
        )),
        "a restart's process names no rebuild:\n{log}"
    );
    assert!(
        log.lines().all(|line| line.contains("Instance '")) && !log.contains("boom"),
        "each stack log line is one event, the failure's first line alone:\n{log}"
    );
    let instance = serialized(&started, ID);
    assert!(
        matches!(&instance.restart_status.last_exit, Some(ExitCause::Message(text))
            if text.starts_with("node process exited during startup (exit code 3)")
                && !text.contains('\n')),
        "a start failure's first line is the last exit: {:?}",
        instance.restart_status.last_exit
    );
    // The restarts go on while the marker exists, so the newest run log can
    // be of a start that has not failed yet: the log of the start that
    // failed is one of the logs before it.
    assert!(
        every_run_log(&started, ID)
            .iter()
            .any(|log| log.contains("boom")),
        "the run log of the failed start holds the whole failure, its stderr included"
    );

    std::fs::remove_file(&marker_path).expect("marker removed");
    let _ready = common::answer_ready_probe(&started, NODE, ID).await;
    wait_for_restarts(&started, ID, 1).await;
    // Every start fails while the marker exists, and the backoff has no
    // wait, so the attempt is 2 or more by the time one starts.
    assert!(serialized(&started, ID).restart_status.attempt >= 2);
    signal_process(serialized_pid(&started, ID), "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cap_in_the_crash_loop_leaves_the_instance_failed() {
    const NODE: &str = "capped";
    const ID: &str = "capped_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, capped(3)).await;
    assert_eq!(
        serialized(&started, ID)
            .restart_status
            .restart
            .max_restarts(),
        NonZeroU32::new(3)
    );
    signal_process(pid, "-9");
    for restarts in 1..=3 {
        wait_for_restarts(&started, ID, restarts).await;
        signal_process(serialized_pid(&started, ID), "-9");
    }
    wait_for_state(&started, ID, InstanceState::Failed).await;
    wait_for_line(&started, "`max_restarts` reached: 3 attempts in a row").await;
    let instance = serialized(&started, ID);
    assert_eq!(instance.restart_status.attempt, 4);
    assert_eq!(instance.restart_status.restart_count, 3);
    assert_eq!(instance.restart_status.next_start_ms, None);
    let lines = lines_for(&started, ID);
    for attempt in 1..=3 {
        for needle in [
            format!("restarting in 0 s (attempt {attempt})"),
            format!("restarted (attempt {attempt})"),
        ] {
            assert!(
                lines.iter().any(|line| line.contains(&needle)),
                "`{needle}` in {lines:?}"
            );
        }
    }
    assert!(
        lines.last().is_some_and(|line| line.ends_with(&format!(
            "`max_restarts` reached: 3 attempts in a row; the instance is failed, and `peppy \
             node stop {ID} --core-node {cn}` clears it",
            cn = started.core_node_name
        ))),
        "{lines:?}"
    );
    tokio::time::sleep(QUIET).await;
    assert_eq!(state_of(&started, ID), Some(InstanceState::Failed));
}

/// A process that exits during its first start fails the run under every
/// policy, with no restart scheduled: the policy applies once an instance
/// has been running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_start_failure_fails_the_run_under_every_policy() {
    const NODE: &str = "stillborn";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, r#"["sh", "-c", "exit 3"]"#, None).await;
    // No ready answer, so the start is still waiting when the process exits.
    for (id, restart) in [
        ("stillborn_default", RestartRequest::Unset),
        ("stillborn_never", never()),
        ("stillborn_on_failure", on_failure()),
        ("stillborn_always", always()),
    ] {
        let end = run_plan(&started, NODE, common::default_instance_plan(id), restart)
            .await
            .expect("node_run completes");
        // The start fails, and a first start names the rebuild.
        assert_failure(
            end.failed(),
            PeppyFailureKind::AttemptFailed,
            "exited during startup",
        );
        assert!(
            end.failure_message()
                .contains(peppylib::runtime::REBUILD_REMEDY),
            "{id}: a first start names the rebuild: {:?}",
            end.failure_message()
        );
        assert_eq!(state_of(&started, id), None, "{id} leaves no slot");
    }
    tokio::time::sleep(QUIET).await;
    assert!(
        !stack_log(&started).contains("restarting"),
        "{}",
        stack_log(&started)
    );
}

/// A respawn whose process cannot be spawned schedules the next attempt with
/// the spawn error as the last exit, and runs again once it can be spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_respawn_whose_spawn_fails_schedules_the_next_attempt() {
    const NODE: &str = "unspawnable";
    const ID: &str = "unspawnable_inst";
    // The copy keeps the name `sleep`: a multi-call coreutils binary runs the
    // program its argv[0] names.
    let binaries = TempDir::new().expect("binaries dir");
    let sleeper = binaries.path().join("sleep");
    let sleep = which_sleep();
    std::fs::copy(&sleep, &sleeper).expect("a copy of sleep");
    let run_cmd = format!(r#"["{}", "300"]"#, sleeper.display());
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, &run_cmd, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, on_failure()).await;

    std::fs::remove_file(&sleeper).expect("the binary is removed");
    assert_eq!(
        state_of(&started, ID),
        Some(InstanceState::Running),
        "stack log:\n{}\nrun log:\n{}",
        stack_log(&started),
        run_log(&started, ID)
    );
    signal_process(pid, "-9");
    wait_for_line(&started, "restarting in 0 s (attempt 2)").await;
    assert!(
        matches!(&serialized(&started, ID).restart_status.last_exit, Some(ExitCause::Message(text))
            if text.contains("failed to spawn")),
        "{:?}",
        serialized(&started, ID).restart_status.last_exit
    );

    std::fs::copy(&sleep, &sleeper).expect("the binary is back");
    wait_for_restarts(&started, ID, 1).await;
    signal_process(serialized_pid(&started, ID), "-9");
}

/// The `sleep` binary of this machine.
fn which_sleep() -> std::path::PathBuf {
    let output = std::process::Command::new("sh")
        .args(["-c", "command -v sleep"])
        .output()
        .expect("sh runs");
    std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
}

/// A reset during a respawn's start cancels it at once, well before the
/// ready wait it sits in would end: the slot is gone and the stack log
/// records nothing after the attempt it was waiting on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_respawn_cancelled_during_its_start_records_nothing() {
    const NODE: &str = "reset_mid_start";
    const ID: &str = "reset_mid_start_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let ready = common::answer_ready_probe(&started, NODE, ID).await;
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    settle().await;
    let pid = run(&started, NODE, ID, on_failure()).await;

    drop(ready);
    signal_process(pid, "-9");
    wait_for_state(&started, ID, InstanceState::Starting).await;
    reset(&started).await;
    assert!(
        run_log(&started, ID).contains("cancelled"),
        "the reset cancels the start:\n{}",
        run_log(&started, ID)
    );
    assert_eq!(state_of(&started, ID), None);
    tokio::time::sleep(QUIET).await;
    let lines = lines_for(&started, ID);
    assert!(
        lines
            .last()
            .is_some_and(|line| line.contains("restarting in 0 s (attempt 1)")),
        "nothing is recorded after the cancelled attempt: {lines:?}"
    );
}

/// A `node run` its client cancels during the ready wait ends cancelled,
/// with the reason of the cancel, and leaves no slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_cancelled_during_its_start_names_why() {
    const NODE: &str = "abandoned";
    const ID: &str = "abandoned_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let end = common::send_node_run_then_cancel(
        &started.caller_handle,
        &started.core_node_name,
        common::default_instance_plan(ID),
        NODE,
        TEST_NODE_TAG,
        &TIMEOUTS,
        wait_for_state(&started, ID, InstanceState::Starting),
    )
    .await
    .expect("node_run completes");
    assert_eq!(end.cancel_reason(), "cancelled by its caller");
    assert!(
        run_log(&started, ID).contains("cancelled during ready-signal wait"),
        "the run log names why the start was abandoned:\n{}",
        run_log(&started, ID)
    );
    assert_eq!(
        state_of(&started, ID),
        None,
        "the cancelled start leaves no slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_past_the_reset_window_starts_the_backoff_over() {
    const NODE: &str = "forgiven";
    const ID: &str = "forgiven_inst";
    let started = daemon(resetting_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, capped(2)).await;
    // A run shorter than the window backs off further: attempt 2 after a
    // kill right after `running`.
    signal_process(pid, "-9");
    wait_for_restarts(&started, ID, 1).await;
    assert_eq!(serialized(&started, ID).restart_status.attempt, 1);
    signal_process(serialized_pid(&started, ID), "-9");
    wait_for_restarts(&started, ID, 2).await;
    assert_eq!(
        serialized(&started, ID).restart_status.attempt,
        2,
        "a run shorter than the reset window backs off further"
    );
    // A run past the window starts the backoff and the cap's count over:
    // attempt 1 again, under a cap of 2 that a third attempt would exceed.
    tokio::time::sleep(RESET_WINDOW * 2).await;
    signal_process(serialized_pid(&started, ID), "-9");
    wait_for_restarts(&started, ID, 3).await;
    assert_eq!(
        serialized(&started, ID).restart_status.attempt,
        1,
        "a run of the reset window starts the backoff over"
    );
    let lines = lines_for(&started, ID);
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("restarting in 0 s (attempt 1)"))
            .count(),
        2,
        "{lines:?}"
    );
    signal_process(serialized_pid(&started, ID), "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forked_child_is_killed_on_self_exit_under_never() {
    const NODE: &str = "forker";
    const ID: &str = "forker_inst";
    let started = daemon(instant_restart_backoff()).await;
    let run_cmd = r#"["sh", "-c", "sh -c 'trap \"\" TERM; while true; do sleep 0.2; done' & exec sleep 300"]"#;
    let _dir = add_node(&started, NODE, run_cmd, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, never()).await;
    wait_for_the_forked_child(pid).await;
    signal_process(pid, "-9");
    wait_for_state(&started, ID, InstanceState::Failed).await;
    wait_for_group_gone(pid, "the exited process").await;

    // Under `on_failure` and `always` the group of the exited process is
    // killed the same way, and the new process leads a group of its own.
    for (id, policy) in [
        ("forker_on_failure", on_failure()),
        ("forker_always", always()),
    ] {
        let _restarted_mocks = mocks(&started, NODE, id).await;
        let pid = run(&started, NODE, id, policy).await;
        wait_for_the_forked_child(pid).await;
        signal_process(pid, "-9");
        wait_for_restarts(&started, id, 1).await;
        wait_for_group_gone(pid, &format!("the exited process under `{id}`")).await;
        let new_pid = serialized_pid(&started, id);
        assert!(node_stack::process_group_exists(new_pid));
        signal_process(new_pid, "-9");
    }
}

/// Waits until the group `pid` leads holds the child its process forked.
async fn wait_for_the_forked_child(pid: u32) {
    poll_until(WAIT, "the forked child runs in the group", || {
        (node_stack::process_group_size(pid) >= 2).then_some(())
    })
    .await;
}

/// Runs [`PUBLISHER_ID`], publishing the domain `sim` under `publisher_restart`,
/// and [`READER_ID`], reading it under `on_failure`, and returns the domain.
/// The caller mocks both instances' services.
async fn run_publisher_and_reader(
    started: &StartedCoreNode,
    publisher_restart: RestartRequest,
) -> ClockDomainId {
    let domain = ClockDomainId::new(
        Name::new("sim").unwrap(),
        CoreNodeName::new(&started.core_node_name).unwrap(),
        ClockIncarnation::try_from(1).unwrap(),
    );
    let publisher_plan = NodeInstancePlan {
        clock: ClockBinding::publisher(domain.clone()),
        ..NodeInstancePlan::new(name(PUBLISHER_ID))
    };
    run_plan(started, PUBLISHER, publisher_plan, publisher_restart)
        .await
        .expect("node_run completes")
        .completed();
    let reader_plan = NodeInstancePlan {
        clock: ClockBinding::consumer(
            domain.clone(),
            ProducerRef::new(&started.core_node_name, PUBLISHER_ID),
        ),
        ..NodeInstancePlan::new(name(READER_ID))
    };
    run_plan(started, READER, reader_plan, on_failure())
        .await
        .expect("node_run completes")
        .completed();
    domain
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_respawn_leaves_the_instance_failed_and_schedules_nothing() {
    let started = daemon(instant_restart_backoff()).await;
    let _publisher_dir = add_node(&started, PUBLISHER, SLEEPER, None).await;
    let _reader_dir = add_node(&started, READER, SLEEPER, None).await;
    let _publisher_mocks = mocks(&started, PUBLISHER, PUBLISHER_ID).await;
    let _reader_mocks = mocks(&started, READER, READER_ID).await;
    run_publisher_and_reader(&started, never()).await;

    // The domain's publisher dies under `never`; the reader keeps running on
    // the last tick, and its restart is refused because the domain is gone.
    signal_process(serialized_pid(&started, PUBLISHER_ID), "-9");
    wait_for_state(&started, PUBLISHER_ID, InstanceState::Failed).await;
    signal_process(serialized_pid(&started, READER_ID), "-9");
    wait_for_line(&started, "restart refused: the clock domain it reads").await;
    let lines = lines_for(&started, READER_ID);
    assert!(
        lines.last().is_some_and(|line| line
            .contains("restart refused: the clock domain it reads")
            && line.ends_with(&format!(
                "ended with its publisher, and nothing publishes `sim` now; the instance is \
                 failed, and `peppy node stop {READER_ID} --core-node {cn}` clears it",
                cn = started.core_node_name
            ))),
        "{lines:?}"
    );
    let instance = serialized(&started, READER_ID);
    assert_eq!(instance.restart_status.restart_count, 0);
    assert_eq!(instance.restart_status.next_start_ms, None);
    tokio::time::sleep(QUIET).await;
    assert_eq!(state_of(&started, READER_ID), Some(InstanceState::Failed));
}

/// A clock publisher restarts under its policy like any instance and keeps
/// its domain: its next process starts on the same lifetime of the domain, a
/// second publisher of the name is refused while it waits, and its reader
/// runs on. A reader that restarts while the publisher awaits its own
/// restart fails each start in that wait and starts once the publisher's
/// start has begun, and a first start of a reader in the wait is refused.
/// Once the publisher is stopped and its id publishes a new lifetime, the
/// reader's restart is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clock_publisher_restarts_on_its_domain_and_a_restarting_reader_waits_for_it() {
    // Waits of 250 ms doubling to 4 s for attempts 1 to 5: the publisher's
    // fifth wait outlasts the reader's first three attempts, due at least
    // 0.25, 0.75 and 1.75 s after the reader's exit (each refused start adds
    // its own duration), which follows the publisher's; its fourth, due at
    // 3.75 s at the earliest, finds the publisher waiting or started.
    let started = daemon(RestartBackoff {
        first_delay: Duration::from_millis(250),
        max_delay: Duration::from_secs(4),
        reset_after: Duration::MAX,
    })
    .await;
    // Each publisher process copies its runtime config under its pid.
    let dumps = TempDir::new().expect("dumps dir");
    let run_cmd = shell(&format!(
        "{}; exec sleep 300",
        dump_runtime_config(dumps.path())
    ));
    let _publisher_dir = add_node(&started, PUBLISHER, &run_cmd, None).await;
    let _reader_dir = add_node(&started, READER, SLEEPER, None).await;
    let _publisher_mocks = mocks(&started, PUBLISHER, PUBLISHER_ID).await;
    let _reader_mocks = mocks(&started, READER, READER_ID).await;
    let domain = run_publisher_and_reader(&started, on_failure()).await;
    let clock_of = |pid: u32| -> serde_json::Value {
        poll_file(&dumps.path().join(format!("{pid}.json5")), |text| {
            serde_json5::from_str::<serde_json::Value>(text).ok()
        })["node_instance"]["framework"]["clock"]
            .clone()
    };

    let first_pid = serialized_pid(&started, PUBLISHER_ID);
    signal_process(first_pid, "-9");
    wait_for_restarts(&started, PUBLISHER_ID, 1).await;
    assert_eq!(
        clock_of(serialized_pid(&started, PUBLISHER_ID)),
        clock_of(first_pid),
        "the restarted publisher publishes the lifetime it published"
    );
    let reader = serialized(&started, READER_ID);
    assert_eq!(
        (reader.state, reader.restart_status.restart_count),
        (InstanceState::Running, 0),
        "the reader runs on through its publisher's restart"
    );

    for restarts in 2..=4 {
        signal_process(serialized_pid(&started, PUBLISHER_ID), "-9");
        wait_for_restarts(&started, PUBLISHER_ID, restarts).await;
    }
    signal_process(serialized_pid(&started, PUBLISHER_ID), "-9");
    wait_for_line(&started, "restarting in 4 s (attempt 5)").await;
    signal_process(serialized_pid(&started, READER_ID), "-9");
    // The waiting publisher keeps its domain's name.
    let rival_plan = NodeInstancePlan {
        clock: ClockBinding::publisher(ClockDomainId::new(
            domain.name.clone(),
            domain.core_node.clone(),
            ClockIncarnation::try_from(2).unwrap(),
        )),
        ..NodeInstancePlan::new(name("rival_inst"))
    };
    let rival = run_plan(&started, PUBLISHER, rival_plan, never())
        .await
        .expect("node_run completes");
    assert_failure(
        rival.failed(),
        PeppyFailureKind::PreconditionFailed,
        &format!(
            "is kept by `{PUBLISHER_ID}` on `{cn}`; choose another name, or stop that \
             instance with `peppy node stop {PUBLISHER_ID} --core-node {cn}`",
            cn = started.core_node_name
        ),
    );
    // A first start of a reader during the wait is refused, and nothing
    // retries it.
    let late_plan = NodeInstancePlan {
        clock: ClockBinding::consumer(
            domain.clone(),
            ProducerRef::new(&started.core_node_name, PUBLISHER_ID),
        ),
        ..NodeInstancePlan::new(name("late_reader_inst"))
    };
    let late = run_plan(&started, READER, late_plan, on_failure())
        .await
        .expect("node_run completes");
    assert_eq!(
        late.failed(),
        &Failure::new(
            PeppyFailureKind::PreconditionFailed,
            format!(
                "its clock publisher `{PUBLISHER_ID}` is restarting; run this again once \
                 `{PUBLISHER_ID}` is running"
            )
        )
    );
    assert_eq!(state_of(&started, "late_reader_inst"), None);
    wait_for_line(
        &started,
        &format!(
            "start failed: its clock publisher `{PUBLISHER_ID}` is restarting; the next attempt \
             of this instance looks for `{domain}` again"
        ),
    )
    .await;
    wait_for_restarts(&started, PUBLISHER_ID, 5).await;
    wait_for_restarts(&started, READER_ID, 1).await;
    assert!(
        serialized(&started, READER_ID).restart_status.attempt > 3,
        "each start that found the publisher waiting took an attempt"
    );
    // A start that failed before its spawn has its own run log, which holds
    // the failure, and the restart's log names it as the previous one.
    let failed_attempt = nth_run_log_path(&started, READER_ID, 1);
    assert!(
        nth_run_log(&started, READER_ID, 1).contains("is restarting; the next attempt"),
        "the failed attempt's run log holds its failure:\n{}",
        nth_run_log(&started, READER_ID, 1)
    );
    assert!(
        run_log(&started, READER_ID)
            .lines()
            .next()
            .is_some_and(|line| {
                line.ends_with(&format!(
                    "; the previous log is {}",
                    failed_attempt.display()
                ))
            }),
        "the restart's run log names the failed attempt's:\n{}",
        run_log(&started, READER_ID)
    );
    let publisher_attempt = run_log(&started, PUBLISHER_ID)
        .lines()
        .rev()
        .find(|line| line.contains(&restart_separator(5)))
        .and_then(time_of)
        .expect("the publisher's run log names its fifth attempt");
    assert!(
        logged_at(&started, READER_ID, "restarted (attempt") >= publisher_attempt,
        "the reader starts once its publisher's fifth attempt has begun"
    );

    // The publisher's id now publishes a new lifetime of the name, which the
    // reader is not bound to.
    stop_ok(&started, PUBLISHER_ID).await;
    let new_lifetime = NodeInstancePlan {
        clock: ClockBinding::publisher(ClockDomainId::new(
            domain.name.clone(),
            domain.core_node.clone(),
            ClockIncarnation::try_from(3).unwrap(),
        )),
        ..NodeInstancePlan::new(name(PUBLISHER_ID))
    };
    run_plan(&started, PUBLISHER, new_lifetime, on_failure())
        .await
        .expect("node_run completes")
        .completed();
    signal_process(serialized_pid(&started, READER_ID), "-9");
    wait_for_line(
        &started,
        &format!(
            "restart refused: the clock domain it reads, `{domain}`, ended with its publisher, \
             and `{PUBLISHER_ID}` publishes `{}` again as a new domain; the instance is \
             failed, and `peppy node stop {READER_ID} --core-node {cn}` clears it",
            domain.name,
            cn = started.core_node_name
        ),
    )
    .await;
    assert_eq!(state_of(&started, READER_ID), Some(InstanceState::Failed));
    signal_process(serialized_pid(&started, PUBLISHER_ID), "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_respawn_waits_for_a_stack_change_and_a_reservation_to_end() {
    const NODE: &str = "patient";
    const ID: &str = "patient_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, on_failure()).await;

    let change = started
        .slice_ownership
        .stack()
        .try_begin_change()
        .expect("the stack is free");
    signal_process(pid, "-9");
    wait_for_line(&started, "restarting in 0 s (attempt 1)").await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        state_of(&started, ID),
        Some(InstanceState::Restarting),
        "the respawn waits behind the stack change"
    );
    drop(change);
    wait_for_restarts(&started, ID, 1).await;

    let ReserveOutcome::Reserved { .. } = started
        .slice_ownership
        .try_reserve("launch-elsewhere", "cn-other")
    else {
        panic!("a free daemon takes a reservation");
    };
    signal_process(serialized_pid(&started, ID), "-9");
    wait_for_line(&started, "restarting in 0 s (attempt 2)").await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        state_of(&started, ID),
        Some(InstanceState::Restarting),
        "the respawn waits behind the reservation"
    );
    assert!(started.slice_ownership.release("launch-elsewhere"));
    wait_for_restarts(&started, ID, 2).await;
    signal_process(serialized_pid(&started, ID), "-9");
}

/// A stop that claims a restart's start between its flip and its fork ends
/// it there: with the respawn held in its archive extraction, which a FIFO
/// at the archive's path holds until the test writes it, the stop's claim
/// lands first, the start ends before it spawns, and nothing runs for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_stop_that_claims_a_restarts_start_before_its_fork_forks_nothing() {
    const NODE: &str = "claimed_before_fork";
    const ID: &str = "claimed_before_fork_inst";
    let markers = TempDir::new().expect("marker dir");
    let forked = markers.path().join("forked");
    let run_cmd = format!(
        r#"["sh", "-c", "touch {} && exec sleep 300"]"#,
        forked.display()
    );
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, &run_cmd, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    poll_until(WAIT, "the first process runs", || {
        forked.exists().then_some(())
    })
    .await;
    std::fs::remove_file(&forked).expect("the marker is removed");

    let archive = started
        .node_stack
        .find(NODE, TEST_NODE_TAG)
        .expect("the node is in the stack")
        .read()
        .artifact_path()
        .expect("a built node has its archive")
        .to_path_buf();
    let bytes = std::fs::read(&archive).expect("the archive reads");
    std::fs::remove_file(&archive).expect("the archive is removed");
    let made_fifo = std::process::Command::new("mkfifo")
        .arg(&archive)
        .status()
        .expect("mkfifo runs");
    assert!(made_fifo.success(), "a FIFO takes the archive's path");
    let mut unblock = UnblockFifoOnDrop(Some(archive.clone()));

    signal_process(pid, "-9");
    wait_for_state(&started, ID, InstanceState::Starting).await;
    let write_the_archive_once_claimed = async {
        wait_for_claim(&started, ID).await;
        tokio::task::spawn_blocking(move || std::fs::write(&archive, bytes))
            .await
            .expect("the writer runs")
            .expect("the archive is written");
        unblock.0 = None;
    };
    tokio::join!(stop_ok(&started, ID), write_the_archive_once_claimed);
    assert_eq!(state_of(&started, ID), None);
    let last_attempt = run_log(&started, ID);
    assert!(
        last_attempt.contains(&format!(
            "the start of instance `{ID}` was ended by a stop before it spawned"
        )),
        "{last_attempt}"
    );
    assert!(!forked.exists(), "nothing ran for the claimed start");
}

/// The FIFO at a path a start may be blocked opening, which a test that ends
/// before it writes the FIFO opens for writing on drop, from a thread of its
/// own: the start then reads an end of file, and the runtime can shut down.
struct UnblockFifoOnDrop(Option<std::path::PathBuf>);

impl Drop for UnblockFifoOnDrop {
    fn drop(&mut self) {
        if let Some(fifo) = self.0.take() {
            std::thread::spawn(move || drop(std::fs::OpenOptions::new().write(true).open(fifo)));
        }
    }
}

/// While a restart's process starts, a launch, join or removal answers busy,
/// an overwrite of the node is refused, stopping nothing, and `peppy node
/// remove` leaves the node, each naming the start; `peppy node stop` cancels
/// that start, waits for its abort, which kills what it started, tells the
/// instance's watchers it stopped and clears the id, and nothing restarts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_ends_a_restarts_start_in_flight() {
    use core_node_api::encoding::{InstanceEnd, RelationshipEvent};
    const NODE: &str = "slow_start";
    const ID: &str = "slow_start_inst";
    const SIBLING: &str = "slow_start_sibling";
    let started = daemon(instant_restart_backoff()).await;
    take_part_in_the_launch(&started).await;
    let dir = add_node(&started, NODE, OUTPUT_HELD_OUTSIDE_THE_GROUP, None).await;
    let ready = common::answer_ready_probe(&started, NODE, ID).await;
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    let (_watcher, mut heard) = watcher(&started, Duration::ZERO).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    watch(&started, ID).await;
    let _sibling_mocks = mocks(&started, NODE, SIBLING).await;
    let sibling = run(&started, NODE, SIBLING, never()).await;

    // Without a ready answer the respawn sits in its ready wait.
    drop(ready);
    signal_process(pid, "-9");
    assert_eq!(
        next_event(&mut heard, ID).await,
        RelationshipEvent::Ended(InstanceEnd::Exited)
    );
    let starting_pid = wait_for_process(&started, ID, InstanceState::Starting).await;
    // A launch, join or removal answers busy.
    let refusal = try_reserve(&started)
        .await
        .expect_err("the reservation is refused");
    assert_eq!(
        refusal.kind().as_str(),
        PeppyFailureKind::PreconditionFailed.name(),
        "{refusal}"
    );
    assert!(
        refusal.message().ends_with(&format!(
            "; the restart of `{ID}` holds this daemon, and `peppy node stop {ID} \
             --core-node {}` ends it and clears the instance",
            started.core_node_name
        )),
        "{refusal}"
    );
    // An overwrite waits for the start.
    let overwrite = overwrite(&started, dir.path()).await;
    assert_failure(
        overwrite.failed(),
        PeppyFailureKind::PreconditionFailed,
        &format!(
            "cannot be overwritten while an instance of it starts: `{ID}` is starting again \
             under its restart policy, and \
             `peppy node stop {ID} --core-node {}` ends it and clears the instance; run this \
             again once none is starting",
            started.core_node_name
        ),
    );
    assert_eq!(
        serialized_pid(&started, SIBLING),
        sibling,
        "the refused overwrite stops nothing"
    );
    assert!(common::is_process_running(sibling));
    // The sibling ends before the removal, which clears an exited instance
    // with no shutdown request.
    signal_process(sibling, "-9");
    wait_for_state(&started, SIBLING, InstanceState::Failed).await;
    // A node with a starting instance stays.
    let removal = remove(&started, NODE, true)
        .await
        .expect_err("the removal fails");
    assert_failure(
        &removal,
        PeppyFailureKind::PreconditionFailed,
        "still has instances",
    );
    assert!(
        removal.message().contains(&format!(
            "`{ID}` is starting again under its restart policy, and `peppy node stop \
             {ID} --core-node {cn}` ends it and clears the instance; run `peppy node \
             remove {NODE}:{TEST_NODE_TAG} --stop-instances --core-node {cn}` again once \
             none is starting",
            cn = started.core_node_name
        )),
        "{removal}"
    );

    stop_ok(&started, ID).await;
    assert!(
        run_log(&started, ID).contains("cancelled"),
        "the stop cancels the start:\n{}",
        run_log(&started, ID)
    );
    assert_eq!(
        state_of(&started, ID),
        None,
        "the stop answers once the start it ended took its slot"
    );
    assert_eq!(
        next_event(&mut heard, ID).await,
        RelationshipEvent::Ended(InstanceEnd::Stopped)
    );
    wait_for_group_gone(starting_pid, "the started process").await;
    let _ready = common::answer_ready_probe(&started, NODE, ID).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(state_of(&started, ID), None, "nothing restarts it");
    signal_process(run(&started, NODE, ID, never()).await, "-9");
}

/// `peppy node stop` of an instance in its first start is refused; beside a
/// restart's start, an overwrite and a removal of its node name both starts,
/// the restart's with the stop that ends it and the first start with the
/// wait alone, and a launch's reservation names the restart; each start runs
/// once its node answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_of_a_first_start_is_refused() {
    const NODE: &str = "first_start";
    const ID: &str = "first_start_inst";
    const RESTARTING: &str = "restart_start_inst";
    let started = daemon(instant_restart_backoff()).await;
    let dir = add_node(&started, NODE, SLEEPER, None).await;
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    let _restarting_health = common::answer_health_probes(&started, NODE, RESTARTING).await;
    // Beside the first start, a restart's start sits in its ready wait.
    let restarting_ready = common::answer_ready_probe(&started, NODE, RESTARTING).await;
    let restarting_pid = run(&started, NODE, RESTARTING, on_failure()).await;
    drop(restarting_ready);
    signal_process(restarting_pid, "-9");
    wait_for_process(&started, RESTARTING, InstanceState::Starting).await;
    settle().await;
    let stop_during_the_start = async {
        let starting_pid = wait_for_process(&started, ID, InstanceState::Starting).await;
        let response = stop(&started, ID).await;
        let overwrite = overwrite(&started, dir.path()).await;
        let removal = remove(&started, NODE, true).await;
        let removal_without_the_flag = remove(&started, NODE, false).await;
        let reserved = try_reserve(&started).await;
        let ready = common::answer_ready_probe(&started, NODE, ID).await;
        (
            starting_pid,
            response,
            overwrite,
            [removal, removal_without_the_flag],
            reserved,
            ready,
        )
    };
    let (pid, (starting_pid, response, overwrite, removals, reserved, _ready)) =
        tokio::join!(run(&started, NODE, ID, on_failure()), stop_during_the_start);

    let reserved = reserved.expect_err("the reservation is refused");
    assert_eq!(
        reserved.kind().as_str(),
        PeppyFailureKind::PreconditionFailed.name(),
        "{reserved}"
    );
    assert!(
        !reserved.message().contains(&format!("`{ID}`"))
            && reserved.message().ends_with(&format!(
                "is in progress on this daemon; run this again once it ends; the restart of \
                 `{RESTARTING}` holds this daemon, and `peppy node stop {RESTARTING} \
                 --core-node {}` ends it and clears the instance",
                started.core_node_name
            )),
        "a first start names no restart: {reserved}"
    );

    // The overwrite and the removal name each start in flight, a restart's
    // with the stop that ends it.
    let starts = format!(
        "`{RESTARTING}` is starting again under its restart policy, and `peppy node stop \
         {RESTARTING} --core-node {}` ends it and clears the instance; `{ID}` is starting",
        started.core_node_name
    );
    let overwrite = overwrite.failed();
    assert_eq!(
        overwrite.kind().as_str(),
        PeppyFailureKind::PreconditionFailed.name(),
        "{overwrite}"
    );
    assert!(
        overwrite.message().ends_with(&format!(
            "cannot be overwritten while an instance of it starts: {starts}; run this again \
             once none is starting"
        )),
        "{overwrite}"
    );
    // With `--stop-instances` or without it: a start in flight is no
    // instance the gate asks the flag for.
    for removal in removals {
        let removal = removal.expect_err("the removal fails");
        assert_eq!(
            removal.kind().as_str(),
            PeppyFailureKind::PreconditionFailed.name(),
            "{removal}"
        );
        assert!(
            removal.message().ends_with(&format!(
                "; {starts}; run `peppy node remove {NODE}:{TEST_NODE_TAG} --stop-instances \
                 --core-node {}` again once none is starting",
                started.core_node_name
            )),
            "{removal}"
        );
    }
    assert_failure(
        &response.expect_err("the stop is refused"),
        PeppyFailureKind::PreconditionFailed,
        "is in Starting state; retry after the start completes",
    );
    assert_eq!(pid, starting_pid);
    assert_eq!(state_of(&started, ID), Some(InstanceState::Running));
    let _restarting_ready = common::answer_ready_probe(&started, NODE, RESTARTING).await;
    wait_for_restarts(&started, RESTARTING, 1).await;
}

/// A daemon whose backoff parks a restart for an hour, with one instance
/// killed into the wait.
async fn parked(node: &str, instance_id: &str) -> (StartedCoreNode, TempDir, (Mock, Mock)) {
    let started = daemon(parked_restart_backoff()).await;
    let dir = add_node(&started, node, SLEEPER, None).await;
    let mocks = mocks(&started, node, instance_id).await;
    let pid = run(&started, node, instance_id, on_failure()).await;
    signal_process(pid, "-9");
    poll_until_explained(
        WAIT,
        || {
            format!(
                "`{instance_id}` waits for its first attempt; stack log:\n{}",
                stack_log(&started)
            )
        },
        || {
            let instance = serialized(&started, instance_id);
            (instance.state == InstanceState::Restarting
                && instance.restart_status.attempt == 1
                && instance.restart_status.next_start_ms.is_some())
            .then_some(())
        },
    )
    .await;
    (started, dir, mocks)
}

/// The token that cancels the restart `instance_id` waits for.
fn restart_token(
    started: &StartedCoreNode,
    instance_id: &str,
) -> tokio_util::sync::CancellationToken {
    slot_of(started, instance_id)
        .expect("the instance has a slot")
        .supervision()
        .restart_token
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_during_the_wait_removes_the_slot() {
    const NODE: &str = "waiting";
    const ID: &str = "waiting_inst";
    let (started, _dir, _mocks) = parked(NODE, ID).await;
    let restart = restart_token(&started, ID);
    stop_ok(&started, ID).await;
    assert_eq!(state_of(&started, ID), None);
    assert!(restart.is_cancelled(), "the stop cancels the restart");
    signal_process(run(&started, NODE, ID, never()).await, "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_remove_during_the_wait_is_refused_then_removes() {
    const NODE: &str = "removable";
    const ID: &str = "removable_inst";
    let (started, _dir, _mocks) = parked(NODE, ID).await;
    let restart = restart_token(&started, ID);
    let refused = remove(&started, NODE, false).await;
    assert_eq!(
        refused,
        Err(Failure::new(
            PeppyFailureKind::PreconditionFailed,
            format!(
                "Node '{NODE}:{TEST_NODE_TAG}' has tracked instances (e.g. '{ID}'); `peppy \
                 node remove {NODE}:{TEST_NODE_TAG} --stop-instances --core-node {}` \
                 clears them before removing",
                started.core_node_name
            )
        ))
    );
    let slot = slot_of(&started, ID).expect("the refused removal leaves the slot");
    assert_eq!(slot.state(), InstanceState::Restarting);
    assert!(!slot.is_stopping(), "a refused removal claims nothing");
    assert!(!slot.supervision().restart_token.is_cancelled());

    remove_ok(&started, NODE, true).await;
    assert_eq!(state_of(&started, ID), None);
    assert!(restart.is_cancelled(), "the removal cancels the restart");
    assert!(started.node_stack.find(NODE, TEST_NODE_TAG).is_none());
    // The id is free: the node added and run again starts under it.
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    assert_eq!(serialized(&started, ID).restart_status.restart_count, 0);
    signal_process(pid, "-9");
}

/// A removal whose shutdown request an instance refuses fails leaving that
/// instance unclaimed and its shutdown request withdrawn: its exit is still
/// the watcher's to record, so a crash afterwards restarts it under its
/// policy. An instance that answered its request and exits on it within the
/// force-kill deadline ends there, under `always` too; one that answered and
/// is still running past the deadline keeps its policy, so a crash then
/// restarts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_whose_shutdown_fails_claims_nothing() {
    const NODE: &str = "refuses_shutdown";
    const REFUSING: &str = "refuses_shutdown_inst";
    const ANSWERING: &str = "answers_shutdown_inst";
    const LINGERING: &str = "lingers_after_shutdown_inst";
    /// How long before its window ends the answering node is asked to exit
    /// (its trap runs after the current 0.2 s sleep), and how far past the
    /// window the lingering one is killed.
    const WINDOW_HEADROOM: Duration = Duration::from_secs(4);
    // The notes stand for the force-kill deadline from the failure, which
    // comes after every answer, so an exit this long after the answer lands
    // inside the window.
    let deadline = core_node::force_kill_deadline(Duration::from_secs(SHORT_GRACE_SECS));
    let exit_after = deadline - WINDOW_HEADROOM;
    let started = start_core_node_with_supervision_and_grace(
        fast_health_monitor(UNHEALTHY_AFTER),
        instant_restart_backoff(),
        SHORT_GRACE_SECS,
    )
    .await;
    let _dir = add_node(&started, NODE, &clean_on_term(), None).await;
    let _refusing_mocks = mocks(&started, NODE, REFUSING).await;
    let _refusing = refusing_shutdown(&started, NODE, REFUSING).await;
    let _answering_mocks = mocks(&started, NODE, ANSWERING).await;
    let _answering = exiting_on_shutdown(
        &started,
        NODE,
        ANSWERING,
        ExitOnShutdown::AfterAnswer(exit_after),
    )
    .await;
    let _lingering_mocks = mocks(&started, NODE, LINGERING).await;
    // A node that answers its shutdown request and runs on.
    let (lingering_shutdown, _asked) = listen_for_shutdown(
        &MessengerHandle::from_shared(Arc::clone(&started.shared_messenger)),
        &started.core_node_name,
        LINGERING,
        common::test_node_target(NODE),
    )
    .await
    .expect("the shutdown service starts");
    let _lingering = AbortOnDrop(lingering_shutdown);
    settle().await;
    let refusing = run(&started, NODE, REFUSING, always()).await;
    run(&started, NODE, ANSWERING, always()).await;
    let lingering = run(&started, NODE, LINGERING, always()).await;

    let response = remove(&started, NODE, true).await;
    let failed = std::time::Instant::now();
    assert_failure(
        &response.expect_err("the removal fails"),
        PeppyFailureKind::AttemptFailed,
        &format!("Failed to stop node instance '{REFUSING}'"),
    );
    let slot = slot_of(&started, REFUSING).expect("the failed removal leaves the instance");
    assert!(!slot.is_stopping(), "a failed removal claims nothing");
    assert_eq!(
        state_of(&started, ANSWERING),
        Some(InstanceState::Running),
        "the answering node exits after the removal failed"
    );
    // Inside the window, the refusing instance's note is already withdrawn.
    signal_process(refusing, "-9");
    wait_for_restarts(&started, REFUSING, 1).await;
    signal_process(serialized_pid(&started, REFUSING), "-9");

    wait_for_state(&started, ANSWERING, InstanceState::Finished).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        state_of(&started, ANSWERING),
        Some(InstanceState::Finished),
        "an instance that answered the failed removal ends at its exit"
    );

    tokio::time::sleep((deadline + WINDOW_HEADROOM).saturating_sub(failed.elapsed())).await;
    signal_process(lingering, "-9");
    wait_for_restarts(&started, LINGERING, 1).await;
    signal_process(serialized_pid(&started, LINGERING), "-9");
}

/// A `shutdown` service for `instance_id` that refuses every request.
async fn refusing_shutdown(started: &StartedCoreNode, name: &str, instance_id: &str) -> Mock {
    node_service_mock(
        started,
        name,
        instance_id,
        peppylib::messaging::SHUTDOWN_SERVICE,
        |_context| async move {
            Err(Failure::new(
                PeppyFailureKind::Rejected,
                "this instance refuses to shut down",
            ))
        },
    )
    .await
}

/// A reset leaves the restarts of the next stack working: an instance run
/// after it restarts after a crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_after_a_stack_reset_is_scheduled() {
    const NODE: &str = "after_reset";
    const ID: &str = "after_reset_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    signal_process(run(&started, NODE, ID, on_failure()).await, "-9");
    wait_for_restarts(&started, ID, 1).await;
    reset(&started).await;

    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    signal_process(run(&started, NODE, ID, on_failure()).await, "-9");
    wait_for_restarts(&started, ID, 1).await;
    signal_process(serialized_pid(&started, ID), "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_add_overwrite_during_the_wait_cancels_the_respawn() {
    const NODE: &str = "overwritten";
    const ID: &str = "overwritten_inst";
    let (started, dir, _mocks) = parked(NODE, ID).await;
    let restart = restart_token(&started, ID);
    overwrite(&started, dir.path()).await.completed();
    assert_eq!(state_of(&started, ID), None);
    assert!(restart.is_cancelled(), "the overwrite cancels the restart");
    // The overwritten node builds again, and a run under the id starts.
    add_and_build(&started, dir.path()).await;
    signal_process(run(&started, NODE, ID, never()).await, "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_reset_during_the_wait_cancels_the_respawn() {
    const NODE: &str = "resettable";
    const ID: &str = "resettable_inst";
    let (started, _dir, _mocks) = parked(NODE, ID).await;
    let restart = restart_token(&started, ID);
    reset(&started).await;
    assert_eq!(started.node_stack.len(), 1, "only the root remains");
    assert!(restart.is_cancelled(), "the reset cancels the restart");
    // The id is free: the node added and run again starts under it.
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    assert_eq!(serialized(&started, ID).restart_status.restart_count, 0);
    signal_process(pid, "-9");
}

/// A stop of a restart's start that a teardown claimed answers that the
/// instance is already being stopped: the start leaves its slot only once
/// its abort ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_of_a_start_a_teardown_claimed_answers_already_being_stopped() {
    const NODE: &str = "torn_down_start";
    const ID: &str = "torn_down_start_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, OUTPUT_HELD_OUTSIDE_THE_GROUP, None).await;
    let ready = common::answer_ready_probe(&started, NODE, ID).await;
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, on_failure()).await;

    // Without a ready answer the respawn sits in its ready wait.
    drop(ready);
    signal_process(pid, "-9");
    wait_for_process(&started, ID, InstanceState::Starting).await;
    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let node_stack = Arc::new(started.node_stack.clone());
    let stop_after_the_teardowns_claim = async {
        wait_for_claim(&started, ID).await;
        let stopped = stop(&started, ID).await;
        assert_eq!(
            state_of(&started, ID),
            Some(InstanceState::Starting),
            "the claimed start is still starting when the stop answers"
        );
        stopped
    };
    let ((), stopped) = tokio::join!(
        core_node::teardown_all_instances(&messenger, &started.core_node_name, "core", &node_stack),
        stop_after_the_teardowns_claim
    );
    assert_eq!(
        stopped,
        Err(Failure::new(
            PeppyFailureKind::PreconditionFailed,
            format!("Node instance '{ID}' is already being stopped")
        ))
    );
    poll_until_explained(
        WAIT,
        || format!("the claimed start of `{ID}` leaves its slot"),
        || state_of(&started, ID).is_none().then_some(()),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_during_the_wait_leaves_no_slot() {
    const NODE: &str = "shut";
    const ID: &str = "shut_inst";
    let (started, _dir, _mocks) = parked(NODE, ID).await;
    let restart = restart_token(&started, ID);
    started.shutdown_token.cancel();
    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let node_stack = Arc::new(started.node_stack.clone());
    core_node::teardown_all_instances(&messenger, &started.core_node_name, "core", &node_stack)
        .await;
    assert_eq!(state_of(&started, ID), None);
    assert!(restart.is_cancelled(), "the teardown cancels the restart");
}

/// A raw goal under an id the stack tracks is refused before it opens a run
/// log, so the tracked instance's run log is left as it was: under a
/// restarting and a failed id of the same node, with the state and what
/// frees the id, and under another node's id, naming that node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_under_a_tracked_id_is_rejected_before_its_run_log() {
    const NODE: &str = "taken";
    const ID: &str = "taken_inst";
    const FAILED: &str = "taken_failed_inst";
    const OTHER_NODE: &str = "taken_other";
    let (started, _dir, _mocks) = parked(NODE, ID).await;
    let _failed_mocks = mocks(&started, NODE, FAILED).await;
    signal_process(run(&started, NODE, FAILED, never()).await, "-9");
    wait_for_state(&started, FAILED, InstanceState::Failed).await;
    let _other_dir = add_node(&started, OTHER_NODE, SLEEPER, None).await;

    for (node, id, refusal) in [
        (
            NODE,
            ID,
            format!(
                "Instance ID `{ID}` already exists for node `{NODE}:{TEST_NODE_TAG}` and is \
                 restarting; `peppy node stop {ID} --core-node {cn}` frees the id",
                cn = started.core_node_name
            ),
        ),
        (
            NODE,
            FAILED,
            format!(
                "Instance ID `{FAILED}` already exists for node `{NODE}:{TEST_NODE_TAG}` and is \
                 failed; `peppy node stop {FAILED} --core-node {cn}` frees the id",
                cn = started.core_node_name
            ),
        ),
        (
            OTHER_NODE,
            ID,
            format!(
                "Instance ID `{ID}` is already tracked by `{NODE}`:{TEST_NODE_TAG}; instance_ids \
                 must be unique across the entire stack"
            ),
        ),
    ] {
        let run_log_before = run_log(&started, id);
        let end = run_plan(&started, node, common::default_instance_plan(id), never())
            .await
            .expect("node_run completes");
        assert_eq!(
            end.refusal(),
            &Failure::new(PeppyFailureKind::PreconditionFailed, refusal)
        );
        assert!(!run_log_before.is_empty());
        assert_eq!(
            run_log(&started, id),
            run_log_before,
            "the refused run opens no run log under `{id}`"
        );
    }
}

/// A hung instance beside a healthy one is killed: the healthy sibling is
/// what tells a hung node from a messaging outage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_instance_beside_a_healthy_one_is_killed() {
    const NODE: &str = "pair";
    const HUNG: &str = "pair_hung";
    const WELL: &str = "pair_well";
    let started = start_core_node_with_supervision(kill_monitor(), instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _hung_ready = common::answer_ready_probe(&started, NODE, HUNG).await;
    let hung_health = common::answer_health_probes(&started, NODE, HUNG).await;
    let _well_mocks = mocks(&started, NODE, WELL).await;
    let hung_pid = run(&started, NODE, HUNG, on_failure()).await;
    let well_pid = run(&started, NODE, WELL, on_failure()).await;

    drop(hung_health);
    wait_for_line(
        &started,
        &format!("'{HUNG}' of node '{NODE}:v1' {}", kill_line()),
    )
    .await;
    let _hung_health = common::answer_health_probes(&started, NODE, HUNG).await;
    wait_for_restarts(&started, HUNG, 1).await;
    let log = stack_log(&started);
    assert!(!log.contains("not killed"), "no outage was read:\n{log}");
    assert_eq!(state_of(&started, WELL), Some(InstanceState::Running));
    assert_eq!(serialized(&started, WELL).restart_status.restart_count, 0);
    assert_eq!(serialized_pid(&started, WELL), well_pid);
    assert_ne!(serialized_pid(&started, HUNG), hung_pid);
    signal_process(serialized_pid(&started, HUNG), "-9");
    signal_process(well_pid, "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unhealthy_instance_is_killed_and_respawned_under_on_failure() {
    const NODE: &str = "hung";
    const ID: &str = "hung_inst";
    let started = start_core_node_with_supervision(kill_monitor(), instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let (_ready, health) = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, on_failure()).await;

    // The process answers for longer than the kill's silence before it goes
    // quiet; the kill line's time then shows the silence is counted from the
    // last answer.
    tokio::time::sleep(KILL_AFTER + MISS_CYCLE).await;
    let silenced_at = chrono::Local::now().naive_local();
    drop(health);
    wait_for_line(&started, &unhealthy_line()).await;
    assert!(
        !logged(&started, ID, &kill_line()),
        "no kill at the unhealthy edge"
    );
    wait_for_line(&started, &kill_line()).await;
    // The silence is counted from the last answered check, which came
    // within one cycle before the mock was dropped.
    let silent_for = logged_at(&started, ID, &kill_line()) - silenced_at;
    let least = chrono::TimeDelta::from_std(KILL_AFTER - MISS_CYCLE).expect("a short bound");
    assert!(
        silent_for >= least,
        "the kill waits out the silence: killed {silent_for} after the mock was dropped"
    );
    wait_for_line(&started, &health_kill_line()).await;
    // The new process answers health again.
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    wait_for_restarts(&started, ID, 1).await;
    assert!(
        logged(&started, ID, "restarting in"),
        "the killed instance restarts under its policy"
    );
    let instance = serialized(&started, ID);
    assert_eq!(
        instance.restart_status.last_exit,
        Some(ExitCause::HealthKill {
            unanswered_secs: KILL_AFTER_SECS
        })
    );
    assert_ne!(serialized_pid(&started, ID), pid);
    signal_process(serialized_pid(&started, ID), "-9");
}

/// A health kill skips an instance a stop path claimed: while a stop waits
/// on its shutdown request, the instance answers no health check past the
/// kill, and its monitor writes no kill line and signals nothing. A sibling
/// silenced at the same moment is killed, which shows the kill fell due; a
/// third instance stays healthy, so the silence is no messaging outage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_health_kill_during_a_stop_is_skipped() {
    /// Three miss cycles of the test monitor (300 ms each, its 200 ms
    /// interval and 100 ms timeout): two monitors silenced together reach
    /// the kill within two cycles of each other, and one more cycle covers a
    /// scheduling stall of one of them.
    const MONITOR_PHASE_MARGIN: Duration = Duration::from_millis(900);
    const NODE: &str = "hung_while_stopped";
    const ID: &str = "hung_while_stopped_inst";
    const SILENT: &str = "hung_unclaimed_inst";
    const WELL: &str = "answering_inst";
    let started = start_core_node_with_supervision(kill_monitor(), parked_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let (_ready, health) = mocks(&started, NODE, ID).await;
    let (_silent_ready, silent_health) = mocks(&started, NODE, SILENT).await;
    let _well_mocks = mocks(&started, NODE, WELL).await;
    let shutdowns =
        held_requests(&started, NODE, ID, peppylib::messaging::SHUTDOWN_SERVICE, 0).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    run(&started, NODE, SILENT, on_failure()).await;
    let well = run(&started, NODE, WELL, never()).await;

    let silent_while_claimed = async {
        wait_for_claim(&started, ID).await;
        drop(health);
        drop(silent_health);
        poll_until(WAIT, "the unclaimed sibling is killed", || {
            logged(&started, SILENT, &kill_line()).then_some(())
        })
        .await;
        tokio::time::sleep(MONITOR_PHASE_MARGIN).await;
        let lines = lines_for(&started, ID);
        assert!(
            lines.iter().any(|line| line.contains(&unhealthy_line())),
            "the claimed instance's monitor counts its misses past the claim: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains(&kill_line())),
            "a claimed instance is not killed: {lines:?}"
        );
        assert!(
            common::is_process_running(pid),
            "the monitor signals nothing"
        );
        shutdowns.release.notify_one();
    };
    tokio::join!(stop_ok(&started, ID), silent_while_claimed);
    assert_eq!(state_of(&started, ID), None);
    signal_process(well, "-9");
}

/// A process that stops answering right after it starts and is killed for
/// it runs past the reset window by the clock but not by its answers, so
/// its restarts back off further: a hang loop reaches `max_restarts`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hang_loop_backs_off_to_its_cap() {
    const NODE: &str = "hangs_again";
    const ID: &str = "hangs_again_inst";
    let started =
        start_core_node_with_supervision(kill_monitor(), resetting_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let (_ready, health) = mocks(&started, NODE, ID).await;
    run(&started, NODE, ID, capped(1)).await;

    // Each process goes silent as it reaches running; the kill comes
    // `KILL_AFTER` into the silence, past the reset window.
    drop(health);
    wait_for_line(&started, &health_kill_line()).await;
    let health = common::answer_health_probes(&started, NODE, ID).await;
    wait_for_restarts(&started, ID, 1).await;
    drop(health);
    wait_for_state(&started, ID, InstanceState::Failed).await;
    wait_for_line(&started, "`max_restarts` reached: 1 attempt in a row").await;
    let instance = serialized(&started, ID);
    assert_eq!(
        instance.restart_status.attempt, 2,
        "the second kill climbs to attempt 2"
    );
    // The premise: by the clock, the second process ran past the reset
    // window before its kill.
    let ran = logged_at(&started, ID, &health_kill_line())
        - logged_at(&started, ID, "restarted (attempt 1)");
    assert!(
        ran >= chrono::TimeDelta::from_std(RESET_WINDOW).expect("a short window"),
        "the second process ran {ran} by the clock"
    );
    assert!(
        lines_for(&started, ID)
            .last()
            .is_some_and(|line| line.contains("`max_restarts` reached: 1 attempt in a row")),
        "{:?}",
        lines_for(&started, ID)
    );
}

/// A process that answers its probes past the reset window before it goes
/// silent ran that long by its answers, so its kill starts the backoff over:
/// restarts that each follow a long answered run never reach
/// `max_restarts`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_after_a_long_answered_run_starts_the_backoff_over() {
    const NODE: &str = "hangs_late";
    const ID: &str = "hangs_late_inst";
    let started =
        start_core_node_with_supervision(kill_monitor(), resetting_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let (_ready, mut health) = mocks(&started, NODE, ID).await;
    run(&started, NODE, ID, capped(1)).await;
    for restarts in 1..=2 {
        // The run counts to its last answer, so it answers for more than a
        // probe cycle past the window.
        tokio::time::sleep(RESET_WINDOW * 2).await;
        drop(health);
        poll_until_explained(
            WAIT,
            || format!("kill {restarts}; stack log:\n{}", stack_log(&started)),
            || {
                let kills = lines_for(&started, ID)
                    .iter()
                    .filter(|line| line.ends_with(&health_kill_line()))
                    .count();
                (kills == restarts as usize).then_some(())
            },
        )
        .await;
        // The next process clears the startup health gate.
        health = common::answer_health_probes(&started, NODE, ID).await;
        wait_for_restarts(&started, ID, restarts).await;
        assert_eq!(
            serialized(&started, ID).restart_status.attempt,
            1,
            "kill {restarts} follows an answered run past the reset window"
        );
    }
    signal_process(serialized_pid(&started, ID), "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_health_kill_is_a_failure_even_when_the_node_exits_cleanly() {
    const NODE: &str = "polite_hung";
    const ID: &str = "polite_hung_inst";
    let started = start_core_node_with_supervision(kill_monitor(), instant_restart_backoff()).await;
    // The process marks the SIGTERM it took before it exits 0, so the kill
    // is seen to begin with SIGTERM.
    let marks = TempDir::new().expect("marks dir");
    let marker = marks.path().join("terminated");
    let run_cmd = format!(
        r#"["sh", "-c", "trap 'touch {}; exit 0' TERM; while true; do sleep 0.2; done"]"#,
        marker.display()
    );
    let _dir = add_node(&started, NODE, &run_cmd, None).await;
    let (_ready, health) = mocks(&started, NODE, ID).await;
    run(&started, NODE, ID, on_failure()).await;
    drop(health);
    wait_for_line(&started, &health_kill_line()).await;
    assert!(marker.exists(), "the kill began with SIGTERM");
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    wait_for_restarts(&started, ID, 1).await;
    assert!(
        !stack_log(&started).contains("finished: process exited cleanly"),
        "the clean exit on SIGTERM counts as the failure it is"
    );
    signal_process(serialized_pid(&started, ID), "-9");
}

/// A hung process that ignores SIGTERM is SIGKILLed once the force-kill
/// deadline passes, the grace plus the runtime's join and finalize budgets,
/// and not before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_instance_that_ignores_sigterm_is_killed_at_the_deadline() {
    const NODE: &str = "stubborn";
    const ID: &str = "stubborn_inst";
    let started = start_core_node_with_supervision_and_grace(
        kill_monitor(),
        instant_restart_backoff(),
        SHORT_GRACE_SECS,
    )
    .await;
    let _dir = add_node(
        &started,
        NODE,
        r#"["sh", "-c", "trap '' TERM; exec sleep 300"]"#,
        None,
    )
    .await;
    let (_ready, health) = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    drop(health);
    wait_for_line(&started, &kill_line()).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        serialized_pid(&started, ID),
        pid,
        "the process outlives SIGTERM until the deadline"
    );
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    let deadline = core_node::force_kill_deadline(Duration::from_secs(SHORT_GRACE_SECS));
    wait_for_restarts_within(&started, ID, 1, deadline + WAIT).await;
    // The kill line is written before SIGTERM and the `failed` line after the
    // SIGKILLed process is reaped, so the two bound the wait; each time is
    // written to the millisecond.
    let waited =
        logged_at(&started, ID, &health_kill_line()) - logged_at(&started, ID, &kill_line());
    let tolerance = chrono::TimeDelta::milliseconds(1);
    let deadline = chrono::TimeDelta::from_std(deadline).expect("a short deadline");
    assert!(
        waited + tolerance >= deadline,
        "SIGKILL waits the deadline: {waited} < {deadline}"
    );
    assert!(
        waited < deadline + chrono::TimeDelta::seconds(2),
        "SIGKILL follows the deadline: {waited} past {deadline}"
    );
    signal_process(serialized_pid(&started, ID), "-9");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unhealthy_instance_under_never_stays_running() {
    const NODE: &str = "unkillable";
    const ID: &str = "unkillable_inst";
    let started = start_core_node_with_supervision(kill_monitor(), instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let (_ready, health) = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, never()).await;
    drop(health);
    wait_for_line(&started, &unhealthy_line()).await;
    tokio::time::sleep(PAST_THE_KILL).await;
    assert!(
        !stack_log(&started).contains("killed"),
        "{}",
        stack_log(&started)
    );
    assert_eq!(state_of(&started, ID), Some(InstanceState::Running));
    assert!(!serialized(&started, ID).healthy);
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    wait_for_line(&started, "recovered after").await;
    assert!(serialized(&started, ID).healthy);
    signal_process(pid, "-9");
}

/// While two or more instances other than the core node's own run and none
/// of them is healthy, the daemon reads a messaging outage and kills none,
/// saying so once for each; once one of them answers again, the other is
/// killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_kill_is_skipped_in_a_messaging_outage() {
    const NODE: &str = "outage";
    const FIRST: &str = "outage_first";
    const SECOND: &str = "outage_second";
    let started = start_core_node_with_supervision(kill_monitor(), instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _first_ready = common::answer_ready_probe(&started, NODE, FIRST).await;
    let first_health = common::answer_health_probes(&started, NODE, FIRST).await;
    let _second_ready = common::answer_ready_probe(&started, NODE, SECOND).await;
    let second_health = common::answer_health_probes(&started, NODE, SECOND).await;
    settle().await;
    let first_pid = run(&started, NODE, FIRST, on_failure()).await;
    let second_pid = run(&started, NODE, SECOND, on_failure()).await;

    drop(first_health);
    drop(second_health);
    for id in [FIRST, SECOND] {
        wait_for_line(
            &started,
            &format!(
                "'{id}' of node 'outage:v1' answered no health check for {KILL_AFTER_SECS} s; not \
                 killed: all 2 running instances on this daemon other than the core node's own \
                 are unhealthy, which the daemon reads as a messaging outage"
            ),
        )
        .await;
    }
    tokio::time::sleep(PAST_THE_KILL).await;
    let log = stack_log(&started);
    assert!(!log.contains(&kill_line()), "{log}");
    assert_eq!(
        log.matches("not killed").count(),
        2,
        "one line per instance:\n{log}"
    );
    assert_eq!(state_of(&started, FIRST), Some(InstanceState::Running));
    assert_eq!(state_of(&started, SECOND), Some(InstanceState::Running));

    // One answer ends the outage, and the hold with it.
    let _first_health = common::answer_health_probes(&started, NODE, FIRST).await;
    wait_for_line(
        &started,
        &format!("'{SECOND}' of node '{NODE}:v1' {}", kill_line()),
    )
    .await;
    let _second_health = common::answer_health_probes(&started, NODE, SECOND).await;
    wait_for_restarts(&started, SECOND, 1).await;
    assert_eq!(
        serialized_pid(&started, FIRST),
        first_pid,
        "the answering instance runs on"
    );
    assert_ne!(serialized_pid(&started, SECOND), second_pid);
    signal_process(first_pid, "-9");
    signal_process(serialized_pid(&started, SECOND), "-9");
}

/// When a node the shutdown mock stands for exits, against its answer.
#[derive(Clone, Copy)]
enum ExitOnShutdown {
    /// After this long it begins to exit, then answers at once.
    BeforeAnswer(Duration),
    /// It answers, then exits after this long, as the runtime does: its
    /// hooks run after the answer.
    AfterAnswer(Duration),
}

/// The shutdown mock of `instance_id`: a node that answers its shutdown
/// request and exits cleanly, in the order `exit` says, by a SIGTERM to the
/// process its slot runs, which [`clean_on_term`] answers with exit 0.
async fn exiting_on_shutdown(
    started: &StartedCoreNode,
    name: &str,
    instance_id: &str,
    exit: ExitOnShutdown,
) -> Mock {
    let node_stack = started.node_stack.clone();
    let id = instance_id.to_owned();
    node_service_mock(
        started,
        name,
        instance_id,
        peppylib::messaging::SHUTDOWN_SERVICE,
        move |context| {
            let pid = node_stack
                .find_instance_and_entity_any_state(&Name::new(&id).expect("valid id"))
                .and_then(|(slot, _)| slot.pid());
            let terminate = move || {
                if let Some(pid) = pid {
                    signal_process(pid, "-TERM");
                }
            };
            async move {
                match exit {
                    ExitOnShutdown::BeforeAnswer(answer_after) => {
                        tokio::time::sleep(answer_after).await;
                        terminate();
                    }
                    ExitOnShutdown::AfterAnswer(exit_after) => {
                        tokio::spawn(async move {
                            tokio::time::sleep(exit_after).await;
                            terminate();
                        });
                    }
                }
                Ok(context.message().payload())
            }
        },
    )
    .await
}

/// A removal ends what it stops and holds restarts off. Under `always`, an
/// instance that exits on the removal's shutdown request while a slower
/// sibling's answer is awaited ends `finished` and schedules no restart, and
/// one that crashed just before the removal, whose restart falls due during
/// it, is removed with its node and never started again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_ends_what_it_stops_and_holds_restarts_off() {
    const NODE: &str = "held";
    const QUICK: &str = "held_quick_inst";
    const SLOW: &str = "held_slow_inst";
    const CRASHED: &str = "held_crashed_inst";
    let started = daemon(RestartBackoff {
        first_delay: HELD_RESTART_DELAY,
        max_delay: HELD_RESTART_DELAY,
        reset_after: Duration::MAX,
    })
    .await;
    let _dir = add_node(&started, NODE, &clean_on_term(), None).await;
    let _quick = mocks(&started, NODE, QUICK).await;
    let _slow = mocks(&started, NODE, SLOW).await;
    let _crashed = mocks(&started, NODE, CRASHED).await;
    let _quick_shutdown = exiting_on_shutdown(
        &started,
        NODE,
        QUICK,
        ExitOnShutdown::BeforeAnswer(Duration::ZERO),
    )
    .await;
    let _slow_shutdown = exiting_on_shutdown(
        &started,
        NODE,
        SLOW,
        ExitOnShutdown::BeforeAnswer(SLOW_SHUTDOWN_ANSWER),
    )
    .await;
    run(&started, NODE, QUICK, always()).await;
    run(&started, NODE, SLOW, always()).await;
    let crashed_pid = run(&started, NODE, CRASHED, always()).await;
    signal_process(crashed_pid, "-9");
    wait_for_line(&started, "restarting in 1 s (attempt 1)").await;

    remove_ok(&started, NODE, true).await;
    assert!(started.node_stack.find(NODE, TEST_NODE_TAG).is_none());
    let quick = lines_for(&started, QUICK);
    assert!(
        quick
            .last()
            .is_some_and(|line| line.ends_with("finished: process exited cleanly")),
        "the quick instance exited on its shutdown request during the removal: {quick:?}"
    );
    assert!(
        !quick.iter().any(|line| line.contains("restart")),
        "the exit the removal asked for schedules no restart: {quick:?}"
    );
    tokio::time::sleep(QUIET).await;
    let crashed = lines_for(&started, CRASHED);
    assert!(
        crashed
            .last()
            .is_some_and(|line| line.contains("restarting in 1 s (attempt 1)")),
        "the restart due during the removal never started: {crashed:?}"
    );
}

/// Sends a participant request as the launch's coordinator does, and gives
/// the response or the failure of the error reply.
async fn participant<R: core_node_api::ServiceRequest>(
    started: &StartedCoreNode,
    request: &R,
) -> Result<R::Response, Failure> {
    poll(
        request,
        &started.caller_handle,
        "coordinator",
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        request_budget(),
    )
    .await
    .map_err(|error| error_reply(error, R::ID.name()))
}

/// Asks this daemon to reserve itself for [`LAUNCH`], as its coordinator
/// does.
async fn try_reserve(
    started: &StartedCoreNode,
) -> Result<core_node_api::encoding::ParticipantReserveResponse, Failure> {
    participant(
        started,
        &core_node_api::encoding::ParticipantReserveRequest::new(LAUNCH, "coordinator"),
    )
    .await
}

/// Reserves this daemon for [`LAUNCH`]; a respawn waits until the release.
async fn reserve(started: &StartedCoreNode) {
    try_reserve(started)
        .await
        .unwrap_or_else(|failure| panic!("the reservation is refused: {failure}"));
}

async fn release(started: &StartedCoreNode) {
    let released = participant(
        started,
        &core_node_api::encoding::ParticipantReleaseRequest::new(LAUNCH),
    )
    .await;
    assert_eq!(released, Ok(FederationAck));
}

/// How a participant request changes this daemon's slice of [`LAUNCH`].
enum SliceChange<'a> {
    /// The slice begins anew and replaces the stack.
    Replace,
    /// The slice grows, as a join's does, naming each instance's watcher.
    Append(&'a [(&'a str, &'a str)]),
}

/// Begins or grows this daemon's slice of [`LAUNCH`].
async fn begin_slice(started: &StartedCoreNode, change: SliceChange<'_>) {
    let mut request =
        core_node_api::encoding::ParticipantSliceBeginRequest::new(LAUNCH, Vec::new());
    if let SliceChange::Append(watched) = change {
        request.append = true;
        for (instance_id, watcher) in watched {
            request.lifecycle_watchers.insert(
                name(instance_id),
                std::collections::BTreeSet::from(
                    [CoreNodeName::new(*watcher).expect("valid name")],
                ),
            );
        }
    }
    participant(started, &request)
        .await
        .unwrap_or_else(|failure| panic!("the slice does not begin: {failure}"));
}

/// A node answers its shutdown request before it exits; `peppy node remove`
/// returns once the process it stopped has exited, its SIGTERM handler run,
/// logs no exit of its own for it, and a child that ignores SIGTERM does not
/// outlive the removal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_instance_takes_its_forked_child_with_it() {
    const NODE: &str = "removed_forker";
    const ID: &str = "removed_forker_inst";
    /// How long after its answer the node exits: past the removal's claim,
    /// which follows the answer at once, and well inside the force-kill
    /// deadline.
    const EXIT_AFTER: Duration = Duration::from_secs(2);
    let started = daemon(instant_restart_backoff()).await;
    let handled = TempDir::new().expect("a marker dir");
    let marker = handled.path().join("handled");
    let run_cmd = format!(
        r#"["sh", "-c", "sh -c 'trap \"\" TERM; while true; do sleep 0.2; done' & trap 'touch {}; exit 0' TERM; while true; do sleep 0.2; done"]"#,
        marker.display()
    );
    let _dir = add_node(&started, NODE, &run_cmd, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let _shutdown =
        exiting_on_shutdown(&started, NODE, ID, ExitOnShutdown::AfterAnswer(EXIT_AFTER)).await;
    let pid = run(&started, NODE, ID, on_failure()).await;

    let response = remove_ok(&started, NODE, true).await;
    assert!(
        !common::is_process_running(pid),
        "the removal returned once the process it stopped exited"
    );
    assert!(
        marker.exists(),
        "the node's own SIGTERM handler ran before the removal killed its group"
    );
    assert!(
        response.force_killed.is_empty(),
        "{:?}",
        response.force_killed
    );
    assert_eq!(state_of(&started, ID), None, "the removal took the slot");
    assert!(
        lines_for(&started, ID).is_empty(),
        "an exit the removal claimed is its own, as a stop's is: {:?}",
        lines_for(&started, ID)
    );
    wait_for_group_gone(pid, "the removed instance's process").await;
}

/// `peppy node remove` tears its instances down together: two instances
/// whose watchers each take [`ACK`] to hear of the end are removed in under
/// twice that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_tells_its_instances_ends_together() {
    use core_node_api::encoding::{InstanceEnd, RelationshipEvent};
    const NODE: &str = "watched_pair";
    const FIRST: &str = "watched_first_inst";
    const SECOND: &str = "watched_second_inst";
    const OTHER_WATCHER: &str = "cloud_two";
    /// How long each watcher takes to acknowledge an event: told one after
    /// the other, the removal would take twice this.
    const ACK: Duration = Duration::from_secs(8);
    /// How long after its answer each node exits: past the removal's claim,
    /// which follows the answers at once.
    const EXIT_AFTER: Duration = Duration::from_secs(1);
    let started = daemon(instant_restart_backoff()).await;
    take_part_in_the_launch(&started).await;
    let _dir = add_node(&started, NODE, &clean_on_term(), None).await;
    let mut answering = Vec::new();
    let mut exiting = Vec::new();
    for id in [FIRST, SECOND] {
        answering.push(mocks(&started, NODE, id).await);
        exiting.push(
            exiting_on_shutdown(&started, NODE, id, ExitOnShutdown::AfterAnswer(EXIT_AFTER)).await,
        );
        run(&started, NODE, id, on_failure()).await;
    }
    reserve(&started).await;
    begin_slice(
        &started,
        SliceChange::Append(&[(FIRST, WATCHER), (SECOND, OTHER_WATCHER)]),
    )
    .await;
    release(&started).await;
    let (_first_watcher, mut first_heard) = watcher_on(&started, WATCHER, ACK).await;
    let (_second_watcher, mut second_heard) = watcher_on(&started, OTHER_WATCHER, ACK).await;

    let removing = std::time::Instant::now();
    remove_ok(&started, NODE, true).await;
    let took = removing.elapsed();
    for (heard, id) in [(&mut first_heard, FIRST), (&mut second_heard, SECOND)] {
        assert_eq!(
            next_event(heard, id).await,
            RelationshipEvent::Ended(InstanceEnd::Stopped)
        );
    }
    assert!(
        took >= ACK && took < ACK * 2,
        "both ends are told at once: {took:?}"
    );
}

/// `peppy node remove` kills what does not exit within the force-kill
/// deadline, as a stop does, and names it: a node that answers its shutdown
/// request and ignores SIGTERM, and one with no shutdown service, which the
/// removal finds unreachable; neither's group outlives the removal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_kills_what_does_not_exit_and_names_it() {
    const NODE: &str = "removed_stuck";
    const ANSWERING: &str = "stuck_answering_inst";
    const UNREACHABLE: &str = "stuck_unreachable_inst";
    let started = start_core_node_with_supervision_and_grace(
        fast_health_monitor(UNHEALTHY_AFTER),
        instant_restart_backoff(),
        SHORT_GRACE_SECS,
    )
    .await;
    let run_cmd = r#"["sh", "-c", "sh -c 'trap \"\" TERM; while true; do sleep 0.2; done' & trap '' TERM; while true; do sleep 0.2; done"]"#;
    let _dir = add_node(&started, NODE, run_cmd, None).await;
    let _answering = mocks(&started, NODE, ANSWERING).await;
    let _unreachable = mocks(&started, NODE, UNREACHABLE).await;
    let _shutdown = exiting_on_shutdown(
        &started,
        NODE,
        ANSWERING,
        ExitOnShutdown::AfterAnswer(Duration::ZERO),
    )
    .await;
    let answering = run(&started, NODE, ANSWERING, on_failure()).await;
    let unreachable = run(&started, NODE, UNREACHABLE, on_failure()).await;

    let removing = std::time::Instant::now();
    let response = remove_ok(&started, NODE, true).await;
    let deadline = core_node::force_kill_deadline(Duration::from_secs(SHORT_GRACE_SECS));
    assert!(
        removing.elapsed() >= deadline,
        "the removal waits the force-kill deadline before it kills: {:?}",
        removing.elapsed()
    );
    let mut force_killed = response.force_killed.clone();
    force_killed.sort();
    assert_eq!(force_killed, [ANSWERING, UNREACHABLE]);
    for pid in [answering, unreachable] {
        assert!(!common::is_process_running(pid), "{pid} runs on");
        wait_for_group_gone(pid, "a removed instance's process").await;
    }
    assert!(started.node_stack.find(NODE, TEST_NODE_TAG).is_none());
}

/// A removal that finds an instance another stop claimed returns once that
/// stop's process is gone, and reports no kill: the stop's force kill ends
/// the node that ignores its shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_waits_for_the_process_another_stop_ends() {
    const NODE: &str = "stopped_and_removed";
    const ID: &str = "stopped_and_removed_inst";
    let started = start_core_node_with_supervision_and_grace(
        fast_health_monitor(UNHEALTHY_AFTER),
        instant_restart_backoff(),
        SHORT_GRACE_SECS,
    )
    .await;
    let _dir = add_node(&started, NODE, IGNORES_TERM, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, on_failure()).await;

    let removal_after_the_stops_claim = async {
        wait_for_claim(&started, ID).await;
        let removal = remove_ok(&started, NODE, true).await;
        (removal, common::is_process_running(pid))
    };
    let (stopped, (removal, running_at_the_removals_return)) =
        tokio::join!(stop_ok(&started, ID), removal_after_the_stops_claim);
    assert!(
        !running_at_the_removals_return,
        "the removal returned once the stopped process was gone"
    );
    assert!(
        removal.force_killed.is_empty(),
        "the stop's kill is not the removal's: {:?}",
        removal.force_killed
    );
    assert!(stopped.force_killed, "the stop force-killed the node");
    assert!(started.node_stack.find(NODE, TEST_NODE_TAG).is_none());
}

/// A stop that finds an instance a removal claimed joins that claim: it
/// returns once the process is gone, which the removal's force kill ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_joins_the_removal_that_claimed_its_process() {
    const NODE: &str = "removed_and_stopped";
    const ID: &str = "removed_and_stopped_inst";
    let started = start_core_node_with_supervision_and_grace(
        fast_health_monitor(UNHEALTHY_AFTER),
        instant_restart_backoff(),
        SHORT_GRACE_SECS,
    )
    .await;
    let _dir = add_node(&started, NODE, IGNORES_TERM, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, on_failure()).await;

    let stop_after_the_removals_claim = async {
        wait_for_claim(&started, ID).await;
        stop_ok(&started, ID).await;
        common::is_process_running(pid)
    };
    let (removal, running_at_the_stops_return) = tokio::join!(
        remove_ok(&started, NODE, true),
        stop_after_the_removals_claim
    );
    assert!(
        !running_at_the_stops_return,
        "the stop returned once the process was gone"
    );
    assert_eq!(removal.force_killed, [ID]);
    assert!(started.node_stack.find(NODE, TEST_NODE_TAG).is_none());
}

/// A stop that joined an overwrite's claim, and still waits on its own
/// shutdown request when a new run under the id commits in the entity the
/// overwrite replaced in place, leaves that run alone: it removes no slot
/// running a process it did not stop, and tears nothing down. The daemon
/// gives the stop's request its 5 s shutdown timeout, which the overwrite's
/// end, the node's build and the new run fit inside.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_joined_stop_leaves_a_new_run_under_the_id_alone() {
    use core_node_api::encoding::RelationshipEvent;
    use std::sync::atomic::{AtomicBool, Ordering};
    const NODE: &str = "run_again_under_a_stop";
    const ID: &str = "run_again_under_a_stop_inst";
    let started = daemon(instant_restart_backoff()).await;
    let dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    // The overwrite's request is answered; the stop's, the second, is held.
    let shutdowns =
        held_requests(&started, NODE, ID, peppylib::messaging::SHUTDOWN_SERVICE, 1).await;
    let first = run(&started, NODE, ID, never()).await;

    let stop_returned = AtomicBool::new(false);
    let requests = &shutdowns.asked;
    let asked = |count: usize| {
        poll_until_explained(
            WAIT,
            move || format!("{count} shutdown requests"),
            move || (requests.load(Ordering::SeqCst) >= count).then_some(()),
        )
    };
    let stop_after_the_overwrites_request = async {
        wait_for_claim(&started, ID).await;
        asked(1).await;
        stop_ok(&started, ID).await;
        stop_returned.store(true, Ordering::SeqCst);
    };
    let run_again_while_the_stop_waits = async {
        let kill_once_the_stop_asks = async {
            asked(2).await;
            signal_process(first, "-9");
        };
        let (overwritten, ()) =
            tokio::join!(overwrite(&started, dir.path()), kill_once_the_stop_asks);
        overwritten.completed();
        common::build_staged_node(&started, NODE, TEST_NODE_TAG).await;
        let watched = watcher(&started, Duration::ZERO).await;
        let result = common::send_node_run_goal_and_wait(
            &started.caller_handle,
            &started.core_node_name,
            common::node_run_goal(
                common::default_instance_plan(ID),
                NODE,
                TEST_NODE_TAG,
                &TIMEOUTS,
            )
            .with_lifecycle_watchers(vec![WATCHER.to_owned()]),
            &TIMEOUTS,
        )
        .await
        .expect("node_run completes")
        .end
        .completed();
        assert!(
            !stop_returned.load(Ordering::SeqCst),
            "the joined stop still waits on its shutdown request, which the daemon gives up \
             after its 5 s shutdown timeout, when the run commits"
        );
        shutdowns.release.notify_one();
        (result.pid.expect("a started instance has a pid"), watched)
    };
    let ((), (second, (_watcher, mut heard))) = tokio::join!(
        stop_after_the_overwrites_request,
        run_again_while_the_stop_waits
    );
    assert_eq!(state_of(&started, ID), Some(InstanceState::Running));
    assert_eq!(serialized_pid(&started, ID), second);
    assert!(
        common::is_process_running(second),
        "the new run's process runs on"
    );
    assert_eq!(
        next_event(&mut heard, ID).await,
        RelationshipEvent::ReachedRunning
    );
    assert!(
        heard.try_recv().is_err(),
        "the stop announces no end of the new run"
    );
    signal_process(second, "-9");
}

/// A node service mock that holds requests: the requests it got, and what
/// releases the held ones.
struct HeldRequests {
    _mock: Mock,
    /// Every request it got.
    asked: Arc<std::sync::atomic::AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
}

/// The `service` mock of `instance_id`: it answers its first `answered_first`
/// requests at once, then holds each later one until `release` is notified,
/// counting every request.
async fn held_requests(
    started: &StartedCoreNode,
    name: &str,
    instance_id: &str,
    service: &str,
    answered_first: usize,
) -> HeldRequests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let asked = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let mock = node_service_mock(started, name, instance_id, service, {
        let asked = Arc::clone(&asked);
        let release = Arc::clone(&release);
        move |context| {
            let held = asked.fetch_add(1, Ordering::SeqCst) >= answered_first;
            let release = Arc::clone(&release);
            async move {
                if held {
                    release.notified().await;
                }
                Ok(context.message().payload())
            }
        }
    })
    .await;
    HeldRequests {
        _mock: mock,
        asked,
        release,
    }
}

/// Waits until a stop path claimed `instance_id`.
async fn wait_for_claim(started: &StartedCoreNode, instance_id: &str) {
    poll_until_explained(
        WAIT,
        || format!("a stop path claims `{instance_id}`"),
        || {
            slot_of(started, instance_id)
                .is_some_and(|slot| slot.is_stopping())
                .then_some(())
        },
    )
    .await;
}

/// A restart's start that fails while a removal waits out another
/// instance's force-kill deadline is cleared with the node, and nothing
/// restarts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarts_start_that_fails_during_a_removal_is_cleared() {
    use core_node_api::encoding::{InstanceEnd, RelationshipEvent};
    const NODE: &str = "failing_restart";
    const STUCK: &str = "failing_restart_stuck_inst";
    const RESTARTING: &str = "failing_restart_inst";
    let started = start_core_node_with_supervision_and_grace(
        fast_health_monitor(UNHEALTHY_AFTER),
        instant_restart_backoff(),
        SHORT_GRACE_SECS,
    )
    .await;
    take_part_in_the_launch(&started).await;
    let (_watcher, mut heard) = watcher(&started, Duration::ZERO).await;
    let _dir = add_node(&started, NODE, IGNORES_TERM, None).await;
    let _stuck_mocks = mocks(&started, NODE, STUCK).await;
    let stuck = run(&started, NODE, STUCK, on_failure()).await;
    let _health = common::answer_health_probes(&started, NODE, RESTARTING).await;
    let ready = common::answer_ready_probe(&started, NODE, RESTARTING).await;
    let first = run(&started, NODE, RESTARTING, on_failure()).await;
    watch(&started, RESTARTING).await;
    // Without a ready answer the restart's start sits in its ready wait.
    drop(ready);
    signal_process(first, "-9");
    assert_eq!(
        next_event(&mut heard, RESTARTING).await,
        RelationshipEvent::Ended(InstanceEnd::Exited)
    );
    let starting = wait_for_process(&started, RESTARTING, InstanceState::Starting).await;

    let fail_the_start_during_the_removal = async {
        wait_for_claim(&started, STUCK).await;
        signal_process(starting, "-9");
        wait_for_state(&started, RESTARTING, InstanceState::Restarting).await;
    };
    let (removal, ()) = tokio::join!(
        remove_ok(&started, NODE, true),
        fail_the_start_during_the_removal
    );
    assert_eq!(removal.force_killed, [STUCK]);
    assert!(!common::is_process_running(stuck));
    assert!(started.node_stack.find(NODE, TEST_NODE_TAG).is_none());
    assert_eq!(
        next_event(&mut heard, RESTARTING).await,
        RelationshipEvent::Ended(InstanceEnd::Stopped),
        "the cleared instance's watchers hear it end"
    );
    tokio::time::sleep(QUIET).await;
    assert_eq!(state_of(&started, RESTARTING), None, "nothing restarts it");
}

/// A first start in flight when a removal begins, which commits while the
/// removal waits out another instance's force-kill deadline, keeps the
/// node: the removal fails naming it and the instance it force-killed, and
/// the committed process runs on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_start_that_commits_during_a_removal_keeps_the_node() {
    const NODE: &str = "committing_start";
    const STUCK: &str = "committing_start_stuck_inst";
    const LATE: &str = "committing_start_inst";
    let started = start_core_node_with_supervision_and_grace(
        fast_health_monitor(UNHEALTHY_AFTER),
        instant_restart_backoff(),
        SHORT_GRACE_SECS,
    )
    .await;
    let _dir = add_node(&started, NODE, IGNORES_TERM, None).await;
    let _stuck_mocks = mocks(&started, NODE, STUCK).await;
    let stuck = run(&started, NODE, STUCK, on_failure()).await;
    let _health = common::answer_health_probes(&started, NODE, LATE).await;

    let remove_during_the_start = async {
        let starting = wait_for_process(&started, LATE, InstanceState::Starting).await;
        let commit_after_the_claim = async {
            wait_for_claim(&started, STUCK).await;
            let ready = common::answer_ready_probe(&started, NODE, LATE).await;
            wait_for_state(&started, LATE, InstanceState::Running).await;
            ready
        };
        let (removal, _ready) = tokio::join!(remove(&started, NODE, true), commit_after_the_claim);
        (starting, removal)
    };
    let (late, (starting, removal)) = tokio::join!(
        run(&started, NODE, LATE, on_failure()),
        remove_during_the_start
    );
    assert_eq!(late, starting);
    let removal = removal.expect_err("the removal fails");
    assert_failure(
        &removal,
        PeppyFailureKind::PreconditionFailed,
        &format!(
            "; `{LATE}` runs a process this removal did not stop; run `peppy node remove \
             {NODE}:{TEST_NODE_TAG} --stop-instances --core-node {}` again",
            started.core_node_name
        ),
    );
    // A failed removal names what it force-killed.
    assert!(
        removal.message().ends_with(&format!(
            "; the removal force-killed {STUCK}, which did not exit within the shutdown grace"
        )),
        "{removal}"
    );
    assert!(!common::is_process_running(stuck));
    assert!(
        common::is_process_running(late),
        "the committed start runs on"
    );
    assert_eq!(state_of(&started, LATE), Some(InstanceState::Running));
    signal_process(late, "-9");
}

/// A set a join or removal sends for an instance this daemon tracks without
/// a process, outside a restart's wait, is refused naming the slot and the
/// instance's state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_for_an_exited_instance_names_its_state() {
    use core_node_api::encoding::{ParticipantSetsUpdateRequest, SlotMembers, SlotSet};
    const NODE: &str = "exited_consumer";
    const ID: &str = "exited_consumer_inst";
    let started = daemon(instant_restart_backoff()).await;
    take_part_in_the_launch(&started).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    signal_process(run(&started, NODE, ID, never()).await, "-9");
    wait_for_state(&started, ID, InstanceState::Failed).await;

    reserve(&started).await;
    let verdict = participant(
        &started,
        &ParticipantSetsUpdateRequest {
            launch_id: LAUNCH.to_owned(),
            sets: vec![SlotSet {
                instance_id: name(ID),
                link_id: "producers".to_owned(),
                members: SlotMembers::Producers(config::runtime::BoundProducers::from(
                    ProducerRef::new(&started.core_node_name, "producer_inst"),
                )),
            }],
        },
    )
    .await;
    assert_eq!(
        verdict,
        Err(Failure::new(
            PeppyFailureKind::AttemptFailed,
            format!("`{ID}.links.producers`: the instance is failed")
        ))
    );
    release(&started).await;
}

/// A set delivered to an instance whose process exits as it takes the set
/// is kept on its slot for its next process, a producer set and an observed
/// one alike: the node answers the delivery with an error only once its
/// process is gone and the slot awaits its restart, and the set's update
/// succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_whose_delivery_fails_as_its_instance_exits_is_kept_for_the_restart() {
    use core_node_api::encoding::{
        ObservationTarget, ObservationTargets, ParticipantSetsUpdateRequest, SlotMembers, SlotSet,
    };
    const NODE: &str = "exiting_consumer";
    const CONSUMER: &str = "exiting_consumer_inst";
    const OBSERVER: &str = "exiting_observer_inst";
    let started = daemon(parked_restart_backoff()).await;
    take_part_in_the_launch(&started).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let producers = config::runtime::BoundProducers::from(ProducerRef::new(
        &started.core_node_name,
        "producer_inst",
    ));
    let observed = ObservationTargets::new(
        "sources",
        vec![ObservationTarget {
            source: ProducerRef::new(&started.core_node_name, "source_inst"),
            source_link_id: "out".to_owned(),
            peer: None,
        }],
    )
    .expect("one source");
    for (id, service, set) in [
        (
            CONSUMER,
            peppylib::messaging::BINDING_UPDATE_SERVICE,
            SlotSet {
                instance_id: name(CONSUMER),
                link_id: "producers".to_owned(),
                members: SlotMembers::Producers(producers.clone()),
            },
        ),
        (
            OBSERVER,
            peppylib::messaging::OBSERVATION_UPDATE_SERVICE,
            SlotSet {
                instance_id: name(OBSERVER),
                link_id: "sources".to_owned(),
                members: SlotMembers::Observed(observed.clone()),
            },
        ),
    ] {
        let _mocks = mocks(&started, NODE, id).await;
        let pid = run(&started, NODE, id, on_failure()).await;
        let _exiting = node_service_exiting(&started, NODE, id, pid, service).await;
        reserve(&started).await;
        let verdict = participant(
            &started,
            &ParticipantSetsUpdateRequest {
                launch_id: LAUNCH.to_owned(),
                sets: vec![set],
            },
        )
        .await;
        release(&started).await;
        assert_eq!(verdict, Ok(FederationAck), "`{id}`");
        assert_eq!(state_of(&started, id), Some(InstanceState::Restarting));
    }
    assert_eq!(
        serialized(&started, CONSUMER)
            .slot_bindings
            .get("producers"),
        Some(&producers),
        "the producer set waits on the slot for the next process"
    );
    let (_, entity) = started
        .node_stack
        .find_instance_and_entity_any_state(&name(OBSERVER))
        .expect("the observer is tracked");
    let respawn = node_stack::NodeEntity::respawn_inputs_of(&entity, &name(OBSERVER))
        .expect("the observer awaits its restart");
    assert_eq!(
        respawn.goal.planned_observations.get("sources"),
        Some(&observed),
        "the observed set waits on the slot for the next process"
    );
}

/// The `service` of `instance_id` (`binding_update` or
/// `observation_update`), which on a set kills the process `pid`, waits
/// until the slot awaits its restart, and then answers with an error, as a
/// node that exits as it takes the set.
async fn node_service_exiting(
    started: &StartedCoreNode,
    name: &str,
    instance_id: &str,
    pid: u32,
    service: &str,
) -> Mock {
    let node_stack = started.node_stack.clone();
    let id = instance_id.to_owned();
    node_service_mock(started, name, instance_id, service, move |_context| {
        let node_stack = node_stack.clone();
        let instance_id = id.clone();
        async move {
            signal_process(pid, "-9");
            poll_until(WAIT, "the slot awaits its restart", || {
                (node_stack.instance_state(&instance_id) == Some(InstanceState::Restarting))
                    .then_some(())
            })
            .await;
            Err(Failure::new(
                PeppyFailureKind::Rejected,
                "the node exited as it took the set",
            ))
        }
    })
    .await
}

/// Makes this daemon a participant of [`LAUNCH`] that holds no instance of
/// it, so a later append can name watchers. The slice it begins replaces
/// the stack.
async fn take_part_in_the_launch(started: &StartedCoreNode) {
    reserve(started).await;
    begin_slice(started, SliceChange::Replace).await;
    release(started).await;
}

/// Names [`WATCHER`] the watcher of `instance_id`, as a join does.
async fn watch(started: &StartedCoreNode, instance_id: &str) {
    reserve(started).await;
    begin_slice(started, SliceChange::Append(&[(instance_id, WATCHER)])).await;
    release(started).await;
}

type Heard =
    tokio::sync::mpsc::UnboundedReceiver<core_node_api::encoding::RelationshipNotification>;

/// The lifecycle events [`WATCHER`] hears, each acknowledged after
/// `ack_after`.
async fn watcher(started: &StartedCoreNode, ack_after: Duration) -> (Mock, Heard) {
    watcher_on(started, WATCHER, ack_after).await
}

/// The lifecycle events the daemon `core_node` hears, each acknowledged
/// after `ack_after`.
async fn watcher_on(
    started: &StartedCoreNode,
    core_node: &str,
    ack_after: Duration,
) -> (Mock, Heard) {
    use core_node_api::encoding::{RelationshipNotification, RelationshipNotificationAck};
    let (events, heard) = tokio::sync::mpsc::unbounded_channel();
    let mock = peer_daemon_service(
        started,
        core_node,
        core_node_api::ServiceId::RelationshipNotify,
        move |context| {
            let event =
                RelationshipNotification::decode(context.message().payload_bytes().as_ref())
                    .expect("a notification decodes");
            events.send(event).expect("the test reads the events");
            async move {
                tokio::time::sleep(ack_after).await;
                Ok(RelationshipNotificationAck::new()
                    .encode()
                    .expect("an acknowledgement encodes"))
            }
        },
    )
    .await;
    (mock, heard)
}

/// The next event `heard` carries, which must be about `instance_id`.
async fn next_event(
    heard: &mut Heard,
    instance_id: &str,
) -> core_node_api::encoding::RelationshipEvent {
    let notification = tokio::time::timeout(WAIT, heard.recv())
        .await
        .expect("the watcher hears an event")
        .expect("the watcher still listens");
    assert_eq!(notification.instance.instance_id, instance_id);
    notification.event
}

/// The daemons that watch an instance hear its exit as an exit, then its
/// restart; its next process keeps them, and a stop is heard as a stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_watchers_of_an_instance_hear_it_exit_and_run_again() {
    use core_node_api::encoding::{InstanceEnd, RelationshipEvent};
    const NODE: &str = "watched";
    const ID: &str = "watched_inst";
    let started = daemon(instant_restart_backoff()).await;
    // A slice that is not an append replaces the stack, so the daemon
    // takes part in the launch before it runs anything.
    take_part_in_the_launch(&started).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let (_watcher, mut heard) = watcher(&started, Duration::ZERO).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    watch(&started, ID).await;

    for restarts in 1..=2 {
        signal_process(serialized_pid(&started, ID), "-9");
        assert_eq!(
            next_event(&mut heard, ID).await,
            RelationshipEvent::Ended(InstanceEnd::Exited)
        );
        assert_eq!(
            next_event(&mut heard, ID).await,
            RelationshipEvent::ReachedRunning
        );
        wait_for_restarts(&started, ID, restarts).await;
    }
    assert_ne!(serialized_pid(&started, ID), pid);
    stop_ok(&started, ID).await;
    assert_eq!(
        next_event(&mut heard, ID).await,
        RelationshipEvent::Ended(InstanceEnd::Stopped)
    );
}

/// Watchers a join names while the instance waits for its restart hear the
/// restart: the reservation of the join holds the respawn until it is
/// released, so the watchers land on the waiting slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watchers_named_during_the_wait_hear_the_restart() {
    use core_node_api::encoding::RelationshipEvent;
    const NODE: &str = "watched_later";
    const ID: &str = "watched_later_inst";
    let started = daemon(instant_restart_backoff()).await;
    // A slice that is not an append replaces the stack, so the daemon
    // takes part in the launch before it runs anything.
    take_part_in_the_launch(&started).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let (_watcher, mut heard) = watcher(&started, Duration::ZERO).await;
    let pid = run(&started, NODE, ID, on_failure()).await;

    reserve(&started).await;
    signal_process(pid, "-9");
    wait_for_state(&started, ID, InstanceState::Restarting).await;
    begin_slice(&started, SliceChange::Append(&[(ID, WATCHER)])).await;
    release(&started).await;
    wait_for_restarts(&started, ID, 1).await;
    assert_eq!(
        next_event(&mut heard, ID).await,
        RelationshipEvent::ReachedRunning
    );
    signal_process(serialized_pid(&started, ID), "-9");
}

/// The next process starts once the exit's announcement ends, and the wait
/// before it counts from the exit: a watcher slow to answer holds the
/// restart until it answers, and its answer time comes off the wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn announcing_the_exit_counts_toward_the_wait() {
    const NODE: &str = "slowly_watched";
    const ID: &str = "slowly_watched_inst";
    /// The wait before every attempt.
    const DELAY: Duration = Duration::from_secs(8);
    /// How long the watcher takes to answer: past `DELAY`, so the wait is
    /// spent by the time the announcement ends; a wait that followed the
    /// announcement would add `DELAY` to it.
    const ACK: Duration = Duration::from_secs(9);
    let started = daemon(RestartBackoff {
        first_delay: DELAY,
        max_delay: DELAY,
        reset_after: Duration::MAX,
    })
    .await;
    // A slice that is not an append replaces the stack, so the daemon
    // takes part in the launch before it runs anything.
    take_part_in_the_launch(&started).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let (_watcher, mut heard) = watcher(&started, ACK).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    watch(&started, ID).await;

    signal_process(pid, "-9");
    assert_eq!(
        next_event(&mut heard, ID).await,
        core_node_api::encoding::RelationshipEvent::Ended(
            core_node_api::encoding::InstanceEnd::Exited
        ),
        "the watcher hears the exit it answers slowly"
    );
    wait_for_restarts_within(&started, ID, 1, DELAY + ACK + WAIT).await;
    let took =
        logged_at(&started, ID, "restarted (attempt 1)") - logged_at(&started, ID, "failed:");
    assert!(
        took >= chrono::TimeDelta::from_std(ACK).expect("a short answer"),
        "the restart follows the announcement: {took}"
    );
    assert!(
        took < chrono::TimeDelta::from_std(ACK + DELAY).expect("a short wait"),
        "the announcement is inside the wait: {took}"
    );
    signal_process(serialized_pid(&started, ID), "-9");
}

/// Adds a node whose instances play the follower of `joint_link` on
/// [`FOLLOWER_SLOT`], with the pairing in the daemon's package cache.
async fn add_follower(started: &StartedCoreNode, name: &str) -> TempDir {
    add_follower_taking(started, name, Cardinality::ZeroOrOne).await
}

/// [`add_follower`] whose slot takes `cardinality` peers.
async fn add_follower_taking(
    started: &StartedCoreNode,
    name: &str,
    cardinality: Cardinality,
) -> TempDir {
    add_joint_link_node(started, name, JointLinkSide::Follower, cardinality, SLEEPER).await
}

/// The side of `joint_link` a test node plays.
#[derive(Clone, Copy)]
enum JointLinkSide {
    Leader,
    Follower,
}

impl JointLinkSide {
    fn role(self) -> &'static str {
        match self {
            JointLinkSide::Leader => "leader",
            JointLinkSide::Follower => "follower",
        }
    }

    /// The slot the side's node pairs on.
    fn link_id(self) -> &'static str {
        match self {
            JointLinkSide::Leader => LEADER_SLOT,
            JointLinkSide::Follower => FOLLOWER_SLOT,
        }
    }

    /// The topics the side emits and consumes.
    fn topics(self) -> (&'static str, &'static str) {
        match self {
            JointLinkSide::Leader => ("joint_setpoints", "joint_states"),
            JointLinkSide::Follower => ("joint_states", "joint_setpoints"),
        }
    }
}

/// Adds a node playing `side` of `joint_link` on a slot that takes
/// `cardinality` peers and running `run_cmd`, with the pairing in the
/// daemon's package cache.
async fn add_joint_link_node(
    started: &StartedCoreNode,
    name: &str,
    side: JointLinkSide,
    cardinality: Cardinality,
    run_cmd: &str,
) -> TempDir {
    let pairing = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/launch_assets/waldo_codegen/pairings/joint_link.json5");
    let body = std::fs::read_to_string(&pairing).expect("read the joint_link pairing fixture");
    common::TestPackagesCache::new()
        .pairing_fs_entry("joint_link", "v1", &pairing, &body)
        .write(&started.peppy_dirs);
    let (role, link_id) = (side.role(), side.link_id());
    let (emits, consumes) = side.topics();
    let cardinality = cardinality.as_str();
    add_node_with_manifest(
        started,
        &format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{
                    name: "{name}",
                    tag: "{TEST_NODE_TAG}",
                    depends_on: {{ pairings: [{{
                        name: "joint_link", tag: "v1", role: "{role}",
                        link_id: "{link_id}", cardinality: "{cardinality}"
                    }}] }}
                }},
                interfaces: {{ topics: {{
                    emits: [{{ link_id: "{link_id}", name: "{emits}" }}],
                    consumes: [{{ link_id: "{link_id}", name: "{consumes}" }}]
                }} }},
                execution: {{ language: "rust", run_cmd: {run_cmd} }}
            }}"#
        ),
    )
    .await
}

/// A failed start kills its process group before it dissolves the pairs
/// reserved for it: while the dissolve waits on the local peer's answer to
/// its set without the pair, the child the failed process forked is
/// already gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_start_kills_its_group_before_it_dissolves_its_pairs() {
    use std::sync::atomic::Ordering;
    /// Well inside the 5 s the daemon waits for a node's answer to a set it
    /// sends, which the dissolve waits out while the leader's mock holds it.
    const KILLED_WHILE_THE_SET_IS_HELD: Duration = Duration::from_secs(2);
    const LEADER_NODE: &str = "local_leader";
    const LEADER_ID: &str = "local_leader_inst";
    const NODE: &str = "forks_then_fails";
    const ID: &str = "forks_then_fails_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _leader_dir = add_joint_link_node(
        &started,
        LEADER_NODE,
        JointLinkSide::Leader,
        Cardinality::ZeroOrMore,
        SLEEPER,
    )
    .await;
    let _leader_mocks = mocks(&started, LEADER_NODE, LEADER_ID).await;
    run(&started, LEADER_NODE, LEADER_ID, never()).await;
    let held_pins = held_requests(
        &started,
        LEADER_NODE,
        LEADER_ID,
        peppylib::messaging::PEER_UPDATE_SERVICE,
        0,
    )
    .await;

    // The follower forks a child that ignores SIGTERM and exits before it
    // signals ready, so its start fails with its pair to the leader reserved.
    let markers = TempDir::new().expect("marker dir");
    let child_pidfile = markers.path().join("child.pid");
    let run_cmd = format!(
        r#"["sh", "-c", "sh -c 'echo $$ > {}; trap \"\" TERM; while true; do sleep 0.1; done' & sleep 1; exit 3"]"#,
        child_pidfile.display()
    );
    let _dir = add_joint_link_node(
        &started,
        NODE,
        JointLinkSide::Follower,
        Cardinality::ZeroOrOne,
        &run_cmd,
    )
    .await;
    let follower_start = common::send_node_run_goal_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        common::node_run_goal(
            common::default_instance_plan(ID),
            NODE,
            TEST_NODE_TAG,
            &TIMEOUTS,
        )
        .with_requested_pairs(std::collections::BTreeMap::from([(
            FOLLOWER_SLOT.to_owned(),
            vec![core_node_api::encoding::PairTarget {
                peer: ProducerRef::new(&started.core_node_name, LEADER_ID),
                peer_link_id: Some(LEADER_SLOT.to_owned()),
                remote_peer: None,
            }],
        )])),
        &TIMEOUTS,
    );
    let child_gone_while_the_dissolve_waits = async {
        poll_until(WAIT, "the dissolve sends the leader its set", || {
            (held_pins.asked.load(Ordering::SeqCst) >= 1).then_some(())
        })
        .await;
        let child: u32 = std::fs::read_to_string(&child_pidfile)
            .expect("the forked child wrote its pid")
            .trim()
            .parse()
            .expect("a pid");
        poll_until(
            KILLED_WHILE_THE_SET_IS_HELD,
            "the forked child is gone while the leader's set is held",
            || (!common::is_process_running(child)).then_some(()),
        )
        .await;
        held_pins.release.notify_waiters();
    };
    let (result, ()) = tokio::join!(follower_start, child_gone_while_the_dissolve_waits);
    // The start fails.
    result.expect("node_run completes").end.failed();
}

/// Runs follower `instance_id` of `name` with its slot vacant until a peer
/// daemon pairs it, and returns its pid.
async fn run_follower(
    started: &StartedCoreNode,
    name: &str,
    instance_id: &str,
    restart: RestartRequest,
) -> u32 {
    let goal = common::node_run_goal(
        common::default_instance_plan(instance_id),
        name,
        TEST_NODE_TAG,
        &TIMEOUTS,
    )
    .with_vacant_pairs(std::collections::BTreeMap::from([(
        FOLLOWER_SLOT.to_owned(),
        "paired by a peer daemon".to_owned(),
    )]))
    .with_restart(restart);
    let result = common::send_node_run_goal_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        goal,
        &TIMEOUTS,
    )
    .await
    .expect("node_run completes")
    .end;
    result
        .completed()
        .pid
        .expect("a started instance has a pid")
}

/// The `peer_update` mock of `instance_id`'s follower slot, which takes every
/// pin the daemon delivers, and the watch that reads them.
async fn mock_peer_update(
    started: &StartedCoreNode,
    name: &str,
    instance_id: &str,
) -> (
    Mock,
    tokio::sync::watch::Receiver<peppylib::messaging::PeerSetState>,
) {
    mock_peer_update_taking(started, name, instance_id, Cardinality::ZeroOrOne).await
}

/// [`mock_peer_update`] of a slot that takes `cardinality` peers.
async fn mock_peer_update_taking(
    started: &StartedCoreNode,
    name: &str,
    instance_id: &str,
    cardinality: Cardinality,
) -> (
    Mock,
    tokio::sync::watch::Receiver<peppylib::messaging::PeerSetState>,
) {
    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let (pins, watch) = tokio::sync::watch::channel(peppylib::messaging::PeerSetState::empty());
    let slots = Arc::new(std::collections::BTreeMap::from([(
        FOLLOWER_SLOT.to_owned(),
        peppylib::services::slot_update::SlotChannel::new(cardinality, pins),
    )]));
    let mock = AbortOnDrop(
        peppylib::services::peer_update::listen_for_peer_update(
            &messenger,
            &started.core_node_name,
            instance_id,
            common::test_node_target(name),
            slots,
        )
        .await
        .expect("peer_update mock starts"),
    );
    settle().await;
    (mock, watch)
}

/// What [`WATCHER`]'s daemon asks when it pairs [`LEADER`] with
/// `instance_id`'s follower slot: as a start's link, or `held` as
/// [`LEADER`]'s restart asks for the pair again.
fn pair_commit_from_watcher(
    started: &StartedCoreNode,
    instance_id: &str,
    held: bool,
) -> core_node_api::encoding::PairCommitRequest {
    pair_commit_of(started, instance_id, LEADER, held)
}

/// What [`WATCHER`]'s daemon asks when it pairs its instance `leader` with
/// `instance_id`'s follower slot.
fn pair_commit_of(
    started: &StartedCoreNode,
    instance_id: &str,
    leader: &str,
    held: bool,
) -> core_node_api::encoding::PairCommitRequest {
    core_node_api::encoding::PairCommitRequest {
        pairing_name: "joint_link".to_owned(),
        pairing_tag: "v1".to_owned(),
        local: ProducerRef::new(started.core_node_name.clone(), instance_id),
        local_link_id: FOLLOWER_SLOT.to_owned(),
        local_role: "follower".to_owned(),
        peer: ProducerRef::new(WATCHER, leader),
        peer_link_id: LEADER_SLOT.to_owned(),
        peer_role: "leader".to_owned(),
        peer_cardinality: config::node::Cardinality::ZeroOrMore,
        peer_copy: None,
        held,
    }
}

/// [`WATCHER`]'s daemon asks this one to pair [`LEADER`] with
/// `instance_id`, as [`LEADER`]'s link at its start does.
async fn pair_from_watcher(started: &StartedCoreNode, instance_id: &str) -> Verdict {
    ask_pair_from_watcher(started, instance_id, false).await
}

/// [`WATCHER`]'s daemon asks this one again for the pair of [`LEADER`] with
/// `instance_id`, as [`LEADER`]'s restart does for a pair it held.
async fn pair_again_from_watcher(started: &StartedCoreNode, instance_id: &str) -> Verdict {
    ask_pair_from_watcher(started, instance_id, true).await
}

async fn ask_pair_from_watcher(
    started: &StartedCoreNode,
    instance_id: &str,
    held: bool,
) -> Verdict {
    ask_pair(
        started,
        &pair_commit_from_watcher(started, instance_id, held),
    )
    .await
}

/// Sends `request` as [`WATCHER`]'s daemon does.
async fn ask_pair(
    started: &StartedCoreNode,
    request: &core_node_api::encoding::PairCommitRequest,
) -> Verdict {
    poll(
        request,
        &started.caller_handle,
        WATCHER,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        request_budget(),
    )
    .await
    .map_err(|error| error_reply(error, "pair_commit"))
}

/// The pair commits [`WATCHER`]'s daemon was asked for, by whether each
/// asked for a held pair again.
#[derive(Clone, Default)]
struct PairAsks(Arc<std::sync::Mutex<Vec<bool>>>);

impl PairAsks {
    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn held(&self) -> Vec<bool> {
        self.0.lock().unwrap().clone()
    }

    /// Records a request's `held` flag and returns its number, from 0.
    fn record(&self, held: bool) -> usize {
        let mut asks = self.0.lock().unwrap();
        asks.push(held);
        asks.len() - 1
    }
}

/// [`WATCHER`]'s `pair_commit` mock: the requests it got, and what releases
/// the one it holds.
struct PeerDaemon {
    _mock: Mock,
    asked: PairAsks,
    release: Arc<tokio::sync::Notify>,
}

/// [`WATCHER`]'s `pair_commit`, which answers the request numbered `hold`
/// (from 0) with `verdict` once `release` is notified and acknowledges
/// every other one, or, holding none, answers every request with `verdict`
/// at once. It serves one request at a time, so a later one waits behind a
/// held one.
async fn peer_daemon(
    started: &StartedCoreNode,
    verdict: Verdict,
    hold: Option<usize>,
) -> PeerDaemon {
    let asked = PairAsks::default();
    let release = Arc::new(tokio::sync::Notify::new());
    let recorder = asked.clone();
    let released = Arc::clone(&release);
    let mock = peer_daemon_service(
        started,
        WATCHER,
        core_node_api::ServiceId::PairCommit,
        move |context| {
            let request = core_node_api::encoding::PairCommitRequest::decode(
                context.message().payload_bytes().as_ref(),
            )
            .expect("a pair commit decodes");
            let is_held = hold == Some(recorder.record(request.held));
            let answer = if hold.is_none() || is_held {
                answer_with(&verdict)
            } else {
                answer_with(&Ok(FederationAck))
            };
            let released = Arc::clone(&released);
            async move {
                if is_held {
                    released.notified().await;
                }
                answer
            }
        },
    )
    .await;
    PeerDaemon {
        _mock: mock,
        asked,
        release,
    }
}

/// The pairs the slot of `instance_id` holds for its restarts.
fn held_pairs(started: &StartedCoreNode, instance_id: &str) -> Vec<node_stack::PlannedPair> {
    slot_of(started, instance_id)
        .map(|slot| slot.supervision().held_pairs.clone())
        .unwrap_or_default()
}

/// Waits until the exit of a paired instance has dissolved its pairs, after
/// which only its slot names its remote peers.
async fn wait_for_pairs_dissolved(started: &StartedCoreNode) {
    poll_until(WAIT, "the exit dissolves the pair", || {
        started.node_stack.pairs().is_empty().then_some(())
    })
    .await;
}

/// The watchers of an instance waiting for its restart hear a stop of it
/// as a stop, though its exit took them out of the registry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_of_a_waiting_instance_reaches_its_watchers() {
    use core_node_api::encoding::{InstanceEnd, RelationshipEvent};
    const NODE: &str = "watched_parked";
    const ID: &str = "watched_parked_inst";
    let started = daemon(parked_restart_backoff()).await;
    // A slice that is not an append replaces the stack, so the daemon
    // takes part in the launch before it runs anything.
    take_part_in_the_launch(&started).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let (_watcher, mut heard) = watcher(&started, Duration::ZERO).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    watch(&started, ID).await;

    signal_process(pid, "-9");
    assert_eq!(
        next_event(&mut heard, ID).await,
        RelationshipEvent::Ended(InstanceEnd::Exited)
    );
    wait_for_state(&started, ID, InstanceState::Restarting).await;
    stop_ok(&started, ID).await;
    assert_eq!(
        next_event(&mut heard, ID).await,
        RelationshipEvent::Ended(InstanceEnd::Stopped)
    );
}

/// The daemon of a remote peer an instance held a pair with hears a stop of
/// the instance during its wait as a stop, which tells it to forget the pair,
/// though the exit dissolved it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_of_a_waiting_instance_reaches_its_remote_pair_peer() {
    use core_node_api::encoding::{InstanceEnd, RelationshipEvent};
    const NODE: &str = "paired_parked";
    const ID: &str = "paired_parked_inst";
    let started = daemon(parked_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let _peer_update = mock_peer_update(&started, NODE, ID).await;
    let (_watcher, mut heard) = watcher(&started, Duration::ZERO).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));

    signal_process(pid, "-9");
    assert_eq!(
        next_event(&mut heard, ID).await,
        RelationshipEvent::Ended(InstanceEnd::Exited)
    );
    wait_for_state(&started, ID, InstanceState::Restarting).await;
    wait_for_pairs_dissolved(&started).await;
    assert_eq!(held_pairs(&started, ID).len(), 1, "the exit keeps the pair");
    stop_ok(&started, ID).await;
    assert_eq!(
        next_event(&mut heard, ID).await,
        RelationshipEvent::Ended(InstanceEnd::Stopped)
    );
}

/// How a test removes an instance that exited.
#[derive(Debug)]
enum Removal {
    /// `peppy node stop` of the failed instance.
    Stop,
    /// `peppy node remove` of its node.
    NodeRemove,
    /// `peppy node add` of its node again, which overwrites it.
    Overwrite,
    /// The launch coordinator's instances removal, as a `peppy stack
    /// remove` sends it.
    Coordinator,
}

/// Every removal of an instance that exited under `never` reaches the
/// daemon of its remote pair peer as a stop, though the exit dissolved the
/// pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_removal_of_a_failed_instance_reaches_its_remote_pair_peer() {
    use core_node_api::encoding::{
        InstanceEnd, ParticipantInstancesRemoveRequest, RelationshipEvent,
    };
    const NODE: &str = "paired_failed";
    const ID: &str = "paired_failed_inst";
    for removal in [
        Removal::Stop,
        Removal::NodeRemove,
        Removal::Overwrite,
        Removal::Coordinator,
    ] {
        let started = daemon(instant_restart_backoff()).await;
        take_part_in_the_launch(&started).await;
        let dir = add_follower(&started, NODE).await;
        let _mocks = mocks(&started, NODE, ID).await;
        let _peer_update = mock_peer_update(&started, NODE, ID).await;
        let (_watcher, mut heard) = watcher(&started, Duration::ZERO).await;
        let pid = run_follower(&started, NODE, ID, never()).await;
        assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));
        signal_process(pid, "-9");
        assert_eq!(
            next_event(&mut heard, ID).await,
            RelationshipEvent::Ended(InstanceEnd::Exited)
        );
        wait_for_state(&started, ID, InstanceState::Failed).await;
        wait_for_pairs_dissolved(&started).await;
        assert_eq!(held_pairs(&started, ID).len(), 1, "the exit keeps the pair");

        match removal {
            Removal::Stop => {
                stop_ok(&started, ID).await;
            }
            Removal::NodeRemove => {
                remove_ok(&started, NODE, true).await;
            }
            Removal::Overwrite => {
                overwrite(&started, dir.path()).await.completed();
            }
            Removal::Coordinator => {
                reserve(&started).await;
                let removed = participant(
                    &started,
                    &ParticipantInstancesRemoveRequest {
                        launch_id: LAUNCH.to_owned(),
                        instance_ids: vec![name(ID)].try_into().expect("one instance"),
                    },
                )
                .await;
                assert_eq!(removed, Ok(FederationAck), "{removal:?}");
                release(&started).await;
            }
        }
        assert_eq!(state_of(&started, ID), None, "{removal:?} clears the id");
        assert_eq!(
            next_event(&mut heard, ID).await,
            RelationshipEvent::Ended(InstanceEnd::Stopped),
            "{removal:?}"
        );
    }
}

/// A peer daemon's pair asked of an instance waiting for its restart is
/// answered undelivered; one asked of an id the stack does not track is
/// refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_asked_of_an_instance_without_a_process_is_undelivered() {
    const NODE: &str = "asked_parked";
    const ID: &str = "asked_parked_inst";
    let (started, _dir, _mocks) = parked_follower(NODE, ID).await;

    let verdict = pair_from_watcher(&started, ID).await;
    assert!(
        failure_message_of(&verdict, PeppyFailureKind::ResourceUnavailable).is_some_and(|reason| {
            reason.contains(&format!("`{ID}` is restarting, with no process to pair"))
        }),
        "{verdict:?}"
    );
    let verdict = pair_from_watcher(&started, "ghost_inst").await;
    assert!(
        failure_message_of(&verdict, PeppyFailureKind::PreconditionFailed).is_some(),
        "{verdict:?}"
    );

    // A failed instance has no process either: the pair is answered
    // undelivered.
    const FAILED: &str = "asked_failed_inst";
    let _failed_mocks = mocks(&started, NODE, FAILED).await;
    let pid = run_follower(&started, NODE, FAILED, never()).await;
    signal_process(pid, "-9");
    wait_for_state(&started, FAILED, InstanceState::Failed).await;
    let verdict = pair_from_watcher(&started, FAILED).await;
    assert!(
        failure_message_of(&verdict, PeppyFailureKind::ResourceUnavailable).is_some_and(|reason| {
            reason.contains(&format!("`{FAILED}` is failed, with no process to pair"))
        }),
        "{verdict:?}"
    );
}

/// A peer daemon's request to pair with an instance whose restart is
/// starting its process is left undelivered, so the peer keeps the pair
/// held, and the restart asks the peer daemon for the pair itself once its
/// process runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_asked_of_a_restarts_start_waits_for_it_to_run() {
    const NODE: &str = "starting_follower";
    const ID: &str = "starting_follower_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let ready = common::answer_ready_probe(&started, NODE, ID).await;
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    let (_peer_update, _pins) = mock_peer_update(&started, NODE, ID).await;
    let peer = peer_daemon(&started, Ok(FederationAck), None).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));

    // Without a ready answer the restart's start sits in its ready wait.
    drop(ready);
    signal_process(pid, "-9");
    wait_for_process(&started, ID, InstanceState::Starting).await;
    assert_eq!(
        pair_again_from_watcher(&started, ID).await,
        undelivered(format!(
            "`{ID}` is starting again under its restart policy and asks for its pairs once it \
             is running"
        ))
    );
    assert_eq!(
        pair_from_watcher(&started, ID).await,
        undelivered(format!(
            "`{ID}` is starting again under its restart policy; link to it once it is running"
        ))
    );
    assert_eq!(
        peer.asked.count(),
        0,
        "the restart asks nothing before it runs"
    );

    let _ready = common::answer_ready_probe(&started, NODE, ID).await;
    wait_for_restarts(&started, ID, 1).await;
    poll_until_explained(
        WAIT,
        || format!("the restarted `{ID}` asks the peer daemon for its pair"),
        || (peer.asked.count() > 0).then_some(()),
    )
    .await;
    assert!(
        peer.asked.held().iter().all(|held| *held),
        "the restart asks for the pair it held: {:?}",
        peer.asked.held()
    );
    signal_process(serialized_pid(&started, ID), "-9");
}

/// A pair this daemon already holds answers a peer daemon's request for it
/// again as made, and leaves the pin to the operation that holds it: the
/// node is not pinned again and the pair stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_already_held_is_made_for_the_peer_daemon() {
    const NODE: &str = "held_follower";
    const ID: &str = "held_follower_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let (_peer_update, mut pins) = mock_peer_update(&started, NODE, ID).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));
    pins.borrow_and_update();

    assert_eq!(
        pair_again_from_watcher(&started, ID).await,
        Ok(FederationAck)
    );
    settle().await;
    assert!(
        !pins.has_changed().expect("the pin watch is open"),
        "the held pair is not pinned again"
    );
    assert!(
        started
            .node_stack
            .pairs()
            .iter()
            .any(|pairing| pairing.involves(&started.core_node_name, ID)),
        "the pair stays held"
    );
    signal_process(pid, "-9");
}

/// A pair a peer daemon asks for again after its restart is made only while
/// this daemon's endpoint still keeps it, so one it never held, or no longer
/// keeps after a stop of the peer, is refused, and one asked of an instance
/// the stack does not track is refused naming it; a start's link is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_pair_the_asked_end_no_longer_keeps_is_refused() {
    use core_node_api::encoding::InstanceEnd;
    const NODE: &str = "unheld_follower";
    const ID: &str = "unheld_follower_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let (_peer_update, _pins) = mock_peer_update(&started, NODE, ID).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    let not_kept = refused(format!(
        "`{ID}` keeps no pair with `{LEADER}` to make again"
    ));
    assert_eq!(pair_again_from_watcher(&started, ID).await, not_kept);
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));
    assert_eq!(
        pair_again_from_watcher(&started, ID).await,
        Ok(FederationAck)
    );

    leader_ended(&started, InstanceEnd::Stopped).await;
    assert_eq!(pair_again_from_watcher(&started, ID).await, not_kept);
    assert_eq!(
        pair_again_from_watcher(&started, "ghost_inst").await,
        refused("pairing endpoint instance `ghost_inst` is not in this stack".to_owned())
    );
    signal_process(pid, "-9");
}

/// A restart asks for no pair its slot stopped holding while the restart
/// started: here the peer daemon's leader was stopped on purpose meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_asks_for_no_pair_its_slot_forgot_during_its_start() {
    use core_node_api::encoding::InstanceEnd;
    const NODE: &str = "forgetting_follower";
    const ID: &str = "forgetting_follower_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let ready = common::answer_ready_probe(&started, NODE, ID).await;
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    let (_peer_update, _pins) = mock_peer_update(&started, NODE, ID).await;
    let peer = peer_daemon(&started, Ok(FederationAck), None).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));

    // Without a ready answer the restart's start sits in its ready wait,
    // where the stop of the leader reaches its slot.
    drop(ready);
    signal_process(pid, "-9");
    wait_for_process(&started, ID, InstanceState::Starting).await;
    leader_ended(&started, InstanceEnd::Stopped).await;
    assert!(
        held_pairs(&started, ID).is_empty(),
        "the stop of the leader ends the held pair"
    );

    let _ready = common::answer_ready_probe(&started, NODE, ID).await;
    wait_for_restarts(&started, ID, 1).await;
    // A reservation is refused while the restart's admission lasts, which is
    // through its pair requests, so the first one accepted follows them.
    let admitted_until_requested = tokio::time::timeout(WAIT, async {
        while try_reserve(&started).await.is_err() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        admitted_until_requested.is_ok(),
        "the restart's pair requests end"
    );
    release(&started).await;
    assert_eq!(
        peer.asked.count(),
        0,
        "the restart asks the peer daemon for no pair"
    );
    signal_process(serialized_pid(&started, ID), "-9");
}

/// A peer daemon's pair asked of a first start in flight is made: a first
/// start's slot takes its pairs while it starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_asked_of_a_first_start_is_made() {
    const NODE: &str = "first_start_follower";
    const ID: &str = "first_start_follower_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _health = common::answer_health_probes(&started, NODE, ID).await;
    let (_peer_update, _pins) = mock_peer_update(&started, NODE, ID).await;
    let ask_during_the_start = async {
        wait_for_process(&started, ID, InstanceState::Starting).await;
        let verdict = pair_from_watcher(&started, ID).await;
        (
            verdict,
            common::answer_ready_probe(&started, NODE, ID).await,
        )
    };
    let (pid, (verdict, _ready)) = tokio::join!(
        run_follower(&started, NODE, ID, on_failure()),
        ask_during_the_start
    );
    assert_eq!(verdict, Ok(FederationAck));
    signal_process(pid, "-9");
}

/// A peer daemon's pair asked for a slot that takes one peer, which another
/// peer holds, is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_asked_for_a_slot_another_peer_holds_is_refused() {
    const NODE: &str = "taken_follower";
    const ID: &str = "taken_follower_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let (_peer_update, _pins) = mock_peer_update(&started, NODE, ID).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));
    let other = core_node_api::encoding::PairCommitRequest {
        peer: ProducerRef::new(WATCHER, "other_leader_inst"),
        ..pair_commit_from_watcher(&started, ID, false)
    };
    let verdict = ask_pair(&started, &other).await;
    assert!(
        failure_message_of(&verdict, PeppyFailureKind::PreconditionFailed)
            .is_some_and(|reason| reason.contains("is already paired with")),
        "{verdict:?}"
    );
    signal_process(pid, "-9");
}

/// A held pair whose slot a link from another peer took is not taken back
/// by the first peer's restart: the asked end refuses it, though the slot is
/// free again when the restart asks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_pair_another_link_displaced_is_not_taken_back() {
    use core_node_api::encoding::InstanceEnd;
    const NODE: &str = "displaced_follower";
    const ID: &str = "displaced_follower_inst";
    const OTHER: &str = "other_leader_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let (_peer_update, _pins) = mock_peer_update(&started, NODE, ID).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));

    // The leader exits and the other leader links to the free slot, which
    // displaces the held pair; then the other leader exits too.
    leader_ended(&started, InstanceEnd::Exited).await;
    let other = core_node_api::encoding::PairCommitRequest {
        peer: ProducerRef::new(WATCHER, OTHER),
        ..pair_commit_from_watcher(&started, ID, false)
    };
    assert_eq!(ask_pair(&started, &other).await, Ok(FederationAck));
    peer_ended(&started, OTHER, InstanceEnd::Exited).await;

    assert_eq!(
        pair_again_from_watcher(&started, ID).await,
        refused(format!(
            "`{ID}` keeps no pair with `{LEADER}` to make again"
        ))
    );
    assert!(
        started.node_stack.pairs().is_empty(),
        "the refusal reserves nothing on the slot"
    );
    assert_eq!(
        pair_from_watcher(&started, ID).await,
        Ok(FederationAck),
        "the slot was free, so the held check refused the restart's request"
    );
    signal_process(pid, "-9");
}

/// A removal without `--stop-instances` of a node whose running instance
/// has no shutdown service to reach is refused by the gate, and the process
/// runs on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_removal_gate_counts_an_unreachable_instance() {
    const NODE: &str = "unreachable_gate";
    const ID: &str = "unreachable_gate_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_node(&started, NODE, SLEEPER, None).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run(&started, NODE, ID, never()).await;
    let removal = remove(&started, NODE, false).await;
    assert_eq!(
        removal,
        Err(Failure::new(
            PeppyFailureKind::PreconditionFailed,
            format!(
                "Node '{NODE}:{TEST_NODE_TAG}' has tracked instances (e.g. '{ID}'); `peppy node \
                 remove {NODE}:{TEST_NODE_TAG} --stop-instances --core-node {}` clears them \
                 before removing",
                started.core_node_name
            )
        ))
    );
    assert!(common::is_process_running(pid));
    assert!(started.node_stack.find(NODE, TEST_NODE_TAG).is_some());
    signal_process(pid, "-9");
}

/// A daemon with one follower instance killed into an hour's wait.
async fn parked_follower(
    node: &str,
    instance_id: &str,
) -> (StartedCoreNode, TempDir, (Mock, Mock)) {
    let started = daemon(parked_restart_backoff()).await;
    let dir = add_follower(&started, node).await;
    let mocks = mocks(&started, node, instance_id).await;
    let pid = run_follower(&started, node, instance_id, on_failure()).await;
    signal_process(pid, "-9");
    wait_for_state(&started, instance_id, InstanceState::Restarting).await;
    (started, dir, mocks)
}

/// A pair whose pin the asked daemon's node does not take is answered
/// undelivered, and the asked daemon keeps no half of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_its_node_does_not_take_is_undelivered() {
    const NODE: &str = "unpinned";
    const ID: &str = "unpinned_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;

    let verdict = pair_from_watcher(&started, ID).await;
    assert!(
        failure_message_of(&verdict, PeppyFailureKind::ResourceUnavailable)
            .is_some_and(|reason| reason.contains(&format!("could not pin `{ID}`"))),
        "{verdict:?}"
    );
    assert!(held_pairs(&started, ID).is_empty());
    assert!(
        started.node_stack.pairs().is_empty(),
        "the asked daemon keeps no half of the pair"
    );
    signal_process(pid, "-9");
}

/// A respawn asks the peer daemon again for the pair it held; one the peer
/// daemon did not take stays held for the next restart, and one it refuses
/// is forgotten. Either failure reverts the reservation, and the restarted
/// node is sent its set without the pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_pair_stays_held_unless_the_peer_daemon_refuses_it() {
    for (verdict, line, kept) in [
        (
            undelivered("the leader is restarting".to_owned()),
            "requested again when either end restarts",
            1,
        ),
        (
            refused("the arm slot is taken".to_owned()),
            "the pair is forgotten, so launch the stack again on its coordinator with `peppy \
             stack launch`, or, for a pair a copy's `peppy stack join` made, remove that copy \
             with `peppy stack remove NAME` and join it again there",
            0,
        ),
    ] {
        const NODE: &str = "asking";
        const ID: &str = "asking_inst";
        let started = daemon(instant_restart_backoff()).await;
        let _dir = add_follower(&started, NODE).await;
        let _mocks = mocks(&started, NODE, ID).await;
        let (_peer_update, pins) = mock_peer_update(&started, NODE, ID).await;
        let (_watcher, _heard) = watcher(&started, Duration::ZERO).await;
        let pid = run_follower(&started, NODE, ID, on_failure()).await;
        assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));
        let peer = peer_daemon(&started, verdict.clone(), None).await;

        signal_process(pid, "-9");
        wait_for_restarts(&started, ID, 1).await;
        poll_until(WAIT, &format!("the run log says `{line}`"), || {
            run_log(&started, ID).contains(line).then_some(())
        })
        .await;
        assert_eq!(
            peer.asked.count(),
            1,
            "the respawn asked the peer daemon once"
        );
        assert_eq!(held_pairs(&started, ID).len(), kept, "after {verdict:?}");
        assert!(
            started.node_stack.pairs().is_empty(),
            "the failed answer reverts the reservation, after {verdict:?}"
        );
        assert!(
            pins.borrow().members.is_empty(),
            "the restarted node is sent its set without the pair, after {verdict:?}"
        );
        signal_process(serialized_pid(&started, ID), "-9");
    }
}

/// A restart's request the peer daemon answers with a failure is reverted
/// only while the restart's process still runs: once that process exited and
/// the next restart reserved the pair, the late failure leaves the pair alone
/// and writes no line under the next restart's attempt line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_failure_leaves_the_next_restarts_pair_alone() {
    /// Inside the 15 s a restart waits for the peer daemon's answer, so the
    /// answer the mock releases is one the first restart still waits for.
    const ANSWERED_IN_TIME: Duration = Duration::from_secs(10);
    const NODE: &str = "late_answer";
    const ID: &str = "late_answer_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let _peer_update = mock_peer_update(&started, NODE, ID).await;
    let (_watcher, _heard) = watcher(&started, Duration::ZERO).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));
    let peer = peer_daemon(
        &started,
        undelivered("the leader did not take its pin".to_owned()),
        Some(0),
    )
    .await;

    // The first restart's request waits; its process exits, and the next
    // restart reserves the pair before that request's failure comes back.
    signal_process(pid, "-9");
    wait_for_restarts(&started, ID, 1).await;
    poll_until(WAIT, "the first restart asks for the pair", || {
        (peer.asked.count() >= 1).then_some(())
    })
    .await;
    let first_asked = std::time::Instant::now();
    signal_process(serialized_pid(&started, ID), "-9");
    wait_for_restarts(&started, ID, 2).await;
    poll_until(WAIT, "the next restart reserves the pair", || {
        (started.node_stack.pairs().len() == 1).then_some(())
    })
    .await;
    assert!(
        first_asked.elapsed() < ANSWERED_IN_TIME,
        "the first request still waits for its answer: {:?}",
        first_asked.elapsed()
    );
    peer.release.notify_one();
    poll_until(WAIT, "the next restart makes the pair", || {
        run_log(&started, ID).contains("paired: ").then_some(())
    })
    .await;
    // Each restart holds the stack against changes until its pair requests
    // end, so a change taken here follows the late failure's handling.
    poll_until(WAIT, "both restarts' pair requests end", || {
        started
            .slice_ownership
            .stack()
            .try_begin_change()
            .is_ok()
            .then_some(())
    })
    .await;
    assert_eq!(
        started.node_stack.pairs().len(),
        1,
        "the late failure left the next restart's pair"
    );
    let last_attempt = run_log(&started, ID);
    assert!(
        !last_attempt.contains("not made again"),
        "the late failure is not logged under the next restart: {last_attempt}"
    );
    assert_eq!(held_pairs(&started, ID).len(), 1);
    signal_process(serialized_pid(&started, ID), "-9");
}

/// The answer to a restart's request that comes after its process exited is
/// not applied: a refusal then leaves the pair held for the next process to
/// ask for again, which a capped instance keeps while it is failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_after_the_process_exited_leaves_the_pair_held() {
    const NODE: &str = "late_refusal";
    const ID: &str = "late_refusal_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let _peer_update = mock_peer_update(&started, NODE, ID).await;
    let (_watcher, _heard) = watcher(&started, Duration::ZERO).await;
    let pid = run_follower(&started, NODE, ID, capped(1)).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));
    let peer = peer_daemon(
        &started,
        refused("the arm slot is taken".to_owned()),
        Some(0),
    )
    .await;

    // The restart's request waits; its process exits, and the cap leaves the
    // instance failed before the refusal comes back.
    signal_process(pid, "-9");
    wait_for_restarts(&started, ID, 1).await;
    poll_until(WAIT, "the restart asks for the pair", || {
        (peer.asked.count() >= 1).then_some(())
    })
    .await;
    signal_process(serialized_pid(&started, ID), "-9");
    wait_for_state(&started, ID, InstanceState::Failed).await;
    peer.release.notify_one();
    // The restart holds the stack against changes until its requests end.
    poll_until(WAIT, "the restart's requests end", || {
        started
            .slice_ownership
            .stack()
            .try_begin_change()
            .is_ok()
            .then_some(())
    })
    .await;
    assert_eq!(
        held_pairs(&started, ID).len(),
        1,
        "the refusal after the exit is not applied"
    );
}

/// A restart writes each pair it asks for to the run log as that request
/// ends: of two held pairs, the first is logged made while the peer daemon
/// still holds the request for the second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_logs_each_pair_as_its_request_ends() {
    const NODE: &str = "two_leaders";
    const ID: &str = "two_leaders_inst";
    const SECOND_LEADER: &str = "second_leader_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower_taking(&started, NODE, Cardinality::ZeroOrMore).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let _peer_update = mock_peer_update_taking(&started, NODE, ID, Cardinality::ZeroOrMore).await;
    let (_watcher, _heard) = watcher(&started, Duration::ZERO).await;
    let pid = run(&started, NODE, ID, on_failure()).await;
    for leader in [LEADER, SECOND_LEADER] {
        assert_eq!(
            ask_pair(&started, &pair_commit_of(&started, ID, leader, false)).await,
            Ok(FederationAck)
        );
    }
    let peer = peer_daemon(&started, Ok(FederationAck), Some(1)).await;

    signal_process(pid, "-9");
    wait_for_restarts(&started, ID, 1).await;
    poll_until(WAIT, "the restart asks for the second pair", || {
        (peer.asked.count() >= 2).then_some(())
    })
    .await;
    let while_held = run_log(&started, ID);
    assert!(
        while_held.contains(&format!("{WATCHER}/{LEADER}:{LEADER_SLOT}"))
            && !while_held.contains(SECOND_LEADER),
        "the first pair is logged while the second is asked for:\n{while_held}"
    );
    peer.release.notify_one();
    poll_until(WAIT, "the second pair is logged", || {
        run_log(&started, ID).contains(SECOND_LEADER).then_some(())
    })
    .await;
    signal_process(serialized_pid(&started, ID), "-9");
}

/// When both daemons restart their end of one pair at once, each asks the
/// other for it: while a respawn waits on the peer daemon's answer to its
/// request, the peer daemon's own request for the pair is answered here
/// with `Done`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_respawn_answers_the_peer_daemons_request_while_it_asks_its_own() {
    /// Well inside the 15 s a peer daemon's answer to a pair commit may
    /// take.
    const ANSWERED_MEANWHILE: Duration = Duration::from_secs(5);
    const NODE: &str = "both_restart";
    const ID: &str = "both_restart_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let _peer_update = mock_peer_update(&started, NODE, ID).await;
    let (_watcher, _heard) = watcher(&started, Duration::ZERO).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));
    let (_peer, answered) = peer_daemon_asking_back(&started, ID).await;

    signal_process(pid, "-9");
    wait_for_restarts(&started, ID, 1).await;
    let (verdict, took) = poll_until(WAIT, "the peer daemon's own request is answered", || {
        answered.lock().unwrap().clone()
    })
    .await;
    assert_eq!(verdict, Ok(FederationAck));
    assert!(
        took < ANSWERED_MEANWHILE,
        "answered while the respawn waited on its own request, after {took:?}"
    );
    poll_until(WAIT, "the respawn's run log names the pair made", || {
        run_log(&started, ID).contains("paired: ").then_some(())
    })
    .await;
    signal_process(serialized_pid(&started, ID), "-9");
}

/// [`WATCHER`]'s `pair_commit` as the daemon of a peer restarting at the
/// same time answers it: on a held request it first asks this daemon for
/// the same pair again, records that verdict and how long it took, then
/// answers the request made.
async fn peer_daemon_asking_back(
    started: &StartedCoreNode,
    instance_id: &str,
) -> (Mock, Arc<std::sync::Mutex<Option<(Verdict, Duration)>>>) {
    let answered = Arc::new(std::sync::Mutex::new(None));
    let ask_back = pair_commit_from_watcher(started, instance_id, true);
    let messenger = started.caller_handle.clone();
    let core_node_name = started.core_node_name.clone();
    let recorder = Arc::clone(&answered);
    let mock = peer_daemon_service(
        started,
        WATCHER,
        core_node_api::ServiceId::PairCommit,
        move |context| {
            let request = core_node_api::encoding::PairCommitRequest::decode(
                context.message().payload_bytes().as_ref(),
            )
            .expect("a pair commit decodes");
            let ask_back = ask_back.clone();
            let messenger = messenger.clone();
            let core_node_name = core_node_name.clone();
            let recorder = Arc::clone(&recorder);
            async move {
                if request.held {
                    let asked = std::time::Instant::now();
                    let verdict: Verdict = poll(
                        &ask_back,
                        &messenger,
                        WATCHER,
                        CALLER_INSTANCE_ID,
                        &core_node_name,
                        request_budget(),
                    )
                    .await
                    .map_err(|error| error_reply(error, "pair_commit"));
                    *recorder.lock().unwrap() = Some((verdict, asked.elapsed()));
                }
                answer_with(&Ok(FederationAck))
            }
        },
    )
    .await;
    (mock, answered)
}

/// The exit's lines come before its announcement, and a stop that claims
/// the instance while the exit is announced ends it: the instance's lines
/// end with the exit's `restarting` line, or its `max_restarts` line past the
/// cap, and nothing restarts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_during_the_exits_announcement_ends_the_instance() {
    /// How long the watcher takes to answer the exit.
    const ACK: Duration = Duration::from_secs(3);
    for (request, line) in [(on_failure(), "restarting in"), (capped(1), "reached")] {
        const NODE: &str = "stopped_mid_announcement";
        const ID: &str = "stopped_mid_announcement_inst";
        let started = daemon(instant_restart_backoff()).await;
        // A slice that is not an append replaces the stack, so the daemon
        // takes part in the launch before it runs anything.
        take_part_in_the_launch(&started).await;
        let _dir = add_node(&started, NODE, SLEEPER, None).await;
        let _mocks = mocks(&started, NODE, ID).await;
        let (_watcher, mut heard) = watcher(&started, ACK).await;
        let pid = run(&started, NODE, ID, request).await;
        watch(&started, ID).await;
        let capped = line == "reached";
        if capped {
            signal_process(pid, "-9");
            wait_for_restarts_within(&started, ID, 1, ACK + WAIT).await;
            let _ = next_event(&mut heard, ID).await;
            let _ = next_event(&mut heard, ID).await;
        }

        signal_process(serialized_pid(&started, ID), "-9");
        let _ = next_event(&mut heard, ID).await;
        let lines = lines_for(&started, ID);
        assert!(
            lines.last().is_some_and(|logged| logged.contains(line)),
            "the exit's `{line}` line is written before its announcement ends: {lines:?}"
        );
        stop_ok(&started, ID).await;
        tokio::time::sleep(ACK + QUIET).await;
        assert_eq!(state_of(&started, ID), None);
        assert_eq!(lines_for(&started, ID), lines, "nothing follows the stop");
    }
}

/// [`WATCHER`]'s daemon tells this one that [`LEADER`] ended.
async fn leader_ended(started: &StartedCoreNode, end: core_node_api::encoding::InstanceEnd) {
    peer_ended(started, LEADER, end).await;
}

/// [`WATCHER`]'s daemon tells this one that its instance `peer` ended.
async fn peer_ended(
    started: &StartedCoreNode,
    peer: &str,
    end: core_node_api::encoding::InstanceEnd,
) {
    use core_node_api::encoding::{RelationshipEvent, RelationshipNotification};
    poll(
        &RelationshipNotification::new(peer, WATCHER, RelationshipEvent::Ended(end)),
        &started.caller_handle,
        WATCHER,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        request_budget(),
    )
    .await
    .expect("relationship_notify completes");
}

/// A remote peer that exited stays held for the next restart; one its
/// daemon stopped on purpose is forgotten.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_peer_stopped_on_purpose_is_forgotten() {
    use core_node_api::encoding::InstanceEnd;
    const NODE: &str = "peer_of_a_leader";
    const ID: &str = "peer_of_a_leader_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let _peer_update = mock_peer_update(&started, NODE, ID).await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));

    leader_ended(&started, InstanceEnd::Exited).await;
    assert_eq!(held_pairs(&started, ID).len(), 1, "an exit keeps the pair");
    leader_ended(&started, InstanceEnd::Stopped).await;
    assert!(held_pairs(&started, ID).is_empty(), "a stop forgets it");
    signal_process(pid, "-9");
}

/// A reset does not wait out a respawn's request of a held pair from a peer
/// daemon that does not answer: the request ends with the start, and no
/// outcome is logged for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reset_cancels_a_respawns_unanswered_pair_request() {
    /// Well under the 15 s a pair commit waits for its answer.
    const RESET_BOUND: Duration = Duration::from_secs(7);
    const NODE: &str = "unanswered";
    const ID: &str = "unanswered_inst";
    let started = daemon(instant_restart_backoff()).await;
    let _dir = add_follower(&started, NODE).await;
    let _mocks = mocks(&started, NODE, ID).await;
    let _peer_update = mock_peer_update(&started, NODE, ID).await;
    let (_watcher, _heard) = watcher(&started, Duration::ZERO).await;
    // The node answers the reset's shutdown at once, so the teardown is quick.
    let _shutdown = exiting_on_shutdown(
        &started,
        NODE,
        ID,
        ExitOnShutdown::BeforeAnswer(Duration::ZERO),
    )
    .await;
    let pid = run_follower(&started, NODE, ID, on_failure()).await;
    assert_eq!(pair_from_watcher(&started, ID).await, Ok(FederationAck));
    // The peer daemon takes the request and never answers it.
    let (asked, mut request_arrived) = tokio::sync::mpsc::unbounded_channel::<()>();
    let _silent = peer_daemon_service(
        &started,
        WATCHER,
        core_node_api::ServiceId::PairCommit,
        move |_context| {
            let _ = asked.send(());
            async move {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                answer_with(&Ok(FederationAck))
            }
        },
    )
    .await;

    signal_process(pid, "-9");
    tokio::time::timeout(WAIT, request_arrived.recv())
        .await
        .expect("the respawn asks the peer daemon for the pair")
        .expect("the peer daemon listens");
    let resetting = std::time::Instant::now();
    reset(&started).await;
    let took = resetting.elapsed();
    assert!(took < RESET_BOUND, "the reset waited {took:?}");
    assert!(
        !run_log(&started, ID).contains("not made again"),
        "a cancelled request logs no outcome:\n{}",
        run_log(&started, ID)
    );
}
