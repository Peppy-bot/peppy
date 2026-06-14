use crate::helpers::{
    DEFAULT_WAIT_TIMEOUT, STUB_NODE_CONFIG, WaitContext, compile_project, copy_config_to_output,
    init_cargo_user_node, init_test_env, send_shutdown, spawn_cargo_run, test_peppy_dirs,
    try_send_shutdown, wait_for_child, wait_for_health_service_reachable_or_exit,
    wait_for_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::NodeInstanceConfig;
use config::{
    launcher::Name,
    node::{ConsumedService, ExposedService, MessageFormat},
    runtime::RuntimeConfig,
};
use generator::LanguageGenerator;
use std::path::Path;
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const TEST_CORE_NODE: &str = "test_core";
const CONSUMER_NODE_NAME: &str = "consumer_node";
const CONSUMER_INSTANCE_ID: &str = "consumer_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";

// --- Services exposes and its corresponding consumer
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

const CONSUMED_SERVICE_EXAMPLE: &str = r#"
{
  link_id: "uvc_camera",
  name: "enable_camera",
}
"#;

const CONSUMED_SERVICE_REQUEST_FORMAT_EXAMPLE: &str = r#"
{
  enable: "bool"
}
"#;

const CONSUMED_SERVICE_RESPONSE_FORMAT_EXAMPLE: &str = r#"
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

const CONSUMED_SERVICE_NO_REQUEST_EXAMPLE: &str = r#"
{
  link_id: "uvc_camera",
  name: "get_system_status",
}
"#;

const CONSUMED_SERVICE_NO_REQUEST_RESPONSE_FORMAT_EXAMPLE: &str = r#"
{
  healthy: "bool"
}
"#;

#[rstest::rstest]
#[case::peer(crate::helpers::Mode::Peer)]
#[case::router(crate::helpers::Mode::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_no_target_instance_id(#[case] mode: crate::helpers::Mode) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = "the_consumer";
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let consumed_service: ConsumedService =
        serde_json5::from_str(CONSUMED_SERVICE_EXAMPLE).unwrap();
    let consumed_request_format: MessageFormat =
        serde_json5::from_str(CONSUMED_SERVICE_REQUEST_FORMAT_EXAMPLE).unwrap();
    let consumed_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_SERVICE_RESPONSE_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir_consumer, user_node_consumer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_consumer, STUB_NODE_CONFIG);
    generator
        .add_consumed_service(
            &consumed_service,
            &consumed_request_format,
            &consumed_response_format,
            &generator::DependencyContext::native("uvc_camera", "v1"),
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node_consumer, &output_dir_consumer);
    generator
        .build(&output_dir_consumer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let consumer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config = crate::helpers::apply_mode(consumer_runtime_config, mode);
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_consumer);
    let consumer_main = r#"
use peppygen::consumed_services::uvc_camera_enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let request = uvc_camera_enable_camera::Request::new(true);
        let response =
            uvc_camera_enable_camera::poll(&node_runner, Duration::from_secs(5), request).await?;
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
    let main_file = user_node_consumer.join("src").join("main.rs");
    fs::write(main_file, consumer_main).expect("failed to write consumer main");

    // --- Exposer (server) project
    let exposer_instance_id = "the_exposer";
    let temp_dir_exposer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
    let exposed_service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer, STUB_NODE_CONFIG);
    generator
        .add_exposed_service(&exposed_service, None)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator
        .build(&output_dir_exposer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        UVC_CAMERA_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config = crate::helpers::apply_mode(exposer_runtime_config, mode);
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

    compile_project(&user_node_consumer);
    compile_project(&user_node_exposer);

    let user_node_exposer_runtime_config_str =
        exposer_runtime_config_path.to_str().unwrap().to_owned();
    let user_node_consumer_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: TEST_CORE_NODE,
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
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    // Verify broadcast reachability before spawning the consumer.
    // The targeted probe above confirms the exposer individually, but the
    // broadcast subscription pattern may not be fully propagated in the
    // Zenoh routing table yet. This probe exercises the broadcast path,
    // ensuring the exposer's broadcast subscription is active before the
    // consumer sends its broadcast poll (instance_id=None).
    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        None,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &user_node_consumer_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    // Use try_send_shutdown for consumer since it may have already exited
    // after completing its service call
    try_send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        TEST_CORE_NODE,
        consumer_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        TEST_CORE_NODE,
        exposer_instance_id,
        Duration::from_secs(5),
    )
    .await;

    // Wait for both processes to exit
    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &user_node_consumer,
    );
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer,
    );

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    let expected_consumer_log = format!(
        "enable_camera result: service_id={} enabled=true error=handled",
        exposer_instance_id
    );
    assert!(
        consumer_stdout.contains(&expected_consumer_log),
        "consumer did not receive expected service response (expected log: {}).\nstdout:\n{}\nstderr:\n{}",
        expected_consumer_log,
        consumer_stdout,
        consumer_stderr
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
        consumer_instance_id
    );
    assert!(
        exposer_stdout.contains(&expected_request_log)
            && exposer_stdout.contains("enable_camera handler finished"),
        "exposer did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

#[rstest::rstest]
#[case::peer(crate::helpers::Mode::Peer)]
#[case::router(crate::helpers::Mode::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_exposed_service_without_request_body(
    #[case] mode: crate::helpers::Mode,
) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = "the_consumer";
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let consumed_service: ConsumedService =
        serde_json5::from_str(CONSUMED_SERVICE_NO_REQUEST_EXAMPLE).unwrap();
    let consumed_request_format: MessageFormat =
        serde_json5::from_str(EMPTY_MESSAGE_FORMAT).expect("empty request format should parse");
    let consumed_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_SERVICE_NO_REQUEST_RESPONSE_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir_consumer, user_node_consumer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_consumer, STUB_NODE_CONFIG);
    generator
        .add_consumed_service(
            &consumed_service,
            &consumed_request_format,
            &consumed_response_format,
            &generator::DependencyContext::native("uvc_camera", "v1"),
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node_consumer, &output_dir_consumer);
    generator
        .build(&output_dir_consumer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let consumer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config = crate::helpers::apply_mode(consumer_runtime_config, mode);
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_consumer);
    let consumer_main = r#"
