use super::*;
use config::node::Cardinality;
use core_node_api::encoding::{
    CopyAction, CopyChange, FederationAck, NodeStopRequest, NodeStopResponse, StackJoinGoal,
    StackListRequest, StackRemoveGoal,
};
use peppylib::core_node::transport::{poll, poll_node_stop};
use peppylib::services::slot_update::SlotChannel;

/// The answer of the daemon to `request`, sent as the coordinator: its
/// response, or its error reply.
async fn participant_answer<R: core_node_api::ServiceRequest>(
    started: &StartedCoreNode,
    request: &R,
) -> peppylib::PeppyResult<R::Response> {
    poll(
        request,
        &started.caller_handle,
        "coordinator",
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        GOAL_TIMEOUT,
    )
    .await
}

/// The response of the daemon to `request`, sent as the coordinator. Panics
/// on an error reply.
pub(super) async fn participant_request<R: core_node_api::ServiceRequest>(
    started: &StartedCoreNode,
    request: &R,
) -> R::Response {
    participant_answer(started, request)
        .await
        .unwrap_or_else(|error| panic!("{} should succeed: {error}", R::ID.name()))
}

/// The failure of the error reply of the daemon to `request`, sent as the
/// coordinator. Panics when the daemon answers the request.
async fn participant_refusal<R: core_node_api::ServiceRequest>(
    started: &StartedCoreNode,
    request: &R,
) -> Failure
where
    R::Response: std::fmt::Debug,
{
    let error = participant_answer(started, request)
        .await
        .expect_err("the daemon gives an error reply");
    error
        .failure()
        .cloned()
        .unwrap_or_else(|| panic!("{} failed without an error reply: {error}", R::ID.name()))
}

pub(super) async fn execute(
    started: &StartedCoreNode,
    goal: &impl core_node_api::ActionGoal,
) -> StackGoalEnd {
    execute_reporting(started, goal).await.0
}

