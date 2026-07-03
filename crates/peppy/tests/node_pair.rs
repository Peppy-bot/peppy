//! End-to-end pairing flow over an in-process daemon (mock messaging):
//! `repo refresh` discovers the pairing doc, `node add` resolves
//! `depends_on.pairings` through the pairing cache, and `node run` enforces
//! coverage, establishes pairs via `--pair`, delivers live `peer_update`
//! pins to both endpoints, auto-clears on `node stop` (notifying the
//! survivor), supports re-pairing the survivor, and enforces slot
//! exclusivity. The "nodes" are `sleep` processes; their ready/health and
//! `peer_update` services run in-process on the shared mock messenger,
//! exactly the seams a real peppylib node exposes.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;
use peppylib::MessengerHandle;
use peppylib::messaging::PeerPinState;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::peer_update::listen_for_peer_update;
use peppylib::services::ready::listen_for_node_ready;
use tokio::sync::watch;

use super::common::{seed_pairing_repo, test_node_target};

/// A node declaring one pairing slot. `run_cmd` is a plain `sleep` so the
/// daemon has a real process to own; the node's services are emulated
/// in-process by the test.
fn node_config(name: &str, role: &str, link_id: &str) -> String {
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
            execution: {{ language: "rust", run_cmd: ["sleep", "30"] }}
        }}"#
    )
}

/// Writes a node dir with the given config and `node add --build`s it (no
/// `build_cmd`, so "build" just marks the entity Ready).
fn add_node(ctx: &Arc<AppContext>, dir: &Path, config: &str) {
    std::fs::write(dir.join("peppy.json5"), config).expect("write node config");
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(dir.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(ctx)
    .expect("node add should succeed");
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
    listen_for_node_ready(
        messenger,
        core_node_name,
        instance_id,
        test_node_target(node_name),
    )
    .await
    .expect("ready service should start");
    listen_for_node_health(
        messenger,
        core_node_name,
        instance_id,
        test_node_target(node_name),
    )
    .await
    .expect("health service should start");

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

fn run_command(
    instance_id: &str,
    node: &str,
    pairs: Vec<(String, String)>,
    defer: Vec<String>,
) -> NodeCommand {
    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(node.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            binds: Vec::new(),
            pairs,
            defer_pairs: defer,
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_establish_stop_repair_and_exclusivity() {
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
    add_node(
        &ctx,
        arm_dir.path(),
        &node_config("robot_arm", "arm", "controller"),
    );
    let ctrl_dir = tempfile::tempdir().expect("controller node dir");
    add_node(
        &ctx,
        ctrl_dir.path(),
        &node_config("arm_controller", "controller", "arm"),
    );

    // ── Coverage is enforced loudly ─────────────────────────────────────
    let err = run_command("arm_0", "robot_arm", Vec::new(), Vec::new())
        .execute(&ctx)
        .expect_err("a required pairing slot without --pair/--defer-pair must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("controller") && msg.contains("--pair") && msg.contains("--defer-pair"),
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
    run_command(
        "arm_1",
        "robot_arm",
        Vec::new(),
        vec!["controller".to_string()],
    )
    .execute(&ctx)
    .expect("run with --defer-pair should succeed");
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
    run_command(
        "ctrl_1",
        "arm_controller",
        vec![("arm".to_string(), "arm_1".to_string())],
        Vec::new(),
    )
    .execute(&ctx)
    .expect("run with --pair should succeed");

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
    let listing = peppy::commands::stack::list_nodes_collecting(&ctx, None, false)
        .await
        .expect("stack list should succeed");
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
    let err = run_command(
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
    let listing = peppy::commands::stack::list_nodes_collecting(&ctx, None, false)
        .await
        .expect("stack list should succeed");
    assert!(
        listing.contains("controller ⇌ (unpaired) [role arm of arm_link:v1]"),
        "stack list should show the survivor's slot unpaired:\n{listing}"
    );

    // ── Delivery failure unwinds: ready+health but NO peer_update ───────
    // The instance passes startup, but the daemon cannot deliver its pin;
    // the pair is reverted and the run fails loudly.
    listen_for_node_ready(
        &messenger,
        &core_node_name,
        "ctrl_2b",
        test_node_target("arm_controller"),
    )
    .await
    .expect("ready service should start");
    listen_for_node_health(
        &messenger,
        &core_node_name,
        "ctrl_2b",
        test_node_target("arm_controller"),
    )
    .await
    .expect("health service should start");
    let err = run_command(
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
    run_command(
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
}
