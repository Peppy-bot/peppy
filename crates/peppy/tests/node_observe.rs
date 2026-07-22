//! End-to-end observer flow over an in-process daemon (mock messaging),
//! exercised through the SAME `node run` CLI path a user drives, NOT the
//! launcher. It proves two things the launcher path already had but the lone
//! `node run` path did not:
//!
//! 1. `node run --link <observer>@<source>` carries the resolved observation on
//!    the goal, so the daemon registers it and delivers the source pin the
//!    moment the observer commits to Running, exactly as a `stack launch`
//!    observer is served. Without it the observer booted validated but silent.
//! 2. Stopping OR removing the source runs the single teardown seam, so the
//!    observer is live-notified its source went down (`source_live = false`,
//!    pin retained), identically to how a paired peer is notified. Every
//!    stopped node is torn down the same way, pairing or observing or neither.
//!
//! The "nodes" are `sleep` processes; their ready/health and `observation_update`
//! services run in-process on the shared mock messenger, the same seams a real
//! peppylib node exposes.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;
use peppylib::MessengerHandle;
use peppylib::messaging::ObservationState;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::observation_update::listen_for_observation_update;
use peppylib::services::ready::listen_for_node_ready;
use tokio::sync::watch;

use super::common::{seed_pairing_repo, test_node_target};

/// The source: plays the `arm` role of `arm_link/v1` through participant slot
/// `controller`, emitting that role's `joint_states`. It boots standalone by
/// deferring its own participant slot (it is observed, never paired here).
fn source_config() -> &'static str {
    r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "robot_arm",
            tag: "v1",
            depends_on: {
                pairings: [
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }
                ]
            }
        },
        interfaces: {
            topics: {
                emits: [{ link_id: "controller", name: "joint_states" }],
                consumes: [{ link_id: "controller", name: "joint_commands" }]
            }
        },
        execution: { language: "rust", run_cmd: ["sleep", "30"] }
    }"#
}

/// The observer: watches the `arm` role of `arm_link/v1` through observer slot
/// `watch`, consuming the topic that role emits. Emits nothing.
fn observer_config() -> &'static str {
    r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "recorder",
            tag: "v1",
            depends_on: {
                pairings: [
                    { name: "arm_link", tag: "v1", observes_role: "arm", link_id: "watch" }
                ]
            }
        },
        interfaces: {
            topics: {
                consumes: [{ link_id: "watch", name: "joint_states" }]
            }
        },
        execution: { language: "rust", run_cmd: ["sleep", "30"] }
    }"#
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
            links: Vec::new(),
            defer_links: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(ctx)
    .expect("node add should succeed");
}

