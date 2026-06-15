mod common;

use common::test_node_target;
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::launcher::Name;
use config::runtime::{NodeInstanceConfig, RuntimeConfig};
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

        let previous_runtime_config = std::env::var(RUNTIME_CONFIG_VAR_NAME).ok();
        let previous_dir = std::env::current_dir().expect("current dir should be readable");

        // SAFETY: environment mutation is guarded by a global mutex to avoid races.
        unsafe { std::env::set_var(RUNTIME_CONFIG_VAR_NAME, runtime_config_path) };
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

        let previous_runtime_config = std::env::var(RUNTIME_CONFIG_VAR_NAME).ok();
        let previous_dir = std::env::current_dir().expect("current dir should be readable");

        // SAFETY: environment mutation is guarded by a global mutex to avoid races.
        // Remove the env var to ensure standalone mode is used.
        unsafe {
            std::env::remove_var(RUNTIME_CONFIG_VAR_NAME);
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
                Some(value) => std::env::set_var(RUNTIME_CONFIG_VAR_NAME, value),
                None => std::env::remove_var(RUNTIME_CONFIG_VAR_NAME),
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
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
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
            arguments: serde_json5::from_str(&format!("{{ frequency_hz: {TEST_FREQUENCY_HZ} }}"))
                .expect("runtime args should parse"),
            ..NodeInstanceConfig::new(
                Name::new(TEST_INSTANCE_ID).expect("instance id should be valid"),
            )
        },
        TEST_NODE_NAME,
        "v1",
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
            test_node_target(TEST_NODE_NAME),
            SHUTDOWN_SERVICE,
            ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
        test_node_target(TEST_NODE_NAME),
        NODE_HEALTH_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
        test_node_target(TEST_NODE_NAME),
        SHUTDOWN_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
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
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
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
            arguments: serde_json5::from_str(&format!("{{ frequency_hz: {TEST_FREQUENCY_HZ} }}"))
                .expect("runtime args should parse"),
            ..NodeInstanceConfig::new(
                Name::new(TEST_INSTANCE_ID).expect("instance id should be valid"),
            )
        },
        TEST_NODE_NAME,
        "v1",
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
            test_node_target(TEST_NODE_NAME),
            NODE_READY_SERVICE,
            ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
        test_node_target(TEST_NODE_NAME),
        NODE_READY_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
            test_node_target(TEST_NODE_NAME),
            SHUTDOWN_SERVICE,
            ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
            test_node_target(TEST_NODE_NAME),
            NODE_HEALTH_SERVICE,
            ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
        test_node_target(TEST_NODE_NAME),
        NODE_HEALTH_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
            test_node_target(TEST_NODE_NAME),
            NODE_HEALTH_SERVICE,
            ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
        test_node_target(TEST_NODE_NAME),
        NODE_HEALTH_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
        test_node_target(TEST_NODE_NAME),
        SHUTDOWN_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
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
            arguments: serde_json5::from_str(&format!("{{ frequency_hz: {TEST_FREQUENCY_HZ} }}"))
                .expect("runtime args should parse"),
            ..NodeInstanceConfig::new(
                Name::new(TEST_INSTANCE_ID).expect("instance id should be valid"),
            )
        },
        TEST_NODE_NAME,
        "v1",
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
            test_node_target(TEST_NODE_NAME),
            SHUTDOWN_SERVICE,
            ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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
        test_node_target(TEST_NODE_NAME),
        SHUTDOWN_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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

/// A shutdown that arrives while `setup_fn` is still running must cancel the
/// cancellation token and exit the node without waiting for setup to finish.
/// Exercises the during-setup select arm of `run_with_closure`; the post-setup
/// arm is covered by `daemon_cancellation_token_cancelled_on_shutdown`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_during_setup_cancels_token_and_exits() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
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
            arguments: serde_json5::from_str(&format!("{{ frequency_hz: {TEST_FREQUENCY_HZ} }}"))
                .expect("runtime args should parse"),
            ..NodeInstanceConfig::new(
                Name::new(TEST_INSTANCE_ID).expect("instance id should be valid"),
            )
        },
        TEST_NODE_NAME,
        "v1",
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

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger");

    // The shutdown service is a pre-setup service, so it must be reachable
    // while setup_fn is still blocked
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
            test_node_target(TEST_NODE_NAME),
            SHUTDOWN_SERVICE,
            ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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

    // Send the shutdown request while setup_fn is still blocked
    let shutdown_payload = Payload::from_static(b"shutdown");
    peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
        SHUTDOWN_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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

