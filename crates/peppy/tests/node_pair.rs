//! End-to-end pairing flow over an in-process daemon (mock messaging):
//! `repo refresh` discovers the pairing doc, `node add` resolves
//! `depends_on.pairings` through the pairing cache, and `node run` enforces
//! coverage, establishes pairs via `--link`, delivers live `peer_update`
//! pins to both endpoints, auto-clears on `node stop` (notifying the
//! survivor), supports re-pairing the survivor, and enforces slot
//! exclusivity. The "nodes" are `sleep` processes; their ready/health and
//! `peer_update` services run in-process on the shared mock messenger,
//! exactly the seams a real peppylib node exposes. Removing a paired node with
//! `--stop-instances` dissolves its pairs and notifies the survivor the same
//! way `node stop` does. A `zero_or_more` slot holds one pair per peer:
//! peers pair into it one at a time or all at once from its own run's links,
//! a stop shrinks the set and a rerun grows it again, and every delivery
//! carries the whole set.

use std::sync::Arc;

use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::context::AppContext;
use peppy::test_support::{InstanceLifetime, ServeCommandEmulation};
use peppylib::MessengerHandle;
use peppylib::messaging::PeerSetState;
use tokio::sync::watch;

use super::common::{
    add_built_node, emulate_cooperative_shutdown, emulate_pairing_instance,
    emulate_startup_services, node_run_command, pairing_node_config, seed_pairing_repo,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_establish_stop_repair_exclusivity_and_remove() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    let messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let work_dir = tempfile::tempdir().expect("temp work dir");
    let ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Both instances must stay in the stack across the whole
    // establish/stop/repair/exclusivity/remove sequence below.
    let instances = InstanceLifetime::new();

    // Each node records its live instance's pid here, so a stop ends the
    // process instead of waiting out the daemon's deadline. One file per node
    // is exact: this test never runs two instances of the same node at once.
    let pid_dir = tempfile::tempdir().expect("temp pid dir");
    let arm_pidfile = pid_dir.path().join("robot_arm.pid");
    let ctrl_pidfile = pid_dir.path().join("arm_controller.pid");

    // Pairing doc into the daemon's repo cache, then both nodes.
    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    seed_pairing_repo(&serve, &ctx, repo_dir.path());
    let arm_dir = tempfile::tempdir().expect("arm node dir");
    add_built_node(
        &ctx,
        arm_dir.path(),
        // `zero_or_one`: both sides of this fixture boot solo before the
        // other exists, so a run may write the slot vacant.
        &pairing_node_config(
            "robot_arm",
            "arm",
            "controller",
            config::node::Cardinality::ZeroOrOne,
            &instances,
            &arm_pidfile,
        ),
    );
    let ctrl_dir = tempfile::tempdir().expect("controller node dir");
    add_built_node(
        &ctx,
        ctrl_dir.path(),
        &pairing_node_config(
            "arm_controller",
            "controller",
            "arm",
            config::node::Cardinality::ZeroOrOne,
            &instances,
            &ctrl_pidfile,
        ),
    );

    // ── Coverage is enforced loudly ─────────────────────────────────────
    // Optional means vacatable, not exempt: a slot with neither flag is still
    // uncovered, and its message offers both remedies.
    let err = node_run_command("arm_0", "robot_arm", Vec::new(), Vec::new())
        .execute(&ctx)
        .expect_err("a pairing slot without --link/--vacant-link must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("controller") && msg.contains("--link") && msg.contains("--vacant-link"),
        "coverage failure should name the slot and both flags: {msg}"
    );

    // ── Vacant boot: the arm starts unpaired ────────────────────────────
    let mut arm_rx = emulate_pairing_instance(
        &messenger,
        &core_node_name,
        "robot_arm",
        "arm_1",
        "controller",
        &arm_pidfile,
    )
    .await;
    node_run_command(
        "arm_1",
        "robot_arm",
        Vec::new(),
        vec![(
            "controller".to_string(),
            "test rig: this slot has no peer".to_string(),
        )],
    )
    .execute(&ctx)
    .expect("run with --vacant-link should succeed");
    assert!(
        arm_rx.borrow().members.is_empty(),
        "a slot declared vacant must boot unpaired"
    );

    // ── Establish: the controller pairs at start ────────────────────────
    let mut ctrl_rx = emulate_pairing_instance(
        &messenger,
        &core_node_name,
        "arm_controller",
        "ctrl_1",
        "arm",
        &ctrl_pidfile,
    )
    .await;
    node_run_command(
        "ctrl_1",
        "arm_controller",
        vec![("arm".to_string(), "arm_1".to_string())],
        Vec::new(),
    )
    .execute(&ctx)
    .expect("run with --link should succeed");

    // Both endpoints received their absolute pin state live.
    let arm_pin = arm_rx.borrow_and_update().clone();
    let pin = arm_pin
        .peers()
        .next()
        .expect("arm_1's slot should be pinned");
    assert_eq!(pin.producer.instance_id, "ctrl_1");
    assert_eq!(pin.peer_link_id, "arm");
    let ctrl_pin = ctrl_rx.borrow_and_update().clone();
    let pin = ctrl_pin
        .peers()
        .next()
        .expect("ctrl_1's slot should be pinned");
    assert_eq!(pin.producer.instance_id, "arm_1");
    assert_eq!(pin.peer_link_id, "controller");

    // `stack list` shows the pair with the bidirectional arrow, the peer
    // slot carrying its core node like the bindings table's producers.
    let listing = peppy::commands::stack::list_nodes_collecting(&ctx, false, None)
        .await
        .expect("stack list should succeed")
        .output;
    assert!(
        listing.contains(&format!(
            "controller ⇌ ctrl_1:arm@{core_node_name} (arm_link:v1)"
        )),
        "stack list should show the established pair:\n{listing}"
    );

    // ── Exclusivity: a second controller cannot claim the same slot ─────
    let _ctrl2_rx = emulate_pairing_instance(
        &messenger,
        &core_node_name,
        "arm_controller",
        "ctrl_2",
        "arm",
        &ctrl_pidfile,
    )
    .await;
    let err = node_run_command(
        "ctrl_2",
        "arm_controller",
        vec![("arm".to_string(), "arm_1".to_string())],
        Vec::new(),
    )
    .execute(&ctx)
    .expect_err("pairing at an exclusively-claimed slot must fail");
    assert!(
        err.to_string().to_lowercase().contains("pair"),
        "exclusivity failure should mention pairing: {err}"
    );

    // ── Death auto-clears: stopping the controller unpairs the arm ──────
    NodeCommand {
        command: NodeCommands::Stop {
            instance_id: "ctrl_1".to_string(),
        },
    }
    .execute(&ctx)
    .expect("node stop should succeed");
    assert!(
        arm_rx.borrow_and_update().members.is_empty(),
        "the surviving arm must be live-notified Unpaired on peer death"
    );
    let listing = peppy::commands::stack::list_nodes_collecting(&ctx, false, None)
        .await
        .expect("stack list should succeed")
        .output;
    assert!(
        listing.contains("controller ⇌ (unpaired) [role arm of arm_link:v1]"),
        "stack list should show the survivor's slot unpaired:\n{listing}"
    );

    // ── Delivery failure unwinds: ready+health but NO peer_update ───────
    // The instance passes startup, but the daemon cannot deliver its pin;
    // the pair is reverted and the run fails loudly.
    emulate_startup_services(&messenger, &core_node_name, "arm_controller", "ctrl_2b").await;
    emulate_cooperative_shutdown(
        &messenger,
        &core_node_name,
        "arm_controller",
        "ctrl_2b",
        ctrl_pidfile.clone(),
    )
    .await;
    let err = node_run_command(
        "ctrl_2b",
        "arm_controller",
        vec![("arm".to_string(), "arm_1".to_string())],
        Vec::new(),
    )
    .execute(&ctx)
    .expect_err("an undeliverable peer_update must fail the run");
    assert!(
        err.to_string().contains("pairing"),
        "delivery failure should mention pairing: {err}"
    );
    // The failed delivery reverted the pair: the survivor stays unpaired.
    assert!(arm_rx.borrow_and_update().members.is_empty());

    let _ctrl3_rx = emulate_pairing_instance(
        &messenger,
        &core_node_name,
        "arm_controller",
        "ctrl_3",
        "arm",
        &ctrl_pidfile,
    )
    .await;
    node_run_command(
        "ctrl_3",
        "arm_controller",
        vec![("arm".to_string(), "arm_1".to_string())],
        Vec::new(),
    )
    .execute(&ctx)
    .expect("re-pairing the survivor should succeed");
    let arm_pin = arm_rx.borrow_and_update().clone();
    let pin = arm_pin.peers().next().expect("arm_1 should be re-pinned");
    assert_eq!(
        pin.producer.instance_id, "ctrl_3",
        "the survivor must be pinned to the NEW controller"
    );

    // ── Remove dissolves pairs and notifies the survivor ────────────────
    // `node remove --stop-instances` must dissolve the removed node's pairs
    // and live-notify each surviving peer Unpaired, exactly as `node stop`
    // does. Removing arm_controller tears down its paired instance ctrl_3, so
    // arm_1's slot must go Unpaired. Before the remove path threaded the
    // PairingCoordinator, this notification never happened and the arm kept
    // pinning a dead peer.
    NodeCommand {
        command: NodeCommands::Remove {
            node_ref: ("arm_controller".to_string(), "v1".to_string()),
            stop_instances: true,
            force: true,
        },
    }
    .execute(&ctx)
    .expect("node remove --stop-instances should succeed");
    assert!(
        arm_rx.borrow_and_update().members.is_empty(),
        "removing the paired controller must live-notify the surviving arm Unpaired"
    );
}

