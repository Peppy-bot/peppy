//! The fleet of a launch through `fleet:v1`, which the daemon serves on the
//! messaging layer: the listing of each robot with its software, the
//! leases, the move check, the halt and the changes, each call checked
//! against the scope of its sender; and the refusal of the raw calls of
//! the daemon from the instances of the stack.

use super::copies::{execute, fleet_launch, participant_request, robot_goal};
use super::*;
use crate::common::{new_test_data_dir, start_core_node_with_mock_messenger_on};
use core_node_api::encoding::{
    NodeStopRequest, StackListRequest, StackRemoveGoal, StackResetRequest,
};
use daemon_config::consts::PeppyDirs;
use daemon_config::daemon_interface::DaemonInterface;
use daemon_config::daemon_interface::fleet::{FleetChange, FleetListing, FleetMember};
use peppy_codec::consumer::{
    ActionClient, ConsumerError, ConsumerIdentity, GoalHandle, GoalOutcome, MemberBinding,
    ServiceClient,
};
use peppylib::core_node::transport::{poll, poll_node_stop, send_goal};
use peppylib::messaging::{DEFAULT_FEEDBACK_BOUND, ProducerRef};
use serde_json::{Value, json};

/// The lease holder of the launch, deployed outside any copy.
const DIRECTOR: &str = "director_inst";
/// The instance of the copy `alpha`.
const ALPHA_ARM: &str = "alpha_arm_inst";

/// A client of a daemon interface that sends as `instance_id` on the
/// daemon's machine, to the daemon that serves the interface.
pub(super) struct InterfaceClient<'a> {
    started: &'a StartedCoreNode,
    interface: DaemonInterface,
    identity: ConsumerIdentity,
    daemon: ProducerRef,
}

/// A client of `fleet:v1`.
type FleetClient<'a> = InterfaceClient<'a>;

impl<'a> InterfaceClient<'a> {
    /// A client of `fleet:v1` that sends as `instance_id`.
    async fn sending_as(started: &'a StartedCoreNode, instance_id: &str) -> Self {
        Self::of(started, DaemonInterface::Fleet, instance_id).await
    }

    /// A client of `interface` that sends as `instance_id`.
    pub(super) async fn of(
        started: &'a StartedCoreNode,
        interface: DaemonInterface,
        instance_id: &str,
    ) -> Self {
        let listed = participant_request(started, &StackListRequest::new()).await;
        Self {
            started,
            interface,
            identity: ConsumerIdentity {
                core_node: started.core_node_name.clone(),
                instance_id: instance_id.to_owned(),
            },
            daemon: ProducerRef::new(&listed.core_node, &listed.instance_id),
        }
    }

    /// The answer of the service `member` to `request`.
    pub(super) async fn call(
        &self,
        member: impl Member,
        request: Value,
    ) -> Result<Value, ConsumerError> {
        let document = self.interface.document();
        let name = member.name();
        ServiceClient::of_contract(document, name)
            .expect("the interface declares the service")
            .call(
                &self.started.caller_handle,
                &self.identity,
                &MemberBinding::of_contract(document, name).expect("a binding"),
                &self.daemon,
                &request,
                GOAL_TIMEOUT,
            )
            .await
    }

    /// The failure of the error reply of `member` to `request`.
    pub(super) async fn refusal(&self, member: impl Member, request: Value) -> Failure {
        let error = self
            .call(member, request)
            .await
            .expect_err("the daemon refuses the call");
        let ConsumerError::Messaging(error) = error else {
            panic!("an error reply, got {error}");
        };
        error.failure().cloned().expect("an error reply")
    }

    async fn listing(&self) -> FleetListing {
        let answer = self
            .call(FleetMember::List, json!({}))
            .await
            .expect("the daemon lists the fleet");
        serde_json::from_value(answer).expect("the listing parses")
    }

