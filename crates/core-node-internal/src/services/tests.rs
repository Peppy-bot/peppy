use super::*;
use crate::test_support::LogCapture;
use daemon_config::consts::PeppyDirs;
use peppylib::CoreNodePresenceMessenger;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::sync::Arc;
use tokio::sync::Mutex;

async fn wait_for_log(capture: &LogCapture, needle: &str) {
    let observed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if capture.logs().contains(needle) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    assert!(
        observed.is_ok(),
        "timed out waiting for log entry `{needle}`; captured logs:\n{}",
        capture.logs()
    );
}

async fn create_mock_messenger() -> Arc<Mutex<Messenger>> {
    let adapter = MockAdapter::default();
    let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
    messenger
        .start_session()
        .await
        .expect("failed to start mock session");
    Arc::new(Mutex::new(messenger))
}

/// A messenger handle over a started in-memory mock session, enough for
/// service listeners and publisher loops to register and exchange messages.
/// Shared with the sibling modules' unit tests (clock, presence).
pub(crate) async fn started_mock_messenger() -> MessengerHandle {
    MessengerHandle::from_shared(create_mock_messenger().await)
}

/// A named core node booted to its ready signal on a fresh mock messenger,
/// holding the pieces the presence tests poke afterwards.
struct BootedCoreNode {
    /// Owns the on-disk layout backing the boot's `PeppyDirs`.
    _root: tempfile::TempDir,
    core_node: Arc<CoreNode>,
    /// Second handle on the boot messenger, for presence queries.
    presence_handle: MessengerHandle,
    boot_task: tokio::task::JoinHandle<crate::Result<()>>,
}

impl BootedCoreNode {
    async fn start(core_node_name: &str) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let messenger = create_mock_messenger().await;
        let presence_handle = MessengerHandle::from_shared(Arc::clone(&messenger));
        let core_node = Arc::new(CoreNode::new(test_core_node_config(
            messenger,
            Some(core_node_name),
            PeppyDirs::new(root.path()),
        )));

        let boot = Arc::clone(&core_node);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let boot_task = tokio::spawn(async move { boot.start_with_ready(Some(ready_tx)).await });
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .expect("presence-based boot should reach ready promptly")
            .expect("ready signal should fire");

        Self {
            _root: root,
            core_node,
            presence_handle,
            boot_task,
        }
    }

    /// Stops the serving task and drops the core node, modeling the serve
    /// runner's shutdown boundary (the last owner releases the retained
    /// presence token).
    async fn shut_down(self) {
        self.boot_task.abort();
        let _ = self.boot_task.await;
        drop(self.core_node);
    }
}

/// Live presence claims for one name, bounded for a mock router that answers
/// immediately.
async fn live_claims(handle: &MessengerHandle, core_node_name: &str) -> Vec<pmi::CoreNodePresence> {
    CoreNodePresenceMessenger::list_live(handle, Some(core_node_name), Duration::from_secs(1))
        .await
        .expect("presence list should succeed")
}

fn test_node_arguments() -> CoreNodeArguments {
    CoreNodeArguments {
        node_startup_timeout: Duration::from_secs(5),
        node_start_health_timeout: Duration::from_secs(5),
        health_monitor_interval: Duration::from_secs(5),
        health_monitor_timeout: Duration::from_secs(3),
        clock_publish_interval: Duration::from_millis(100),
        heartbeat_interval: Duration::from_secs(5),
        daemon_use_sim_time: false,
    }
}

/// Builds a `CoreNodeConfig` for the constructor tests; the cases differ only in
/// the messenger, the explicit name, and the dirs.
fn test_core_node_config(
    messenger: Arc<Mutex<Messenger>>,
    node_name: Option<&str>,
    peppy_dirs: PeppyDirs,
) -> CoreNodeConfig {
    CoreNodeConfig {
        messenger,
        node_name: node_name.map(str::to_string),
        arguments: test_node_arguments(),
        root_dir: std::env::temp_dir(),
        peppy_dirs,
        peppy_config: daemon_config::peppy_config::PeppyConfig::default(),
        organization_namespace: "local".to_string(),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
    }
}

