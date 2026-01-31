mod helpers;

use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::NodeInstance;
use config::{
    node::{ExposedService, MessageFormat, SubscribedService},
    peppy_config::Name,
    runtime::RuntimeConfig,
};
use generator::LanguageGenerator;
use helpers::{
    WaitContext, compile_project, copy_config_to_output, init_cargo_user_node, init_test_env,
    send_shutdown, spawn_cargo_run, try_send_shutdown, wait_for_child,
    wait_for_health_service_reachable_or_exit, wait_for_service_reachable_or_exit,
};
use std::path::Path;
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const TEST_MASTER_NODE: &str = "test_master";
const SUBSCRIBER_NODE_NAME: &str = "subscriber_node";
const SUBSCRIBER_INSTANCE_ID: &str = "subscriber_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";

// --- Services exposes and its corresponding subscriber
const EXPOSED_SERVICE_EXAMPLE: &str = r#"
{
  name: "enable_camera",
  request_message_format: {
    enable: "bool"
  },
  response_message_format: {
    enabled: "bool",
    error_msg: {
      $type: "string",
      $optional: true
    },
  }
}
"#;

const SUBSCRIBED_SERVICE_EXAMPLE: &str = r#"
{
  id: "uvc_camera_enable_camera",
  node: "uvc_camera",
  name: "enable_camera",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_SERVICE_REQUEST_FORMAT_EXAMPLE: &str = r#"
{
  enable: "bool"
}
"#;

const SUBSCRIBED_SERVICE_RESPONSE_FORMAT_EXAMPLE: &str = r#"
{
    enabled: "bool",
    error_msg: {
      $type: "string",
      $optional: true
    },
}
"#;

const EMPTY_MESSAGE_FORMAT: &str = r#"{}"#;

// --- Service without request body
const EXPOSED_SERVICE_NO_REQUEST_EXAMPLE: &str = r#"
{
  name: "get_system_status",
  response_message_format: {
    healthy: "bool"
  }
}
"#;

const SUBSCRIBED_SERVICE_NO_REQUEST_EXAMPLE: &str = r#"
{
  id: "uvc_camera_get_system_status",
  node: "uvc_camera",
  name: "get_system_status",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_SERVICE_NO_REQUEST_RESPONSE_FORMAT_EXAMPLE: &str = r#"
{
  healthy: "bool"
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_no_target_instance_id() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Subscriber (client) project
    let subscriber_instance_id = "the_subscriber";
    let temp_dir_subscriber = TempDir::new().unwrap();
    let subscribed_service: SubscribedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE).unwrap();
    let subscribed_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_FORMAT_EXAMPLE).unwrap();
    let subscribed_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir_subscriber, user_node_subscriber, peppy_node_config_path) =
        init_test_env(&temp_dir_subscriber);
    generator
        .add_subscribed_service(
            &subscribed_service,
            &subscribed_request_format,
            &subscribed_response_format,
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir_subscriber);
    generator.build(&output_dir_subscriber).unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let subscriber_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstance {
            instance_id: Name::new(subscriber_instance_id).unwrap(),
            arguments: Default::default(),
        },
        SUBSCRIBER_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_subscriber.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = r#"
use peppygen::subscribed_services::uvc_camera_enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let request = uvc_camera_enable_camera::Request::new(true);
        let response =
            uvc_camera_enable_camera::poll(&node_runner, Duration::from_secs(5), None, None, request).await?;
        let error_msg = response.data.error_msg.as_deref().unwrap_or("<none>");
        println!(
            "enable_camera result: service_id={} enabled={} error={}",
            &response.instance_id,
            response.data.enabled,
            error_msg
        );

        Ok(())
    })
}
"#;
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write subscriber main");

    // --- Exposer (server) project
    let exposer_instance_id = "the_exposer";
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env(&temp_dir_exposer);
    generator.add_exposed_service(&exposed_service).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator.build(&output_dir_exposer).unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstance {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
        },
        UVC_CAMERA_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer);
    let exposer_main = r#"