    /// The refusal that `check_move` gives for a call on `instance`, if
    /// any: its failure kind and its message.
    async fn check_move(&self, instance: &str) -> Option<(String, String)> {
        let answer = self
            .call(FleetMember::CheckMove, json!({ "instance": instance }))
            .await
            .expect("check_move answers every sender");
        answer.get("refusal").map(|refusal| {
            (
                refusal["failure_kind"].as_str().unwrap().to_owned(),
                refusal["message"].as_str().unwrap().to_owned(),
            )
        })
    }

    /// Starts the goal of the action `member`, which the daemon accepts.
    async fn goal(&self, member: impl Member) -> GoalHandle {
        let goal = self.send_goal(member, json!({})).await;
        assert!(goal.refusal().is_none(), "{:?}", goal.refusal());
        goal
    }

    /// Sends the goal `goal` of the action `member`.
    pub(super) async fn send_goal(&self, member: impl Member, goal: Value) -> GoalHandle {
        let document = self.interface.document();
        let name = member.name();
        ActionClient::of_contract(document, name)
            .expect("the interface declares the action")
            .fire_goal(
                &self.started.caller_handle,
                &self.identity,
                &MemberBinding::of_contract(document, name).expect("a binding"),
                &self.daemon,
                &goal,
                DEFAULT_FEEDBACK_BOUND,
                GOAL_TIMEOUT,
            )
            .await
            .expect("the goal reaches the daemon")
    }

    /// The end of `goal`.
    pub(super) async fn end_of(&self, goal: &GoalHandle) -> GoalOutcome {
        goal.result(&self.started.caller_handle, RESULT_TIMEOUT)
            .await
            .expect("the goal ends")
    }
}

/// A member of a daemon interface, by its name.
pub(super) trait Member {
    fn name(&self) -> &'static str;
}

impl Member for FleetMember {
    fn name(&self) -> &'static str {
        FleetMember::name(*self)
    }
}

impl Member for &'static str {
    fn name(&self) -> &'static str {
        self
    }
}

/// The next change that a follow of `changes` gives.
async fn next_change(follow: &mut GoalHandle) -> FleetChange {
    let feedback = tokio::time::timeout(RESULT_TIMEOUT, follow.next_feedback())
        .await
        .expect("a change comes")
        .expect("the follow goes on")
        .expect("the follow is open");
    serde_json::from_value(feedback).expect("the change parses")
}

/// The fleet of these tests, written under a new directory: the director,
/// a lease holder with the daemon target `fleet` scoped to `scope`, outside
/// any copy, and the axis `robot`, whose option `real` deploys `named_robot`
/// as the instance `arm_inst` of each copy.
fn fleet_with_a_director(started: &StartedCoreNode, scope: &str) -> (tempfile::TempDir, PathBuf) {
    fleet_with_a_director_and_copies(started, scope, "")
}

/// The fleet of [`fleet_with_a_director`], whose launch starts `copies`, the
/// `deployments` entries of the copies beside the director.
fn fleet_with_a_director_and_copies(
    started: &StartedCoreNode,
    scope: &str,
    copies: &str,
) -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let director = write_node_config_with_options(
        directory.path(),
        "director",
        "v1",
        NodeConfigOptions {
            run_cmd: &["sleep", "300"],
            depends_on: r#"{ daemon: [{ name: "fleet", tag: "v1", link_id: "fleet" }] }"#,
            interfaces: r#"{
                services: { consumes: [{ link_id: "fleet", name: "list" }] },
                actions: { consumes: [{ link_id: "fleet", name: "lease" }] },
            }"#,
            ..Default::default()
        },
    );
    let named_robot = write_node_config(directory.path(), "named_robot", "v1", &["sleep", "300"]);
    TestPackagesCache::new()
        .fs_entry("director", "v1", &director)
        .fs_entry("named_robot", "v1", &named_robot)
        .write(&started.peppy_dirs);
    let launcher = directory.path().join("fleet.json5");
    fs::write(
        &launcher,
        format!(
            r#"{{
        peppy_schema: "launcher/v1",
        deployments: [
            {{ source: {{ name: "director", tag: "v1" }}, instances: [
                {{ instance_id: "{DIRECTOR}", daemon_scopes: {{ fleet: {scope} }} }}
            ] }},
            {copies}
        ],
        components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
            real: {{ deployments: [{{ source: {{ name: "named_robot", tag: "v1" }},
                instances: [{{ instance_id: "arm_inst" }}] }}] }}
        }} }}]
    }}"#
        ),
    )
    .unwrap();
    (directory, launcher)
}

