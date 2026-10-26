//! The daemon targets of a node in a launch: the launch holds each one to the
//! rules of daemon scopes before anything starts, and the daemon binds the
//! slot of each one to itself when the instance starts.

use super::copies::{execute, fleet_launch, participant_request, robot_goal};
use super::*;
use core_node_api::encoding::{PlacementSpec, StackListRequest};

/// A launch of the launcher at `path`, under the id `launch_id`, with every
/// core node link of the launcher on this machine.
fn local_launch(path: PathBuf, launch_id: &str) -> LaunchGoal {
    fleet_launch(path, launch_id)
        .with_layout(PlacementSpec::Local, Vec::new(), Vec::new())
        .expect("a local launch with no word")
}

/// The `depends_on` of a node with the daemon target `fleet` of `fleet:v1`.
const FLEET_TARGET: &str = r#"{ daemon: [{ name: "fleet", tag: "v1", link_id: "fleet" }] }"#;

/// The `interfaces` of a lease holder of `fleet:v1`.
const LEASE_HOLDER: &str = r#"{
    services: { consumes: [{ link_id: "fleet", name: "list" }, { link_id: "fleet", name: "take" }] },
    actions: { consumes: [{ link_id: "fleet", name: "lease" }] },
}"#;

/// A scope of `fleet:v1` that holds.
const LEASE_SCOPE: &str = r#"{ members: ["list", "lease"] }"#;

/// Writes the nodes of these tests under `directory` and records them in the
/// nodes cache of `started`: `director:v1`, whose daemon target is
/// `fleet:v1`; `twin_director:v1`, which names `fleet` twice; and
/// `future_director:v1`, which names `fleet:v2`, a tag this peppy does not
/// serve. Each runs `sleep`, and each consumes the members of a lease
/// holder.
fn write_directors(started: &StartedCoreNode, directory: &Path) {
    let node = |name: &str, depends_on: &str| {
        write_node_config_with_options(
            directory,
            name,
            "v1",
            NodeConfigOptions {
                run_cmd: &["sleep", "300"],
                depends_on,
                interfaces: LEASE_HOLDER,
                ..Default::default()
            },
        )
    };
    let director = node("director", FLEET_TARGET);
    let twin_director = node(
        "twin_director",
        r#"{ daemon: [
            { name: "fleet", tag: "v1", link_id: "fleet" },
            { name: "fleet", tag: "v1", link_id: "other_fleet" },
        ] }"#,
    );
    let future_director = node(
        "future_director",
        r#"{ daemon: [{ name: "fleet", tag: "v2", link_id: "fleet" }] }"#,
    );
    TestPackagesCache::new()
        .fs_entry("director", "v1", &director)
        .fs_entry("twin_director", "v1", &twin_director)
        .fs_entry("future_director", "v1", &future_director)
        .write(&started.peppy_dirs);
}

/// A launcher file under `directory` whose top-level deployments are
/// `deployments`, beside a `robot` axis that runs as copies and whose
/// `directed` option deploys the director.
fn launcher(directory: &Path, deployments: &str) -> PathBuf {
    let path = directory.join("directors.json5");
    fs::write(
        &path,
        format!(
            r#"{{
            peppy_schema: "launcher/v1",
            core_nodes: ["robot_pc"],
            deployments: [{deployments}],
            components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
                directed: {{ deployments: [{{ source: {{ name: "director", tag: "v1" }}, instances: [
                    {{ instance_id: "director_inst" }}
                ] }}] }},
            }} }}],
        }}"#
        ),
    )
    .expect("write the launcher");
    path
}

