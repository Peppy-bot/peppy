use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::launcher::Name;
use config::runtime::{NodeInstanceConfig, RuntimeConfig};
use peppylib::encoding::health::{NodeHealthRequest, NodeHealthResponse};
use peppylib::messaging::{NODE_HEALTH_SERVICE, NODE_READY_SERVICE, SHUTDOWN_SERVICE};
use peppylib::runtime::CancellationToken;
use peppylib::runtime::NodeBuilder;
use peppylib::types::Payload;
use pmi::ZenohAdapter;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const TEST_CORE_NODE: &str = "test_core";
const TEST_NODE_NAME: &str = "test_node";
const TEST_INSTANCE_ID: &str = "test_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const TEST_FREQUENCY_HZ: f64 = 10.0;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct Parameters {
    frequency_hz: f64,
}

struct EnvAndDirGuard {
    previous_runtime_config: Option<String>,
    previous_dir: PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

impl EnvAndDirGuard {
    fn new(temp_dir: &Path, runtime_config_path: &Path) -> Self {
        let lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned by a previous test panic");

        let previous_runtime_config = std::env::var(peppylib::config::RUNTIME_CONFIG_VAR_NAME).ok();
        let previous_dir = std::env::current_dir().expect("current dir should be readable");

        // SAFETY: environment mutation is guarded by a global mutex to avoid races.
        unsafe {
            std::env::set_var(
                peppylib::config::RUNTIME_CONFIG_VAR_NAME,
                runtime_config_path,
            )
        };
        std::env::set_current_dir(temp_dir).expect("set_current_dir should succeed");

        Self {
            previous_runtime_config,
            previous_dir,
            _lock: lock,
        }
    }

