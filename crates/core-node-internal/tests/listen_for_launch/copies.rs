use super::*;
use core_node_api::encoding::{StackJoinGoal, StackListRequest, StackRemoveGoal};
use peppylib::core_node::transport::{poll, send_goal};

async fn participant_request<R: core_node_api::ServiceRequest>(
    started: &StartedCoreNode,
    request: &R,
) -> R::Response {
    poll(
        request,
        &started.caller_handle,
        "coordinator",
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        GOAL_TIMEOUT,
    )
    .await
    .unwrap()
}

async fn execute(started: &StartedCoreNode, goal: &impl core_node_api::ActionGoal) -> LaunchResult {
    tokio::time::timeout(RESULT_TIMEOUT, async {
        let mut handle = send_goal(
            goal,
            &started.caller_handle,
            &started.core_node_name,
            CALLER_INSTANCE_ID,
            Some(&started.core_node_name),
            GOAL_TIMEOUT,
        )
        .await
        .expect("send stack goal");
        let response = LaunchGoalResponse::decode(handle.goal_reply().body.as_ref())
            .expect("decode acceptance");
        assert!(response.accepted, "{:?}", response.rejection_reason);
        while handle.on_next_feedback().await.is_ok() {}
        let body =
            ActionMessenger::request_result_body(&started.caller_handle, &handle, GOAL_TIMEOUT)
                .await
                .expect("fetch result");
        LaunchResult::decode(body.as_ref()).expect("decode result")
    })
    .await
    .expect("stack operation completes within its test budget")
}

/// Sends a goal the daemon rejects at admission and returns its reason.
async fn refusal(started: &StartedCoreNode, goal: &impl core_node_api::ActionGoal) -> String {
    let handle = send_goal(
        goal,
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        Some(&started.core_node_name),
        GOAL_TIMEOUT,
    )
    .await
    .expect("send stack goal");
    let response =
        LaunchGoalResponse::decode(handle.goal_reply().body.as_ref()).expect("decode acceptance");
    assert!(!response.accepted, "the daemon admitted the goal");
    response
        .rejection_reason
        .expect("a refusal names its reason")
}

fn robot_goal(name: &str, option: &str) -> StackJoinGoal {
    StackJoinGoal::new(Name::new(name).unwrap(), option, StackBudgets::default())
}

fn robot_pid(started: &StartedCoreNode, name: &str) -> u32 {
    let node = started
        .node_stack
        .find("named_robot", "v1")
        .expect("robot node");
    node.read()
        .instances()
        .iter()
        .find(|instance| instance.instance_id().as_str() == format!("{name}_arm_inst"))
        .expect("robot instance")
        .pid()
        .expect("running process")
}

#[tokio::test]
async fn appended_remote_watchers_receive_existing_source_stop_notifications() {
    use config::runtime::CoreNodeName;
    use core_node_api::{
        ServiceId,
        encoding::{
            NodeStopRequest, ParticipantReleaseRequest, ParticipantReserveRequest,
            ParticipantSliceBeginRequest, RelationshipEvent, RelationshipNotification,
            RelationshipNotificationAck,
        },
    };
    use peppylib::ServiceMessenger;
    use peppylib::core_node::transport::poll_node_stop;

    let started = start_core_node_with_mock_messenger().await;
    let launch_id = "watcher-join-test";
    let reserve = ParticipantReserveRequest::new(launch_id, "coordinator");
    let request = ParticipantSliceBeginRequest::new(launch_id, Vec::new());
    let reserved = participant_request(&started, &reserve).await;
    assert!(reserved.accepted, "{:?}", reserved.rejection_reason);
    let begun = participant_request(&started, &request).await;
    assert!(begun.ok, "{:?}", begun.rejection_reason);
    let released = participant_request(&started, &ParticipantReleaseRequest::new(launch_id)).await;
    assert!(released.ok);

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
                        .map_err(Into::into)
                }
            })
            .await
    }));
    let reserved = participant_request(&started, &reserve).await;
    assert!(reserved.accepted);
    let mut append = ParticipantSliceBeginRequest::new(launch_id, Vec::new());
    append.append = true;
    append.lifecycle_watchers.insert(
        Name::new("arm_inst").unwrap(),
        std::collections::BTreeSet::from([CoreNodeName::new("cloud").unwrap()]),
    );
    let begun = participant_request(&started, &append).await;
    assert!(begun.ok, "{:?}", begun.rejection_reason);
    let released = participant_request(&started, &ParticipantReleaseRequest::new(launch_id)).await;
    assert!(released.ok);
    assert!(is_process_running(pid));
    let _shutdown = common::install_kill_on_shutdown_listener(
        &started,
        "watched_arm",
        &Name::new("arm_inst").unwrap(),
        pid,
    )
    .await;
    let stopped = poll_node_stop(
        &NodeStopRequest::new("arm_inst"),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        &started.core_node_name,
        GOAL_TIMEOUT,
    )
    .await
    .unwrap();
    assert!(stopped.success, "{:?}", stopped.error_message);
    let event = tokio::time::timeout(GOAL_TIMEOUT, received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        RelationshipNotification::new(
            "arm_inst",
            &started.core_node_name,
            RelationshipEvent::Stopped
        )
    );
}