/// The instance ids of the peers a slot's watch currently holds, in the
/// order the set lists them.
fn held_peers(rx: &mut watch::Receiver<PeerSetState>) -> Vec<String> {
    rx.borrow_and_update()
        .peers()
        .map(|peer| peer.producer.instance_id.clone())
        .collect()
}

/// A `zero_or_more` slot holds one pair per peer. An engine boots with the
/// slot empty and no link, three controllers pair into it one at a time and
/// each side reads the set it holds, a scalar slot already in a pair admits
/// no second one, a stopped peer leaves the engine's set while the others
/// keep their pairs, a rerun under the same instance id joins the set again,
/// and a removal takes its pairs out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multi_slot_holds_one_pair_per_peer_across_stops_and_reruns() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    let messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let work_dir = tempfile::tempdir().expect("temp work dir");
    let ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    let instances = InstanceLifetime::new();
    let pid_dir = tempfile::tempdir().expect("temp pid dir");
    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    seed_pairing_repo(&serve, &ctx, repo_dir.path());

    // The engine plays `arm` for every controller through one slot.
    let engine_pidfile = pid_dir.path().join("sim_engine.pid");
    let engine_dir = tempfile::tempdir().expect("engine node dir");
    add_built_node(
        &ctx,
        engine_dir.path(),
        &pairing_node_config(
            "sim_engine",
            "arm",
            "limbs",
            config::node::Cardinality::ZeroOrMore,
            &instances,
            &engine_pidfile,
        ),
    );
    // One controller node per instance, so each instance records its own
    // pid and a stop ends exactly that process.
    let controllers = ["ctrl_1", "ctrl_2", "ctrl_3"];
    let controller_node = |instance_id: &str| format!("{instance_id}_node");
    let mut controller_dirs = Vec::new();
    let mut controller_pidfiles = Vec::new();
    for instance_id in controllers {
        let pidfile = pid_dir.path().join(format!("{instance_id}.pid"));
        let dir = tempfile::tempdir().expect("controller node dir");
        add_built_node(
            &ctx,
            dir.path(),
            &pairing_node_config(
                &controller_node(instance_id),
                "controller",
                "arm",
                config::node::Cardinality::One,
                &instances,
                &pidfile,
            ),
        );
        controller_dirs.push(dir);
        controller_pidfiles.push(pidfile);
    }

    // ── An empty multi slot needs no link and no vacancy ────────────────
    let mut engine_rx = emulate_pairing_instance(
        &messenger,
        &core_node_name,
        "sim_engine",
        "engine_1",
        "limbs",
        &engine_pidfile,
    )
    .await;
    node_run_command("engine_1", "sim_engine", Vec::new(), Vec::new())
        .execute(&ctx)
        .expect("a zero_or_more slot boots empty with no link");
    assert!(
        held_peers(&mut engine_rx).is_empty(),
        "an unlinked zero_or_more slot boots holding no pair"
    );

    // ── Each controller pairs into the open slot, one at a time ─────────
    let mut controller_rxs = Vec::new();
    for (instance_id, pidfile) in controllers.iter().zip(&controller_pidfiles) {
        let rx = emulate_pairing_instance(
            &messenger,
            &core_node_name,
            &controller_node(instance_id),
            instance_id,
            "arm",
            pidfile,
        )
        .await;
        node_run_command(
            instance_id,
            &controller_node(instance_id),
            vec![("arm".to_string(), "engine_1".to_string())],
            Vec::new(),
        )
        .execute(&ctx)
        .expect("pairing into an open multi slot should succeed");
        controller_rxs.push(rx);
    }
    assert_eq!(
        held_peers(&mut engine_rx),
        vec!["ctrl_1", "ctrl_2", "ctrl_3"],
        "the engine holds one pair per controller, in establishment order"
    );
    for rx in &mut controller_rxs {
        let state = rx.borrow_and_update().clone();
        let pin = state.peers().next().expect("each controller is paired");
        assert_eq!(pin.producer.instance_id, "engine_1");
        assert_eq!(pin.peer_link_id, "limbs");
    }
    let listing = peppy::commands::stack::list_nodes_collecting(&ctx, false, None)
        .await
        .expect("stack list should succeed")
        .output;
    for instance_id in controllers {
        assert!(
            listing.contains(&format!(
                "limbs ⇌ {instance_id}:arm@{core_node_name} (arm_link:v1)"
            )),
            "stack list renders one row per pair of the multi slot:\n{listing}"
        );
    }

    // ── A scalar slot in a pair admits no second one ────────────────────
    let err = node_run_command(
        "engine_2",
        "sim_engine",
        vec![("limbs".to_string(), "ctrl_1".to_string())],
        Vec::new(),
    )
    .execute(&ctx)
    .expect_err("a controller's scalar slot already in a pair refuses a second engine");
    assert!(
        err.to_string().to_lowercase().contains("pair"),
        "exclusivity failure should mention pairing: {err}"
    );

    // ── A stopped peer leaves the set; the others keep their pairs ──────
    NodeCommand {
        command: NodeCommands::Stop {
            instance_id: "ctrl_2".to_string(),
        },
    }
    .execute(&ctx)
    .expect("node stop should succeed");
    assert_eq!(
        held_peers(&mut engine_rx),
        vec!["ctrl_1", "ctrl_3"],
        "the stopped controller's pair dissolves and the survivors keep their positions"
    );
    assert_eq!(held_peers(&mut controller_rxs[0]), vec!["engine_1"]);
    assert_eq!(held_peers(&mut controller_rxs[2]), vec!["engine_1"]);

    // ── A rerun under the same instance id joins the set again ──────────
    // Its in-process services are still listening, so the run delivers to
    // the watch the test already holds for it.
    node_run_command(
        "ctrl_2",
        &controller_node("ctrl_2"),
        vec![("arm".to_string(), "engine_1".to_string())],
        Vec::new(),
    )
    .execute(&ctx)
    .expect("a stopped peer pairs into the slot again");
    assert_eq!(
        held_peers(&mut engine_rx),
        vec!["ctrl_1", "ctrl_3", "ctrl_2"],
        "the rejoined controller takes the last position"
    );
    assert_eq!(held_peers(&mut controller_rxs[1]), vec!["engine_1"]);

    // ── Removing a peer's node takes its pair out of the set ────────────
    NodeCommand {
        command: NodeCommands::Remove {
            node_ref: (controller_node("ctrl_1"), "v1".to_string()),
            stop_instances: true,
            force: true,
        },
    }
    .execute(&ctx)
    .expect("node remove --stop-instances should succeed");
    assert_eq!(
        held_peers(&mut engine_rx),
        vec!["ctrl_3", "ctrl_2"],
        "the removed controller's pair dissolves; the others stay"
    );
}