/// Launches the fleet with a director of `scope` and joins the copy
/// `alpha`.
async fn launched_with_alpha(
    started: &StartedCoreNode,
    scope: &str,
    launch_id: &str,
) -> (
    tempfile::TempDir,
    Vec<AbortOnDrop<peppylib::PeppyResult<()>>>,
) {
    let (directory, launcher) = fleet_with_a_director(started, scope);
    let mut responders = answer_readiness(started, "director", DIRECTOR).await;
    responders.extend(answer_readiness(started, "named_robot", ALPHA_ARM).await);
    execute(started, &fleet_launch(launcher, launch_id))
        .await
        .assert_completed();
    execute(started, &robot_goal("alpha", "real"))
        .await
        .assert_completed();
    (directory, responders)
}

/// The daemon holds the record of a launch from its commit point on: while
/// the launch waits for the start of the copy `alpha`, the director, an
/// instance of the launch, calls the fleet within its scope and takes the
/// lease of alpha, and the followers read the join of alpha.
#[tokio::test]
async fn an_instance_calls_the_fleet_within_its_scope_while_its_launch_starts() {
    let started = common::start_core_node_with_mock_messenger_and_no_ready_bound().await;
    let (_directory, launcher) = fleet_with_a_director_and_copies(
        &started,
        r#"{ members: ["list", "lease"] }"#,
        r#"{ robot: "real", instances: [{ instance_id: "alpha" }] }"#,
    );
    let command_line =
        FleetClient::sending_as(&started, core_node_api::names::COMMAND_LINE_INSTANCE_ID).await;
    let mut changes = command_line.goal(FleetMember::Changes).await;
    let first = next_change(&mut changes).await;
    assert!(first.listing.robots.is_empty(), "no launch runs yet");
    let mut responders = answer_readiness(&started, "director", DIRECTOR).await;

    let launch = fleet_launch(launcher, "fleet-commit-test");
    let during_the_launch = async {
        let joined = next_change(&mut changes).await;
        assert_eq!(serde_json::to_value(joined.change).unwrap(), json!("join"));
        assert_eq!(joined.robot, "alpha");
        assert_eq!(
            serde_json::to_value(joined.listing.robots[0].software).unwrap(),
            json!("stopped"),
            "alpha has not started yet"
        );

        let director = FleetClient::sending_as(&started, DIRECTOR).await;
        let robots: Vec<String> = director
            .listing()
            .await
            .robots
            .into_iter()
            .map(|robot| robot.name)
            .collect();
        assert_eq!(robots, ["alpha"], "the scope of the director holds");
        let lease = director.goal(FleetMember::Lease).await;
        director
            .call(
                FleetMember::Take,
                json!({ "robot": "alpha", "name": "first run" }),
            )
            .await
            .expect("the director takes alpha while the launch starts it");
        assert_eq!(
            command_line
                .check_move(ALPHA_ARM)
                .await
                .map(|(kind, _)| kind),
            Some("leased".to_owned()),
            "the move check knows the robot of the instance"
        );
        responders.extend(answer_readiness(&started, "named_robot", ALPHA_ARM).await);
        lease
    };
    let (end, _lease) = tokio::join!(execute(&started, &launch), during_the_launch);
    end.assert_completed();
    let listing = command_line.listing().await;
    assert_eq!(listing.leases.len(), 1);
    assert_eq!(listing.leases[0].holder, DIRECTOR);
    assert_eq!(
        serde_json::to_value(listing.robots[0].software).unwrap(),
        json!("running")
    );
}