/// Verifies that the core node is configured to run in-process, not as a spawned
/// subprocess or inside a container.
#[tokio::test]
async fn core_node_execution_has_no_run_cmd_and_no_container() {
    let messenger = create_mock_messenger().await;
    let peppy_dirs = PeppyDirs::new(std::env::temp_dir());
    let core_node = CoreNode::new(test_core_node_config(
        messenger,
        Some("test_core_node"),
        peppy_dirs,
    ));

    let execution = &core_node.node_config().execution;
    assert!(
        execution.run_cmd.is_none(),
        "core node must not have a run_cmd (it runs in-process, not as a spawned process)"
    );
    assert!(
        execution.container.is_none(),
        "core node must not have a container config"
    );
}

/// Verifies that, with no explicit name, the core node derives a deterministic
/// machine-uid based name with the `core-node-` prefix. Two instances built on
/// the same machine must produce the same name.
#[tokio::test]
async fn core_node_default_name_is_deterministic_and_machine_uid_based() {
    let peppy_dirs = PeppyDirs::new(std::env::temp_dir());

    let a = CoreNode::new(test_core_node_config(
        create_mock_messenger().await,
        None,
        peppy_dirs.clone(),
    ));
    let b = CoreNode::new(test_core_node_config(
        create_mock_messenger().await,
        None,
        peppy_dirs,
    ));

    let name_a = a.node_config().manifest.name.as_str();
    let name_b = b.node_config().manifest.name.as_str();
    assert_eq!(
        name_a, name_b,
        "default core node name must be deterministic across instances on the same machine"
    );
    assert!(
        name_a.starts_with("core-node-"),
        "default core node name must use the `core-node-` prefix, got `{name_a}`"
    );
    assert!(
        name_a.len() > "core-node-".len(),
        "default core node name must include a machine-uid suffix, got `{name_a}`"
    );
}

/// Verifies the explicit `node_name` override still wins over the machine-uid default.
#[tokio::test]
async fn core_node_explicit_name_overrides_machine_uid() {
    let messenger = create_mock_messenger().await;
    let peppy_dirs = PeppyDirs::new(std::env::temp_dir());
    let core_node = CoreNode::new(test_core_node_config(
        messenger,
        Some("custom_name"),
        peppy_dirs,
    ));
    assert_eq!(
        core_node.node_config().manifest.name.as_str(),
        "custom_name"
    );
}

/// The derived name is a pure function of the host identifier: same id, same
/// name — including the digit suffix, which comes from the same digest that
/// seeds the generator.
#[test]
fn derived_name_is_deterministic_per_host_id() {
    assert_eq!(
        derive_name_from_host_id("host-a"),
        derive_name_from_host_id("host-a"),
    );
    assert_ne!(
        derive_name_from_host_id("host-a"),
        derive_name_from_host_id("host-b"),
    );
}

/// Shape: `core-node-{adj}-{surname}-{NNNN}-{DDDDDDDDDD}` — the generator's
/// 4-digit base followed by the 10-digit zero-padded suffix — and the result
/// passes `Name::new` validation.
#[test]
fn derived_name_has_generator_base_and_ten_digit_suffix() {
    let name = derive_name_from_host_id("some-machine-uid");
    let name = name.as_str();

    assert!(
        name.starts_with("core-node-"),
        "derived name must use the `core-node-` prefix, got `{name}`"
    );
    let mut segments = name.rsplit('-');
    let suffix = segments.next().unwrap();
    assert_eq!(
        suffix.len(),
        10,
        "suffix must be 10 zero-padded decimal digits, got `{suffix}`"
    );
    assert!(suffix.chars().all(|c| c.is_ascii_digit()));
    let generator_digits = segments.next().unwrap();
    assert_eq!(
        generator_digits.len(),
        4,
        "the generator's 4-digit block must precede the suffix, got `{generator_digits}`"
    );
    assert!(generator_digits.chars().all(|c| c.is_ascii_digit()));

    assert!(Name::new(name.to_string()).is_ok());
}

/// Distinct host identifiers must yield distinct digit suffixes (the suffix is
/// the collision-entropy extension; if it collapsed to a constant it would add
/// nothing over the generator's ~304M combinations).
#[test]
fn distinct_host_ids_yield_distinct_suffixes() {
    let suffix = |host_id: &str| {
        derive_name_from_host_id(host_id)
            .as_str()
            .rsplit('-')
            .next()
            .unwrap()
            .to_string()
    };
    assert_ne!(suffix("host-a"), suffix("host-b"));
}