/// Runs `goal` to its end, with every output line the daemon sent for it as
/// feedback, in order.
async fn execute_reporting(
    started: &StartedCoreNode,
    goal: &impl core_node_api::ActionGoal,
) -> (StackGoalEnd, Vec<LaunchOutput>) {
    let mut feedback = Vec::new();
    let end = run_stack_goal(
        &started.caller_handle,
        &started.core_node_name,
        goal,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        &mut |output| feedback.push(output),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    (end, feedback)
}

/// The lines of `feedback` that report the stop of an instance, in order,
/// each from the launcher's step.
fn stop_reports(feedback: &[LaunchOutput]) -> Vec<&str> {
    feedback
        .iter()
        .filter(|message| message.line.starts_with("Stopping instance "))
        .inspect(|message| {
            assert_eq!(
                message.step,
                core_node_api::encoding::LaunchFeedbackStep::LauncherStep
            );
        })
        .map(|message| message.line.as_str())
        .collect()
}

/// Sends a goal the daemon refuses at admission and returns the failure of
/// the refusal.
async fn refusal(started: &StartedCoreNode, goal: &impl core_node_api::ActionGoal) -> Failure {
    let handle = send_daemon_goal(
        &started.caller_handle,
        &started.core_node_name,
        goal,
        GOAL_TIMEOUT,
    )
    .await
    .expect("send stack goal");
    goal_answer(&handle, LaunchGoalResponse::decode, "goal response")
        .expect("decode the goal answer")
        .expect_err("the daemon refuses the goal")
}

/// A join of a copy `name` of `option`, under budgets no test reaches.
pub(super) fn robot_goal(name: &str, option: &str) -> StackJoinGoal {
    StackJoinGoal::new(
        format!("{option}:{name}").parse().unwrap(),
        unreached_budgets(),
    )
}

fn remove_goal(name: &str) -> StackRemoveGoal {
    StackRemoveGoal::new(config::runtime::CopyName::new(name).unwrap())
}

/// A launch of the fleet `launcher` under `launch_id`, under budgets no test
/// reaches.
pub(super) fn fleet_launch(launcher: PathBuf, launch_id: &str) -> LaunchGoal {
    LaunchGoal::new(LauncherOrigin::Fs(launcher), launch_id, unreached_budgets())
}

/// Stops `instance_id` through the daemon's `node_stop` service.
async fn stop_instance(started: &StartedCoreNode, instance_id: &str) -> NodeStopResponse {
    poll_node_stop(
        &NodeStopRequest::new(instance_id),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        &started.core_node_name,
        GOAL_TIMEOUT,
    )
    .await
    .unwrap()
}

fn robot_pid(started: &StartedCoreNode, name: &str) -> u32 {
    instance_pid(started, "named_robot", &format!("{name}_arm_inst"))
}

#[tokio::test]
async fn appended_remote_watchers_receive_existing_source_stop_notifications() {
    use config::runtime::CoreNodeName;
    use core_node_api::{
        ServiceId,
        encoding::{
            InstanceEnd, ParticipantReleaseRequest, ParticipantReserveRequest,
            ParticipantSliceBeginRequest, RelationshipEvent, RelationshipNotification,
            RelationshipNotificationAck,
        },
    };
    use peppylib::ServiceMessenger;

    let started = start_core_node_with_mock_messenger().await;
    let launch_id = "watcher-join-test";
    let reserve = ParticipantReserveRequest::new(launch_id, "coordinator");
    let request = ParticipantSliceBeginRequest::new(launch_id, Vec::new());
    participant_request(&started, &reserve).await;
    participant_request(&started, &request).await;
    let released = participant_request(&started, &ParticipantReleaseRequest::new(launch_id)).await;
    assert_eq!(released, FederationAck::new());

    let (_directory, _instance, pid) =
        seed_running_node(&started, "watched_arm", "v1", "arm_inst").await;
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut listener = ServiceMessenger::listen(
        &started.caller_handle,
        "cloud",
        "cloud_root",
        common::core_node_target("cloud"),
        ServiceId::RelationshipNotify.name(),
    )
    .await
    .unwrap();
    let _listener = AbortOnDrop(peppylib::runtime::spawn(async move {
        listener
            .handle_requests(move |context| {
                let event =
                    RelationshipNotification::decode(context.message().payload_bytes().as_ref())
                        .unwrap();
                events.send(event).unwrap();
                async {
                    RelationshipNotificationAck::new()
                        .encode()
                        .map_err(|error| {
                            Failure::new(PeppyFailureKind::AttemptFailed, error.to_string())
                        })
                }
            })
            .await
    }));
    participant_request(&started, &reserve).await;
    let mut append = ParticipantSliceBeginRequest::new(launch_id, Vec::new());
    append.append = true;
    append.lifecycle_watchers.insert(
        Name::new("arm_inst").unwrap(),
        std::collections::BTreeSet::from([CoreNodeName::new("cloud").unwrap()]),
    );
    participant_request(&started, &append).await;
    let released = participant_request(&started, &ParticipantReleaseRequest::new(launch_id)).await;
    assert_eq!(released, FederationAck::new());
    assert!(is_process_running(pid));
    let _shutdown = common::install_kill_on_shutdown_listener(
        &started,
        "watched_arm",
        &Name::new("arm_inst").unwrap(),
        pid,
    )
    .await;
    stop_instance(&started, "arm_inst").await;
    let event = tokio::time::timeout(GOAL_TIMEOUT, received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        RelationshipNotification::new(
            "arm_inst",
            &started.core_node_name,
            RelationshipEvent::Ended(InstanceEnd::Stopped)
        )
    );
}

#[tokio::test]
async fn six_copies_join_remove_and_rejoin_without_restarting_their_neighbors() {
    let started = start_core_node_with_mock_messenger().await;
    let directory = tempdir().unwrap();
    let robot = write_node_config(directory.path(), "named_robot", "v1", &["sleep", "300"]);
    let broken = write_node_config_with_options(
        directory.path(),
        "broken_robot",
        "v1",
        NodeConfigOptions {
            build_cmd: &["false"],
            run_cmd: &["sleep", "300"],
            ..Default::default()
        },
    );
    TestPackagesCache::new()
        .fs_entry("named_robot", "v1", &robot)
        .fs_entry("broken_robot", "v1", &broken)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    fs::write(&launcher, r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "robot", cardinality: "zero_or_more", options: {
            real: { deployments: [{ source: { name: "named_robot", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] },
            broken: { deployments: [{ source: { name: "broken_robot", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] }
        } }]
    }"#).unwrap();
    let launch = fleet_launch(launcher.clone(), "named-fragment-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    assert_eq!(
        started.node_stack.len(),
        1,
        "the fleet starts with no robot"
    );

    let mut responders = Vec::new();
    let mut pids = std::collections::BTreeMap::new();
    for name in ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"] {
        let id = format!("{name}_arm_inst");
        responders.extend(answer_readiness(&started, "named_robot", &id).await);
        let result = execute(&started, &robot_goal(name, "real")).await;
        result.assert_completed();
        pids.insert(name, robot_pid(&started, name));
        for (neighbor, pid) in &pids {
            assert_eq!(
                robot_pid(&started, neighbor),
                *pid,
                "joining cannot restart {neighbor}"
            );
            assert!(is_process_running(*pid));
        }
    }

    // Selection uses the launch-time snapshot, even if its source file changes.
    fs::write(&launcher, "not a launcher anymore").unwrap();
    let duplicate = execute(&started, &robot_goal("alpha", "real")).await;
    duplicate.assert_failed(PeppyFailureKind::PreconditionFailed, "alpha");
    // A failing build fails the join.
    let failure = execute(&started, &robot_goal("failed", "broken")).await;
    failure.assert_failed(PeppyFailureKind::AttemptFailed, "broken_robot");
    assert!(
        !started
            .node_stack
            .to_serialized_graph()
            .nodes
            .iter()
            .any(|node| node.name == "broken_robot"),
        "a failed join removes the node it added"
    );
    for (name, pid) in &pids {
        assert_eq!(robot_pid(&started, name), *pid);
        assert!(is_process_running(*pid));
    }

    let result = execute(&started, &remove_goal("bravo")).await;
    result.assert_completed();
    assert!(
        !is_process_running(pids.remove("bravo").unwrap()),
        "removed process has exited"
    );
    let list =
        common::poll_core_node_with_timeout(&started, &StackListRequest::new(), GOAL_TIMEOUT).await;
    assert_eq!(list.copies.len(), 5);
    assert!(
        !list
            .copies
            .iter()
            .any(|copy| copy.name == "bravo" || copy.name == "failed")
    );
    for copy in list.copies {
        assert_eq!(copy.core_node.as_str(), started.core_node_name);
        assert_eq!(copy.instance_ids, [format!("{}_arm_inst", copy.name)]);
        assert_eq!(copy.option, "real");
        assert!(copy.selections.is_empty());
    }
    // A stopped, unrelated recorded member does not participate in this join.
    stop_instance(&started, "charlie_arm_inst").await;
    assert!(!is_process_running(pids.remove("charlie").unwrap()));
    let result = execute(&started, &robot_goal("bravo", "real")).await;
    result.assert_completed();
    for (name, pid) in pids {
        assert_eq!(robot_pid(&started, name), pid);
        assert!(is_process_running(pid));
    }
}

#[tokio::test]
async fn join_and_remove_require_an_active_launcher() {
    let started = start_core_node_with_mock_messenger().await;
    for result in [
        execute(&started, &robot_goal("alpha", "real")).await,
        execute(&started, &remove_goal("alpha")).await,
    ] {
        result.assert_failed(PeppyFailureKind::PreconditionFailed, "stack launch");
    }
    assert_eq!(started.node_stack.len(), 1);
}

/// While a join builds, `stack list` reports the join as its copy change,
/// and another join and a remove are refused as busy; a reset interrupts the
/// build, after which no change holds the stack, and a fresh launch follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_join_build_refuses_other_changes_until_reset_interrupts_it() {
    let started = start_core_node_with_mock_messenger().await;
    let directory = tempdir().unwrap();
    let pid_file = directory.path().join("build.pid");
    let command = format!("echo $$ > '{}'; exec sleep 300", pid_file.display());
    let robot = write_node_config_with_options(
        directory.path(),
        "blocked_robot",
        "v1",
        NodeConfigOptions {
            build_cmd: &[&command],
            run_cmd: &["true"],
            ..Default::default()
        },
    );
    TestPackagesCache::new()
        .fs_entry("blocked_robot", "v1", &robot)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    fs::write(
        &launcher,
        r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "robot", cardinality: "zero_or_more", options: {
            simulated: { deployments: [{
                source: { name: "blocked_robot", tag: "v1" },
                instances: [{ instance_id: "arm_inst" }],
            }] },
        } }],
    }"#,
    )
    .unwrap();
    let launch = fleet_launch(launcher, "reset-join");
    execute(&started, &launch).await.assert_completed();
    let join = robot_goal("alpha", "simulated");
    let (result, ()) = tokio::join!(execute(&started, &join), async {
        let pid = common::poll_until(GOAL_TIMEOUT, "join build records its process", || {
            fs::read_to_string(&pid_file)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
        })
        .await;
        let list = participant_request(&started, &StackListRequest::new()).await;
        assert_eq!(
            list.copy_change,
            Some(CopyChange {
                action: CopyAction::Join,
                name: config::runtime::CopyName::new("alpha").unwrap(),
                option: "simulated".to_owned(),
            })
        );
        assert!(list.copies.is_empty(), "a joining copy is no copy yet");
        let busy = "a stack launch, build, join, removal or reset, or a coordinator's change to \
                    this daemon's slice, is in progress on this daemon; run this again once it \
                    ends";
        for refused in [
            refusal(&started, &robot_goal("bravo", "simulated")).await,
            refusal(&started, &remove_goal("alpha")).await,
        ] {
            assert_failure(&refused, DaemonFailureKind::Busy, busy);
        }
        assert!(is_process_running(pid), "refusals leave the build running");
        participant_request(&started, &core_node_api::encoding::StackResetRequest::new()).await;
        assert!(
            !is_process_running(pid),
            "reset reaps the joined build process"
        );
        assert_eq!(started.node_stack.len(), 1);
    },);
    // The reset cancels the join.
    assert_eq!(result.end.cancel_reason(), "cancelled by a stack reset");
    let list = participant_request(&started, &StackListRequest::new()).await;
    assert_eq!(
        list.copy_change, None,
        "the interrupted join reports nothing"
    );
    let result = execute(&started, &launch).await;
    result.assert_completed();
}