fn run_command(
    instance_id: &str,
    node: &str,
    links: Vec<(String, String)>,
    defer: Vec<String>,
) -> NodeCommand {
    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(node.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            links,
            defer_links: defer,
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
}

/// Emulates the startup services (`ready`, `health`) every spawned instance
/// exposes. The returned join handles are intentionally dropped: tokio detaches
/// the tasks, which keep serving on the shared messenger until the test ends.
async fn emulate_startup_services(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    instance_id: &str,
) {
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
}

/// Emulates an observer instance's services (ready, health, observation_update)
/// and hands back the observer slot's absolute-state watch.
async fn emulate_observer_services(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    instance_id: &str,
    observer_link_id: &str,
) -> watch::Receiver<ObservationState> {
    emulate_startup_services(messenger, core_node_name, node_name, instance_id).await;
    let (tx, rx) = watch::channel(ObservationState::unregistered());
    let slots = Arc::new(BTreeMap::from([(observer_link_id.to_string(), tx)]));
    listen_for_observation_update(
        messenger,
        core_node_name,
        instance_id,
        test_node_target(node_name),
        slots,
    )
    .await
    .expect("observation_update service should start");
    rx
}

/// A running daemon with the source and observer nodes added (not yet run), plus
/// the handles a test needs to drive `node run`/`stop`/`remove`. The temp dirs
/// and the serve emulation are held so nothing is torn down mid-test.
struct Fixture {
    _serve: ServeCommandEmulation,
    ctx: Arc<AppContext>,
    messenger: MessengerHandle,
    core_node_name: String,
    _work_dir: tempfile::TempDir,
    _repo_dir: tempfile::TempDir,
    _source_dir: tempfile::TempDir,
    _observer_dir: tempfile::TempDir,
}

async fn setup() -> Fixture {
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
    let source_dir = tempfile::tempdir().expect("source node dir");
    add_node(&ctx, source_dir.path(), source_config());
    let observer_dir = tempfile::tempdir().expect("observer node dir");
    add_node(&ctx, observer_dir.path(), observer_config());

    Fixture {
        _serve: serve,
        ctx,
        messenger,
        core_node_name,
        _work_dir: work_dir,
        _repo_dir: repo_dir,
        _source_dir: source_dir,
        _observer_dir: observer_dir,
    }
}

/// Runs the source (deferring its participant slot so it boots standalone) then
/// the observer with `--link watch@arm_1`, and returns the observer's state
/// watch already advanced past the initial live delivery.
async fn run_source_then_observer(fx: &Fixture) -> watch::Receiver<ObservationState> {
    emulate_startup_services(&fx.messenger, &fx.core_node_name, "robot_arm", "arm_1").await;
    run_command(
        "arm_1",
        "robot_arm",
        Vec::new(),
        vec!["controller".to_string()],
    )
    .execute(&fx.ctx)
    .expect("source run (participant slot deferred) should succeed");

    let mut obs_rx =
        emulate_observer_services(&fx.messenger, &fx.core_node_name, "recorder", "rec_1", "watch")
            .await;
    run_command(
        "rec_1",
        "recorder",
        vec![("watch".to_string(), "arm_1".to_string())],
        Vec::new(),
    )
    .execute(&fx.ctx)
    .expect("observer run with --link should succeed");

    // FIX #1: the lone `node run` observer received its source pin live, at a
    // bumped incarnation generation, exactly as a launcher observer would.
    let state = obs_rx.borrow_and_update().clone();
    let pin = state
        .source
        .expect("rec_1's observer slot should have a resolved source after node run");
    assert_eq!(pin.producer.instance_id, "arm_1");
    // The daemon re-stamps the source's core_node from its own name (the wire
    // carries only the instance id). Asserting it guards the routing-critical
    // half of register_instance's ProducerRef: a wrong core_node still delivers
    // a pin but pins the subscription to a non-existent address, the exact
    // "booted validated but silent" failure this path exists to prevent.
    assert_eq!(pin.producer.core_node, fx.core_node_name);
    assert_eq!(pin.source_link_id, "controller");
    assert!(state.source_live, "the source is Running, so it is live");
    assert!(
        state.source_generation >= 1,
        "a live source carries a bumped incarnation generation, got {}",
        state.source_generation
    );
    obs_rx
}

/// Fix #1 (delivery on the CLI path) + fix #2 (a `node stop` of the source runs
/// the teardown seam and live-notifies the observer).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_observer_receives_source_pin_and_stop_notifies() {
    let fx = setup().await;

    // Coverage is enforced loudly on the observer's own slot: no `--link` and no
    // `--defer-link` fails at preflight, naming the slot and the opt-out flag.
    let err = run_command("rec_1", "recorder", Vec::new(), Vec::new())
        .execute(&fx.ctx)
        .expect_err("a required observer slot without --link/--defer-link must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("watch") && msg.contains("--defer-link"),
        "coverage failure should name the observer slot and its opt-out: {msg}"
    );

    let mut obs_rx = run_source_then_observer(&fx).await;

    // FIX #2: stopping the source runs the same teardown seam a paired peer's
    // death runs, live-notifying the observer that its source went down. The pin
    // is retained (the observer keeps its subscription declared), only
    // `source_live` flips false.
    NodeCommand {
        command: NodeCommands::Stop {
            instance_id: "arm_1".to_string(),
        },
    }
    .execute(&fx.ctx)
    .expect("node stop should succeed");

    let state = obs_rx.borrow_and_update().clone();
    assert!(
        !state.source_live,
        "a stopped source must live-notify its observer source_live=false"
    );
    assert!(
        state.source.is_some(),
        "the source pin is retained on stop so the observer keeps its subscription declared"
    );
}

/// Fix #2 on the remove path: `node remove --stop-instances` of the source tears
/// down its observed instance through the same seam, so a running observer is
/// live-notified its source went down, exactly as `node stop` does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_remove_source_notifies_running_observer() {
    let fx = setup().await;
    let mut obs_rx = run_source_then_observer(&fx).await;

    NodeCommand {
        command: NodeCommands::Remove {
            node_ref: ("robot_arm".to_string(), "v1".to_string()),
            stop_instances: true,
            force: true,
        },
    }
    .execute(&fx.ctx)
    .expect("node remove --stop-instances should succeed");

    let state = obs_rx.borrow_and_update().clone();
    assert!(
        !state.source_live,
        "removing the observed source must live-notify the observer source_live=false"
    );
    assert!(
        state.source.is_some(),
        "the source pin is retained on remove so the observer keeps its subscription declared"
    );
}