/// A second `start_with_ready` on the same instance is rejected rather than
/// re-running the destructive setup and double-registering listeners.
#[tokio::test]
async fn start_with_ready_rejects_a_second_start() {
    let core_node = Arc::new(CoreNode::new(test_core_node_config(
        create_mock_messenger().await,
        Some("dup_start_node"),
        PeppyDirs::new(std::env::temp_dir()),
    )));

    // Drive the first start on a task: it registers listeners then serves until
    // the session closes. The ready signal is a deterministic barrier: once it
    // fires, the `started` flag is set.
    let first = Arc::clone(&core_node);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let first_task = tokio::spawn(async move { first.start_with_ready(Some(ready_tx)).await });
    ready_rx
        .await
        .expect("first start should reach the ready signal");

    let err = core_node
        .start_with_ready(None)
        .await
        .expect_err("a second start must be rejected");
    assert!(matches!(err, crate::Error::AlreadyStarted));

    first_task.abort();
}

/// Boot refuses with `CoreNodeNameTaken` when a foreign daemon presence token
/// already claims the same core-node name. The refusal must also happen before
/// any destructive setup — the instances dir survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_refuses_to_boot_when_name_already_claimed() {
    let messenger = create_mock_messenger().await;

    // Simulate a foreign daemon generation. Keep the token alive until after
    // the boot attempt so the presence query observes the claim.
    let _foreign_presence = CoreNodePresenceMessenger::declare(
        &MessengerHandle::from_shared(Arc::clone(&messenger)),
        "collision_node",
        "foreign_instance",
    )
    .await
    .expect("foreign presence token should be declared");

    let root = tempfile::tempdir().expect("tempdir");
    let peppy_dirs = PeppyDirs::new(root.path());
    let marker = peppy_dirs.instances_dir().join("stale_instance");
    std::fs::create_dir_all(&marker).expect("pre-existing instances dir");

    let core_node = CoreNode::new(test_core_node_config(
        messenger,
        Some("collision_node"),
        peppy_dirs,
    ));

    let err = core_node
        .start_with_ready(None)
        .await
        .expect_err("boot must refuse while the name is claimed");
    assert!(
        matches!(err, crate::Error::CoreNodeNameTaken { ref name } if name == "collision_node"),
        "expected CoreNodeNameTaken for `collision_node`, got: {err}"
    );
    assert!(
        marker.exists(),
        "a refused boot must not clear the instances dir"
    );
}

/// With no other daemon advertising the name, boot proceeds to the ready
/// signal and the daemon's own presence token is live immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_boots_clean_when_name_unclaimed() {
    let booted = BootedCoreNode::start("unclaimed_node").await;

    let live = live_claims(&booted.presence_handle, "unclaimed_node").await;
    assert_eq!(live.len(), 1, "exactly this daemon should claim the name");
    assert_eq!(live[0].core_node, "unclaimed_node");
    assert_eq!(live[0].instance_id, booted.core_node.instance_id());

    booted.shut_down().await;
}

