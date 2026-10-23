mod common;

use common::test_node_target;
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::{Name, NodeInstanceConfig, RuntimeConfig};
use peppylib::PeppyError;
use peppylib::encoding::health::{NodeHealthRequest, NodeHealthResponse};
use peppylib::messaging::{
    NODE_HEALTH_SERVICE, NODE_READY_SERVICE, ProducerRef, SHUTDOWN_SERVICE, ServiceTarget,
};
use peppylib::runtime::CancellationToken;
use peppylib::runtime::{NodeBuilder, StandaloneConfig};
use peppylib::types::Payload;
use pmi::ZenohAdapter;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TEST_CORE_NODE: &str = "test_core";
/// The per-boot instance id of the daemon that the daemon-mode configs name.
/// No daemon runs in these tests, so nothing beats under it.
const TEST_CORE_NODE_INSTANCE_ID: &str = "test-core-boot";
const TEST_NODE_NAME: &str = "test_node";
const TEST_INSTANCE_ID: &str = "test_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const TEST_FREQUENCY_HZ: f64 = 10.0;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct Parameters {
    frequency_hz: f64,
}

/// The test node's manifest: the one parameter the test `Parameters` type
/// reads, and no endpoints.
const TEST_NODE_PEPPY_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "test_node",
    tag: "v1",
  },
  execution: {
    language: "rust",
    parameters: {
      frequency_hz: "f64"
    },
    run_cmd: ["./target/debug/test_node"]
  },
}"#;

struct EnvAndDirGuard {
    previous_runtime_config: Option<String>,
    previous_dir: PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

impl EnvAndDirGuard {
    /// Takes the env lock and records the runtime config variable and the
    /// current dir that `Drop` restores.
    fn lock_and_record() -> Self {
        let lock = ENV_LOCK
            .lock()
            .expect("env lock poisoned by a previous test panic");

        Self {
            previous_runtime_config: std::env::var(RUNTIME_CONFIG_VAR_NAME).ok(),
            previous_dir: std::env::current_dir().expect("current dir should be readable"),
            _lock: lock,
        }
    }

    fn new(temp_dir: &Path, runtime_config_path: &Path) -> Self {
        let guard = Self::lock_and_record();

        // SAFETY: environment mutation is guarded by a global mutex to avoid races.
        unsafe { std::env::set_var(RUNTIME_CONFIG_VAR_NAME, runtime_config_path) };
        std::env::set_current_dir(temp_dir).expect("set_current_dir should succeed");

        guard
    }

    /// Create a guard that ensures the runtime config env var is NOT set.
    /// Used by standalone tests to prevent races with daemon tests.
    fn new_standalone() -> Self {
        let guard = Self::lock_and_record();

        // SAFETY: environment mutation is guarded by a global mutex to avoid races.
        // Remove the env var to ensure standalone mode is used.
        unsafe {
            std::env::remove_var(RUNTIME_CONFIG_VAR_NAME);
        };

        guard
    }
}

impl Drop for EnvAndDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous_dir).expect("restore current dir");
        // SAFETY: environment mutation is guarded by a global mutex to avoid races.
        unsafe {
            match &self.previous_runtime_config {
                Some(value) => std::env::set_var(RUNTIME_CONFIG_VAR_NAME, value),
                None => std::env::remove_var(RUNTIME_CONFIG_VAR_NAME),
            }
        }
    }
}

/// Calls the test node's `service` as the shutdown sender, pinned to the
/// test instance.
async fn poll_test_node(
    messenger: &peppylib::MessengerHandle,
    service: &str,
    request: Payload,
    timeout: Duration,
) -> peppylib::PeppyResult<peppylib::Message> {
    peppylib::ServiceMessenger::poll(
        messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
        service,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
        request,
        timeout,
    )
    .await
}

/// Probes once whether the test instance answers on `service`.
async fn is_test_node_service_reachable(
    messenger: &peppylib::MessengerHandle,
    service: &str,
) -> bool {
    peppylib::ServiceMessenger::is_reachable(
        messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
        service,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
    )
    .await
    .expect("reachability check should succeed")
}