/// The listing gives each robot with its software and the halt; the halt
/// refuses every move until it is cleared; and the daemon checks each call
/// against the scope of its sender.
#[tokio::test]
async fn the_fleet_lists_each_robot_and_the_halt_refuses_every_move_until_cleared() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, _responders) =
        launched_with_alpha(&started, r#"{ members: ["list"] }"#, "fleet-listing-test").await;
    let command_line =
        FleetClient::sending_as(&started, core_node_api::names::COMMAND_LINE_INSTANCE_ID).await;
    let director = FleetClient::sending_as(&started, DIRECTOR).await;

    let listing = director.listing().await;
    assert_eq!(listing.core_node, started.core_node_name);
    assert!(!listing.halted);
    let robots: Vec<(&str, &str)> = listing
        .robots
        .iter()
        .map(|robot| (robot.name.as_str(), robot.option.as_str()))
        .collect();
    assert_eq!(robots, [("alpha", "real")]);
    assert_eq!(
        serde_json::to_value(listing.robots[0].software).unwrap(),
        json!("running"),
        "every instance of the copy runs"
    );
    assert_eq!(listing.robots[0].instances[0].instance, ALPHA_ARM);

    let refusal = director.refusal(FleetMember::Halt, json!({})).await;
    assert_eq!(refusal.kind().as_str(), "rejected");
    assert!(
        refusal
            .message()
            .contains("has no `fleet` target whose scope gives `halt`"),
        "{}",
        refusal.message()
    );

    command_line
        .call(FleetMember::Halt, json!({}))
        .await
        .expect("the command line halts the stack");
    let listing = director.listing().await;
    assert!(listing.halted);
    assert!(
        listing.halted_by.contains("peppy-cli"),
        "{}",
        listing.halted_by
    );
    let (kind, _message) = director
        .check_move(ALPHA_ARM)
        .await
        .expect("the halt refuses the move");
    assert_eq!(kind, "halted");
    assert_eq!(
        command_line
            .check_move(DIRECTOR)
            .await
            .map(|(kind, _)| kind),
        Some("halted".to_owned()),
        "the halt refuses a call on an instance outside any copy too"
    );

    command_line
        .call(FleetMember::ClearHalt, json!({}))
        .await
        .expect("the command line clears the halt");
    assert_eq!(director.check_move(ALPHA_ARM).await, None);
    assert!(!director.listing().await.halted);
}

/// A lease holder takes a robot: the move check refuses every other client,
/// a removal of the robot by another client is refused, and the release
/// lets every client move it again. The follow of the changes gives each
/// change in order, the stop and the start of the software of the robot
/// included.
#[tokio::test]
async fn a_lease_holder_takes_a_robot_and_the_moves_follow_its_lease() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, _responders) = launched_with_alpha(
        &started,
        r#"{ members: ["list", "lease", "changes"] }"#,
        "fleet-lease-test",
    )
    .await;
    let command_line =
        FleetClient::sending_as(&started, core_node_api::names::COMMAND_LINE_INSTANCE_ID).await;
    let director = FleetClient::sending_as(&started, DIRECTOR).await;

    let mut changes = director.goal(FleetMember::Changes).await;
    let first = next_change(&mut changes).await;
    assert_eq!(
        serde_json::to_value(first.change).unwrap(),
        json!("listing")
    );

    let refusal = director
        .refusal(
            FleetMember::Take,
            json!({ "robot": "alpha", "name": "tidy" }),
        )
        .await;
    assert_eq!(
        refusal.kind().as_str(),
        "precondition_failed",
        "no lease goal runs: {}",
        refusal.message()
    );
    let _lease = director.goal(FleetMember::Lease).await;
    director
        .call(
            FleetMember::Take,
            json!({ "robot": "alpha", "name": "tidy" }),
        )
        .await
        .expect("the director takes alpha");
    let taken = next_change(&mut changes).await;
    assert_eq!(
        serde_json::to_value(taken.change).unwrap(),
        json!("lease_start")
    );
    assert_eq!(taken.robot, "alpha");
    assert_eq!(taken.listing.leases[0].holder, DIRECTOR);

    assert_eq!(director.check_move(ALPHA_ARM).await, None);
    let (kind, message) = command_line
        .check_move(ALPHA_ARM)
        .await
        .expect("another client may not move alpha");
    assert_eq!(kind, "leased");
    assert!(
        message.contains(DIRECTOR) && message.contains("tidy"),
        "{message}"
    );

    let refused = refused_removal(&started, "alpha").await;
    assert_eq!(refused.kind().as_str(), "leased", "{}", refused.message());

    let stopped = poll_node_stop(
        &NodeStopRequest::new(ALPHA_ARM),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        &started.core_node_name,
        GOAL_TIMEOUT,
    )
    .await;
    assert!(stopped.is_ok(), "{stopped:?}");
    let stop = next_change(&mut changes).await;
    assert_eq!(
        serde_json::to_value(stop.change).unwrap(),
        json!("software_stop")
    );
    assert_eq!(stop.robot, "alpha");
    assert_eq!(
        serde_json::to_value(stop.listing.robots[0].software).unwrap(),
        json!("stopped")
    );

    director
        .call(FleetMember::Release, json!({ "robot": "alpha" }))
        .await
        .expect("the director releases alpha");
    let released = next_change(&mut changes).await;
    assert_eq!(
        serde_json::to_value(released.change).unwrap(),
        json!("lease_end")
    );
    assert_eq!(command_line.check_move(ALPHA_ARM).await, None);
}