/// The deployment of the node `node` with the one instance `instance`.
fn deployment(node: &str, instance: &str) -> String {
    format!(r#"{{ source: {{ name: "{node}", tag: "v1" }}, instances: [{instance}] }}"#)
}

/// A launch binds the daemon target of a node to the daemon that runs its
/// instance: the stack records the daemon, by its core node and its per-boot
/// instance id, as the one producer of the slot, with no `links` entry.
#[tokio::test]
async fn a_launch_binds_the_daemon_target_of_a_node_to_the_daemon() {
    let started = start_core_node_with_mock_messenger().await;
    let directory = tempdir().unwrap();
    write_directors(&started, directory.path());
    let _readiness = answer_readiness(&started, "director", "director_inst").await;
    let path = launcher(
        directory.path(),
        &deployment(
            "director",
            &format!(
                r#"{{ instance_id: "director_inst", daemon_scopes: {{ fleet: {LEASE_SCOPE} }} }}"#
            ),
        ),
    );

    execute(&started, &local_launch(path, "daemon-target-binding-test"))
        .await
        .assert_completed();

    let list = participant_request(&started, &StackListRequest::new()).await;
    let graph: core_node_api::SerializedNodeGraph =
        serde_json::from_str(&list.graph_json).expect("the listing carries the stack's graph");
    let director = graph
        .nodes
        .iter()
        .flat_map(|node| &node.instances)
        .find(|instance| instance.instance_id == "director_inst")
        .expect("the director runs");
    let fleet: Vec<config::runtime::ProducerRef> = director.slot_bindings["fleet"]
        .producers()
        .cloned()
        .collect();
    assert_eq!(
        fleet,
        [config::runtime::ProducerRef::new(
            &list.core_node,
            &list.instance_id
        )],
        "the daemon that runs the director is the producer of its daemon target"
    );
    assert_eq!(
        director.slot_bindings.len(),
        1,
        "the director has no other slot: {:?}",
        director.slot_bindings
    );
}

/// A launch refuses, before anything starts, a daemon target with no scope,
/// one with a link, one on an instance placed off the coordinator, two
/// daemon targets of one interface on one node, and one that names an
/// interface this peppy does not serve. A join refuses a copy whose instance
/// has a daemon target.
#[tokio::test]
async fn a_launch_refuses_a_daemon_target_that_breaks_a_rule() {
    let started = start_core_node_with_mock_messenger().await;
    let directory = tempdir().unwrap();
    write_directors(&started, directory.path());
    let scoped = |extra: &str| {
        format!(
            r#"{{ instance_id: "director_inst", {extra} daemon_scopes: {{ fleet: {LEASE_SCOPE} }} }}"#
        )
    };
    for (index, (deployments, expected)) in [
        (
            deployment("director", r#"{ instance_id: "director_inst" }"#),
            "daemon target `fleet` (fleet:v1) of instance `director_inst` has no scope",
        ),
        (
            deployment("director", &scoped(r#"links: { fleet: "director_inst" },"#)),
            "`links.fleet` of instance `director_inst` names daemon target `fleet`, which takes \
             no link",
        ),
        (
            deployment("director", &scoped(r#"core_node: "robot_pc","#)),
            "instance `director_inst` has daemon target `fleet` and declares `core_node: \
             \"robot_pc\"`; an instance with a daemon target runs on the coordinator",
        ),
        (
            deployment("twin_director", &scoped("")),
            "depends_on.daemon names daemon interface `fleet` twice, with link_ids `fleet` and \
             `other_fleet`",
        ),
        (
            deployment("future_director", &scoped("")),
            "daemon target `fleet`: daemon interface `fleet:v2` is not one this peppy serves; \
             it serves `fleet` at tag `v1` only",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = launcher(directory.path(), &deployments);
        let result = execute(
            &started,
            &local_launch(path, &format!("daemon-target-refusal-{index}")),
        )
        .await;
        result.assert_failed(PeppyFailureKind::Rejected, expected);
    }

    let path = launcher(directory.path(), "");
    execute(&started, &local_launch(path, "daemon-target-copy-test"))
        .await
        .assert_completed();
    let result = execute(&started, &robot_goal("delta", "directed")).await;
    result.assert_failed(
        PeppyFailureKind::Rejected,
        "instance `delta_director_inst` of copy `delta` has daemon target `fleet`; an instance \
         with a daemon target runs on the coordinator and belongs to the stack",
    );
}