/// A join accepts a cancel from its client. The build in progress stops,
/// the join undoes what it did as a join whose build fails undoes it, and
/// the goal ends cancelled with the reason of the cancel. The stack keeps
/// its launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_join_is_undone_and_ends_cancelled() {
    let started = start_core_node_with_mock_messenger().await;
    let directory = tempdir().unwrap();
    let pid_file = directory.path().join("build.pid");
    let command = format!("echo $$ > '{}'; exec sleep 300", pid_file.display());
    let robot = write_node_config_with_options(
        directory.path(),
        "blocked_robot",
        "v1",
        NodeConfigOptions {
            build_cmd: &[&command],
            run_cmd: &["true"],
            ..Default::default()
        },
    );
    TestPackagesCache::new()
        .fs_entry("blocked_robot", "v1", &robot)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    fs::write(
        &launcher,
        r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "robot", cardinality: "zero_or_more", options: {
            simulated: { deployments: [{
                source: { name: "blocked_robot", tag: "v1" },
                instances: [{ instance_id: "arm_inst" }],
            }] },
        } }],
    }"#,
    )
    .unwrap();
    execute(&started, &fleet_launch(launcher, "cancel-join"))
        .await
        .assert_completed();

    let handle = send_accepted_stack_goal(&started, &robot_goal("alpha", "simulated")).await;
    let pid = common::poll_until(GOAL_TIMEOUT, "join build records its process", || {
        fs::read_to_string(&pid_file)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
    })
    .await;
    let end = cancel_and_follow_to_end(&started, handle, async {}).await;

    assert_eq!(end.cancel_reason(), "cancelled by its caller");
    wait_until_process_exits(
        pid,
        RESULT_TIMEOUT,
        &format!("the cancel stops the build process {pid}"),
    )
    .await;
    let list = participant_request(&started, &StackListRequest::new()).await;
    assert_eq!(list.copy_change, None, "no change holds the stack");
    assert!(list.copies.is_empty(), "the cancelled join leaves no copy");
    assert!(list.launch.is_some(), "the stack keeps its launch");
    assert!(
        !started.node_stack.contains("blocked_robot", "v1"),
        "the join removes the node it added"
    );
}

/// A removal accepts a cancel from its client. The removal ends before its
/// next stop, as a removal whose stop fails ends: the copy stays listed, the
/// instance it stopped is gone, and the instance it did not reach runs. The
/// goal ends cancelled with the reason of the cancel. Each instance hears
/// the shutdown request and exits only when the test kills it, under a
/// grace that outlasts the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_removal_keeps_the_copy_and_the_instance_it_did_not_stop() {
    const INSTANCES: [&str; 2] = ["alpha_arm_inst", "alpha_cam_inst"];
    let started = common::start_core_node_with_shutdown_grace(3600).await;
    let directory = tempdir().unwrap();
    let robot = write_node_config(directory.path(), "named_robot", "v1", &["sleep", "300"]);
    TestPackagesCache::new()
        .fs_entry("named_robot", "v1", &robot)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    fs::write(
        &launcher,
        r#"{
        peppy_schema: "launcher/v1",
        deployments: [{ robot: "real", instances: [{ instance_id: "alpha" }] }],
        components: [{ name: "robot", cardinality: "zero_or_more", options: {
            real: { deployments: [{
                source: { name: "named_robot", tag: "v1" },
                instances: [{ instance_id: "arm_inst" }, { instance_id: "cam_inst" }],
            }] },
        } }],
    }"#,
    )
    .unwrap();
    let mut responders = Vec::new();
    for instance in INSTANCES {
        responders.extend(answer_readiness(&started, "named_robot", instance).await);
    }
    execute(&started, &fleet_launch(launcher, "cancel-removal"))
        .await
        .assert_completed();

    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let mut listeners = Vec::new();
    let mut shutdowns = Vec::new();
    for instance in INSTANCES {
        let (listener, shutdown) = peppylib::services::shutdown::listen_for_shutdown(
            &messenger,
            &started.core_node_name,
            instance,
            common::test_node_target("named_robot"),
        )
        .await
        .unwrap();
        listeners.push(listener);
        shutdowns.push(shutdown);
    }
    let pids = INSTANCES.map(|instance| instance_pid(&started, "named_robot", instance));

    let handle = send_accepted_stack_goal(&started, &remove_goal("alpha")).await;
    // The removal asks one instance to shut down, and waits for its process.
    let [first, second] = shutdowns.try_into().expect("two shutdown listeners");
    let (stopped, kept) = tokio::select! {
        asked = first => {
            asked.expect("the removal asks an instance to shut down");
            (0, 1)
        }
        asked = second => {
            asked.expect("the removal asks an instance to shut down");
            (1, 0)
        }
    };
    let end = cancel_and_follow_to_end(&started, handle, async {
        std::process::Command::new("kill")
            .arg("-KILL")
            .arg(pids[stopped].to_string())
            .status()
            .expect("kill runs");
    })
    .await;

    assert_eq!(end.cancel_reason(), "cancelled by its caller");
    let list = participant_request(&started, &StackListRequest::new()).await;
    assert_eq!(list.copy_change, None, "no change holds the stack");
    assert_eq!(list.copies.len(), 1, "the copy stays listed");
    assert_eq!(list.copies[0].name, "alpha");
    assert!(
        !is_process_running(pids[stopped]),
        "the instance the removal stopped is gone"
    );
    assert!(
        is_process_running(pids[kept]),
        "the instance the removal did not reach runs"
    );
    std::process::Command::new("kill")
        .arg("-KILL")
        .arg(pids[kept].to_string())
        .status()
        .expect("kill runs");
}

/// A fleet of one shared node and a repeatable robot axis, both nodes idling
/// in `sleep`; `copies` are the `deployments` entries the file starts beside
/// the shared node.
fn fleet_with_a_shared_node(
    started: &StartedCoreNode,
    copies: &str,
) -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let robot = write_node_config(directory.path(), "named_robot", "v1", &["sleep", "300"]);
    let shared = write_node_config(directory.path(), "shared_node", "v1", &["sleep", "300"]);
    TestPackagesCache::new()
        .fs_entry("named_robot", "v1", &robot)
        .fs_entry("shared_node", "v1", &shared)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    fs::write(&launcher, format!(r#"{{
        peppy_schema: "launcher/v1",
        deployments: [
            {{ source: {{ name: "shared_node", tag: "v1" }}, instances: [{{ instance_id: "shared_inst" }}] }},
            {copies}
        ],
        components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
            real: {{ deployments: [{{ source: {{ name: "named_robot", tag: "v1" }}, instances: [{{ instance_id: "arm_inst" }}] }}] }}
        }} }}]
    }}"#)).unwrap();
    (directory, launcher)
}

fn instance_pid(started: &StartedCoreNode, node: &str, instance_id: &str) -> u32 {
    started
        .node_stack
        .find(node, "v1")
        .expect("node in the stack")
        .read()
        .instances()
        .iter()
        .find(|instance| instance.instance_id().as_str() == instance_id)
        .expect("instance in the stack")
        .pid()
        .expect("running process")
}

