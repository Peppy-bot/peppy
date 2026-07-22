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
//! way `node stop` does.

use std::collections::BTreeMap;
use std::sync::Arc;

use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;
use peppylib::MessengerHandle;
use peppylib::messaging::PeerPinState;
use peppylib::services::peer_update::listen_for_peer_update;
use tokio::sync::watch;

use super::common::{
    add_built_node, emulate_startup_services, node_run_command, seed_pairing_repo, test_node_target,
};

/// A node declaring one pairing slot, with the `interfaces.topics` entries
/// the role owes: `emits` must cover the role's topics exactly, `consumes`
/// names the counterpart's. `run_cmd` is a plain `sleep` so the daemon has a
/// real process to own; the node's services are emulated in-process by the
/// test.
fn node_config(name: &str, role: &str, link_id: &str) -> String {
    let (emits, consumes) = arm_link_topics(role);
    format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "{name}",
                tag: "v1",
                depends_on: {{
                    pairings: [
                        {{ name: "arm_link", tag: "v1", role: "{role}", link_id: "{link_id}" }}
                    ]
                }}
            }},
            interfaces: {{
                topics: {{
                    emits: [{{ link_id: "{link_id}", name: "{emits}" }}],
                    consumes: [{{ link_id: "{link_id}", name: "{consumes}" }}]
                }}
            }},
            execution: {{ language: "rust", run_cmd: ["sleep", "30"] }}
        }}"#
    )
}

/// The `(emitted, consumed)` topic names of `common::ARM_LINK_PAIRING` for a
/// role, so a manifest's entries stay in step with the document.
fn arm_link_topics(role: &str) -> (&'static str, &'static str) {
    match role {
        "controller" => ("joint_commands", "joint_states"),
        "arm" => ("joint_states", "joint_commands"),
        other => panic!("arm_link declares no role `{other}`"),
    }
}

/// Emulates a spawned instance's in-process services (ready, health,
/// peer_update) and hands back the pairing slot's pin-state watch.
async fn emulate_instance_services(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    instance_id: &str,
    link_id: &str,
) -> watch::Receiver<PeerPinState> {
    emulate_startup_services(messenger, core_node_name, node_name, instance_id).await;

    let (tx, rx) = watch::channel(PeerPinState::unpaired());
    let slots = Arc::new(BTreeMap::from([(link_id.to_string(), tx)]));
    listen_for_peer_update(
        messenger,
        core_node_name,
        instance_id,
        test_node_target(node_name),
        slots,
    )
    .await
    .expect("peer_update service should start");
    rx
}

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

    // Pairing doc into the daemon's repo cache, then both nodes.
    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    seed_pairing_repo(&serve, &ctx, repo_dir.path());
    let arm_dir = tempfile::tempdir().expect("arm node dir");
    add_built_node(
        &ctx,
        arm_dir.path(),
        &node_config("robot_arm", "arm", "controller"),
    );
    let ctrl_dir = tempfile::tempdir().expect("controller node dir");
    add_built_node(
        &ctx,
        ctrl_dir.path(),
        &node_config("arm_controller", "controller", "arm"),
    );

    // ── Coverage is enforced loudly ─────────────────────────────────────
    let err = node_run_command("arm_0", "robot_arm", Vec::new(), Vec::new())
        .execute(&ctx)
        .expect_err("a required pairing slot without --link/--defer-link must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("controller") && msg.contains("--link") && msg.contains("--defer-link"),
        "coverage failure should name the slot and both flags: {msg}"
    );

    // ── Deferred boot: the arm starts unpaired ──────────────────────────
    let mut arm_rx = emulate_instance_services(
        &messenger,
        &core_node_name,
        "robot_arm",
        "arm_1",
        "controller",
    )
    .await;
    node_run_command(
        "arm_1",
        "robot_arm",
        Vec::new(),
        vec!["controller".to_string()],
    )
    .execute(&ctx)
    .expect("run with --defer-link should succeed");
    assert!(
        arm_rx.borrow().pin.is_none(),
        "a deferred slot must boot unpaired"
    );

    // ── Establish: the controller pairs at start ────────────────────────
    let mut ctrl_rx = emulate_instance_services(
        &messenger,
        &core_node_name,
        "arm_controller",
        "ctrl_1",
        "arm",
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
    let pin = arm_pin.pin.expect("arm_1's slot should be pinned");
    assert_eq!(pin.producer.instance_id, "ctrl_1");
    assert_eq!(pin.peer_link_id, "arm");
    let ctrl_pin = ctrl_rx.borrow_and_update().clone();
    let pin = ctrl_pin.pin.expect("ctrl_1's slot should be pinned");
    assert_eq!(pin.producer.instance_id, "arm_1");
    assert_eq!(pin.peer_link_id, "controller");

    // `stack list` shows the pair with the bidirectional arrow, the peer
    // slot carrying its core node like the bindings table's producers.
    let listing = peppy::commands::stack::list_nodes_collecting(&ctx, false)
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
    let _ctrl2_rx = emulate_instance_services(
        &messenger,
        &core_node_name,
        "arm_controller",
        "ctrl_2",
        "arm",
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
        arm_rx.borrow_and_update().pin.is_none(),
        "the surviving arm must be live-notified Unpaired on peer death"
    );
    let listing = peppy::commands::stack::list_nodes_collecting(&ctx, false)
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
    assert!(arm_rx.borrow_and_update().pin.is_none());

    let _ctrl3_rx = emulate_instance_services(
        &messenger,
        &core_node_name,
        "arm_controller",
        "ctrl_3",
        "arm",
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
    let pin = arm_pin.pin.expect("arm_1 should be re-pinned");
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
        arm_rx.borrow_and_update().pin.is_none(),
        "removing the paired controller must live-notify the surviving arm Unpaired"
    );
}