#[tokio::test]
async fn six_copies_join_remove_and_rejoin_without_restarting_their_neighbors() {
    let started = start_core_node_with_mock_messenger().await;
    let directory = tempdir().unwrap();
    let robot = write_node_config(
        directory.path(),
        "named_robot",
        "v1",
        "test-hash",
        &["sleep", "300"],
        false,
        false,
    );
    let broken = write_node_config_with_options(
        directory.path(),
        "broken_robot",
        "v1",
        "test-hash",
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
    let launch = LaunchGoal::new(
        LauncherOrigin::Fs(launcher.clone()),
        "named-fragment-test",
        StackBudgets::new(30, 30, 30, Some(120)),
    );
    let result = execute(&started, &launch).await;
    assert!(result.success, "{:?}", result.error_message);
    assert_eq!(
        started.node_stack.len(),
        1,
        "the fleet starts with no robot"
    );

    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let mut responders = Vec::new();
    let mut pids = std::collections::BTreeMap::new();
    for name in ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"] {
        let id = format!("{name}_arm_inst");
        responders.push(AbortOnDrop(
            listen_for_node_ready(
                &messenger,
                &started.core_node_name,
                &id,
                common::test_node_target("named_robot"),
            )
            .await
            .unwrap(),
        ));
        responders.push(AbortOnDrop(
            listen_for_node_health(
                &messenger,
                &started.core_node_name,
                &id,
                common::test_node_target("named_robot"),
            )
            .await
            .unwrap(),
        ));
        let result = execute(&started, &robot_goal(name, "real")).await;
        assert!(result.success, "{name}: {:?}", result.error_message);
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
    assert!(!duplicate.success);
    assert!(duplicate.error_message.unwrap().contains("alpha"));
    let failure = execute(&started, &robot_goal("failed", "broken")).await;
    assert!(!failure.success, "a failing build must refuse the join");
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

    let result = execute(&started, &StackRemoveGoal::new(Name::new("bravo").unwrap())).await;
    assert!(result.success, "{:?}", result.error_message);
    assert!(
        !is_process_running(pids.remove("bravo").unwrap()),
        "removed process has exited"
    );
    let list = poll(
        &StackListRequest::new(),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        GOAL_TIMEOUT,
    )
    .await
    .unwrap();
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
    let stopped = peppylib::core_node::transport::poll_node_stop(
        &core_node_api::encoding::NodeStopRequest::new("charlie_arm_inst"),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        &started.core_node_name,
        GOAL_TIMEOUT,
    )
    .await
    .unwrap();
    assert!(stopped.success, "{:?}", stopped.error_message);
    assert!(!is_process_running(pids.remove("charlie").unwrap()));
    let result = execute(&started, &robot_goal("bravo", "real")).await;
    assert!(result.success, "{:?}", result.error_message);
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
        execute(&started, &StackRemoveGoal::new(Name::new("alpha").unwrap())).await,
    ] {
        assert!(!result.success);
        assert!(result.error_message.unwrap().contains("stack launch"));
    }
    assert_eq!(started.node_stack.len(), 1);
}