/// No client takes the lease of a robot while a removal of it runs: the
/// removal checks the leases once, at its admission, and the robot leaves
/// the stack with no lease. The copy hears the shutdown request of the
/// removal and exits only when the test kills it, under a grace that
/// outlasts the test.
#[tokio::test]
async fn no_client_takes_the_lease_of_a_robot_that_a_removal_takes_off_the_stack() {
    let started = common::start_core_node_with_shutdown_grace(3600).await;
    let (_directory, launcher) = fleet_with_a_director_and_copies(
        &started,
        r#"{ members: ["list", "lease"] }"#,
        r#"{ robot: "real", instances: [{ instance_id: "alpha" }] }"#,
    );
    let mut responders = answer_readiness(&started, "director", DIRECTOR).await;
    responders.extend(answer_readiness(&started, "named_robot", ALPHA_ARM).await);
    execute(&started, &fleet_launch(launcher, "fleet-removal-test"))
        .await
        .assert_completed();
    let director = FleetClient::sending_as(&started, DIRECTOR).await;
    let _lease = director.goal(FleetMember::Lease).await;

    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let (_listener, shutdown) = peppylib::services::shutdown::listen_for_shutdown(
        &messenger,
        &started.core_node_name,
        ALPHA_ARM,
        common::test_node_target("named_robot"),
    )
    .await
    .unwrap();
    let alpha_pid = started
        .node_stack
        .find("named_robot", "v1")
        .expect("the robot node is in the stack")
        .read()
        .instances()
        .iter()
        .find(|instance| instance.instance_id().as_str() == ALPHA_ARM)
        .expect("the instance of alpha is in the stack")
        .pid()
        .expect("alpha runs");
    let removal = StackRemoveGoal::new(config::runtime::CopyName::new("alpha").unwrap());
    let (removed, ()) = tokio::join!(execute(&started, &removal), async {
        shutdown
            .await
            .expect("the removal asks the copy to shut down");
        let refusal = director
            .refusal(
                FleetMember::Take,
                json!({ "robot": "alpha", "name": "late run" }),
            )
            .await;
        assert_eq!(refusal.kind().as_str(), "precondition_failed");
        assert!(
            refusal.message().contains("a removal of it runs"),
            "{}",
            refusal.message()
        );
        std::process::Command::new("kill")
            .arg("-KILL")
            .arg(alpha_pid.to_string())
            .status()
            .expect("kill runs");
    });
    removed.assert_completed();
    let listing = director.listing().await;
    assert!(listing.robots.is_empty(), "alpha left the stack");
    assert!(listing.leases.is_empty(), "with no lease");
}

/// The failure of the removal of the copy `name` that the command line
/// sends.
async fn refused_removal(started: &StartedCoreNode, name: &str) -> Failure {
    refused_removal_goal(
        started,
        &StackRemoveGoal::new(config::runtime::CopyName::new(name).unwrap()),
    )
    .await
}