use peppygen::consumed_services::uvc_camera_get_system_status;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let response =
            uvc_camera_get_system_status::poll(&node_runner, Duration::from_secs(5)).await?;
        println!(
            "get_system_status result: service_id={} healthy={}",
            &response.instance_id,
            response.data.healthy
        );
        Ok(())
    })
}
"#;
    let main_file = user_node_consumer.join("src").join("main.rs");
    fs::write(main_file, consumer_main).expect("failed to write consumer main");

    // --- Exposer (server) project
    let exposer_instance_id = "the_exposer";
    let temp_dir_exposer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
    let exposed_service: ExposedService =
        serde_json5::from_str(EXPOSED_SERVICE_NO_REQUEST_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer, STUB_NODE_CONFIG);
    generator
        .add_exposed_service(&exposed_service, None)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator
        .build(&output_dir_exposer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        UVC_CAMERA_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config = crate::helpers::apply_mode(exposer_runtime_config, mode);
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

    compile_project(&user_node_consumer);
    compile_project(&user_node_exposer);

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: TEST_CORE_NODE,
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
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    // Verify broadcast reachability before spawning the consumer.
    // The targeted probe above confirms the exposer individually, but the
    // broadcast subscription pattern may not be fully propagated in the
    // Zenoh routing table yet. This probe exercises the broadcast path,
    // ensuring the exposer's broadcast subscription is active before the
    // consumer sends its broadcast poll (instance_id=None).
    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "get_system_status",
        None,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        TEST_CORE_NODE,
        consumer_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        TEST_CORE_NODE,
        exposer_instance_id,
        Duration::from_secs(5),
    )
    .await;

    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &user_node_consumer,
    );
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer,
    );

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    let expected_consumer_log = format!(
        "get_system_status result: service_id={} healthy=true",
        exposer_instance_id
    );
    assert!(
        consumer_stdout.contains(&expected_consumer_log),
        "consumer did not receive expected service response (expected log: {}).\nstdout:\n{}\nstderr:\n{}",
        expected_consumer_log,
        consumer_stdout,
        consumer_stderr
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
        consumer_instance_id
    );
    assert!(
        exposer_stdout.contains(&expected_request_log)
            && exposer_stdout.contains("get_system_status handler finished"),
        "exposer did not process the get_system_status request.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// If there are multiple services of the same name and the consumer does not specify an instance_id, it's the first service that respond that connects with the consumer
#[rstest::rstest]
#[case::peer(crate::helpers::Mode::Peer)]
#[case::router(crate::helpers::Mode::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_multiple_exposed_instances_same_service_no_target_instance_id(
    #[case] mode: crate::helpers::Mode,
) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let consumed_service: ConsumedService =
        serde_json5::from_str(CONSUMED_SERVICE_EXAMPLE).unwrap();
    let consumed_request_format: MessageFormat =
        serde_json5::from_str(CONSUMED_SERVICE_REQUEST_FORMAT_EXAMPLE).unwrap();
    let consumed_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_SERVICE_RESPONSE_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir_consumer, user_node_consumer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_consumer, STUB_NODE_CONFIG);
    generator
        .add_consumed_service(
            &consumed_service,
            &consumed_request_format,
            &consumed_response_format,
            &generator::DependencyContext::native("uvc_camera", "v1"),
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node_consumer, &output_dir_consumer);
    generator
        .build(&output_dir_consumer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let consumer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config = crate::helpers::apply_mode(consumer_runtime_config, mode);
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_consumer);
    let consumer_main = r#"