/// While a join builds, another join and a remove are refused as busy; a
/// reset interrupts the build and a fresh launch follows.
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
        "test-hash",
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
    let launch = LaunchGoal::new(
        LauncherOrigin::Fs(launcher),
        "reset-join",
        StackBudgets::new(30, 30, 30, Some(120)),
    );
    assert!(execute(&started, &launch).await.success);
    let join = robot_goal("alpha", "simulated");
    let (result, ()) = tokio::join!(execute(&started, &join), async {
        let pid = common::poll_until(GOAL_TIMEOUT, "join build records its process", || {
            fs::read_to_string(&pid_file)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
        })
        .await;
        let busy = "a stack or node operation is in progress";
        assert!(
            refusal(&started, &robot_goal("bravo", "simulated"))
                .await
                .contains(busy)
        );
        let remove = StackRemoveGoal::new(Name::new("alpha").unwrap());
        assert!(refusal(&started, &remove).await.contains(busy));
        assert!(is_process_running(pid), "refusals leave the build running");
        let response =
            participant_request(&started, &core_node_api::encoding::StackResetRequest::new()).await;
        assert!(response.success, "{:?}", response.error_message);
        assert!(
            !is_process_running(pid),
            "reset reaps the joined build process"
        );
        assert_eq!(started.node_stack.len(), 1);
    },);
    assert!(!result.success);
    assert!(result.error_message.unwrap().contains("reset"));
    let result = execute(&started, &launch).await;
    assert!(result.success, "{:?}", result.error_message);
}

/// A fleet of one shared node and a repeatable robot axis, both nodes idling
/// in `sleep`; `copies` are the `deployments` entries the file starts beside
/// the shared node.
fn fleet_with_a_shared_node(
    started: &StartedCoreNode,
    copies: &str,
) -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let robot = write_node_config(
        directory.path(),
        "named_robot",
        "v1",
        "test-hash",
        &["sleep", "300"],
        false,
        false,
    );
    let shared = write_node_config(
        directory.path(),
        "shared_node",
        "v1",
        "test-hash",
        &["sleep", "300"],
        false,
        false,
    );
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

async fn answer_readiness(
    started: &StartedCoreNode,
    node: &str,
    instance_id: &str,
) -> Vec<AbortOnDrop<peppylib::PeppyResult<()>>> {
    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    vec![
        AbortOnDrop(
            listen_for_node_ready(
                &messenger,
                &started.core_node_name,
                instance_id,
                common::test_node_target(node),
            )
            .await
            .unwrap(),
        ),
        AbortOnDrop(
            listen_for_node_health(
                &messenger,
                &started.core_node_name,
                instance_id,
                common::test_node_target(node),
            )
            .await
            .unwrap(),
        ),
    ]
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

    let launch = LaunchGoal::new(
        LauncherOrigin::Fs(launcher),
        "file-copy-test",
        StackBudgets::new(30, 30, 30, Some(120)),
    );
    let result = execute(&started, &launch).await;
    assert!(result.success, "{:?}", result.error_message);
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
    let result = execute(&started, &StackRemoveGoal::new(Name::new("alpha").unwrap())).await;
    assert!(result.success, "{:?}", result.error_message);
    assert!(!is_process_running(alpha_pid));
    assert_eq!(
        instance_pid(&started, "shared_node", "shared_inst"),
        shared_pid
    );
    let again = execute(&started, &StackRemoveGoal::new(Name::new("alpha").unwrap())).await;
    assert!(!again.success);
    assert!(again.error_message.unwrap().contains("absent"));

    // The name is free again, and the stack node keeps running through it.
    let result = execute(&started, &robot_goal("alpha", "real")).await;
    assert!(result.success, "{:?}", result.error_message);
    let result = execute(&started, &robot_goal("bravo", "real")).await;
    assert!(result.success, "{:?}", result.error_message);
    assert_eq!(
        instance_pid(&started, "shared_node", "shared_inst"),
        shared_pid
    );
    let list = participant_request(&started, &StackListRequest::new()).await;
    let names: Vec<_> = list.copies.iter().map(|copy| copy.name.as_str()).collect();
    assert_eq!(names, ["alpha", "bravo"]);
}