/// While a removal stops its copy, `stack list` reports the removal, with
/// the copy's option, as the copy change; once the removal ends, it reports
/// none. The copy's instance hears the shutdown request and exits only when
/// the test kills it, under a grace that outlasts the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_list_reports_a_removal_while_it_stops_its_copy() {
    let started = common::start_core_node_with_shutdown_grace(3600).await;
    let (_directory, launcher) = fleet_with_a_shared_node(
        &started,
        r#"{ robot: "real", instances: [{ instance_id: "alpha" }] }"#,
    );
    let mut responders = answer_readiness(&started, "shared_node", "shared_inst").await;
    responders.extend(answer_readiness(&started, "named_robot", "alpha_arm_inst").await);
    let launch = fleet_launch(launcher, "removal-change-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    let list = participant_request(&started, &StackListRequest::new()).await;
    assert_eq!(list.copy_change, None, "no change holds the stack");

    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let (_listener, shutdown) = peppylib::services::shutdown::listen_for_shutdown(
        &messenger,
        &started.core_node_name,
        "alpha_arm_inst",
        common::test_node_target("named_robot"),
    )
    .await
    .unwrap();
    let alpha_pid = instance_pid(&started, "named_robot", "alpha_arm_inst");
    let removal = remove_goal("alpha");
    let (result, ()) = tokio::join!(execute(&started, &removal), async {
        shutdown
            .await
            .expect("the removal asks the copy to shut down");
        let list = participant_request(&started, &StackListRequest::new()).await;
        assert_eq!(
            list.copy_change,
            Some(CopyChange {
                action: CopyAction::Remove,
                name: config::runtime::CopyName::new("alpha").unwrap(),
                option: "real".to_owned(),
            })
        );
        std::process::Command::new("kill")
            .arg("-KILL")
            .arg(alpha_pid.to_string())
            .status()
            .expect("kill runs");
    });
    result.assert_completed();
    let list = participant_request(&started, &StackListRequest::new()).await;
    assert_eq!(list.copy_change, None);
    assert!(list.copies.is_empty());
}

#[tokio::test]
async fn a_copy_the_file_deploys_launches_with_the_stack_and_removal_keeps_the_stack() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_shared_node(
        &started,
        r#"{ robot: "real", instances: [{ instance_id: "alpha" }] }"#,
    );
    let mut responders = answer_readiness(&started, "shared_node", "shared_inst").await;
    responders.extend(answer_readiness(&started, "named_robot", "alpha_arm_inst").await);
    responders.extend(answer_readiness(&started, "named_robot", "bravo_arm_inst").await);

    let launch = fleet_launch(launcher, "file-copy-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    let shared_pid = instance_pid(&started, "shared_node", "shared_inst");
    assert!(is_process_running(instance_pid(
        &started,
        "named_robot",
        "alpha_arm_inst"
    )));
    let list = participant_request(&started, &StackListRequest::new()).await;
    assert_eq!(list.copies.len(), 1);
    assert_eq!(list.copies[0].name, "alpha");
    assert_eq!(list.copies[0].option, "real");
    assert_eq!(list.copies[0].instance_ids, ["alpha_arm_inst"]);
    assert!(list.copies[0].selections.is_empty());

    let alpha_pid = instance_pid(&started, "named_robot", "alpha_arm_inst");
    let result = execute(&started, &remove_goal("alpha")).await;
    result.assert_completed();
    assert!(!is_process_running(alpha_pid));
    assert_eq!(
        instance_pid(&started, "shared_node", "shared_inst"),
        shared_pid
    );
    let again = execute(&started, &remove_goal("alpha")).await;
    again.assert_failed(PeppyFailureKind::Rejected, "absent");

    // The name is free again, and the stack node keeps running through it.
    let result = execute(&started, &robot_goal("alpha", "real")).await;
    result.assert_completed();
    let result = execute(&started, &robot_goal("bravo", "real")).await;
    result.assert_completed();
    assert_eq!(
        instance_pid(&started, "shared_node", "shared_inst"),
        shared_pid
    );
    let list = participant_request(&started, &StackListRequest::new()).await;
    let names: Vec<_> = list.copies.iter().map(|copy| copy.name.as_str()).collect();
    assert_eq!(names, ["alpha", "bravo"]);
}

/// A fleet whose robot carries a tool axis of its own, the gripper unless
/// selected otherwise, with `entry` written on the robot's option entry
/// beside the copy `alpha` it lists.
fn fleet_with_a_tool_axis(started: &StartedCoreNode, entry: &str) -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let robot = write_node_config(directory.path(), "named_robot", "v1", &["sleep", "300"]);
    TestPackagesCache::new()
        .fs_entry("named_robot", "v1", &robot)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    fs::write(&launcher, format!(r#"{{
        peppy_schema: "launcher/v1",
        components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
            real: {{
                deployments: [{{ tool: "gripper" }}],
                components: [{{ name: "tool", options: {{
                    gripper: {{ deployments: [{{ source: {{ name: "named_robot", tag: "v1" }}, instances: [{{ instance_id: "gripper_inst" }}] }}] }},
                    camera: {{ deployments: [{{ source: {{ name: "named_robot", tag: "v1" }}, instances: [{{ instance_id: "camera_inst" }}] }}] }},
                }} }}],
            }}
        }} }}],
        deployments: [{{ robot: "real", {entry} instances: [{{ instance_id: "alpha" }}] }}],
    }}"#)).unwrap();
    (directory, launcher)
}