/// Polls until the test node's `service` is reachable. Panics with the
/// runner's result if the runner exits first, and with `timeout_message`
/// when the service is still unreachable after 10 s.
async fn wait_until_reachable<T: std::fmt::Debug>(
    messenger: &peppylib::MessengerHandle,
    runner_task: &mut tokio::task::JoinHandle<T>,
    service: &str,
    timeout_message: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if runner_task.is_finished() {
            let result = runner_task.await.expect("runner task should not panic");
            panic!("runner exited early: {result:?}");
        }

        if is_test_node_service_reachable(messenger, service).await {
            return;
        }

        if Instant::now() >= deadline {
            panic!("{timeout_message}");
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Daemon-mode scaffolding: an ephemeral router, a node `peppy.json5` +
/// launch config in a temp dir, and the env guard that points `NodeBuilder`
/// at them. Declaration order matters for drop: the env guard restores cwd
/// before the temp dir it points into is removed.
struct DaemonStack {
    _env_guard: EnvAndDirGuard,
    _temp_dir: tempfile::TempDir,
    router: pmi::ZenohdInstance,
}

impl DaemonStack {
    /// Connects a control messenger to the stack's router. The daemon runner
    /// opens its session under the `local` workspace namespace (no workspace
    /// id in the runtime config); this messenger must too, or its
    /// reachability probe never routes to the node's services.
    async fn connect_control_messenger(&self) -> peppylib::MessengerHandle {
        peppylib::MessengerHandle::connect(&self.router.host, self.router.port)
            .await
            .expect("failed to create messenger")
    }
}

/// Stands up the daemon-mode scaffolding for the manifest `peppy_config`,
/// with an optional `lifecycle.shutdown_grace_secs` override in the launch
/// config (mirroring what the daemon ships to spawned nodes).
async fn start_daemon_stack(peppy_config: &str, shutdown_grace_secs: Option<u64>) -> DaemonStack {
    start_daemon_stack_with_lifecycle(peppy_config, |lifecycle| {
        if let Some(grace) = shutdown_grace_secs {
            lifecycle.shutdown_grace_secs = grace;
        }
    })
    .await
}

/// [`start_daemon_stack`] with the `lifecycle` block of the launch config
/// set by `configure`, as the daemon resolves it for a spawned node.
async fn start_daemon_stack_with_lifecycle(
    peppy_config: &str,
    configure: impl FnOnce(&mut config::runtime::LifecycleRuntimeConfig),
) -> DaemonStack {
    let router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&peppy_config_path, peppy_config).expect("failed to write peppy config");
    config::fingerprint::create_codegen_fingerprint(
        &peppy_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let mut runtime_config = RuntimeConfig::new(
        &router.host,
        router.port,
        NodeInstanceConfig {
            arguments: serde_json5::from_str(&format!("{{ frequency_hz: {TEST_FREQUENCY_HZ} }}"))
                .expect("runtime args should parse"),
            ..NodeInstanceConfig::new(
                Name::new(TEST_INSTANCE_ID).expect("instance id should be valid"),
            )
        },
        TEST_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
        TEST_CORE_NODE_INSTANCE_ID,
    )
    .expect("runtime config should build");
    configure(&mut runtime_config.lifecycle);
    let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
    runtime_config
        .save_json5_launch_config(&runtime_config_path)
        .expect("failed to write runtime config");

    let env_guard = EnvAndDirGuard::new(temp_dir.path(), &runtime_config_path);

    DaemonStack {
        _env_guard: env_guard,
        _temp_dir: temp_dir,
        router,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_runner_succeed() {
    let stack = start_daemon_stack(TEST_NODE_PEPPY_CONFIG, None).await;

    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<f64>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|parameters: Parameters, _node_runner| async move {
            let _ = setup_tx.send(parameters.frequency_hz);
            Ok(())
        })
    });

    let frequency_hz = tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");
    assert_eq!(frequency_hz, TEST_FREQUENCY_HZ);

    let messenger = stack.connect_control_messenger().await;

    wait_until_reachable(
        &messenger,
        &mut runner_task,
        SHUTDOWN_SERVICE,
        "shutdown service did not become reachable",
    )
    .await;

    let health_request = NodeHealthRequest::new()
        .encode()
        .expect("failed to encode health request");
    let health_response = poll_test_node(
        &messenger,
        NODE_HEALTH_SERVICE,
        health_request,
        Duration::from_secs(2),
    )
    .await
    .expect("health service should respond");
    NodeHealthResponse::decode(&health_response.payload()).expect("health response should decode");

    let shutdown_payload = Payload::from_static(b"shutdown");
    let shutdown_response = poll_test_node(
        &messenger,
        SHUTDOWN_SERVICE,
        shutdown_payload.clone(),
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");

    assert_eq!(shutdown_response.payload(), &shutdown_payload);
    assert_eq!(shutdown_response.instance_id(), TEST_INSTANCE_ID);

    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");
}

/// A manifest declaring `panel` and `viewer` under `execution.endpoints`,
/// with the one parameter the test `Parameters` type reads.
const ENDPOINTS_PEPPY_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "test_node",
    tag: "v1",
  },
  execution: {
    language: "rust",
    parameters: {
      frequency_hz: "f64"
    },
    run_cmd: ["./target/debug/test_node"],
    endpoints: {
      panel: { kind: "page", description: "The operator panel." },
      viewer: { kind: "page", description: "The viewer page." },
    },
  },
}"#;

fn binding(scheme: &str, address: &str, path: &str) -> peppylib::runtime::EndpointBinding {
    peppylib::runtime::EndpointBinding {
        scheme: scheme.to_string(),
        address: address.parse().expect("a socket address"),
        path: path.to_string(),
    }
}

/// Inside setup, a declared label is announced once; an undeclared label and
/// a second announcement are refused naming the label. Once setup returned
/// the set is sealed, so a late announcement is refused too, and the sealed
/// set is what the runner reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_runner_announces_declared_endpoints_during_setup_only() {
    let _env_guard = EnvAndDirGuard::new_standalone();

    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&peppy_config_path, ENDPOINTS_PEPPY_CONFIG)
        .expect("failed to write peppy config");

    let standalone_config = StandaloneConfig::new()
        .with_parameters_json(serde_json::json!({ "frequency_hz": TEST_FREQUENCY_HZ }))
        .with_messaging(&router_host, router_port)
        .with_instance_id(TEST_INSTANCE_ID);

    let (setup_tx, setup_rx) =
        tokio::sync::oneshot::channel::<std::sync::Arc<peppylib::runtime::NodeRunner>>();
    let runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new()
            .with_config_path(&peppy_config_path)
            .standalone(standalone_config)
            .run(|_parameters: Parameters, node_runner| async move {
                node_runner
                    .announce_endpoint("panel", binding("http", "0.0.0.0:8765", ""))
                    .expect("a declared label is accepted");
                let undeclared = node_runner
                    .announce_endpoint("admin", binding("http", "127.0.0.1:9000", ""))
                    .expect_err("an undeclared label is refused");
                assert!(
                    matches!(undeclared, PeppyError::UndeclaredEndpoint { ref label } if label == "admin"),
                    "{undeclared}"
                );
                let twice = node_runner
                    .announce_endpoint("panel", binding("http", "0.0.0.0:8766", ""))
                    .expect_err("a second announcement is refused");
                assert!(
                    matches!(twice, PeppyError::EndpointAlreadyAnnounced { ref label } if label == "panel"),
                    "{twice}"
                );
                node_runner
                    .announce_endpoint("viewer", binding("https", "127.0.0.1:8080", "/"))
                    .expect("the second declared label is accepted");
                let _ = setup_tx.send(node_runner);
                Ok(())
            })
    });

    let node_runner = tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");

    // Setup has returned by the time the runner handed itself out, but the
    // seal happens right after; wait on the observable consequence rather
    // than on time.
    let deadline = Instant::now() + Duration::from_secs(5);
    let sealed = loop {
        match node_runner.announce_endpoint("panel", binding("http", "0.0.0.0:8765", "")) {
            Err(PeppyError::EndpointsSealed { label }) => break label,
            Err(PeppyError::EndpointAlreadyAnnounced { .. }) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            other => panic!("expected the set to seal after setup, got {other:?}"),
        }
    };
    assert_eq!(sealed, "panel");
    let announced: Vec<(String, String)> = node_runner
        .announced_endpoints()
        .into_iter()
        .map(|endpoint| (endpoint.label, endpoint.binding.address.to_string()))
        .collect();
    assert_eq!(
        announced,
        [
            ("panel".to_string(), "0.0.0.0:8765".to_string()),
            ("viewer".to_string(), "127.0.0.1:8080".to_string()),
        ]
    );

    node_runner.cancellation_token().cancel();
    tokio::time::timeout(Duration::from_secs(10), runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");
}