/// A join whose new instance id already runs on the host, under a node
/// outside the launcher, is refused by name.
#[tokio::test]
async fn a_join_refuses_a_name_whose_instance_already_runs_on_the_host() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_shared_node(&started, "");
    let _responders = answer_readiness(&started, "shared_node", "shared_inst").await;
    let launch = LaunchGoal::new(
        LauncherOrigin::Fs(launcher),
        "taken-name-test",
        StackBudgets::new(30, 30, 30, Some(120)),
    );
    let result = execute(&started, &launch).await;
    assert!(result.success, "{:?}", result.error_message);
    let shared_pid = instance_pid(&started, "shared_node", "shared_inst");
    let (_source, _outsider, outsider_pid) =
        seed_running_node(&started, "outsider", "v1", "bravo_arm_inst").await;

    let result = execute(&started, &robot_goal("bravo", "real")).await;
    assert!(!result.success);
    let message = result.error_message.unwrap();
    assert!(
        message.contains("`bravo_arm_inst` already exists"),
        "{message}"
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
    let launch = LaunchGoal::new(
        LauncherOrigin::Fs(launcher),
        "stray-instance-test",
        StackBudgets::new(30, 30, 30, Some(120)),
    );
    let result = execute(&started, &launch).await;
    assert!(result.success, "{:?}", result.error_message);
    let (_source, _stray, stray_pid) =
        seed_running_node(&started, "named_robot", "v1", "stray_inst").await;

    let result = execute(&started, &robot_goal("bravo", "real")).await;
    assert!(!result.success);
    let message = result.error_message.unwrap();
    assert!(message.contains("named_robot:v1"), "{message}");
    assert!(message.contains("outside this launcher"), "{message}");
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
    let launch = LaunchGoal::new(
        LauncherOrigin::Fs(launcher),
        "own-stack-test",
        StackBudgets::new(30, 30, 30, Some(120)),
    );
    let result = execute(&started, &launch).await;
    assert!(result.success, "{:?}", result.error_message);

    let launch_id = "someone-elses-launch";
    let reserved = participant_request(
        &started,
        &ParticipantReserveRequest::new(launch_id, "coordinator"),
    )
    .await;
    let mut append = ParticipantSliceBeginRequest::new(launch_id, Vec::new());
    append.append = true;
    assert!(reserved.accepted, "{:?}", reserved.rejection_reason);
    let response = participant_request(&started, &append).await;
    participant_request(&started, &ParticipantReleaseRequest::new(launch_id)).await;
    assert!(
        !response.ok,
        "another launch cannot append onto a stack this daemon coordinates"
    );
    let refusal = response.rejection_reason.unwrap();
    assert!(refusal.contains("different stack"), "{refusal}");

    // The launcher this daemon coordinates is intact: its joins still work.
    let result = execute(&started, &robot_goal("alpha", "real")).await;
    assert!(result.success, "{:?}", result.error_message);
}

/// A fleet whose every deployment comes from its repeatable robot axis, so
/// the launcher file starts nothing of its own.
fn fleet_of_copies_only(started: &StartedCoreNode) -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let robot = write_node_config(
        directory.path(),
        "named_robot",
        "v1",
        "test-hash",
        &["sleep", "300"],
        false,
        false,
    );
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
    let launch = LaunchGoal::new(
        LauncherOrigin::Fs(launcher),
        "last-copy-test",
        StackBudgets::new(30, 30, 30, Some(120)),
    );
    let result = execute(&started, &launch).await;
    assert!(result.success, "{:?}", result.error_message);

    let result = execute(&started, &robot_goal("alpha", "real")).await;
    assert!(result.success, "{:?}", result.error_message);
    let alpha_pid = instance_pid(&started, "named_robot", "alpha_arm_inst");

    let result = execute(&started, &StackRemoveGoal::new(Name::new("alpha").unwrap())).await;
    assert!(result.success, "{:?}", result.error_message);
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
    assert!(result.success, "{:?}", result.error_message);
    assert!(is_process_running(instance_pid(
        &started,
        "named_robot",
        "bravo_arm_inst"
    )));
}