/// The failure of the removal `goal` that the command line sends.
async fn refused_removal_goal(started: &StartedCoreNode, goal: &StackRemoveGoal) -> Failure {
    let handle = send_daemon_goal(
        &started.caller_handle,
        &started.core_node_name,
        goal,
        GOAL_TIMEOUT,
    )
    .await
    .expect("the removal reaches the daemon");
    goal_answer(&handle, LaunchGoalResponse::decode, "goal response")
        .expect("decode the goal answer")
        .expect_err("the daemon refuses the removal")
}

/// Only the daemon names the client of a stack goal: a removal of another
/// sender that names the lease holder as its client is refused, and the
/// leased robot stays on the stack.
#[tokio::test]
async fn a_stack_goal_of_another_sender_does_not_act_for_a_lease_holder() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, _responders) = launched_with_alpha(
        &started,
        r#"{ members: ["list", "lease"] }"#,
        "fleet-goal-client-test",
    )
    .await;
    let director = FleetClient::sending_as(&started, DIRECTOR).await;
    let _lease = director.goal(FleetMember::Lease).await;
    director
        .call(
            FleetMember::Take,
            json!({ "robot": "alpha", "name": "tidy" }),
        )
        .await
        .expect("the director takes alpha");

    let refusal = refused_removal_goal(
        &started,
        &StackRemoveGoal {
            client: Some(core_node_api::encoding::GoalClient {
                core_node: started.core_node_name.clone(),
                instance_id: DIRECTOR.to_owned(),
            }),
            ..StackRemoveGoal::new(config::runtime::CopyName::new("alpha").unwrap())
        },
    )
    .await;
    assert_eq!(refusal.kind().as_str(), "rejected");
    assert!(
        refusal
            .message()
            .contains("only the daemon names the client"),
        "{}",
        refusal.message()
    );
    let robots: Vec<String> = director
        .listing()
        .await
        .robots
        .into_iter()
        .map(|robot| robot.name)
        .collect();
    assert_eq!(robots, ["alpha"], "the leased robot stays");
}

/// The daemon refuses its raw goals, and its raw services that change the
/// stack, from an instance of the stack, and answers the services that
/// read.
#[tokio::test]
async fn the_daemon_refuses_its_raw_calls_that_change_the_stack_from_its_instances() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, _responders) =
        launched_with_alpha(&started, r#"{ members: ["list"] }"#, "fleet-raw-calls-test").await;

    let reset = poll(
        &StackResetRequest::new(),
        &started.caller_handle,
        &started.core_node_name,
        ALPHA_ARM,
        &started.core_node_name,
        GOAL_TIMEOUT,
    )
    .await
    .expect_err("a node of the stack does not reset the stack");
    let failure = reset.failure().expect("an error reply");
    assert_eq!(failure.kind().as_str(), "rejected");
    assert!(
        failure
            .message()
            .contains("raw call `stack_reset` from `alpha_arm_inst`"),
        "{}",
        failure.message()
    );

    let removal = send_goal(
        &StackRemoveGoal::new(config::runtime::CopyName::new("alpha").unwrap()),
        &started.caller_handle,
        &started.core_node_name,
        DIRECTOR,
        None,
        GOAL_TIMEOUT,
    )
    .await
    .expect("the goal reaches the daemon");
    let refusal = goal_answer(&removal, LaunchGoalResponse::decode, "goal response")
        .expect("decode the goal answer")
        .expect_err("a node of the stack does not remove a copy");
    assert!(
        refusal
            .message()
            .contains("raw call `stack_remove` from `director_inst`"),
        "{}",
        refusal.message()
    );

    poll(
        &StackListRequest::new(),
        &started.caller_handle,
        &started.core_node_name,
        DIRECTOR,
        &started.core_node_name,
        GOAL_TIMEOUT,
    )
    .await
    .expect("a node of the stack reads the stack listing");
}