/// A setup that returns without announcing a declared label fails the run,
/// naming the label.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_runner_fails_when_a_declared_endpoint_is_not_announced() {
    let _env_guard = EnvAndDirGuard::new_standalone();

    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&peppy_config_path, ENDPOINTS_PEPPY_CONFIG)
        .expect("failed to write peppy config");

    let standalone_config = StandaloneConfig::new()
        .with_parameters_json(serde_json::json!({ "frequency_hz": TEST_FREQUENCY_HZ }))
        .with_messaging(&router_host, router_port)
        .with_instance_id(TEST_INSTANCE_ID);

    let runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new()
            .with_config_path(&peppy_config_path)
            .standalone(standalone_config)
            .run(|_parameters: Parameters, node_runner| async move {
                node_runner
                    .announce_endpoint("panel", binding("http", "0.0.0.0:8765", ""))
                    .expect("a declared label is accepted");
                Ok(())
            })
    });

    let error = tokio::time::timeout(Duration::from_secs(10), runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect_err("a declared label left unannounced fails the run");
    assert!(
        matches!(error, PeppyError::EndpointNotAnnounced { ref label } if label == "viewer"),
        "{error}"
    );
    assert!(error.to_string().contains("`viewer`"), "{error}");
}

/// Polls `service` on the daemon-mode test node until it answers or the
/// runner exits, returning the reply payload.
async fn poll_test_node_until_answered(
    messenger: &peppylib::MessengerHandle,
    runner_task: &mut tokio::task::JoinHandle<Result<(), PeppyError>>,
    service: &str,
    request: Payload,
) -> Payload {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if runner_task.is_finished() {
            let result = runner_task.await.expect("runner task should not panic");
            panic!("runner exited early: {result:?}");
        }
        match poll_test_node(
            messenger,
            service,
            request.clone(),
            Duration::from_millis(500),
        )
        .await
        {
            Ok(response) => return response.payload(),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(err) => panic!("`{service}` never answered: {err}"),
        }
    }
}

/// In daemon mode a node whose manifest declares endpoints offers
/// `node_endpoints` once setup returned, answering with the sealed set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_runner_offers_node_endpoints_for_a_manifest_that_declares_them() {
    use peppylib::encoding::endpoints::{NodeEndpointsRequest, NodeEndpointsResponse};
    use peppylib::messaging::NODE_ENDPOINTS_SERVICE;

    let stack = start_daemon_stack(ENDPOINTS_PEPPY_CONFIG, None).await;

    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, node_runner| async move {
            node_runner
                .announce_endpoint("panel", binding("http", "0.0.0.0:8765", ""))
                .expect("declared");
            node_runner
                .announce_endpoint("viewer", binding("https", "[::]:8080", "/"))
                .expect("declared");
            Ok(())
        })
    });

    let messenger = stack.connect_control_messenger().await;
    let request = NodeEndpointsRequest::new()
        .encode()
        .expect("failed to encode endpoints request");
    let reply = poll_test_node_until_answered(
        &messenger,
        &mut runner_task,
        NODE_ENDPOINTS_SERVICE,
        request,
    )
    .await;
    let response = NodeEndpointsResponse::decode(&reply).expect("endpoints response decodes");
    let announced: Vec<(String, String, String, String)> = response
        .endpoints
        .into_iter()
        .map(|endpoint| {
            (
                endpoint.label,
                endpoint.binding.scheme,
                endpoint.binding.address.to_string(),
                endpoint.binding.path,
            )
        })
        .collect();
    assert_eq!(
        announced,
        [
            (
                "panel".to_string(),
                "http".to_string(),
                "0.0.0.0:8765".to_string(),
                String::new()
            ),
            (
                "viewer".to_string(),
                "https".to_string(),
                "[::]:8080".to_string(),
                "/".to_string()
            ),
        ]
    );

    let shutdown_response = poll_test_node(
        &messenger,
        SHUTDOWN_SERVICE,
        Payload::from_static(b"shutdown"),
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");
    assert_eq!(shutdown_response.instance_id(), TEST_INSTANCE_ID);
    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");
}