    /// Create a guard that ensures the runtime config env var is NOT set.
    /// Used by standalone tests to prevent races with daemon tests.
    fn new_standalone() -> Self {
        let lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned by a previous test panic");

        let previous_runtime_config = std::env::var(peppylib::config::RUNTIME_CONFIG_VAR_NAME).ok();
        let previous_dir = std::env::current_dir().expect("current dir should be readable");

        // SAFETY: environment mutation is guarded by a global mutex to avoid races.
        // Remove the env var to ensure standalone mode is used.
        unsafe {
            std::env::remove_var(peppylib::config::RUNTIME_CONFIG_VAR_NAME);
        };

        Self {
            previous_runtime_config,
            previous_dir,
            _lock: lock,
        }
    }
}

impl Drop for EnvAndDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous_dir).expect("restore current dir");
        // SAFETY: environment mutation is guarded by a global mutex to avoid races.
        unsafe {
            match &self.previous_runtime_config {
                Some(value) => std::env::set_var(peppylib::config::RUNTIME_CONFIG_VAR_NAME, value),
                None => std::env::remove_var(peppylib::config::RUNTIME_CONFIG_VAR_NAME),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_runner_succeed() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(peppylib::config::NODE_CONFIG_FILE);
    let peppy_config = r#"{
      peppy_schema: "node_v1",
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
    std::fs::write(&peppy_config_path, peppy_config).expect("failed to write peppy config");
    config::fingerprint::create_codegen_fingerprint(
        &peppy_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig {
            instance_id: Name::new(TEST_INSTANCE_ID).expect("instance id should be valid"),
            arguments: serde_json5::from_str(&format!("{{ frequency_hz: {TEST_FREQUENCY_HZ} }}"))
                .expect("runtime args should parse"),
            framework: Default::default(),
        },
        TEST_NODE_NAME,
        TEST_CORE_NODE,
    )
    .expect("runtime config should build");
    let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
    runtime_config
        .save_json5_launch_config(&runtime_config_path)
        .expect("failed to write runtime config");

    let _env_guard = EnvAndDirGuard::new(temp_dir.path(), &runtime_config_path);

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

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if runner_task.is_finished() {
            let result = runner_task.await.expect("runner task should not panic");
            panic!("runner exited early: {result:?}");
        }

        if peppylib::ServiceMessenger::is_reachable(
            &messenger,
            TEST_CORE_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            SHUTDOWN_SERVICE,
            Some(TEST_CORE_NODE),
            Some(TEST_INSTANCE_ID),
        )
        .await
        .expect("reachability check should succeed")
        {
            break;
        }

        if Instant::now() >= deadline {
            panic!("shutdown service did not become reachable");
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let health_request = NodeHealthRequest::new()
        .encode()
        .expect("failed to encode health request");
    let health_response = peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        NODE_HEALTH_SERVICE,
        Some(TEST_CORE_NODE),
        Some(TEST_INSTANCE_ID),
        health_request,
        Duration::from_secs(2),
    )
    .await
    .expect("health service should respond");
    NodeHealthResponse::decode(&health_response.payload()).expect("health response should decode");

    let shutdown_payload = Payload::from_static(b"shutdown");
    let shutdown_response = peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        SHUTDOWN_SERVICE,
        Some(TEST_CORE_NODE),
        Some(TEST_INSTANCE_ID),
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
async fn standalone_runner_succeed() {
    use peppylib::runtime::CancellationToken;

    // Acquire the env guard to prevent races with daemon tests that set PEPPY_RUNTIME_CONFIG.
    // This ensures we run in standalone mode.
    let _env_guard = EnvAndDirGuard::new_standalone();

    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(peppylib::config::NODE_CONFIG_FILE);
    let peppy_config = r#"{
      peppy_schema: "node_v1",
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
    std::fs::write(&peppy_config_path, peppy_config).expect("failed to write peppy config");

    let standalone_config = peppylib::runtime::StandaloneConfig::new()
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

    // Wait for setup to complete and get the cancellation token
    let cancellation_token = tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");

    // Signal shutdown via cancellation token
    cancellation_token.cancel();

    // Runner should exit after cancellation
    tokio::time::timeout(Duration::from_secs(10), runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_ready_but_not_healthy() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(peppylib::config::NODE_CONFIG_FILE);
    let peppy_config = r#"{
      peppy_schema: "node_v1",
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
    std::fs::write(&peppy_config_path, peppy_config).expect("failed to write peppy config");
    config::fingerprint::create_codegen_fingerprint(
        &peppy_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig {
            instance_id: Name::new(TEST_INSTANCE_ID).expect("instance id should be valid"),
            arguments: serde_json5::from_str(&format!("{{ frequency_hz: {TEST_FREQUENCY_HZ} }}"))
                .expect("runtime args should parse"),
            framework: Default::default(),
        },
        TEST_NODE_NAME,
        TEST_CORE_NODE,
    )
    .expect("runtime config should build");
    let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
    runtime_config
        .save_json5_launch_config(&runtime_config_path)
        .expect("failed to write runtime config");

    let _env_guard = EnvAndDirGuard::new(temp_dir.path(), &runtime_config_path);

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

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if runner_task.is_finished() {
            let result = runner_task.await.expect("runner task should not panic");
            panic!("runner exited early: {result:?}");
        }

        if peppylib::ServiceMessenger::is_reachable(
            &messenger,
            TEST_CORE_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            NODE_READY_SERVICE,
            Some(TEST_CORE_NODE),
            Some(TEST_INSTANCE_ID),
        )
        .await
        .expect("reachability check should succeed")
        {
            break;
        }

        if Instant::now() >= deadline {
            panic!("ready service did not become reachable");
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let ready_payload = Payload::from_static(b"ready");
    let ready_response = peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        NODE_READY_SERVICE,
        Some(TEST_CORE_NODE),
        Some(TEST_INSTANCE_ID),
        ready_payload.clone(),
        Duration::from_secs(2),
    )
    .await
    .expect("ready service should respond while setup is blocked");
    assert_eq!(ready_response.payload(), &ready_payload);
    assert_eq!(ready_response.instance_id(), TEST_INSTANCE_ID);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if runner_task.is_finished() {
            let result = runner_task.await.expect("runner task should not panic");
            panic!("runner exited early: {result:?}");
        }

        if peppylib::ServiceMessenger::is_reachable(
            &messenger,
            TEST_CORE_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            SHUTDOWN_SERVICE,
            Some(TEST_CORE_NODE),
            Some(TEST_INSTANCE_ID),
        )
        .await
        .expect("reachability check should succeed")
        {
            break;
        }

        if Instant::now() >= deadline {
            panic!("shutdown service should be reachable while setup is blocked");
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        !peppylib::ServiceMessenger::is_reachable(
            &messenger,
            TEST_CORE_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            NODE_HEALTH_SERVICE,
            Some(TEST_CORE_NODE),
            Some(TEST_INSTANCE_ID),
        )
        .await
        .expect("reachability check should succeed"),
        "health service should not be reachable while setup is blocked"
    );

    let health_request = NodeHealthRequest::new()
        .encode()
        .expect("failed to encode health request");
    let health_err = peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        NODE_HEALTH_SERVICE,
        Some(TEST_CORE_NODE),
        Some(TEST_INSTANCE_ID),
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

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if runner_task.is_finished() {
            let result = runner_task.await.expect("runner task should not panic");
            panic!("runner exited early: {result:?}");
        }

        if peppylib::ServiceMessenger::is_reachable(
            &messenger,
            TEST_CORE_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            NODE_HEALTH_SERVICE,
            Some(TEST_CORE_NODE),
            Some(TEST_INSTANCE_ID),
        )
        .await
        .expect("reachability check should succeed")
        {
            break;
        }

        if Instant::now() >= deadline {
            panic!("health service did not become reachable");
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let health_response = peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        NODE_HEALTH_SERVICE,
        Some(TEST_CORE_NODE),
        Some(TEST_INSTANCE_ID),
        health_request,
        Duration::from_secs(2),
    )
    .await
    .expect("health service should respond after setup completes");
    NodeHealthResponse::decode(&health_response.payload()).expect("health response should decode");

    let shutdown_payload = Payload::from_static(b"shutdown");
    let shutdown_response = peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        SHUTDOWN_SERVICE,
        Some(TEST_CORE_NODE),
        Some(TEST_INSTANCE_ID),
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
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(peppylib::config::NODE_CONFIG_FILE);
    let peppy_config = r#"{
      peppy_schema: "node_v1",
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
    std::fs::write(&peppy_config_path, peppy_config).expect("failed to write peppy config");
    config::fingerprint::create_codegen_fingerprint(
        &peppy_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig {
            instance_id: Name::new(TEST_INSTANCE_ID).expect("instance id should be valid"),
            arguments: serde_json5::from_str(&format!("{{ frequency_hz: {TEST_FREQUENCY_HZ} }}"))
                .expect("runtime args should parse"),
            framework: Default::default(),
        },
        TEST_NODE_NAME,
        TEST_CORE_NODE,
    )
    .expect("runtime config should build");
    let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
    runtime_config
        .save_json5_launch_config(&runtime_config_path)
        .expect("failed to write runtime config");

    let _env_guard = EnvAndDirGuard::new(temp_dir.path(), &runtime_config_path);

    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<CancellationToken>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        NodeBuilder::new().run(|_parameters: Parameters, node_runner| async move {
            let _ = setup_tx.send(node_runner.cancellation_token().clone());
            Ok(())
        })
    });

    // Wait for setup to complete and get the cancellation token
    let cancellation_token = tokio::time::timeout(Duration::from_secs(5), setup_rx)
        .await
        .expect("runner setup should complete")
        .expect("runner setup signal should be sent");

    // Verify the token is NOT cancelled before shutdown
    assert!(
        !cancellation_token.is_cancelled(),
        "cancellation token should not be cancelled before shutdown request"
    );

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger");

    // Wait for shutdown service to become reachable
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if runner_task.is_finished() {
            let result = runner_task.await.expect("runner task should not panic");
            panic!("runner exited early: {result:?}");
        }

        if peppylib::ServiceMessenger::is_reachable(
            &messenger,
            TEST_CORE_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            SHUTDOWN_SERVICE,
            Some(TEST_CORE_NODE),
            Some(TEST_INSTANCE_ID),
        )
        .await
        .expect("reachability check should succeed")
        {
            break;
        }

        if Instant::now() >= deadline {
            panic!("shutdown service did not become reachable");
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Send shutdown request
    let shutdown_payload = Payload::from_static(b"shutdown");
    peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        SHUTDOWN_SERVICE,
        Some(TEST_CORE_NODE),
        Some(TEST_INSTANCE_ID),
        shutdown_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");

    // Wait for runner to exit
    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");

    // Verify the cancellation token IS cancelled after shutdown
    assert!(
        cancellation_token.is_cancelled(),
        "cancellation token should be cancelled after shutdown request"
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
    let peppy_config_path = temp_dir.path().join(peppylib::config::NODE_CONFIG_FILE);
    let peppy_config = r#"{
      peppy_schema: "node_v1",
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
    std::fs::write(&peppy_config_path, peppy_config).expect("failed to write peppy config");

    let standalone_config = peppylib::runtime::StandaloneConfig::new()
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