use peppygen::exposed_services::enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        enable_camera::handle_next_request(&node_runner, |request| -> Result<enable_camera::Response> {
            println!("received enable_camera request from {}: enable = {}", request.instance_id, request.data.enable);
            Ok(enable_camera::Response::new(
                request.data.enable,
                Some("handled".to_owned()),
            ))
        })
        .await?;

        println!("enable_camera handler finished");

        Ok(())
    })
}
"#;
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer);

    let user_node_exposer_runtime_config_str =
        exposer_runtime_config_path.to_str().unwrap().to_owned();
    let user_node_subscriber_config_str =
        subscriber_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_master_node: TEST_MASTER_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_master_node: Some(TEST_MASTER_NODE),
    };

    // Spawn exposer first so it's ready to handle requests
    let mut exposer_child = spawn_cargo_run(
        &user_node_exposer,
        &[(
            RUNTIME_CONFIG_VAR_NAME,
            &user_node_exposer_runtime_config_str,
        )],
    );

    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        Some(exposer_instance_id),
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(10),
    )
    .await;

    let mut subscriber_child = spawn_cargo_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &user_node_subscriber_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        SUBSCRIBER_NODE_NAME,
        subscriber_instance_id,
        &mut subscriber_child,
        &user_node_subscriber,
        Duration::from_secs(10),
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(10),
    )
    .await;

    // Use try_send_shutdown for subscriber since it may have already exited
    // after completing its service call
    try_send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        SUBSCRIBER_NODE_NAME,
        Some(TEST_MASTER_NODE),
        subscriber_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        Some(TEST_MASTER_NODE),
        exposer_instance_id,
        Duration::from_secs(5),
    )
    .await;

    // Wait for both processes to exit
    let subscriber_output = wait_for_child(
        &mut subscriber_child,
        Some(Duration::from_secs(10)),
        &user_node_subscriber,
    );
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer,
    );

    let subscriber_stdout = String::from_utf8_lossy(&subscriber_output.stdout).into_owned();
    let subscriber_stderr = String::from_utf8_lossy(&subscriber_output.stderr).into_owned();
    assert!(
        subscriber_output.status.success(),
        "subscriber cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        subscriber_output.status.code(),
        subscriber_stdout,
        subscriber_stderr
    );
    let expected_subscriber_log = format!(
        "enable_camera result: service_id={} enabled=true error=handled",
        exposer_instance_id
    );
    assert!(
        subscriber_stdout.contains(&expected_subscriber_log),
        "subscriber did not receive expected service response (expected log: {}).\nstdout:\n{}\nstderr:\n{}",
        expected_subscriber_log,
        subscriber_stdout,
        subscriber_stderr
    );

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
    );
    let expected_request_log = format!(
        "received enable_camera request from {}: enable = true",
        subscriber_instance_id
    );
    assert!(
        exposer_stdout.contains(&expected_request_log)
            && exposer_stdout.contains("enable_camera handler finished"),
        "exposer did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_exposed_service_without_request_body() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Subscriber (client) project
    let subscriber_instance_id = "the_subscriber";
    let temp_dir_subscriber = TempDir::new().unwrap();
    let subscribed_service: SubscribedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_NO_REQUEST_EXAMPLE).unwrap();
    let subscribed_request_format: MessageFormat =
        serde_json5::from_str(EMPTY_MESSAGE_FORMAT).expect("empty request format should parse");
    let subscribed_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_NO_REQUEST_RESPONSE_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir_subscriber, user_node_subscriber, peppy_node_config_path) =
        init_test_env(&temp_dir_subscriber);
    generator
        .add_subscribed_service(
            &subscribed_service,
            &subscribed_request_format,
            &subscribed_response_format,
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir_subscriber);
    generator.build(&output_dir_subscriber).unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let subscriber_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstance {
            instance_id: Name::new(subscriber_instance_id).unwrap(),
            arguments: Default::default(),
        },
        SUBSCRIBER_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_subscriber.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = r#"
