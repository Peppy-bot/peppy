//! Python counterpart of `tests/rust/daemon_target_client.rs`: a Python node
//! with a daemon target gets a client of its daemon interface, and the
//! modules of that client import and resolve their schemas.

use crate::daemon_target_fixtures::{
    ACTION, LINK_ID, SERVICES, lease_holder_interfaces, lease_holder_manifest,
};
use crate::helpers;
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::PeppygenLanguage;
use std::fs;
use tempfile::TempDir;

/// Imports the three modules of the client, loads each schema, checks the
/// request and the failure kinds of `take` and the coroutines a lease holder
/// awaits, and
/// reads a listing written with the schema of `list`, so the generated
/// decoder of the nested listing runs.
const PYTHON_PROBE: &str = r#"
import inspect
from peppygen.consumed_actions.fleet import lease as fleet_lease
from peppygen.consumed_services.fleet import list as fleet_list
from peppygen.consumed_services.fleet import take as fleet_take

for module in (fleet_list, fleet_take, fleet_lease):
    assert module.LINK_ID == "fleet", module.LINK_ID
    for name in dir(module):
        if name.startswith("_") and name.endswith("_capnp"):
            getattr(module, name)()

kinds = {kind.value for kind in fleet_take.FailureKind}
assert {"halted", "leased"} <= kinds, kinds
request = fleet_take.Request(robot="alpha", name="a run")
assert (request.robot, request.name) == ("alpha", "a run"), request
failure = fleet_take.Failure("leased", "another client holds alpha")
assert failure.kind is fleet_take.FailureKind.LEASED, failure
for coroutine in (
    fleet_list.poll,
    fleet_take.poll,
    fleet_lease.ActionHandle.fire_goal,
    fleet_lease.ActionHandle.cancel_goal,
    fleet_lease.ActionHandle.on_next_feedback_message,
):
    assert inspect.iscoroutinefunction(coroutine), coroutine

schema = fleet_list._poll_robot_leases_list_response_message_capnp().PollRobotLeasesListResponseMessage
message = schema.new_message()
message.version = "unknown"
robots = message.init("robots", 1)
robots[0].name = "alpha"
robots[0].kind = "simulated"
instances = robots[0].init("instances", 1)
instances[0].name = "arm_inst"
instances[0].instance = "alpha_arm_inst"
listing = fleet_list._deserialize_response(message.to_bytes())
assert listing.version == "unknown", listing.version
robot = listing.robots[0]
assert (robot.name, robot.kind) == ("alpha", "simulated"), robot
assert robot.instances[0].instance == "alpha_arm_inst", robot.instances
print("the client of the daemon target fleet imports and decodes")
"#;

#[test]
fn a_python_node_with_a_daemon_target_gets_a_client_of_its_interface() {
    let temp_dir = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp directory");
    let user_node_dir = temp_dir.path().join("user_node");
    fs::create_dir_all(&user_node_dir).expect("failed to create user_node directory");
    fs::write(
        user_node_dir.join(NODE_CONFIG_FILE),
        lease_holder_manifest("lease_holder", helpers::PYTHON_STUB_EXECUTION),
    )
    .expect("failed to write peppy.json5");

    helpers::generate_test_peppygen_lib(
        PeppygenLanguage::Python,
        &user_node_dir,
        lease_holder_interfaces(),
    )
    .expect("failed to generate peppygen lib");

    let peppygen_dir = user_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let modules = SERVICES
        .iter()
        .map(|service| format!("peppygen/consumed_services/{LINK_ID}/{service}.py"))
        .chain([format!("peppygen/consumed_actions/{LINK_ID}/{ACTION}.py")]);
    for module in modules {
        let code = fs::read_to_string(peppygen_dir.join(&module))
            .unwrap_or_else(|error| panic!("{module} is generated: {error}"));
        config_test_support::assert_contains_all(&code, &[LINK_ID, "robot_leases"]);
    }

    helpers::run_python_check(
        &user_node_dir,
        PYTHON_PROBE,
        "Python probe of the client of a daemon target",
    );
}