/// A node whose manifest declares no endpoint never offers `node_endpoints`:
/// once `node_health` answers (registered in the same post-setup step), the
/// endpoints service is still unreachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_runner_offers_no_node_endpoints_for_a_manifest_without_them() {
    use peppylib::messaging::NODE_ENDPOINTS_SERVICE;

    let stack = start_daemon_stack(TEST_NODE_PEPPY_CONFIG, None).await;

    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, _node_runner| async move { Ok(()) })
    });

    let messenger = stack.connect_control_messenger().await;
    let health_request = NodeHealthRequest::new()
        .encode()
        .expect("failed to encode health request");
    let reply = poll_test_node_until_answered(
        &messenger,
        &mut runner_task,
        NODE_HEALTH_SERVICE,
        health_request,
    )
    .await;
    NodeHealthResponse::decode(&reply).expect("health response should decode");

    let endpoints_reachable =
        is_test_node_service_reachable(&messenger, NODE_ENDPOINTS_SERVICE).await;
    assert!(
        !endpoints_reachable,
        "a node that declares no endpoint must not offer `node_endpoints`"
    );

    let shutdown_response = poll_test_node(
        &messenger,
        SHUTDOWN_SERVICE,
        Payload::from_static(b"shutdown"),
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");
    assert_eq!(shutdown_response.instance_id(), TEST_INSTANCE_ID);
    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_runner_succeed() {
    // Acquire the env guard to prevent races with daemon tests that set PEPPY_RUNTIME_CONFIG.
    // This ensures we run in standalone mode.
    let _env_guard = EnvAndDirGuard::new_standalone();

    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&peppy_config_path, TEST_NODE_PEPPY_CONFIG)
        .expect("failed to write peppy config");

    let standalone_config = StandaloneConfig::new()
        .with_parameters_json(serde_json::json!({ "frequency_hz": TEST_FREQUENCY_HZ }))
        .with_messaging(&router_host, router_port)
        .with_instance_id(TEST_INSTANCE_ID);

    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<CancellationToken>();
    let runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new()
            .with_config_path(&peppy_config_path)
            .standalone(standalone_config)
            .run(|parameters: Parameters, node_runner| async move {
                assert_eq!(parameters.frequency_hz, TEST_FREQUENCY_HZ);
                let _ = setup_tx.send(node_runner.cancellation_token().clone());
                Ok(())
            })
    });

    let cancellation_token = tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");

    cancellation_token.cancel();

    tokio::time::timeout(Duration::from_secs(10), runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");
}

/// `init_standalone` must not be hijacked into daemon mode by an inherited
/// `PEPPY_RUNTIME_CONFIG`: with the variable pointing at a nonexistent runtime
/// config (which would make `init()` fail resolving daemon mode), the context
/// still comes up standalone with the provided config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_standalone_ignores_inherited_runtime_config_env() {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&peppy_config_path, TEST_NODE_PEPPY_CONFIG)
        .expect("failed to write peppy config");

    // Point the daemon-mode variable at a config that does not exist; the
    // guard holds the env lock so parallel tests cannot race the variable.
    let bogus_runtime_config = temp_dir.path().join("does_not_exist.json5");
    let _env_guard = EnvAndDirGuard::new(temp_dir.path(), &bogus_runtime_config);

    let standalone_config = StandaloneConfig::new()
        .with_parameters_json(serde_json::json!({ "frequency_hz": TEST_FREQUENCY_HZ }))
        .with_instance_id(TEST_INSTANCE_ID);

    let mut context = NodeBuilder::new()
        .with_config_path(&peppy_config_path)
        .standalone(standalone_config)
        .init_standalone()
        .expect("init_standalone must succeed despite PEPPY_RUNTIME_CONFIG being set");
    assert!(context.is_standalone());
    assert!(!context.is_daemon());
    assert_eq!(context.instance_id(), TEST_INSTANCE_ID);
    let parameters: Parameters = context.take_parameters().expect("parameters should parse");
    assert_eq!(parameters.frequency_hz, TEST_FREQUENCY_HZ);

    // `init()` under the same environment resolves daemon mode and fails on
    // the bogus config: the exact hijack init_standalone exists to avoid.
    let daemon_attempt = NodeBuilder::<Parameters>::new()
        .with_config_path(&peppy_config_path)
        .init();
    assert!(
        daemon_attempt.is_err(),
        "init() should have been hijacked into daemon mode and failed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_ready_but_not_healthy() {
    let stack = start_daemon_stack(TEST_NODE_PEPPY_CONFIG, None).await;

    let (setup_started_tx, setup_started_rx) = tokio::sync::oneshot::channel::<()>();
    let (setup_continue_tx, setup_continue_rx) = tokio::sync::oneshot::channel::<()>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, _node_runner| async move {
            let _ = setup_started_tx.send(());
            let _ = setup_continue_rx.await;
            Ok(())
        })
    });

    tokio::time::timeout(Duration::from_secs(5), setup_started_rx)
        .await
        .expect("runner setup should start")
        .expect("setup start signal should be sent");

    let messenger = stack.connect_control_messenger().await;

    wait_until_reachable(
        &messenger,
        &mut runner_task,
        NODE_READY_SERVICE,
        "ready service did not become reachable",
    )
    .await;

    let ready_payload = Payload::from_static(b"ready");
    let ready_response = poll_test_node(
        &messenger,
        NODE_READY_SERVICE,
        ready_payload.clone(),
        Duration::from_secs(2),
    )
    .await
    .expect("ready service should respond while setup is blocked");
    assert_eq!(ready_response.payload(), &ready_payload);
    assert_eq!(ready_response.instance_id(), TEST_INSTANCE_ID);
    wait_until_reachable(
        &messenger,
        &mut runner_task,
        SHUTDOWN_SERVICE,
        "shutdown service should be reachable while setup is blocked",
    )
    .await;

    assert!(
        !is_test_node_service_reachable(&messenger, NODE_HEALTH_SERVICE).await,
        "health service should not be reachable while setup is blocked"
    );

    let health_request = NodeHealthRequest::new()
        .encode()
        .expect("failed to encode health request");
    let health_err = poll_test_node(
        &messenger,
        NODE_HEALTH_SERVICE,
        health_request.clone(),
        Duration::from_millis(200),
    )
    .await
    .expect_err("health service should not respond while setup is blocked");

    match health_err {
        peppylib::PeppyError::ServiceUnreachable { service_name, .. }
        | peppylib::PeppyError::ServiceTimeout { service_name, .. } => {
            assert_eq!(service_name, NODE_HEALTH_SERVICE);
        }
        other => panic!("unexpected health error: {other:?}"),
    }

    let _ = setup_continue_tx.send(());

    wait_until_reachable(
        &messenger,
        &mut runner_task,
        NODE_HEALTH_SERVICE,
        "health service did not become reachable",
    )
    .await;

    let health_response = poll_test_node(
        &messenger,
        NODE_HEALTH_SERVICE,
        health_request,
        Duration::from_secs(2),
    )
    .await
    .expect("health service should respond after setup completes");
    NodeHealthResponse::decode(&health_response.payload()).expect("health response should decode");

    let shutdown_payload = Payload::from_static(b"shutdown");
    let shutdown_response = poll_test_node(
        &messenger,
        SHUTDOWN_SERVICE,
        shutdown_payload.clone(),
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");

    assert_eq!(shutdown_response.payload(), &shutdown_payload);
    assert_eq!(shutdown_response.instance_id(), TEST_INSTANCE_ID);

    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_cancellation_token_cancelled_on_shutdown() {
    let stack = start_daemon_stack(TEST_NODE_PEPPY_CONFIG, None).await;

    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<CancellationToken>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, node_runner| async move {
            let _ = setup_tx.send(node_runner.cancellation_token().clone());
            Ok(())
        })
    });

    let cancellation_token = tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");

    assert!(
        !cancellation_token.is_cancelled(),
        "cancellation token should not be cancelled before shutdown request"
    );

    let messenger = stack.connect_control_messenger().await;

    wait_until_reachable(
        &messenger,
        &mut runner_task,
        SHUTDOWN_SERVICE,
        "shutdown service did not become reachable",
    )
    .await;

    let shutdown_payload = Payload::from_static(b"shutdown");
    poll_test_node(
        &messenger,
        SHUTDOWN_SERVICE,
        shutdown_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");

    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");

    assert!(
        cancellation_token.is_cancelled(),
        "cancellation token should be cancelled after shutdown request"
    );
}

