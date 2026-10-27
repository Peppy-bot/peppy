//! A Rust node with a daemon target gets a client of its daemon interface:
//! the generator writes the consumed members of `robot_leases:v1` as for a
//! consumed contract slot, each addressing the daemon as the provider of the
//! contract through the `one` slot of the target, and the node that calls
//! them compiles.

use crate::daemon_target_fixtures::{
    ACTION, LINK_ID, SERVICES, lease_holder_interfaces, lease_holder_manifest,
};
use crate::helpers;
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::PeppygenLanguage;
use std::fs;
use tempfile::TempDir;

/// A lease holder that lists the fleet, holds a lease goal, takes robots and
/// cancels its goal, each through the generated client of its daemon target:
/// the code of the guide of daemon targets (`core_node_functions.mdx`).
const USER_MAIN: &str = r#"
use peppygen::consumed_actions::fleet::lease;
use peppygen::consumed_services::fleet::{list, take};
use std::time::Duration;

async fn hold_leases(node_runner: &peppygen::NodeRunner) -> peppygen::Result<()> {
    let daemon = list::bound_producer(node_runner);
    let fleet = list::poll(node_runner, daemon, Duration::from_secs(5)).await?;
    let mut goal = lease::ActionHandle::fire_goal(
        node_runner,
        daemon,
        Duration::from_secs(5),
        peppygen::DEFAULT_FEEDBACK_BOUND,
    )
    .await?;
    if let Some(refusal) = &goal.refusal {
        println!("the lease goal is refused: {}", refusal.message);
        return Ok(());
    }
    for robot in &fleet.data.robots {
        let request = take::Request::new(robot.name.clone(), "a run".to_owned());
        match take::poll(node_runner, daemon, Duration::from_secs(5), request).await {
            Ok(()) => println!("leased {}", robot.name),
            Err(error) => match take::Failure::of(&error) {
                Some(take::Failure {
                    kind: take::FailureKind::Leased | take::FailureKind::Halted,
                    message,
                }) => println!("{message}"),
                _ => return Err(error),
            },
        }
    }
    let feedback = goal.on_next_feedback_message().await?;
    println!("{} {} {}", feedback.event, feedback.robot, feedback.cause);
    goal.cancel_goal(Duration::from_secs(5)).await?;
    Ok(())
}

fn main() {
    let _ = hold_leases;
}
"#;

#[test]
fn a_rust_node_with_a_daemon_target_gets_a_client_of_its_interface() {
    let temp_dir = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp directory");
    let user_node_dir = temp_dir.path().join("user_node");
    fs::create_dir_all(&user_node_dir).expect("failed to create user_node directory");
    fs::write(
        user_node_dir.join(NODE_CONFIG_FILE),
        lease_holder_manifest("lease_holder", helpers::RUST_STUB_EXECUTION),
    )
    .expect("failed to write peppy.json5");

    helpers::generate_test_peppygen_lib(
        PeppygenLanguage::Rust,
        &user_node_dir,
        lease_holder_interfaces(),
    )
    .expect("failed to generate peppygen lib");

    let peppygen_dir = user_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let modules = SERVICES
        .iter()
        .map(|service| format!("src/consumed_services/{LINK_ID}/{service}.rs"))
        .chain([format!("src/consumed_actions/{LINK_ID}/{ACTION}.rs")]);
    for module in modules {
        let code = fs::read_to_string(peppygen_dir.join(&module))
            .unwrap_or_else(|error| panic!("{module} is generated: {error}"));
        config_test_support::assert_contains_all(
            &code,
            &[
                &format!("const LINK_ID: &str = \"{LINK_ID}\";"),
                "SenderTarget::contract(",
                "\"robot_leases\"",
                "pub fn bound_producer(",
            ],
        );
    }

    helpers::init_cargo_user_node(&user_node_dir);
    fs::write(user_node_dir.join("src").join("main.rs"), USER_MAIN)
        .expect("failed to write user main.rs");
    helpers::compile_project(&user_node_dir);
}