use peppygen::subscribed_services::uvc_camera_get_system_status;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let response =
            uvc_camera_get_system_status::poll(&node_runner, Duration::from_secs(5), None, None).await?;
        println!(
            "get_system_status result: service_id={} healthy={}",
            &response.instance_id,
            response.data.healthy
        );
        Ok(())
    })
}
"#;
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write subscriber main");

    // --- Exposer (server) project
    let exposer_instance_id = "the_exposer";
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_service: ExposedService =
        serde_json5::from_str(EXPOSED_SERVICE_NO_REQUEST_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env(&temp_dir_exposer);
    generator.add_exposed_service(&exposed_service).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator.build(&output_dir_exposer).unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstance {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
        },
        UVC_CAMERA_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer);
    let exposer_main = r#"
use peppygen::exposed_services::get_system_status;
use peppygen::NodeBuilder;
use peppygen::Result;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        get_system_status::handle_next_request(&node_runner, |request| -> Result<get_system_status::Response> {
            println!("received get_system_status request from {}", request.instance_id);
            Ok(get_system_status::Response::new(true))
        })
        .await?;

        println!("get_system_status handler finished");

        Ok(())
    })
}
"#;
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer);

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let subscriber_runtime_config_str = subscriber_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_master_node: TEST_MASTER_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_master_node: Some(TEST_MASTER_NODE),
    };

    // Spawn exposer first so it's ready to handle requests
    let mut exposer_child = spawn_cargo_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "get_system_status",
        Some(exposer_instance_id),
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(10),
    )
    .await;

    let mut subscriber_child = spawn_cargo_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &subscriber_runtime_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        SUBSCRIBER_NODE_NAME,
        subscriber_instance_id,
        &mut subscriber_child,
        &user_node_subscriber,
        Duration::from_secs(10),
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(10),
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        SUBSCRIBER_NODE_NAME,
        Some(TEST_MASTER_NODE),
        subscriber_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        Some(TEST_MASTER_NODE),
        exposer_instance_id,
        Duration::from_secs(5),
    )
    .await;

    let subscriber_output = wait_for_child(
        &mut subscriber_child,
        Some(Duration::from_secs(10)),
        &user_node_subscriber,
    );
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer,
    );

    let subscriber_stdout = String::from_utf8_lossy(&subscriber_output.stdout).into_owned();
    let subscriber_stderr = String::from_utf8_lossy(&subscriber_output.stderr).into_owned();
    assert!(
        subscriber_output.status.success(),
        "subscriber cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        subscriber_output.status.code(),
        subscriber_stdout,
        subscriber_stderr
    );
    let expected_subscriber_log = format!(
        "get_system_status result: service_id={} healthy=true",
        exposer_instance_id
    );
    assert!(
        subscriber_stdout.contains(&expected_subscriber_log),
        "subscriber did not receive expected service response (expected log: {}).\nstdout:\n{}\nstderr:\n{}",
        expected_subscriber_log,
        subscriber_stdout,
        subscriber_stderr
    );

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
    );
    let expected_request_log = format!(
        "received get_system_status request from {}",
        subscriber_instance_id
    );
    assert!(
        exposer_stdout.contains(&expected_request_log)
            && exposer_stdout.contains("get_system_status handler finished"),
        "exposer did not process the get_system_status request.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// If there are multiple services of the same name and the subscriber does not specify an instance_id, it's the first service that respond that connects with the subscriber
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_multiple_exposed_instances_same_service_not_target_instance_id() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Subscriber (client) project
    let subscriber_instance_id = SUBSCRIBER_INSTANCE_ID;
    let temp_dir_subscriber = TempDir::new().unwrap();
    let subscribed_service: SubscribedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE).unwrap();
    let subscribed_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_FORMAT_EXAMPLE).unwrap();
    let subscribed_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir_subscriber, user_node_subscriber, peppy_node_config_path) =
        init_test_env(&temp_dir_subscriber);
    generator
        .add_subscribed_service(
            &subscribed_service,
            &subscribed_request_format,
            &subscribed_response_format,
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir_subscriber);
    generator.build(&output_dir_subscriber).unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let subscriber_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstance {
            instance_id: Name::new(subscriber_instance_id).unwrap(),
            arguments: Default::default(),
        },
        SUBSCRIBER_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_subscriber.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = r#"
