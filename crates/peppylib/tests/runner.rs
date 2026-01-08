use bytes::Bytes;
use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::peppy_config::{DeploymentInstance, Name};
use config::runtime::RuntimeConfig;
use peppylib::encoding::health::{NodeHealthRequest, NodeHealthResponse};
use peppylib::messaging::{NODE_HEALTH_SERVICE, NODE_READY_SERVICE, SHUTDOWN_SERVICE};
use peppylib::runtime::runner;
use pmi::MessengerBackend;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const TEST_MASTER_NODE: &str = "test_master";
const TEST_NODE_NAME: &str = "test_node";
const TEST_INSTANCE_ID: &str = "test_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const TEST_FREQUENCY_HZ: f64 = 10.0;

#[derive(Debug, serde::Deserialize)]
struct Parameters {
    frequency_hz: f64,
}

struct EnvAndDirGuard {
    previous_runtime_config: Option<String>,
    previous_dir: PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvAndDirGuard {
    fn new(temp_dir: &Path, runtime_config_path: &Path) -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock poisoned");

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

struct RouterGuard {
    router: Option<pmi::Messenger>,
    _temp_dir: Option<tempfile::TempDir>,
}

impl RouterGuard {
    async fn stop(&mut self) {
        if let Some(mut router) = self.router.take() {
            let _ = router.stop_router().await;
        }
    }
}

impl Drop for RouterGuard {
    fn drop(&mut self) {
        let Some(mut router) = self.router.take() else {
            return;
        };
        let _ = std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Runtime::new() {
                let _ = rt.block_on(async move { router.stop_router().await });
            }
        })
        .join();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_succeed() {
    let (router, router_temp_dir, router_host, router_port) =
        peppylib::start_zenohd_process("127.0.0.1", None)
            .await
            .expect("failed to start zenoh router for test");
    let mut router_guard = RouterGuard {
        router: Some(router),
        _temp_dir: Some(router_temp_dir),
    };

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(peppylib::config::NODE_CONFIG_FILE);
    let peppy_config = r#"{
      schema_version: 1,
      manifest: {
        name: "test_node",
        tag: "0.1.0",
        start_cmd: ["cargo", "run"]
      },
      parameters: {
        frequency_hz: "f64"
      }
    }"#;
    std::fs::write(&peppy_config_path, peppy_config).expect("failed to write peppy config");

    let fingerprint = RuntimeConfig::generate_peppy_config_fingerprint(&peppy_config_path)
        .expect("failed to generate peppy config fingerprint");
    let fingerprint_path = temp_dir
        .path()
        .join(PEPPYGEN_OUTPUT_PATH)
        .join(peppylib::config::NODE_CONFIG_FINGERPRINT_FILE);
    std::fs::create_dir_all(
        fingerprint_path
            .parent()
            .expect("fingerprint path should have a parent dir"),
    )
    .expect("failed to create peppygen output dir");
    std::fs::write(&fingerprint_path, format!("{fingerprint}\n"))
        .expect("failed to write fingerprint file");

    let runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(TEST_INSTANCE_ID).expect("instance id should be valid"),
            arguments: serde_json5::from_str(&format!("{{ frequency_hz: {TEST_FREQUENCY_HZ} }}"))
                .expect("runtime args should parse"),
        },
        TEST_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .expect("runtime config should build");
    let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
    runtime_config
        .save_json5_launch_config(&runtime_config_path)
        .expect("failed to write runtime config");