/// The selection a launcher writes on an option's entry reaches the copies
/// a join adds: a join with no words runs what the listed copy runs, and a
/// word wins over the entry on its axis.
#[tokio::test]
async fn a_joined_copy_starts_from_the_launchers_entry_for_its_option() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_tool_axis(&started, r#"with: { tool: "camera" },"#);
    let mut responders = Vec::new();
    for instance_id in [
        "alpha_camera_inst",
        "bravo_camera_inst",
        "charlie_gripper_inst",
    ] {
        responders.extend(answer_readiness(&started, "named_robot", instance_id).await);
    }

    let launch = fleet_launch(launcher, "entry-settings-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();

    let result = execute(&started, &robot_goal("bravo", "real")).await;
    result.assert_completed();
    let mut with_a_word = robot_goal("charlie", "real");
    with_a_word.selections = vec!["gripper".parse().unwrap()];
    let result = execute(&started, &with_a_word).await;
    result.assert_completed();

    let list = participant_request(&started, &StackListRequest::new()).await;
    let copies: Vec<_> = list
        .copies
        .iter()
        .map(|copy| {
            (
                copy.name.as_str(),
                copy.instance_ids
                    .iter()
                    .map(Name::as_str)
                    .collect::<Vec<_>>(),
                copy.selections
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        copies,
        [
            (
                "alpha",
                vec!["alpha_camera_inst"],
                vec![String::from("tool=camera")]
            ),
            (
                "bravo",
                vec!["bravo_camera_inst"],
                vec![String::from("tool=camera")]
            ),
            (
                "charlie",
                vec!["charlie_gripper_inst"],
                vec![String::from("tool=gripper")]
            ),
        ]
    );
    for instance_id in [
        "alpha_camera_inst",
        "bravo_camera_inst",
        "charlie_gripper_inst",
    ] {
        assert!(is_process_running(instance_pid(
            &started,
            "named_robot",
            instance_id
        )));
    }
}

/// A join whose new instance id already runs on the host, under a node
/// outside the launcher, is refused by name.
#[tokio::test]
async fn a_join_refuses_a_name_whose_instance_already_runs_on_the_host() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_shared_node(&started, "");
    let _responders = answer_readiness(&started, "shared_node", "shared_inst").await;
    let launch = fleet_launch(launcher, "taken-name-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    let shared_pid = instance_pid(&started, "shared_node", "shared_inst");
    let (_source, _outsider, outsider_pid) =
        seed_running_node(&started, "outsider", "v1", "bravo_arm_inst").await;

    let result = execute(&started, &robot_goal("bravo", "real")).await;
    result.assert_failed(
        PeppyFailureKind::PreconditionFailed,
        "`bravo_arm_inst` already exists",
    );
    assert!(is_process_running(outsider_pid));
    assert_eq!(
        instance_pid(&started, "shared_node", "shared_inst"),
        shared_pid
    );
    let list = participant_request(&started, &StackListRequest::new()).await;
    assert!(list.copies.is_empty());
}

/// A join is refused when the node it would deploy already runs instances
/// the launcher does not track on that host.
#[tokio::test]
async fn a_join_refuses_a_node_running_instances_outside_the_launcher() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_shared_node(&started, "");
    let _responders = answer_readiness(&started, "shared_node", "shared_inst").await;
    let launch = fleet_launch(launcher, "stray-instance-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    let (_source, _stray, stray_pid) =
        seed_running_node(&started, "named_robot", "v1", "stray_inst").await;

    let result = execute(&started, &robot_goal("bravo", "real")).await;
    result.assert_failed(PeppyFailureKind::PreconditionFailed, "named_robot:v1");
    result.assert_failed(
        PeppyFailureKind::PreconditionFailed,
        "outside this launcher",
    );
    assert!(is_process_running(stray_pid));
    let list = participant_request(&started, &StackListRequest::new()).await;
    assert!(list.copies.is_empty());
}

#[tokio::test]
async fn a_coordinator_refuses_to_append_another_launch_onto_its_own_stack() {
    use core_node_api::encoding::{
        ParticipantReleaseRequest, ParticipantReserveRequest, ParticipantSliceBeginRequest,
    };

    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_shared_node(&started, "");
    let mut responders = answer_readiness(&started, "shared_node", "shared_inst").await;
    responders.extend(answer_readiness(&started, "named_robot", "alpha_arm_inst").await);
    let launch = fleet_launch(launcher, "own-stack-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();

    let launch_id = "someone-elses-launch";
    participant_request(
        &started,
        &ParticipantReserveRequest::new(launch_id, "coordinator"),
    )
    .await;
    let mut append = ParticipantSliceBeginRequest::new(launch_id, Vec::new());
    append.append = true;
    // Another launch cannot append onto a stack this daemon coordinates.
    let refusal = participant_refusal(&started, &append).await;
    participant_request(&started, &ParticipantReleaseRequest::new(launch_id)).await;
    assert_failure(
        &refusal,
        PeppyFailureKind::PreconditionFailed,
        "different stack",
    );

    // The launcher this daemon coordinates is intact: its joins still work.
    let result = execute(&started, &robot_goal("alpha", "real")).await;
    result.assert_completed();
}

/// The participant side of a set change: a daemon replaces sets only for the
/// launch whose slice it holds, and only for instances it runs. Each refusal
/// names what to do, and no set is replaced.
#[tokio::test]
async fn a_participant_refuses_sets_for_another_launch_or_an_instance_it_does_not_run() {
    use core_node_api::encoding::{
        ParticipantReserveRequest, ParticipantSetsUpdateRequest, ParticipantSliceBeginRequest,
        SlotMembers, SlotSet,
    };

    let started = start_core_node_with_mock_messenger().await;
    let launch_id = "sets-update-test";
    let sets = |instance: &str| ParticipantSetsUpdateRequest {
        launch_id: launch_id.to_string(),
        sets: vec![SlotSet {
            instance_id: Name::new(instance).unwrap(),
            link_id: "robots".to_string(),
            members: SlotMembers::Producers(config::runtime::BoundProducers::from(
                config::runtime::ProducerRef::new(&started.core_node_name, "alpha_arm_inst"),
            )),
        }],
    };

    // No reservation, no slice: the sets belong to a launch this daemon is not
    // part of.
    let refused = participant_refusal(&started, &sets("monitor_inst")).await;
    assert_failure(
        &refused,
        PeppyFailureKind::PreconditionFailed,
        "must both belong to launch",
    );
    assert!(refused.message().contains("peppy stack reset"), "{refused}");

    // Holding the slice, the daemon still refuses a set for an instance it does
    // not run, naming the slot.
    participant_request(
        &started,
        &ParticipantReserveRequest::new(launch_id, "coordinator"),
    )
    .await;
    participant_request(
        &started,
        &ParticipantSliceBeginRequest::new(launch_id, Vec::new()),
    )
    .await;
    let refused = participant_refusal(&started, &sets("monitor_inst")).await;
    assert_failure(
        &refused,
        PeppyFailureKind::AttemptFailed,
        "`monitor_inst.links.robots`",
    );
    assert!(refused.message().contains("no such instance"), "{refused}");
}

/// A fleet whose every deployment comes from its repeatable robot axis, so
/// the launcher file starts nothing of its own.
fn fleet_of_copies_only(started: &StartedCoreNode) -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let robot = write_node_config(directory.path(), "named_robot", "v1", &["sleep", "300"]);
    TestPackagesCache::new()
        .fs_entry("named_robot", "v1", &robot)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    fs::write(&launcher, r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "robot", cardinality: "zero_or_more", options: {
            real: { deployments: [{ source: { name: "named_robot", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] }
        } }]
    }"#).unwrap();
    (directory, launcher)
}

/// Removing the only copy of a launcher that deploys nothing else empties
/// the stack and leaves the launcher active, so the next copy joins it.
#[tokio::test]
async fn removing_the_last_copy_leaves_the_launcher_active_on_an_empty_stack() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_of_copies_only(&started);
    let mut responders = answer_readiness(&started, "named_robot", "alpha_arm_inst").await;
    responders.extend(answer_readiness(&started, "named_robot", "bravo_arm_inst").await);
    let launch = fleet_launch(launcher, "last-copy-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();

    let result = execute(&started, &robot_goal("alpha", "real")).await;
    result.assert_completed();
    let alpha_pid = instance_pid(&started, "named_robot", "alpha_arm_inst");

    let result = execute(&started, &remove_goal("alpha")).await;
    result.assert_completed();
    assert!(!is_process_running(alpha_pid));
    let list = participant_request(&started, &StackListRequest::new()).await;
    assert!(list.copies.is_empty(), "{:?}", list.copies);
    let graph: core_node_api::SerializedNodeGraph =
        serde_json::from_str(&list.graph_json).expect("the listing carries the stack's graph");
    let running: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| node.stage != core_node_api::NodeStage::Root)
        .flat_map(|node| &node.instances)
        .map(|instance| instance.instance_id.as_str())
        .collect();
    assert!(running.is_empty(), "{running:?}");

    let result = execute(&started, &robot_goal("bravo", "real")).await;
    result.assert_completed();
    assert!(is_process_running(instance_pid(
        &started,
        "named_robot",
        "bravo_arm_inst"
    )));
}

/// A fleet of copies only, whose `real` option deploys `robot`, the
/// deployments of one copy: `named_robot` idles in `sleep`, and
/// `failing_tool` exits as soon as it starts.
fn fleet_of_robots(started: &StartedCoreNode, robot: &str) -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let named_robot = write_node_config(directory.path(), "named_robot", "v1", &["sleep", "300"]);
    let failing_tool = write_node_config_with_options(
        directory.path(),
        "failing_tool",
        "v1",
        NodeConfigOptions {
            run_cmd: &["false"],
            ..Default::default()
        },
    );
    TestPackagesCache::new()
        .fs_entry("named_robot", "v1", &named_robot)
        .fs_entry("failing_tool", "v1", &failing_tool)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    fs::write(
        &launcher,
        format!(
            r#"{{
        peppy_schema: "launcher/v1",
        components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
            real: {{ deployments: [{robot}] }}
        }} }}]
    }}"#
        ),
    )
    .unwrap();
    (directory, launcher)
}