use peppygen::consumed_services::uvc_camera_enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let request = uvc_camera_enable_camera::Request::new(true);
        let response =
            uvc_camera_enable_camera::poll(&node_runner, Duration::from_secs(5), request).await?;
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
    let main_file = user_node_consumer.join("src").join("main.rs");
    fs::write(main_file, consumer_main).expect("failed to write consumer main");

    // --- Exposer 1
    let exposer1_instance_id = "exposer1_instance";
    let temp_dir_exposer1 = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
    let exposed_service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer1, user_node_exposer1, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer1, STUB_NODE_CONFIG);
    generator
        .add_exposed_service(&exposed_service, None)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_exposer1, &output_dir_exposer1);
    generator
        .build(&output_dir_exposer1, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer1_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(exposer1_instance_id).unwrap()),
        UVC_CAMERA_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer1_runtime_config = crate::helpers::apply_mode(exposer1_runtime_config, mode);
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
    let temp_dir_exposer2 = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
    let exposed_service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer2, user_node_exposer2, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer2, STUB_NODE_CONFIG);
    generator
        .add_exposed_service(&exposed_service2, None)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_exposer2, &output_dir_exposer2);
    generator
        .build(&output_dir_exposer2, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer2_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(exposer2_instance_id).unwrap()),
        UVC_CAMERA_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer2_runtime_config = crate::helpers::apply_mode(exposer2_runtime_config, mode);
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
    compile_project(&user_node_consumer);
    compile_project(&user_node_exposer1);
    compile_project(&user_node_exposer2);

    let exposer1_runtime_config_str = exposer1_runtime_config_path.to_str().unwrap().to_owned();
    let exposer2_runtime_config_str = exposer2_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: TEST_CORE_NODE,
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
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        Some(exposer2_instance_id),
        &mut exposer2_child,
        &user_node_exposer2,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    // Verify broadcast reachability before spawning the consumer.
    // The targeted probes above confirm each exposer individually, but the
    // broadcast subscription pattern may not be fully propagated in the
    // Zenoh routing table yet. This probe exercises the broadcast path,
    // ensuring both exposers' broadcast subscriptions are active before the
    // consumer sends its broadcast poll.
    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        None,
        &mut exposer1_child,
        &user_node_exposer1,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    // Do NOT wait for health on the exposers here. Under discover-then-pin
    // only the winning exposer's `handle_next_request` returns and lets
    // `setup_fn` complete; the loser stays parked on its queryable, so
    // `run_post_setup_services` (which registers the health endpoint) never
    // runs there. Probing health on the loser would now panic with a
    // wait-timeout (post-`DEFAULT_WAIT_TIMEOUT` addition) instead of
    // hanging, but it's still the wrong question to ask. The pre-setup
    // shutdown queryable is up on both exposers, so the `send_shutdown`
    // calls below land cleanly. The consumer's health probe above already
    // implies the response round-trip completed, which guarantees the
    // winner printed `enable_camera handler finished` before we send
    // shutdown.

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        TEST_CORE_NODE,
        consumer_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        TEST_CORE_NODE,
        exposer1_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        TEST_CORE_NODE,
        exposer2_instance_id,
        Duration::from_secs(5),
    )
    .await;

    // Wait for all processes to exit
    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &user_node_consumer,
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

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );

    let exposer1_stdout = String::from_utf8_lossy(&exposer_output1.stdout).into_owned();
    let exposer1_stderr = String::from_utf8_lossy(&exposer_output1.stderr).into_owned();
    let exposer2_stdout = String::from_utf8_lossy(&exposer_output2.stdout).into_owned();
    let exposer2_stderr = String::from_utf8_lossy(&exposer_output2.stderr).into_owned();

    // Under discover-then-pin, the consumer probes both exposers and pins
    // to whichever responds first; the real request goes only to the
    // winner. The loser must NOT run its handler — that's the load-bearing
    // safety guarantee of the wildcard flow. Either exposer can win the
    // probe race; identify the winner by the response marker the consumer
    // printed (exposer1 emits `error=handled`, exposer2 emits
    // `error=handled_by_exposer2`).
    let expected_request_log = format!(
        "received enable_camera request for {}: true",
        consumer_instance_id
    );
    let consumer_saw_exposer2 = consumer_stdout.contains("error=handled_by_exposer2");
    let consumer_saw_exposer1 = consumer_stdout
        .lines()
        .any(|line| line.trim_end() == "enable_camera result: enabled=true error=handled");
    assert!(
        consumer_saw_exposer1 ^ consumer_saw_exposer2,
        "consumer must have received exactly one response.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    let (winner_stdout, winner_stderr, loser_stdout, loser_stderr, winner_label, loser_label) =
        if consumer_saw_exposer1 {
            (
                &exposer1_stdout,
                &exposer1_stderr,
                &exposer2_stdout,
                &exposer2_stderr,
                "exposer1",
                "exposer2",
            )
        } else {
            (
                &exposer2_stdout,
                &exposer2_stderr,
                &exposer1_stdout,
                &exposer1_stderr,
                "exposer2",
                "exposer1",
            )
        };
    assert!(
        winner_stdout.contains(&expected_request_log)
            && winner_stdout.contains("enable_camera handler finished"),
        "{} won the discover-then-pin race but did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        winner_label,
        winner_stdout,
        winner_stderr
    );
    assert!(
        !loser_stdout.contains(&expected_request_log),
        "{} must NOT process the enable_camera request — discover-then-pin pins the consumer to the first responder before the real request is sent.\nstdout:\n{}\nstderr:\n{}",
        loser_label,
        loser_stdout,
        loser_stderr
    );
}