/// A halt that a daemon before this one wrote keeps the stack halted from
/// the start of the daemon.
#[tokio::test]
async fn a_halt_on_disk_keeps_the_stack_halted_from_the_start() {
    let data_dir = new_test_data_dir();
    let halt = PeppyDirs::new(data_dir.path()).fleet_halt_path();
    fs::create_dir_all(halt.parent().unwrap()).unwrap();
    fs::write(&halt, r#"{ "halted_by": "`peppy-cli` on `cn-desk`" }"#).unwrap();
    let started = start_core_node_with_mock_messenger_on(data_dir).await;

    let command_line =
        FleetClient::sending_as(&started, core_node_api::names::COMMAND_LINE_INSTANCE_ID).await;
    let listing = command_line.listing().await;
    assert!(listing.halted);
    assert_eq!(listing.halted_by, "`peppy-cli` on `cn-desk`");
    command_line
        .call(FleetMember::ClearHalt, json!({}))
        .await
        .expect("the command line clears the halt");
    assert!(!halt.exists(), "the end of the halt removes it from disk");
}

/// A daemon that runs a slice of a launch of another machine refuses the
/// calls of the daemon interfaces and names the coordinator, whose daemon
/// serves them.
#[tokio::test]
async fn a_participant_refuses_the_fleet_and_names_the_coordinator() {
    let started = start_core_node_with_mock_messenger().await;
    started.slice_ownership.record_slice(
        core_node_api::encoding::LaunchIdentity::new("launch-of-the-desk", "cn-desk"),
        "desk-boot",
    );

    let command_line =
        FleetClient::sending_as(&started, core_node_api::names::COMMAND_LINE_INSTANCE_ID).await;
    let refusal = command_line.refusal(FleetMember::List, json!({})).await;
    assert_eq!(refusal.kind().as_str(), "precondition_failed");
    assert_eq!(
        refusal.message(),
        "this machine runs a slice of launch `launch-of-the-desk`, which `cn-desk` coordinates: \
         call the daemon of `cn-desk`"
    );
    let copies = InterfaceClient::of(&started, DaemonInterface::StackCopies, DIRECTOR).await;
    assert_eq!(
        copies.refusal("list", json!({})).await.kind().as_str(),
        "precondition_failed"
    );
}

/// A lease ends when the `lease` goal of its holder ends, and a reset of the
/// stack ends every lease, each change in order.
#[tokio::test]
async fn a_lease_ends_with_its_goal_and_a_reset_ends_every_lease() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, _responders) = launched_with_alpha(
        &started,
        r#"{ members: ["lease", "changes"] }"#,
        "fleet-lease-end-test",
    )
    .await;
    let director = FleetClient::sending_as(&started, DIRECTOR).await;
    let mut changes = director.goal(FleetMember::Changes).await;
    next_change(&mut changes).await;
    let take = || {
        director.call(
            FleetMember::Take,
            json!({ "robot": "alpha", "name": "tidy" }),
        )
    };

    let lease = director.goal(FleetMember::Lease).await;
    take().await.expect("the director takes alpha");
    assert_eq!(
        serde_json::to_value(next_change(&mut changes).await.change).unwrap(),
        json!("lease_start")
    );
    lease
        .cancel(&started.caller_handle, GOAL_TIMEOUT)
        .await
        .expect("the daemon takes the cancel");
    let ended = next_change(&mut changes).await;
    assert_eq!(
        serde_json::to_value(ended.change).unwrap(),
        json!("lease_end")
    );
    assert!(
        ended.listing.leases.is_empty(),
        "{:?}",
        ended.listing.leases
    );

    let _lease = director.goal(FleetMember::Lease).await;
    take().await.expect("the director takes alpha again");
    next_change(&mut changes).await;
    poll(
        &StackResetRequest::new(),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        GOAL_TIMEOUT,
    )
    .await
    .expect("the command line resets the stack");
    let mut kinds = Vec::new();
    while kinds.len() < 2 {
        let change = next_change(&mut changes).await;
        kinds.push(serde_json::to_value(change.change).unwrap());
    }
    assert_eq!(
        kinds,
        [json!("lease_end"), json!("removal")],
        "the reset ends the lease, then the robot leaves the stack"
    );
}