/// A removal reports each instance of the copy before it stops it, the last
/// started first, then the end of the removal.
#[tokio::test]
async fn a_removal_reports_each_instance_it_stops_in_reverse_start_order() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_of_robots(
        &started,
        r#"{ source: { name: "named_robot", tag: "v1" }, instances: [
            { instance_id: "arm_inst" }, { instance_id: "wrist_inst" }, { instance_id: "base_inst" }
        ] }"#,
    );
    let mut responders = Vec::new();
    for instance_id in ["bravo_arm_inst", "bravo_wrist_inst", "bravo_base_inst"] {
        responders.extend(answer_readiness(&started, "named_robot", instance_id).await);
    }
    let launch = fleet_launch(launcher, "removal-report-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    let result = execute(&started, &robot_goal("bravo", "real")).await;
    result.assert_completed();
    let pids: Vec<u32> = ["bravo_arm_inst", "bravo_wrist_inst", "bravo_base_inst"]
        .into_iter()
        .map(|instance_id| instance_pid(&started, "named_robot", instance_id))
        .collect();

    let (result, feedback) = execute_reporting(&started, &remove_goal("bravo")).await;
    result.assert_completed();
    assert_eq!(
        stop_reports(&feedback),
        [
            "Stopping instance `bravo_base_inst` of copy `bravo`",
            "Stopping instance `bravo_wrist_inst` of copy `bravo`",
            "Stopping instance `bravo_arm_inst` of copy `bravo`",
        ]
    );
    assert_eq!(
        feedback.last().map(|output| output.line.as_str()),
        Some("Copy `bravo` removed"),
        "the removal ends after its stops"
    );
    for pid in pids {
        assert!(!is_process_running(pid));
    }
}

/// A join that fails at the start of a later instance undoes the copy and
/// reports each instance it stops: the instances that started, the last
/// started first, and not the one whose start failed, which the stack no
/// longer holds.
#[tokio::test]
async fn the_undo_of_a_failed_join_reports_each_instance_it_stops() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_of_robots(
        &started,
        r#"{ source: { name: "named_robot", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] },
           { source: { name: "failing_tool", tag: "v1" }, instances: [{ instance_id: "tool_inst" }] }"#,
    );
    let _responders = answer_readiness(&started, "named_robot", "bravo_arm_inst").await;
    let launch = fleet_launch(launcher, "undo-report-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();

    let (result, feedback) = execute_reporting(&started, &robot_goal("bravo", "real")).await;
    // The tool exits as it starts.
    result.assert_failed(
        PeppyFailureKind::AttemptFailed,
        "failed to start node failing_tool:v1 instance bravo_tool_inst",
    );
    let started_instances: Vec<_> = result
        .node_logs
        .node_run_logs
        .iter()
        .map(|log| (log.instance_id.as_str(), log.failed))
        .collect();
    assert_eq!(
        started_instances,
        [("bravo_arm_inst", false), ("bravo_tool_inst", true)],
        "the arm starts, then the tool fails its start"
    );
    assert_eq!(
        stop_reports(&feedback),
        ["Stopping instance `bravo_arm_inst` of copy `bravo`"]
    );
    let list = participant_request(&started, &StackListRequest::new()).await;
    assert!(list.copies.is_empty(), "{:?}", list.copies);
    assert!(
        started
            .node_stack
            .find("named_robot", "v1")
            .is_none_or(|node| node.read().instances().is_empty()),
        "the undo stops the arm"
    );
}

/// The robot of a fleet with a monitor: it plays the follower of `joint_link`
/// through a participant slot the launch writes vacant, and emits a heartbeat a
/// consumer binds to.
const MONITORED_ROBOT_DEPENDS_ON: &str = r#"{ pairings: [
    { name: "joint_link", tag: "v1", role: "follower", link_id: "controller", cardinality: "zero_or_one" }
] }"#;
const MONITORED_ROBOT_INTERFACES: &str = r#"{ topics: {
    emits: [{ name: "heartbeat" }, { link_id: "controller", name: "joint_states" }],
    consumes: [{ link_id: "controller", name: "joint_setpoints" }]
} }"#;
const MONITOR_INTERFACES: &str = r#"{ topics: { consumes: [
    { link_id: "robots", name: "heartbeat" },
    { link_id: "fleet", name: "joint_states" }
] } }"#;

/// A stack running `monitor_inst`, whose producer slot `robots` and observer
/// slot `fleet` both declare `cardinality`, and a `robot` axis whose `real`
/// copies add their arm to both. The `ghost` option adds to a slot the monitor
/// does not declare. `deployments` lists what the launch starts beside the
/// monitor.
fn fleet_with_a_monitor(
    started: &StartedCoreNode,
    cardinality: &str,
    deployments: &str,
) -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let robot = write_node_config_with_options(
        directory.path(),
        "named_robot",
        "v1",
        NodeConfigOptions {
            run_cmd: &["sleep", "300"],
            depends_on: MONITORED_ROBOT_DEPENDS_ON,
            interfaces: MONITORED_ROBOT_INTERFACES,
            ..Default::default()
        },
    );
    let monitor_depends_on = format!(
        r#"{{
        nodes: [{{ name: "named_robot", tag: "v1", link_id: "robots", cardinality: "{cardinality}" }}],
        pairing_observers: [{{ name: "joint_link", tag: "v1", role: "follower", link_id: "fleet", cardinality: "{cardinality}" }}]
    }}"#
    );
    let monitor = write_node_config_with_options(
        directory.path(),
        "monitor",
        "v1",
        NodeConfigOptions {
            run_cmd: &["sleep", "300"],
            depends_on: &monitor_depends_on,
            interfaces: MONITOR_INTERFACES,
            ..Default::default()
        },
    );
    let pairing = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/launch_assets/waldo_codegen/pairings/joint_link.json5");
    let body = fs::read_to_string(&pairing).expect("read the joint_link pairing fixture");
    TestPackagesCache::new()
        .fs_entry("named_robot", "v1", &robot)
        .fs_entry("monitor", "v1", &monitor)
        .pairing_fs_entry("joint_link", "v1", &pairing, &body)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    let robot_deployment = r#"deployments: [{ source: { name: "named_robot", tag: "v1" }, instances: [
                    { instance_id: "arm_inst", links: { controller: { vacant: "observed, never driven" } } }
                ] }]"#;
    fs::write(
        &launcher,
        format!(
            r#"{{
        peppy_schema: "launcher/v1",
        deployments: [
            {{ source: {{ name: "monitor", tag: "v1" }}, instances: [{{ instance_id: "monitor_inst" }}] }},
            {deployments}
        ],
        components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
            real: {{
                {robot_deployment},
                adjustments: [{{ target: "monitor_inst", add_links: {{ robots: ["arm_inst"], fleet: ["arm_inst/controller"] }} }}]
            }},
            ghost: {{
                {robot_deployment},
                adjustments: [{{ target: "monitor_inst", add_links: {{ ghosts: ["arm_inst"] }} }}]
            }}
        }} }}]
    }}"#
        ),
    )
    .unwrap();
    (directory, launcher)
}