/// A shutdown that arrives while `setup_fn` is still running must cancel the
/// cancellation token and exit the node without waiting for setup to finish.
/// Exercises the during-setup select arm of `run_with_closure`; the post-setup
/// arm is covered by `daemon_cancellation_token_cancelled_on_shutdown`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_during_setup_cancels_token_and_exits() {
    let stack = start_daemon_stack(TEST_NODE_PEPPY_CONFIG, None).await;

    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<CancellationToken>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, node_runner| async move {
            let _ = setup_tx.send(node_runner.cancellation_token().clone());
            // Block setup forever: the shutdown must interrupt it, not wait
            // for it to complete.
            std::future::pending::<()>().await;
            Ok(())
        })
    });

    // Wait until the node is inside setup_fn and grab the cancellation token
    let cancellation_token = tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should start")
        .expect("runner setup signal should be sent");

    assert!(
        !cancellation_token.is_cancelled(),
        "cancellation token should not be cancelled before shutdown request"
    );

    let messenger = stack.connect_control_messenger().await;

    // The shutdown service is a pre-setup service, so it must be reachable
    // while setup_fn is still blocked
    wait_until_reachable(
        &messenger,
        &mut runner_task,
        SHUTDOWN_SERVICE,
        "shutdown service did not become reachable",
    )
    .await;

    // Send the shutdown request while setup_fn is still blocked
    let shutdown_payload = Payload::from_static(b"shutdown");
    poll_test_node(
        &messenger,
        SHUTDOWN_SERVICE,
        shutdown_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");

    // The runner must exit even though setup_fn never completed
    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit while setup is still blocked")
        .expect("runner task should not panic")
        .expect("runner should return Ok");

    assert!(
        cancellation_token.is_cancelled(),
        "cancellation token should be cancelled by a shutdown received during setup"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_runner_exposes_messenger_and_metadata() {
    let _env_guard = EnvAndDirGuard::new_standalone();

    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&peppy_config_path, TEST_NODE_PEPPY_CONFIG)
        .expect("failed to write peppy config");

    let standalone_config = StandaloneConfig::new()
        .with_parameters_json(serde_json::json!({ "frequency_hz": TEST_FREQUENCY_HZ }))
        .with_messaging(&router_host, router_port)
        .with_instance_id(TEST_INSTANCE_ID)
        .with_node_name(TEST_NODE_NAME);

    struct RunnerMetadata {
        bound_core_node: String,
        bound_instance_id: String,
        node_name: String,
        messaging_port: u16,
        cancellation_token: CancellationToken,
    }

    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<RunnerMetadata>();
    let runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new()
            .with_config_path(&peppy_config_path)
            .standalone(standalone_config)
            .run(|_parameters: Parameters, node_runner| async move {
                let _ = setup_tx.send(RunnerMetadata {
                    bound_core_node: node_runner.processor().bound_core_node().to_string(),
                    bound_instance_id: node_runner.processor().bound_instance_id().to_string(),
                    node_name: node_runner.processor().node_name().to_string(),
                    messaging_port: node_runner.messenger().messaging_port().await,
                    cancellation_token: node_runner.cancellation_token().clone(),
                });
                Ok(())
            })
    });

    let metadata = tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");

    assert_eq!(metadata.bound_core_node, "standalone-core");
    assert_eq!(metadata.bound_instance_id, TEST_INSTANCE_ID);
    assert_eq!(metadata.node_name, TEST_NODE_NAME);
    assert_eq!(metadata.messaging_port, router_port);

    metadata.cancellation_token.cancel();

    tokio::time::timeout(Duration::from_secs(10), runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");
}