    let _env_guard = EnvAndDirGuard::new(temp_dir.path(), &runtime_config_path);

    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<f64>();
    let mut runner_task = tokio::task::spawn_blocking(move || {
        runner::run(|parameters: Parameters, _node_runner| async move {
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
            TEST_MASTER_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            SHUTDOWN_SERVICE,
            Some(TEST_MASTER_NODE),
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
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        NODE_HEALTH_SERVICE,
        Some(TEST_MASTER_NODE),
        Some(TEST_INSTANCE_ID),
        health_request,
        Duration::from_secs(2),
    )
    .await
    .expect("health service should respond");
    NodeHealthResponse::decode(&health_response.payload().to_bytes())
        .expect("health response should decode");

    let shutdown_payload = Bytes::from_static(b"shutdown");
    let shutdown_response = peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        SHUTDOWN_SERVICE,
        Some(TEST_MASTER_NODE),
        Some(TEST_INSTANCE_ID),
        shutdown_payload.clone(),
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");

    assert_eq!(shutdown_response.payload().to_bytes(), shutdown_payload);
    assert_eq!(shutdown_response.instance_id(), TEST_INSTANCE_ID);

    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");

    router_guard.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_ready_but_not_healthy() {
    let (router, router_temp_dir, router_host, router_port) =
        peppylib::start_zenohd_process("127.0.0.1", None)
            .await
            .expect("failed to start zenoh router for test");
    let mut router_guard = RouterGuard {
        router: Some(router),
        _temp_dir: Some(router_temp_dir),
    };

    let temp_dir = tempfile::tempdir().expect("failed to create temp dir for test runner");
    let peppy_config_path = temp_dir.path().join(peppylib::config::NODE_CONFIG_FILE);
    let peppy_config = r#"{
      schema_version: 1,
      manifest: {
        name: "test_node",
        tag: "0.1.0",
        start_cmd: ["cargo", "run"]
      },
      parameters: {
        frequency_hz: "f64"
      }
    }"#;
    std::fs::write(&peppy_config_path, peppy_config).expect("failed to write peppy config");

    let fingerprint = RuntimeConfig::generate_peppy_config_fingerprint(&peppy_config_path)
        .expect("failed to generate peppy config fingerprint");
    let fingerprint_path = temp_dir
        .path()
        .join(PEPPYGEN_OUTPUT_PATH)
        .join(peppylib::config::NODE_CONFIG_FINGERPRINT_FILE);
    std::fs::create_dir_all(
        fingerprint_path
            .parent()
            .expect("fingerprint path should have a parent dir"),
    )
    .expect("failed to create peppygen output dir");
    std::fs::write(&fingerprint_path, format!("{fingerprint}\n"))
        .expect("failed to write fingerprint file");

    let runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(TEST_INSTANCE_ID).expect("instance id should be valid"),
            arguments: serde_json5::from_str(&format!("{{ frequency_hz: {TEST_FREQUENCY_HZ} }}"))
                .expect("runtime args should parse"),
        },
        TEST_NODE_NAME,
        TEST_MASTER_NODE,
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
        runner::run(|_parameters: Parameters, _node_runner| async move {
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
            TEST_MASTER_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            NODE_READY_SERVICE,
            Some(TEST_MASTER_NODE),
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

    let ready_payload = Bytes::from_static(b"ready");
    let ready_response = peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        NODE_READY_SERVICE,
        Some(TEST_MASTER_NODE),
        Some(TEST_INSTANCE_ID),
        ready_payload.clone(),
        Duration::from_secs(2),
    )
    .await
    .expect("ready service should respond while setup is blocked");
    assert_eq!(ready_response.payload().to_bytes(), ready_payload);
    assert_eq!(ready_response.instance_id(), TEST_INSTANCE_ID);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if runner_task.is_finished() {
            let result = runner_task.await.expect("runner task should not panic");
            panic!("runner exited early: {result:?}");
        }

        if peppylib::ServiceMessenger::is_reachable(
            &messenger,
            TEST_MASTER_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            SHUTDOWN_SERVICE,
            Some(TEST_MASTER_NODE),
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
            TEST_MASTER_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            NODE_HEALTH_SERVICE,
            Some(TEST_MASTER_NODE),
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
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        NODE_HEALTH_SERVICE,
        Some(TEST_MASTER_NODE),
        Some(TEST_INSTANCE_ID),
        health_request.clone(),
        Duration::from_millis(200),
    )
    .await
    .err()
    .expect("health service should not respond while setup is blocked");

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
            TEST_MASTER_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            TEST_NODE_NAME,
            NODE_HEALTH_SERVICE,
            Some(TEST_MASTER_NODE),
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
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        NODE_HEALTH_SERVICE,
        Some(TEST_MASTER_NODE),
        Some(TEST_INSTANCE_ID),
        health_request,
        Duration::from_secs(2),
    )
    .await
    .expect("health service should respond after setup completes");
    NodeHealthResponse::decode(&health_response.payload().to_bytes())
        .expect("health response should decode");

    let shutdown_payload = Bytes::from_static(b"shutdown");
    let shutdown_response = peppylib::ServiceMessenger::poll(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        TEST_NODE_NAME,
        SHUTDOWN_SERVICE,
        Some(TEST_MASTER_NODE),
        Some(TEST_INSTANCE_ID),
        shutdown_payload.clone(),
        Duration::from_secs(2),
    )
    .await
    .expect("shutdown service should respond");

    assert_eq!(shutdown_response.payload().to_bytes(), shutdown_payload);
    assert_eq!(shutdown_response.instance_id(), TEST_INSTANCE_ID);

    tokio::time::timeout(Duration::from_secs(10), &mut runner_task)
        .await
        .expect("runner should exit")
        .expect("runner task should not panic")
        .expect("runner should return Ok");

    router_guard.stop().await;
}