/// The monitor's emulated services, with each set slot's state held in a watch
/// channel exactly as a node's processor holds it.
struct MonitorSlots {
    _services: Vec<AbortOnDrop<peppylib::PeppyResult<()>>>,
    robots: tokio::sync::watch::Receiver<peppylib::messaging::BoundSetState>,
    fleet: tokio::sync::watch::Receiver<peppylib::messaging::ObservationState>,
}

impl MonitorSlots {
    async fn serve(started: &StartedCoreNode, robots_cardinality: Cardinality) -> Self {
        Self::serve_holding(
            started,
            robots_cardinality,
            peppylib::messaging::BoundSetState::seeded(config::runtime::BoundProducers::default()),
        )
        .await
    }

    /// [`Self::serve`] with `robots` already holding `held`, as a monitor that
    /// took deliveries before this test looked.
    async fn serve_holding(
        started: &StartedCoreNode,
        robots_cardinality: Cardinality,
        held: peppylib::messaging::BoundSetState,
    ) -> Self {
        let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
        let mut services = answer_readiness(started, "monitor", "monitor_inst").await;
        let (robots_tx, robots) = tokio::sync::watch::channel(held);
        services.push(AbortOnDrop(
            peppylib::services::binding_update::listen_for_binding_update(
                &messenger,
                &started.core_node_name,
                "monitor_inst",
                common::test_node_target("monitor"),
                Arc::new(std::collections::BTreeMap::from([(
                    "robots".to_string(),
                    SlotChannel::new(robots_cardinality, robots_tx),
                )])),
            )
            .await
            .unwrap(),
        ));
        let (fleet_tx, fleet) =
            tokio::sync::watch::channel(peppylib::messaging::ObservationState::unregistered());
        services.push(AbortOnDrop(
            peppylib::services::observation_update::listen_for_observation_update(
                &messenger,
                &started.core_node_name,
                "monitor_inst",
                common::test_node_target("monitor"),
                Arc::new(std::collections::BTreeMap::from([(
                    "fleet".to_string(),
                    SlotChannel::new(Cardinality::OneOrMore, fleet_tx),
                )])),
            )
            .await
            .unwrap(),
        ));
        Self {
            _services: services,
            robots,
            fleet,
        }
    }

    /// Each member of `robots` as `instance:copy/in_copy`, the copy the
    /// daemon says the member's instance belongs to and the id the copy's
    /// fragment wrote for it.
    fn robots(&self) -> Vec<String> {
        self.robots
            .borrow()
            .producers
            .iter()
            .map(|member| {
                format!(
                    "{}:{}",
                    member.producer.instance_id,
                    member
                        .copy
                        .as_ref()
                        .map(|copy| format!("{}/{}", copy.name, copy.instance_id))
                        .unwrap_or_else(|| "-".to_string())
                )
            })
            .collect()
    }

    fn fleet(&self) -> Vec<String> {
        self.fleet
            .borrow()
            .members
            .iter()
            .map(|member| {
                format!(
                    "{}/{}",
                    member.source.producer.instance_id, member.source.source_link_id
                )
            })
            .collect()
    }
}

/// A monitor already holding a later delivery answers a join's set as one it
/// will not take: the join stands, the monitor keeps what it holds, and the
/// stack keeps the set it recorded for the monitor.
#[tokio::test]
async fn a_monitor_holding_a_later_set_keeps_its_record_through_a_join() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_monitor(&started, "zero_or_more", "");
    let monitor = MonitorSlots::serve_holding(
        &started,
        Cardinality::ZeroOrMore,
        peppylib::messaging::BoundSetState {
            sequence: u64::MAX,
            producers: config::runtime::BoundProducers::default(),
        },
    )
    .await;
    let launch = fleet_launch(launcher, "stale-set-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    let _responders = answer_readiness(&started, "named_robot", "alpha_arm_inst").await;

    let result = execute(&started, &robot_goal("alpha", "real")).await;
    result.assert_completed();
    assert!(
        monitor.robots().is_empty(),
        "the monitor keeps the later set it holds"
    );

    let list = participant_request(&started, &StackListRequest::new()).await;
    let graph: core_node_api::SerializedNodeGraph =
        serde_json::from_str(&list.graph_json).expect("the listing carries the stack's graph");
    let recorded = graph
        .nodes
        .iter()
        .flat_map(|node| &node.instances)
        .find(|instance| instance.instance_id == "monitor_inst")
        .expect("the monitor runs")
        .slot_bindings
        .get("robots")
        .map(|bound| bound.len());
    assert_eq!(
        recorded,
        Some(0),
        "the stack records what the monitor holds, which the refused set is not"
    );
}