/// `peppy node run` takes every `--link` a multi slot's own run names: an
/// engine started with two links on its `zero_or_more` slot holds one pair
/// per link, in the order the run named them, and each peer's slot holds the
/// engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_pairs_every_link_its_multi_slot_names() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    let messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let work_dir = tempfile::tempdir().expect("temp work dir");
    let ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    let instances = InstanceLifetime::new();
    let pid_dir = tempfile::tempdir().expect("temp pid dir");
    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    seed_pairing_repo(&serve, &ctx, repo_dir.path());

    // Two controllers whose own slots start open, so the engine's run is
    // the side that names both pairs.
    let controllers = ["ctrl_1", "ctrl_2"];
    let controller_node = |instance_id: &str| format!("{instance_id}_node");
    let mut controller_dirs = Vec::new();
    let mut controller_rxs = Vec::new();
    for instance_id in controllers {
        let pidfile = pid_dir.path().join(format!("{instance_id}.pid"));
        let dir = tempfile::tempdir().expect("controller node dir");
        add_built_node(
            &ctx,
            dir.path(),
            &pairing_node_config(
                &controller_node(instance_id),
                "controller",
                "arm",
                config::node::Cardinality::ZeroOrMore,
                &instances,
                &pidfile,
            ),
        );
        let rx = emulate_pairing_instance(
            &messenger,
            &core_node_name,
            &controller_node(instance_id),
            instance_id,
            "arm",
            &pidfile,
        )
        .await;
        node_run_command(
            instance_id,
            &controller_node(instance_id),
            Vec::new(),
            Vec::new(),
        )
        .execute(&ctx)
        .expect("an open controller slot boots with no link");
        controller_dirs.push(dir);
        controller_rxs.push(rx);
    }

    let engine_pidfile = pid_dir.path().join("sim_engine.pid");
    let engine_dir = tempfile::tempdir().expect("engine node dir");
    add_built_node(
        &ctx,
        engine_dir.path(),
        &pairing_node_config(
            "sim_engine",
            "arm",
            "limbs",
            config::node::Cardinality::ZeroOrMore,
            &instances,
            &engine_pidfile,
        ),
    );
    let mut engine_rx = emulate_pairing_instance(
        &messenger,
        &core_node_name,
        "sim_engine",
        "engine_1",
        "limbs",
        &engine_pidfile,
    )
    .await;
    node_run_command(
        "engine_1",
        "sim_engine",
        controllers
            .iter()
            .map(|peer| ("limbs".to_string(), (*peer).to_string()))
            .collect(),
        Vec::new(),
    )
    .execute(&ctx)
    .expect("a multi slot takes every link its run names");

    assert_eq!(
        held_peers(&mut engine_rx),
        vec!["ctrl_1", "ctrl_2"],
        "the engine holds one pair per link, in the order the run named them"
    );
    for rx in &mut controller_rxs {
        assert_eq!(
            held_peers(rx),
            vec!["engine_1"],
            "each controller holds the engine"
        );
    }
}