/// Scaffolding for the shutdown-hook tests below: an ephemeral router, a node
/// `peppy.json5` + launch config in a temp dir, and the env guard that points
/// `NodeBuilder` at them in daemon mode. Declaration order matters for drop:
/// the env guard restores cwd before the temp dir it points into is removed.
struct DaemonStack {
    _env_guard: EnvAndDirGuard,
    _temp_dir: tempfile::TempDir,
    _router: pmi::ZenohdInstance,
    router_host: String,
    router_port: u16,
}

/// Stands up the daemon-mode scaffolding with an optional
/// `lifecycle.shutdown_grace_secs` override in the launch config (mirroring
/// what the daemon ships to spawned nodes).
async fn start_daemon_stack(shutdown_grace_secs: Option<u64>) -> DaemonStack {
    let router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (router.host.clone(), router.port);

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(NODE_CONFIG_FILE);
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

    let mut runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
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
    )
    .expect("runtime config should build");
    if let Some(grace) = shutdown_grace_secs {
        runtime_config.lifecycle.shutdown_grace_secs = grace;
    }
    let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
    runtime_config
        .save_json5_launch_config(&runtime_config_path)
        .expect("failed to write runtime config");

    let env_guard = EnvAndDirGuard::new(temp_dir.path(), &runtime_config_path);

    DaemonStack {
        _env_guard: env_guard,
        _temp_dir: temp_dir,
        _router: router,
        router_host,
        router_port,
    }
}

/// Polls until the node's `SHUTDOWN_SERVICE` is reachable (panicking if the
/// runner exits first), then sends the shutdown request: the same in-band ask
/// `peppy node stop` delivers.
async fn send_shutdown_when_reachable<T: std::fmt::Debug>(
    router_host: &str,
    router_port: u16,
    runner_task: &mut tokio::task::JoinHandle<T>,
) {
    let messenger = peppylib::MessengerHandle::from_host_port(router_host, router_port)
        .await
        .expect("failed to create messenger");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if runner_task.is_finished() {
            let result = runner_task.await;
            panic!("runner exited early: {result:?}");
        }

        if peppylib::ServiceMessenger::is_reachable(
            &messenger,
            TEST_CORE_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            test_node_target(TEST_NODE_NAME),
            SHUTDOWN_SERVICE,
            ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
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

    peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
        SHUTDOWN_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(TEST_CORE_NODE, TEST_INSTANCE_ID)),
        Payload::from_static(b"shutdown"),
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");
}

/// The regression test for the `ds_lock_probe` leak: cleanup registered via
/// `on_shutdown` must run to completion, in reverse registration order, with
/// the messenger still usable, before `run()` returns on an in-band shutdown
/// (`peppy node stop`). Before the fix, `run()` returned as soon as the token
/// was cancelled and dropped the runtime under the cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_awaits_hooks_lifo_before_exit() {
    let stack = start_daemon_stack(None).await;

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

    send_shutdown_when_reachable(&stack.router_host, stack.router_port, &mut runner_task).await;

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
    let stack = start_daemon_stack(None).await;

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

    send_shutdown_when_reachable(&stack.router_host, stack.router_port, &mut runner_task).await;

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
    let stack = start_daemon_stack(Some(1)).await;

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

    send_shutdown_when_reachable(&stack.router_host, stack.router_port, &mut runner_task).await;

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
    let _stack = start_daemon_stack(None).await;

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
            peppy_schema: "node_v1",
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