/// Polls until the node's `SHUTDOWN_SERVICE` is reachable (panicking if the
/// runner exits first), then sends the shutdown request: the same in-band ask
/// `peppy node stop` delivers.
async fn send_shutdown_when_reachable<T: std::fmt::Debug>(
    stack: &DaemonStack,
    runner_task: &mut tokio::task::JoinHandle<T>,
) {
    let messenger = stack.connect_control_messenger().await;

    wait_until_reachable(
        &messenger,
        runner_task,
        SHUTDOWN_SERVICE,
        "shutdown service did not become reachable",
    )
    .await;

    poll_test_node(
        &messenger,
        SHUTDOWN_SERVICE,
        Payload::from_static(b"shutdown"),
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");
}

/// Cleanup registered via `on_shutdown` runs to completion, in reverse
/// registration order, with the messenger still usable, before `run()`
/// returns on an in-band shutdown (`peppy node stop`). A `run()` that returned
/// as soon as the token was cancelled would drop the runtime under the cleanup
/// (the `ds_lock_probe` leak).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_awaits_hooks_lifo_before_exit() {
    let stack = start_daemon_stack(TEST_NODE_PEPPY_CONFIG, None).await;

    let order: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let order_first = std::sync::Arc::clone(&order);
    let order_second = std::sync::Arc::clone(&order);
    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<()>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, node_runner| async move {
            node_runner.on_shutdown(async move {
                order_first.lock().expect("order lock").push("first".into());
            });
            let runner_for_hook = std::sync::Arc::clone(&node_runner);
            node_runner.on_shutdown(async move {
                // Prove the hook is awaited (not raced) and that messaging is
                // still alive while hooks run, as `datastore::remove` needs.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let port = runner_for_hook.messenger().messaging_port().await;
                order_second
                    .lock()
                    .expect("order lock")
                    .push(format!("second:port_{}", port != 0));
            });
            let _ = setup_tx.send(());
            Ok(())
        })
    });

    tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");

    send_shutdown_when_reachable(&stack, &mut runner_task).await;

    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");

    assert_eq!(
        *order.lock().expect("order lock"),
        vec!["second:port_true".to_string(), "first".to_string()],
        "hooks must all have completed before run() returned, last registered first"
    );
}

/// A shutdown that interrupts `setup_fn` must still run the hooks registered
/// up to that point (e.g. an instance lock stored early in setup).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_during_setup_runs_registered_hooks() {
    let stack = start_daemon_stack(TEST_NODE_PEPPY_CONFIG, None).await;

    let (hook_tx, hook_rx) = tokio::sync::oneshot::channel::<()>();
    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<()>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, node_runner| async move {
            node_runner.on_shutdown(async move {
                let _ = hook_tx.send(());
            });
            let _ = setup_tx.send(());
            // Block setup forever: the shutdown must interrupt it and still
            // run the hook above.
            std::future::pending::<()>().await;
            Ok(())
        })
    });

    tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should start")
        .expect("setup start signal should be sent");

    send_shutdown_when_reachable(&stack, &mut runner_task).await;

    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit while setup is still blocked")
        .expect("runner task should not panic")
        .expect("runner should return Ok");

    hook_rx
        .await
        .expect("shutdown hook registered during setup should have run");
}

/// Hooks share one grace window (`lifecycle.shutdown_grace_secs` from the
/// launch config): a hook that hangs is abandoned at the deadline and `run()`
/// still returns, so stuck cleanup can never wedge a stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_hooks_are_bounded_by_grace_window() {
    let stack = start_daemon_stack(TEST_NODE_PEPPY_CONFIG, Some(1)).await;

    let order: std::sync::Arc<Mutex<Vec<&'static str>>> =
        std::sync::Arc::new(Mutex::new(Vec::new()));
    let order_never = std::sync::Arc::clone(&order);
    let order_stuck = std::sync::Arc::clone(&order);
    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<()>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, node_runner| async move {
            // LIFO: registered first, so it only runs after the stuck hook
            // below, which the grace window never lets finish.
            node_runner.on_shutdown(async move {
                order_never.lock().expect("order lock").push("never");
            });
            node_runner.on_shutdown(async move {
                order_stuck.lock().expect("order lock").push("stuck-start");
                tokio::time::sleep(Duration::from_secs(600)).await;
                order_stuck.lock().expect("order lock").push("stuck-end");
            });
            let _ = setup_tx.send(());
            Ok(())
        })
    });

    tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");

    send_shutdown_when_reachable(&stack, &mut runner_task).await;

    // Must exit shortly after the 1s grace window, not after the 600s sleep.
    tokio::time::timeout(Duration::from_secs(15), &mut runner_task)
        .await
        .expect("runner should exit once the grace window elapses")
        .expect("runner task should not panic")
        .expect("runner should return Ok");

    assert_eq!(
        *order.lock().expect("order lock"),
        vec!["stuck-start"],
        "the stuck hook must be cut off mid-await and later hooks skipped"
    );
}