use peppygen::subscribed_services::uvc_camera_enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let request = uvc_camera_enable_camera::Request::new(true);
        let response =
            uvc_camera_enable_camera::poll(&node_runner, Duration::from_secs(5), None, None, request).await?;
        let error_msg = response.data.error_msg.as_deref().unwrap_or("<none>");
        println!(
            "enable_camera result: enabled={} error={}",
            response.data.enabled,
            error_msg
        );

        Ok(())
    })
}
"#;
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write subscriber main");

    // --- Exposer 1
    let exposer1_instance_id = "exposer1_instance";
    let temp_dir_exposer1 = TempDir::new().unwrap();
    let exposed_service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer1, user_node_exposer1, peppy_node_config_path) =
        init_test_env(&temp_dir_exposer1);
    generator.add_exposed_service(&exposed_service).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer1, &output_dir_exposer1);
    generator.build(&output_dir_exposer1).unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer1_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstance {
            instance_id: Name::new(exposer1_instance_id).unwrap(),
            arguments: Default::default(),
        },
        UVC_CAMERA_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let exposer1_runtime_config_path = temp_dir_exposer1.path().join("peppy_runtime.json5");
    exposer1_runtime_config
        .save_json5_launch_config(&exposer1_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer1);
    let exposer1_main = r#"
use peppygen::exposed_services::enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        enable_camera::handle_next_request(&node_runner, |request| -> Result<enable_camera::Response> {
            println!("received enable_camera request for {}: {}", request.instance_id, request.data.enable);
            Ok(enable_camera::Response::new(
                request.data.enable,
                Some("handled".to_owned()),
            ))
        })
        .await?;

        println!("enable_camera handler finished");

        Ok(())
    })
}
"#;
    let main_file = user_node_exposer1.join("src").join("main.rs");
    fs::write(main_file, exposer1_main).expect("failed to write exposer main 1");

    // --- Exposer 2
    let exposer2_instance_id = "exposer2_instance";
    let temp_dir_exposer2 = TempDir::new().unwrap();
    let exposed_service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer2, user_node_exposer2, peppy_node_config_path) =
        init_test_env(&temp_dir_exposer2);
    generator.add_exposed_service(&exposed_service2).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer2, &output_dir_exposer2);
    generator.build(&output_dir_exposer2).unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer2_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstance {
            instance_id: Name::new(exposer2_instance_id).unwrap(),
            arguments: Default::default(),
        },
        UVC_CAMERA_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let exposer2_runtime_config_path = temp_dir_exposer2.path().join("peppy_runtime.json5");
    exposer2_runtime_config
        .save_json5_launch_config(&exposer2_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer2);
    let exposer2_main = r#"