/// Two daemons started together arbitrate on their generation ids: one reaches
/// ready and retains the only live claim, while the other fails with
/// `CoreNodeNameTaken`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_daemons_allow_exactly_one_name_claim() {
    const CORE_NODE_NAME: &str = "simultaneous_claim_node";

    let messenger = create_mock_messenger().await;
    let presence_handle = MessengerHandle::from_shared(Arc::clone(&messenger));
    let first_root = tempfile::tempdir().expect("first tempdir");
    let second_root = tempfile::tempdir().expect("second tempdir");
    let first = Arc::new(CoreNode::new(test_core_node_config(
        Arc::clone(&messenger),
        Some(CORE_NODE_NAME),
        PeppyDirs::new(first_root.path()),
    )));
    let second = Arc::new(CoreNode::new(test_core_node_config(
        messenger,
        Some(CORE_NODE_NAME),
        PeppyDirs::new(second_root.path()),
    )));

    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let (first_ready_tx, first_ready_rx) = tokio::sync::oneshot::channel();
    let (second_ready_tx, second_ready_rx) = tokio::sync::oneshot::channel();
    let first_task = {
        let core_node = Arc::clone(&first);
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            core_node.start_with_ready(Some(first_ready_tx)).await
        })
    };
    let second_task = {
        let core_node = Arc::clone(&second);
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            core_node.start_with_ready(Some(second_ready_tx)).await
        })
    };

    barrier.wait().await;
    let (first_ready, second_ready) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(first_ready_rx, second_ready_rx)
    })
    .await
    .expect("both startup outcomes should settle promptly");
    assert_eq!(
        usize::from(first_ready.is_ok()) + usize::from(second_ready.is_ok()),
        1,
        "exactly one competing daemon must reach ready"
    );

    let live = live_claims(&presence_handle, CORE_NODE_NAME).await;
    assert_eq!(live.len(), 1, "exactly one claim must remain live");
    let expected_winner = if first_ready.is_ok() {
        first.instance_id()
    } else {
        second.instance_id()
    };
    assert_eq!(live[0].instance_id, expected_winner);

    let (winner_task, loser_task) = if first_ready.is_ok() {
        (first_task, second_task)
    } else {
        (second_task, first_task)
    };
    let loser_error = loser_task
        .await
        .expect("losing startup task should not panic")
        .expect_err("losing startup must fail");
    assert!(matches!(
        loser_error,
        crate::Error::CoreNodeNameTaken { ref name } if name == CORE_NODE_NAME
    ));
    winner_task.abort();
}

/// Dropping a stopped core node drops its retained token, removing the daemon
/// generation from presence enumeration without an explicit heartbeat grace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presence_token_is_released_when_core_node_drops() {
    let booted = BootedCoreNode::start("released_node").await;
    let presence_handle = booted.presence_handle.clone();

    assert_eq!(
        live_claims(&presence_handle, "released_node").await.len(),
        1
    );

    booted.shut_down().await;

    assert!(
        live_claims(&presence_handle, "released_node")
            .await
            .is_empty(),
        "dropping the core node must release its retained presence token"
    );
}

/// A foreign token appearing after boot is observed by the daemon's real
/// history-enabled watcher and produces one error alarm. A second foreign
/// `Alive` within the cooldown is consumed but must not produce another alarm.
#[tokio::test]
async fn duplicate_name_presence_alarm_is_rate_limited() {
    const CORE_NODE_NAME: &str = "duplicate_alarm_node";
    const FIRST_FOREIGN_INSTANCE: &str = "foreign_instance_one";
    const SECOND_FOREIGN_INSTANCE: &str = "foreign_instance_two";
    const ALARM: &str = "core-node name collision:";

    // A current-thread runtime keeps this scoped subscriber active for the
    // daemon's spawned watcher tasks, avoiding process-global log state and
    // interference with parallel tests.
    let log_capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(log_capture.clone())
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let booted = BootedCoreNode::start(CORE_NODE_NAME).await;

    let _first_foreign = CoreNodePresenceMessenger::declare(
        &booted.presence_handle,
        CORE_NODE_NAME,
        FIRST_FOREIGN_INSTANCE,
    )
    .await
    .expect("first foreign presence should be declared");
    wait_for_log(&log_capture, FIRST_FOREIGN_INSTANCE).await;
    assert_eq!(
        log_capture.logs().matches(ALARM).count(),
        1,
        "the first foreign Alive must emit one collision alarm; logs:\n{}",
        log_capture.logs()
    );

    let _second_foreign = CoreNodePresenceMessenger::declare(
        &booted.presence_handle,
        CORE_NODE_NAME,
        SECOND_FOREIGN_INSTANCE,
    )
    .await
    .expect("second foreign presence should be declared");
    // The debug observation proves the watcher consumed the second Alive; the
    // unchanged error count therefore exercises the cooldown, not scheduling.
    wait_for_log(&log_capture, SECOND_FOREIGN_INSTANCE).await;
    assert_eq!(
        log_capture.logs().matches(ALARM).count(),
        1,
        "a second foreign Alive within the cooldown must not emit another alarm; logs:\n{}",
        log_capture.logs()
    );

    booted.shut_down().await;
}