/// SIGINT converges on the same path as `peppy node stop`: token cancelled,
/// hooks awaited, clean exit, no node-side signal handling required.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_sigint_runs_hooks_and_exits() {
    // Held for scope: the env guard and router must outlive the runner.
    let _stack = start_daemon_stack(TEST_NODE_PEPPY_CONFIG, None).await;

    // Safety net: install a process-wide SIGINT handler in the test runtime
    // before raising, so the raise below can never kill the test process even
    // if the node's own bridge has not registered yet.
    let _sigint_guard = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("test sigint handler should install");

    let (hook_tx, hook_rx) = tokio::sync::oneshot::channel::<()>();
    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<()>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, node_runner| async move {
            node_runner.on_shutdown(async move {
                let _ = hook_tx.send(());
            });
            let _ = setup_tx.send(());
            Ok(())
        })
    });

    tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");

    // Give the node's signal bridge a moment to register its handler, then
    // deliver a process-directed SIGINT, as a terminal Ctrl+C would.
    tokio::time::sleep(Duration::from_millis(300)).await;
    // SAFETY: plain kill(2) targeting our own pid with a handled signal.
    unsafe {
        libc::kill(std::process::id() as i32, libc::SIGINT);
    }

    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit after SIGINT")
        .expect("runner task should not panic")
        .expect("runner should return Ok");

    hook_rx
        .await
        .expect("shutdown hook should have run on SIGINT");
}

/// Standalone mode awaits hooks on cancellation too (it has no daemon, so the
/// grace window is the built-in default).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_cancel_awaits_hooks_before_exit() {
    let _env_guard = EnvAndDirGuard::new_standalone();

    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&peppy_config_path, TEST_NODE_PEPPY_CONFIG)
        .expect("failed to write peppy config");

    let standalone_config = StandaloneConfig::new()
        .with_parameters_json(serde_json::json!({ "frequency_hz": TEST_FREQUENCY_HZ }))
        .with_messaging(&router_host, router_port)
        .with_instance_id(TEST_INSTANCE_ID);

    let (hook_tx, hook_rx) = tokio::sync::oneshot::channel::<()>();
    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<CancellationToken>();
    let runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new()
            .with_config_path(&peppy_config_path)
            .standalone(standalone_config)
            .run(|_parameters: Parameters, node_runner| async move {
                node_runner.on_shutdown(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = hook_tx.send(());
                });
                let _ = setup_tx.send(node_runner.cancellation_token().clone());
                Ok(())
            })
    });

    let cancellation_token = tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");

    cancellation_token.cancel();

    tokio::time::timeout(Duration::from_secs(10), runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");

    hook_rx
        .await
        .expect("shutdown hook should have run before standalone exit");
}

// `NodeBuilder::init()` parses parameters eagerly and `take_parameters()` is
// take-once. These cases exercise that behavior through the real builder, so
// they run in standalone mode via `EnvAndDirGuard::new_standalone()`, which
// clears `PEPPY_RUNTIME_CONFIG` and serializes against the daemon tests that
// set it. They live here rather than as in-crate unit tests because forcing
// standalone needs to control the process environment, and env mutation is
// `unsafe` while the library crate is `#![deny(unsafe_code)]`.

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ValueParam {
    value: i64,
}

/// Write a minimal node manifest with a single parameter `value` of the given
/// type (for example `"i64"`). Returns the manifest path.
fn write_value_param_config(dir: &Path, parameter_type: &str) -> PathBuf {
    let path = dir.join(NODE_CONFIG_FILE);
    let content = format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{ name: "test_node", tag: "v1" }},
            execution: {{ language: "rust", parameters: {{ value: "{parameter_type}" }}, run_cmd: ["./test"] }},
        }}"#,
    );
    std::fs::write(&path, content).expect("peppy config should be written");
    path
}

#[test]
fn init_parameters_can_only_be_taken_once() {
    let _env_guard = EnvAndDirGuard::new_standalone();

    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let peppy_config = write_value_param_config(temp_dir.path(), "i64");

    let config = StandaloneConfig::new().with_parameters_json(serde_json::json!({ "value": 42 }));

    let mut ctx = NodeBuilder::<ValueParam>::new()
        .with_config_path(&peppy_config)
        .standalone(config)
        .init()
        .expect("init should succeed");

    let params = ctx.take_parameters().expect("first take should succeed");
    assert_eq!(params.value, 42);

    let err = ctx.take_parameters().expect_err("second take should fail");
    assert!(
        matches!(err, PeppyError::ParametersAlreadyTaken),
        "expected ParametersAlreadyTaken, got: {err:?}"
    );
}

#[test]
fn init_fails_eagerly_on_invalid_parameter_types() {
    let _env_guard = EnvAndDirGuard::new_standalone();

    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let peppy_config = write_value_param_config(temp_dir.path(), "i64");

    // Provide a string where i64 is expected; this must fail at init(), not
    // when parameters are later read.
    let config = StandaloneConfig::new()
        .with_parameters_json(serde_json::json!({ "value": "not_a_number" }));

    let Err(err) = NodeBuilder::<ValueParam>::new()
        .with_config_path(&peppy_config)
        .standalone(config)
        .init()
    else {
        panic!("init should fail with type mismatch");
    };

    let err_string = err.to_string();
    assert!(
        err_string.contains("value"),
        "error should mention the invalid parameter, got: {err_string}"
    );
}

#[test]
fn init_parses_parameters_eagerly() {
    let _env_guard = EnvAndDirGuard::new_standalone();

    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let peppy_config = write_value_param_config(temp_dir.path(), "i64");

    let config = StandaloneConfig::new().with_parameters_json(serde_json::json!({ "value": 99 }));

    let mut ctx = NodeBuilder::<ValueParam>::new()
        .with_config_path(&peppy_config)
        .standalone(config)
        .init()
        .expect("init should succeed");

    let params = ctx.take_parameters().expect("should take parameters");
    assert_eq!(params.value, 99);
}

