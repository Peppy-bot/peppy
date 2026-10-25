//! The copies of a launch through `stack_copies:v1`, which the daemon serves
//! on the messaging layer: a client lists, joins and removes the copies of
//! the options of its scope, and the daemon refuses a copy outside it.

use super::copies::{execute, fleet_launch};
use super::fleet::InterfaceClient;
use super::*;
use daemon_config::daemon_interface::DaemonInterface;
use peppy_codec::consumer::GoalOutcome;
use serde_json::json;

/// The manager, a client of `stack_copies:v1` deployed outside any copy.
const MANAGER: &str = "manager_inst";

/// The fleet of these tests, written under a new directory: the manager,
/// whose daemon target `stack` gives the option `real` and two copies at
/// most, and the axis `robot`, whose options `real` and `other` deploy
/// `named_robot` as the instance `arm_inst` of each copy.
fn fleet_with_a_manager(started: &StartedCoreNode) -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let manager = write_node_config_with_options(
        directory.path(),
        "manager",
        "v1",
        NodeConfigOptions {
            run_cmd: &["sleep", "300"],
            depends_on: r#"{ daemon: [{ name: "stack_copies", tag: "v1", link_id: "stack" }] }"#,
            interfaces: r#"{
                services: { consumes: [{ link_id: "stack", name: "list" }] },
                actions: { consumes: [
                    { link_id: "stack", name: "join" },
                    { link_id: "stack", name: "remove" },
                ] },
            }"#,
            ..Default::default()
        },
    );
    let named_robot = write_node_config(directory.path(), "named_robot", "v1", &["sleep", "300"]);
    TestPackagesCache::new()
        .fs_entry("manager", "v1", &manager)
        .fs_entry("named_robot", "v1", &named_robot)
        .write(&started.peppy_dirs);
    let robot = r#"{ deployments: [{ source: { name: "named_robot", tag: "v1" },
        instances: [{ instance_id: "arm_inst" }] }] }"#;
    let launcher = directory.path().join("fleet.json5");
    fs::write(
        &launcher,
        format!(
            r#"{{
        peppy_schema: "launcher/v1",
        deployments: [{{ source: {{ name: "manager", tag: "v1" }}, instances: [
            {{ instance_id: "{MANAGER}", daemon_scopes: {{ stack: {{
                max_copies: 2,
                options: [{{ option: "real", model: "named", description: "A named robot" }}],
            }} }} }}
        ] }}],
        components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
            real: {robot},
            other: {robot},
        }} }}]
    }}"#
        ),
    )
    .unwrap();
    (directory, launcher)
}

/// A join goal of the copy `name` of `option` on the coordinator.
fn join(started: &StartedCoreNode, name: &str, option: &str) -> serde_json::Value {
    json!({
        "name": name,
        "option": option,
        "core_node": started.core_node_name,
        "features": [],
    })
}

/// The names of the copies that the listing gives.
fn copies(listing: &serde_json::Value) -> Vec<&str> {
    listing["copies"]
        .as_array()
        .expect("the listing gives the copies")
        .iter()
        .map(|copy| copy["name"].as_str().expect("a copy has a name"))
        .collect()
}

/// A client lists the options of its scope, joins a copy of one, finds the
/// copy in the listing, and removes it; the daemon refuses a copy of an
/// option outside the scope before anything starts.
#[tokio::test]
async fn a_client_joins_and_removes_the_copies_of_its_scope() {
    let started = start_core_node_with_mock_messenger().await;
    let (_directory, launcher) = fleet_with_a_manager(&started);
    let mut responders = answer_readiness(&started, "manager", MANAGER).await;
    responders.extend(answer_readiness(&started, "named_robot", "bravo_arm_inst").await);
    execute(&started, &fleet_launch(launcher, "stack-copies-test"))
        .await
        .assert_completed();
    let manager = InterfaceClient::of(&started, DaemonInterface::StackCopies, MANAGER).await;

    let listing = manager
        .call("list", json!({}))
        .await
        .expect("the manager lists the copies");
    let options: Vec<&str> = listing["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["option"].as_str().unwrap())
        .collect();
    assert_eq!(
        options,
        ["real"],
        "the listing gives the options of the scope"
    );
    assert!(copies(&listing).is_empty());

    let outside = manager
        .send_goal("join", join(&started, "charlie", "other"))
        .await;
    let refusal = outside.refusal().expect("the daemon refuses the join");
    assert_eq!(refusal.kind().as_str(), "rejected");
    assert!(
        refusal.message().contains("other"),
        "the refusal names the option: {}",
        refusal.message()
    );

    let joined = manager
        .send_goal("join", join(&started, "bravo", "real"))
        .await;
    assert!(joined.refusal().is_none(), "{:?}", joined.refusal());
    assert_eq!(
        manager.end_of(&joined).await,
        GoalOutcome::Completed(json!({})),
        "the join runs to its end"
    );
    let listing = manager.call("list", json!({})).await.unwrap();
    assert_eq!(copies(&listing), ["bravo"]);

    let removed = manager
        .send_goal("remove", json!({ "name": "bravo" }))
        .await;
    assert!(removed.refusal().is_none(), "{:?}", removed.refusal());
    assert_eq!(
        manager.end_of(&removed).await,
        GoalOutcome::Completed(json!({}))
    );
    let listing = manager.call("list", json!({})).await.unwrap();
    assert!(copies(&listing).is_empty(), "the copy is off the stack");
}