/// A running monitor's `zero_or_more` producer and observer slots follow the
/// copies: each join adds the copy's arm to both in join order, a removal
/// takes out that copy's arm alone, a rejoin under the same name adds it once,
/// and the monitor keeps running throughout. `stack list` names the members
/// each copy added, and a join adding to a slot the monitor does not declare
/// is refused, naming it.
#[tokio::test]
async fn joins_grow_and_removals_shrink_a_running_monitors_sets() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_monitor(&started, "zero_or_more", "");
    let monitor = MonitorSlots::serve(&started, Cardinality::ZeroOrMore).await;
    let launch = fleet_launch(launcher, "growing-sets-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    assert!(
        monitor.robots().is_empty(),
        "the monitor starts binding no robot"
    );
    assert!(
        monitor.fleet().is_empty(),
        "the monitor starts observing no robot"
    );
    let monitor_pid = instance_pid(&started, "monitor", "monitor_inst");

    let mut responders = Vec::new();
    for name in ["alpha", "bravo"] {
        responders
            .extend(answer_readiness(&started, "named_robot", &format!("{name}_arm_inst")).await);
    }
    let join = async |name: &str| {
        let result = execute(&started, &robot_goal(name, "real")).await;
        result.assert_completed();
    };

    join("alpha").await;
    assert_eq!(monitor.robots(), ["alpha_arm_inst:alpha/arm_inst"]);
    assert_eq!(monitor.fleet(), ["alpha_arm_inst/controller"]);
    join("bravo").await;
    assert_eq!(
        monitor.robots(),
        [
            "alpha_arm_inst:alpha/arm_inst",
            "bravo_arm_inst:bravo/arm_inst"
        ]
    );
    assert_eq!(
        monitor.fleet(),
        ["alpha_arm_inst/controller", "bravo_arm_inst/controller"]
    );

    let list =
        common::poll_core_node_with_timeout(&started, &StackListRequest::new(), GOAL_TIMEOUT).await;
    let bravo = list
        .copies
        .iter()
        .find(|copy| copy.name == "bravo")
        .expect("bravo is listed");
    assert_eq!(
        bravo
            .set_members
            .iter()
            .map(|member| format!(
                "{}.links.{} -> {}",
                member.instance_id, member.link_id, member.target
            ))
            .collect::<Vec<_>>(),
        [
            "monitor_inst.links.fleet -> bravo_arm_inst/controller",
            "monitor_inst.links.robots -> bravo_arm_inst",
        ]
    );

    let result = execute(&started, &remove_goal("alpha")).await;
    result.assert_completed();
    assert_eq!(monitor.robots(), ["bravo_arm_inst:bravo/arm_inst"]);
    assert_eq!(monitor.fleet(), ["bravo_arm_inst/controller"]);

    join("alpha").await;
    assert_eq!(
        monitor.robots(),
        [
            "bravo_arm_inst:bravo/arm_inst",
            "alpha_arm_inst:alpha/arm_inst"
        ]
    );
    assert_eq!(
        monitor.fleet(),
        ["bravo_arm_inst/controller", "alpha_arm_inst/controller"]
    );

    // A join adding to an undeclared slot fails.
    let refused = execute(&started, &robot_goal("charlie", "ghost")).await;
    refused.assert_failed(PeppyFailureKind::Rejected, "`monitor_inst.links.ghosts`");
    refused.assert_failed(PeppyFailureKind::Rejected, "does not declare");
    assert_eq!(
        instance_pid(&started, "monitor", "monitor_inst"),
        monitor_pid,
        "the monitor keeps running while its sets change"
    );
}

/// A member a join adds behaves like one the launch wrote: the monitor reads
/// it stamped with its source's incarnation and live while the copy runs, and
/// not live, at its position, once the copy's instance stops.
#[tokio::test]
async fn a_joined_member_reads_as_not_live_while_its_source_is_down() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_monitor(&started, "zero_or_more", "");
    let monitor = MonitorSlots::serve(&started, Cardinality::ZeroOrMore).await;
    let launch = fleet_launch(launcher, "joined-member-liveness-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    let _responders = answer_readiness(&started, "named_robot", "alpha_arm_inst").await;
    let result = execute(&started, &robot_goal("alpha", "real")).await;
    result.assert_completed();

    let liveness = |state: &peppylib::messaging::ObservationState| -> Vec<(String, bool)> {
        state
            .members
            .iter()
            .map(|member| {
                (
                    member.source.producer.instance_id.clone(),
                    member.source_live,
                )
            })
            .collect()
    };
    let mut fleet = monitor.fleet.clone();
    let generation = tokio::time::timeout(
        GOAL_TIMEOUT,
        fleet.wait_for(|state| liveness(state) == [("alpha_arm_inst".to_string(), true)]),
    )
    .await
    .expect("the joined member reads as live while its copy runs")
    .unwrap()
    .members[0]
        .source_generation;
    assert!(
        generation >= 1,
        "a joined member carries its source's incarnation"
    );

    stop_instance(&started, "alpha_arm_inst").await;
    tokio::time::timeout(
        GOAL_TIMEOUT,
        fleet.wait_for(|state| liveness(state) == [("alpha_arm_inst".to_string(), false)]),
    )
    .await
    .expect("the joined member reads as not live, at its position, while its source is down")
    .unwrap();
}

/// The last copy of a `one_or_more` axis stays: its removal is refused,
/// naming the copy and the axis, while a copy above the floor goes and the
/// whole stack still comes down.
#[tokio::test]
async fn the_last_copy_of_a_one_or_more_axis_stays_until_the_stack_goes() {
    let started = start_core_node_with_mock_messenger().await;
    let directory = tempdir().unwrap();
    let robot = write_node_config(directory.path(), "named_robot", "v1", &["sleep", "300"]);
    TestPackagesCache::new()
        .fs_entry("named_robot", "v1", &robot)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    fs::write(
        &launcher,
        r#"{
        peppy_schema: "launcher/v1",
        components: [{ name: "robot", cardinality: "one_or_more", options: {
            real: { deployments: [{ source: { name: "named_robot", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }] }
        } }],
        deployments: [{ robot: "real", instances: [{ instance_id: "alpha" }] }]
    }"#,
    )
    .unwrap();
    let _alpha = answer_readiness(&started, "named_robot", "alpha_arm_inst").await;
    let launch = fleet_launch(launcher, "one-or-more-axis-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    let pid = robot_pid(&started, "alpha");

    let _bravo = answer_readiness(&started, "named_robot", "bravo_arm_inst").await;
    let joined = execute(&started, &robot_goal("bravo", "real")).await;
    joined.assert_completed();
    let above_floor = execute(&started, &remove_goal("bravo")).await;
    above_floor.assert_completed();

    // The last copy of a one_or_more axis stays.
    let refused = execute(&started, &remove_goal("alpha")).await;
    refused.assert_failed(
        PeppyFailureKind::PreconditionFailed,
        "copy `alpha` is the last one on axis `robot`, which has cardinality `one_or_more`",
    );
    assert!(
        is_process_running(pid),
        "a refused removal leaves the copy running"
    );

    participant_request(&started, &core_node_api::encoding::StackResetRequest::new()).await;
    assert!(
        !is_process_running(pid),
        "reset takes the last copy down with the stack"
    );
}

/// A removal that would leave a running `one_or_more` set with no member is
/// refused, naming the slot, and the copy keeps running.
#[tokio::test]
async fn a_removal_cannot_empty_a_running_one_or_more_set() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_monitor(
        &started,
        "one_or_more",
        r#"{ robot: "real", instances: [{ instance_id: "first" }] }"#,
    );
    let _monitor = MonitorSlots::serve(&started, Cardinality::OneOrMore).await;
    let _first = answer_readiness(&started, "named_robot", "first_arm_inst").await;
    let launch = fleet_launch(launcher, "one-or-more-set-test");
    let result = execute(&started, &launch).await;
    result.assert_completed();
    let pid = instance_pid(&started, "named_robot", "first_arm_inst");

    // The last member of a one_or_more set cannot leave.
    let refused = execute(&started, &remove_goal("first")).await;
    refused.assert_failed(
        PeppyFailureKind::PreconditionFailed,
        "`monitor_inst.links.fleet`",
    );
    refused.assert_failed(PeppyFailureKind::PreconditionFailed, "one_or_more");
    assert!(
        is_process_running(pid),
        "a refused removal leaves the copy running"
    );
}