/// The CLI sets `PEPPY_RUNTIME_CONFIG` for every node it launches, so a set
/// value selects daemon mode even when it is not UTF-8. Daemon mode then
/// reports the unreadable value instead of the node running standalone
/// against the default router.
#[cfg(unix)]
#[test]
fn init_takes_a_non_utf8_runtime_config_as_daemon_mode() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let peppy_config = write_value_param_config(temp_dir.path(), "i64");
    let non_utf8_runtime_config = Path::new(OsStr::from_bytes(b"/tmp/\xffruntime.json5"));
    let _env_guard = EnvAndDirGuard::new(temp_dir.path(), non_utf8_runtime_config);

    // A standalone config with valid parameters, so a standalone init would
    // succeed.
    let config = StandaloneConfig::new().with_parameters_json(serde_json::json!({ "value": 7 }));

    let Err(err) = NodeBuilder::<ValueParam>::new()
        .with_config_path(&peppy_config)
        .standalone(config)
        .init()
    else {
        panic!("init should select daemon mode and fail on the non-UTF-8 value");
    };

    assert!(
        matches!(err, PeppyError::MissingInstanceIdEnvVar { .. }),
        "expected MissingInstanceIdEnvVar, got: {err:?}"
    );
}

/// The slot helpers that generated code splices refuse a link_id the manifest
/// declares no slot for, naming the slot kind and the link_id they were given.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slot_helpers_name_an_undeclared_slot() {
    let router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let peppy_config = write_value_param_config(temp_dir.path(), "i64");
    let standalone_config = StandaloneConfig::new()
        .with_parameters_json(serde_json::json!({ "value": 1 }))
        .with_messaging(&router.host, router.port);
    let processor = peppylib::runtime::Processor::new_standalone(&peppy_config, &standalone_config)
        .expect("standalone processor");
    let node_runner = peppylib::runtime::NodeRunner::new(processor)
        .await
        .expect("node runner");

    let Err(subscribe_error) = peppylib::runtime::subscribe_peer(
        &node_runner,
        "missing",
        "test_pairing",
        "v1",
        "topic",
        config::node::QoSProfile::Reliable,
    )
    .await
    else {
        panic!("subscribe_peer must refuse an undeclared pairing slot");
    };
    assert!(
        matches!(&subscribe_error, PeppyError::UnknownPairingSlot { link_id } if link_id == "missing"),
        "expected UnknownPairingSlot for `missing`, got: {subscribe_error:?}"
    );

    let Err(publisher_error) = peppylib::runtime::declare_peer_publisher(
        &node_runner,
        "missing",
        "test_pairing",
        "v1",
        "topic",
        config::node::QoSProfile::Reliable,
    ) else {
        panic!("declare_peer_publisher must refuse an undeclared pairing slot");
    };
    assert!(
        matches!(&publisher_error, PeppyError::UnknownPairingSlot { link_id } if link_id == "missing"),
        "expected UnknownPairingSlot for `missing`, got: {publisher_error:?}"
    );

    let Err(observe_error) = peppylib::runtime::subscribe_observed(
        &node_runner,
        "missing",
        "test_pairing",
        "v1",
        "topic",
        config::node::QoSProfile::Reliable,
    )
    .await
    else {
        panic!("subscribe_observed must refuse an undeclared observer slot");
    };
    assert!(
        matches!(&observe_error, PeppyError::UnknownObservationSlot { link_id } if link_id == "missing"),
        "expected UnknownObservationSlot for `missing`, got: {observe_error:?}"
    );
}

/// A node under a daemon reads the setup budget the daemon wrote into its
/// launch config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_node_reads_the_setup_budget_of_its_launch_config() {
    let stack = start_daemon_stack_with_lifecycle(TEST_NODE_PEPPY_CONFIG, |lifecycle| {
        lifecycle.setup_timeout_secs =
            config::node::SetupTimeout::from_secs(180).expect("in range");
    })
    .await;

    let (budget_tx, budget_rx) = tokio::sync::oneshot::channel::<Duration>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, node_runner| async move {
            let _ = budget_tx.send(node_runner.setup_timeout());
            Ok(())
        })
    });

    let budget = tokio::time::timeout(Duration::from_secs(5), budget_rx)
        .await
        .expect("runner setup should run")
        .expect("the setup budget should be sent");
    assert_eq!(budget, Duration::from_secs(180));

    send_shutdown_when_reachable(&stack, &mut runner_task).await;
    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");
}

/// A node that answered ready answers the shutdown request, and the shutdown
/// ends a setup that waits forever, with its hook run: this is how the daemon
/// stops a starting instance that answered ready. The order in which the
/// runtime declares the two services, the shutdown request first, is checked
/// by `the_shutdown_request_is_served_before_the_ready_signal` in the
/// runtime's unit tests, on a mock session that records it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_answers_ready_answers_the_shutdown_request() {
    let stack = start_daemon_stack(TEST_NODE_PEPPY_CONFIG, None).await;

    let (hook_tx, hook_rx) = tokio::sync::oneshot::channel::<()>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, node_runner| async move {
            node_runner.on_shutdown(async move {
                let _ = hook_tx.send(());
            });
            std::future::pending::<()>().await;
            Ok(())
        })
    });

    let messenger = stack.connect_control_messenger().await;

    // The daemon's ready wait: poll until the node answers.
    poll_test_node_until_answered(
        &messenger,
        &mut runner_task,
        NODE_READY_SERVICE,
        Payload::from_static(b"ready"),
    )
    .await;

    assert!(
        is_test_node_service_reachable(&messenger, SHUTDOWN_SERVICE).await,
        "a node that answered ready must already serve the shutdown request"
    );
    poll_test_node(
        &messenger,
        SHUTDOWN_SERVICE,
        Payload::from_static(b"shutdown"),
        Duration::from_secs(2),
    )
    .await
    .expect("the shutdown request should be answered");

    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit while its setup still waits")
        .expect("runner task should not panic")
        .expect("runner should return Ok");
    hook_rx
        .await
        .expect("the shutdown hook registered during setup should have run");
}