use peppygen::exposed_services::enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        enable_camera::handle_next_request(&node_runner, |request| -> Result<enable_camera::Response> {
            println!("received enable_camera request for {}: {}", request.instance_id, request.data.enable);
            // Sleep to ensure exposer1 responds first
            std::thread::sleep(Duration::from_secs(2));
            Ok(enable_camera::Response::new(
                request.data.enable,
                Some("handled_by_exposer2".to_owned()),
            ))
        })
        .await?;

        println!("enable_camera handler finished");

        Ok(())
    })
}
"#;
    let main_file = user_node_exposer2.join("src").join("main.rs");
    fs::write(main_file, exposer2_main).expect("failed to write exposer main 2");

    // Compilation + execution
    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer1);
    compile_project(&user_node_exposer2);

    let exposer1_runtime_config_str = exposer1_runtime_config_path.to_str().unwrap().to_owned();
    let exposer2_runtime_config_str = exposer2_runtime_config_path.to_str().unwrap().to_owned();
    let subscriber_runtime_config_str = subscriber_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_master_node: TEST_MASTER_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_master_node: Some(TEST_MASTER_NODE),
    };

    // Spawn both exposers first so they're ready to handle requests
    let mut exposer1_child = spawn_cargo_run(
        &user_node_exposer1,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer1_runtime_config_str)],
    );
    let mut exposer2_child = spawn_cargo_run(
        &user_node_exposer2,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer2_runtime_config_str)],
    );

    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        Some(exposer1_instance_id),
        &mut exposer1_child,
        &user_node_exposer1,
        Duration::from_secs(10),
    )
    .await;
    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        Some(exposer2_instance_id),
        &mut exposer2_child,
        &user_node_exposer2,
        Duration::from_secs(10),
    )
    .await;

    let mut subscriber_child = spawn_cargo_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &subscriber_runtime_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        SUBSCRIBER_NODE_NAME,
        subscriber_instance_id,
        &mut subscriber_child,
        &user_node_subscriber,
        Duration::from_secs(10),
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer1_instance_id,
        &mut exposer1_child,
        &user_node_exposer1,
        Duration::from_secs(10),
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer2_instance_id,
        &mut exposer2_child,
        &user_node_exposer2,
        Duration::from_secs(10),
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        SUBSCRIBER_NODE_NAME,
        Some(TEST_MASTER_NODE),
        subscriber_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        Some(TEST_MASTER_NODE),
        exposer1_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        Some(TEST_MASTER_NODE),
        exposer2_instance_id,
        Duration::from_secs(5),
    )
    .await;

    // Wait for all processes to exit
    let subscriber_output = wait_for_child(
        &mut subscriber_child,
        Some(Duration::from_secs(10)),
        &user_node_subscriber,
    );
    let exposer_output1 = wait_for_child(
        &mut exposer1_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer1,
    );
    let exposer_output2 = wait_for_child(
        &mut exposer2_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer2,
    );

    let subscriber_stdout = String::from_utf8_lossy(&subscriber_output.stdout).into_owned();
    let subscriber_stderr = String::from_utf8_lossy(&subscriber_output.stderr).into_owned();
    assert!(
        subscriber_output.status.success(),
        "subscriber cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        subscriber_output.status.code(),
        subscriber_stdout,
        subscriber_stderr
    );

    let exposer1_stdout = String::from_utf8_lossy(&exposer_output1.stdout).into_owned();
    let exposer1_stderr = String::from_utf8_lossy(&exposer_output1.stderr).into_owned();
    let exposer2_stdout = String::from_utf8_lossy(&exposer_output2.stdout).into_owned();
    let exposer2_stderr = String::from_utf8_lossy(&exposer_output2.stderr).into_owned();

    // Both exposers should have received the request
    let expected_request_log = format!(
        "received enable_camera request for {}: true",
        subscriber_instance_id
    );
    assert!(
        exposer1_stdout.contains(&expected_request_log)
            && exposer1_stdout.contains("enable_camera handler finished"),
        "exposer1 did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        exposer1_stdout,
        exposer1_stderr
    );
    assert!(
        exposer2_stdout.contains(&expected_request_log)
            && exposer2_stdout.contains("enable_camera handler finished"),
        "exposer2 did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        exposer2_stdout,
        exposer2_stderr
    );

    // Subscriber should have received a response from exposer1 (the faster responder)
    assert!(
        subscriber_stdout.contains("enable_camera result: enabled=true error=handled"),
        "subscriber should have received response from exposer1 (the faster responder), not exposer2.\nstdout:\n{}\nstderr:\n{}",
        subscriber_stdout,
        subscriber_stderr
    );
}
