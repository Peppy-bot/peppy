use crate::helpers::{
    ConsumerExposerOutput, DEFAULT_WAIT_TIMEOUT, NodeProject, SOLE_CONSUMER_INSTANCE_ID,
    SOLE_EXPOSER_INSTANCE_ID, STUB_NODE_CONFIG, ServicePair, TEST_CORE_NODE,
    assert_exited_successfully, bind_slot, compile_project, consumer_stub_node_config,
    generate_node_project, init_cargo_user_node, native_dep, save_runtime_config,
    shut_down_test_node, spawn_cargo_run, test_node_runtime_config, test_wait_context,
    wait_for_child, wait_for_health_service_reachable_or_exit, wait_for_service_reachable_or_exit,
};
use config::consts::RUNTIME_CONFIG_VAR_NAME;
use config::node::NativeExposedService;
use generator::LanguageGenerator;
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const CONSUMER_NODE_NAME: &str = "consumer_node";
const CONSUMER_INSTANCE_ID: &str = "consumer_instance";
const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";

/// Generates the consumer of `service`, deployed as the `instance_id`
/// instance of the consumer node with its `uvc_camera` slot bound to the
/// `bound_exposer_instance` instance, and writes `consumer_main` as its main.
fn prepare_uvc_camera_consumer(
    router_host: &str,
    router_port: u16,
    topology: crate::helpers::LocalNodesTopology,
    service: &ServicePair,
    instance_id: &str,
    bound_exposer_instance: &str,
    consumer_main: &str,
) -> NodeProject {
    let temp_dir = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let dir = generate_node_project::<generator::RustGenerator>(
        &temp_dir,
        &consumer_stub_node_config("uvc_camera", "v1", "uvc_camera"),
        |generator| {
            generator
                .add_consumed_service(
                    &service.consumed,
                    &service.request_format,
                    &service.response_format,
                    &native_dep("uvc_camera", "v1", "uvc_camera"),
                )
                .unwrap();
        },
    );
    let runtime_config =
        test_node_runtime_config(router_host, router_port, CONSUMER_NODE_NAME, instance_id);
    let runtime_config = bind_slot(
        runtime_config,
        "uvc_camera",
        TEST_CORE_NODE,
        bound_exposer_instance,
    );
    let runtime_config = crate::helpers::apply_topology(runtime_config, topology);
    let runtime_config = save_runtime_config(&runtime_config, temp_dir.path());
    init_cargo_user_node(&dir);
    fs::write(dir.join("src").join("main.rs"), consumer_main)
        .expect("failed to write consumer main");
    NodeProject::new(temp_dir, dir, runtime_config)
}

/// Generates an exposer of `exposed`, deployed as the `instance_id` instance
/// of the `uvc_camera` node the consumers' slot depends on, and writes
/// `exposer_main` as its main.
fn prepare_uvc_camera_exposer(
    router_host: &str,
    router_port: u16,
    topology: crate::helpers::LocalNodesTopology,
    exposed: &NativeExposedService,
    instance_id: &str,
    exposer_main: &str,
) -> NodeProject {
    let temp_dir = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
    let dir = generate_node_project::<generator::RustGenerator>(
        &temp_dir,
        STUB_NODE_CONFIG,
        |generator| {
            generator.add_exposed_service(exposed, None).unwrap();
        },
    );
    let runtime_config =
        test_node_runtime_config(router_host, router_port, UVC_CAMERA_NODE_NAME, instance_id);
    let runtime_config = crate::helpers::apply_topology(runtime_config, topology);
    let runtime_config = save_runtime_config(&runtime_config, temp_dir.path());
    init_cargo_user_node(&dir);
    fs::write(dir.join("src").join("main.rs"), exposer_main).expect("failed to write exposer main");
    NodeProject::new(temp_dir, dir, runtime_config)
}

/// Runs one consumer of `service` against its one exposer, over a router of
/// its own. The consumer's `uvc_camera` slot is bound to the exposer, the
/// exposer starts first, and both nodes are shut down once both health
/// services answer: a node registers its health service only after its setup
/// returned, so by then the consumer has made its call. Returns what the two
/// nodes printed; both exited successfully.
async fn run_single_exposer_service_flow(
    topology: crate::helpers::LocalNodesTopology,
    service: ServicePair,
    consumer_main: &str,
    exposer_main: &str,
) -> ConsumerExposerOutput {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);
    let consumer = prepare_uvc_camera_consumer(
        &router_host,
        router_port,
        topology,
        &service,
        SOLE_CONSUMER_INSTANCE_ID,
        SOLE_EXPOSER_INSTANCE_ID,
        consumer_main,
    );
    let exposer = prepare_uvc_camera_exposer(
        &router_host,
        router_port,
        topology,
        &service.exposed,
        SOLE_EXPOSER_INSTANCE_ID,
        exposer_main,
    );
    compile_project(&consumer.dir);
    compile_project(&exposer.dir);

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = test_wait_context(&messenger);

    // Spawn exposer first so it's ready to handle requests
    let mut exposer_child = spawn_cargo_run(
        &exposer.dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer.runtime_config)],
    );
    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        &service.exposed.name,
        Some(SOLE_EXPOSER_INSTANCE_ID),
        &mut exposer_child,
        &exposer.dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer_child = spawn_cargo_run(
        &consumer.dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer.runtime_config)],
    );
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        SOLE_CONSUMER_INSTANCE_ID,
        &mut consumer_child,
        &consumer.dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        SOLE_EXPOSER_INSTANCE_ID,
        &mut exposer_child,
        &exposer.dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    shut_down_test_node(&messenger, CONSUMER_NODE_NAME, SOLE_CONSUMER_INSTANCE_ID).await;
    shut_down_test_node(&messenger, UVC_CAMERA_NODE_NAME, SOLE_EXPOSER_INSTANCE_ID).await;

    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &consumer.dir,
    );
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &exposer.dir,
    );
    let (consumer_stdout, consumer_stderr) =
        assert_exited_successfully("consumer cargo run", &consumer_output);
    let (exposer_stdout, exposer_stderr) =
        assert_exited_successfully("exposer cargo run", &exposer_output);
    ConsumerExposerOutput {
        consumer_stdout,
        consumer_stderr,
        exposer_stdout,
        exposer_stderr,
    }
}

#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_no_target_instance_id(
    #[case] topology: crate::helpers::LocalNodesTopology,
) {
    let service = ServicePair::enable_camera();
    let consumer_main = r#"
use peppygen::consumed_services::uvc_camera::enable_camera as uvc_camera_enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let camera = uvc_camera_enable_camera::bound_producer(&node_runner);
        let request = uvc_camera_enable_camera::Request::new(true);
        let response =
            uvc_camera_enable_camera::poll(&node_runner, camera, Duration::from_secs(5), request)
                .await?;
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
    let ConsumerExposerOutput {
        consumer_stdout,
        consumer_stderr,
        exposer_stdout,
        exposer_stderr,
    } = run_single_exposer_service_flow(topology, service, consumer_main, exposer_main).await;
    let expected_consumer_log = format!(
        "enable_camera result: service_id={} enabled=true error=handled",
        SOLE_EXPOSER_INSTANCE_ID
    );
    assert!(
        consumer_stdout.contains(&expected_consumer_log),
        "consumer did not receive expected service response (expected log: {}).\nstdout:\n{}\nstderr:\n{}",
        expected_consumer_log,
        consumer_stdout,
        consumer_stderr
    );

    let expected_request_log = format!(
        "received enable_camera request from {}: enable = true",
        SOLE_CONSUMER_INSTANCE_ID
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
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_exposed_service_without_request_body(
    #[case] topology: crate::helpers::LocalNodesTopology,
) {
    let service = ServicePair::get_system_status();
    let consumer_main = r#"
use peppygen::consumed_services::uvc_camera::get_system_status as uvc_camera_get_system_status;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let camera = uvc_camera_get_system_status::bound_producer(&node_runner);
        let response =
            uvc_camera_get_system_status::poll(&node_runner, camera, Duration::from_secs(5))
                .await?;
        println!(
            "get_system_status result: service_id={} healthy={}",
            &response.instance_id,
            response.data.healthy
        );
        Ok(())
    })
}
"#;
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
    let ConsumerExposerOutput {
        consumer_stdout,
        consumer_stderr,
        exposer_stdout,
        exposer_stderr,
    } = run_single_exposer_service_flow(topology, service, consumer_main, exposer_main).await;
    let expected_consumer_log = format!(
        "get_system_status result: service_id={} healthy=true",
        SOLE_EXPOSER_INSTANCE_ID
    );
    assert!(
        consumer_stdout.contains(&expected_consumer_log),
        "consumer did not receive expected service response (expected log: {}).\nstdout:\n{}\nstderr:\n{}",
        expected_consumer_log,
        consumer_stdout,
        consumer_stderr
    );

    let expected_request_log = format!(
        "received get_system_status request from {}",
        SOLE_CONSUMER_INSTANCE_ID
    );
    assert!(
        exposer_stdout.contains(&expected_request_log)
            && exposer_stdout.contains("get_system_status handler finished"),
        "exposer did not process the get_system_status request.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// Two instances expose the same service; the consumer's slot binding routes
/// the request to the bound instance, and the other instance never runs its
/// handler.
#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_multiple_exposed_instances_bound_slot_routes_to_bound_one(
    #[case] topology: crate::helpers::LocalNodesTopology,
) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let service = ServicePair::enable_camera();
    let exposer1_instance_id = "exposer1_instance";
    let exposer2_instance_id = "exposer2_instance";

    // --- Consumer (client) project, bound to exposer1: only the bound
    // instance may serve the call.
    let consumer_main = r#"
use peppygen::consumed_services::uvc_camera::enable_camera as uvc_camera_enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let camera = uvc_camera_enable_camera::bound_producer(&node_runner);
        let request = uvc_camera_enable_camera::Request::new(true);
        let response =
            uvc_camera_enable_camera::poll(&node_runner, camera, Duration::from_secs(5), request)
                .await?;
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
    let consumer = prepare_uvc_camera_consumer(
        &router_host,
        router_port,
        topology,
        &service,
        CONSUMER_INSTANCE_ID,
        exposer1_instance_id,
        consumer_main,
    );

    // --- Exposer 1
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
    let exposer1 = prepare_uvc_camera_exposer(
        &router_host,
        router_port,
        topology,
        &service.exposed,
        exposer1_instance_id,
        exposer1_main,
    );

    // --- Exposer 2
    let exposer2_main = r#"
use peppygen::exposed_services::enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        enable_camera::handle_next_request(&node_runner, |request| -> Result<enable_camera::Response> {
            println!("received enable_camera request for {}: {}", request.instance_id, request.data.enable);
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
    let exposer2 = prepare_uvc_camera_exposer(
        &router_host,
        router_port,
        topology,
        &service.exposed,
        exposer2_instance_id,
        exposer2_main,
    );

    // Compilation + execution
    compile_project(&consumer.dir);
    compile_project(&exposer1.dir);
    compile_project(&exposer2.dir);

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = test_wait_context(&messenger);

    // Spawn both exposers first so they're ready to handle requests
    let mut exposer1_child = spawn_cargo_run(
        &exposer1.dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer1.runtime_config)],
    );
    let mut exposer2_child = spawn_cargo_run(
        &exposer2.dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer2.runtime_config)],
    );

    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        Some(exposer1_instance_id),
        &mut exposer1_child,
        &exposer1.dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        Some(exposer2_instance_id),
        &mut exposer2_child,
        &exposer2.dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer_child = spawn_cargo_run(
        &consumer.dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer.runtime_config)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        CONSUMER_INSTANCE_ID,
        &mut consumer_child,
        &consumer.dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    // Do NOT wait for health on the exposers here. The consumer's slot is
    // bound to exposer1, so exposer2 never receives a request: its
    // `handle_next_request` never returns, `setup_fn` never completes, and
    // `run_post_setup_services` (which registers the health endpoint) never
    // runs there, so probing health on exposer2 would panic with a
    // wait-timeout. The pre-setup shutdown queryable is up on both
    // exposers, so the shutdown requests below land cleanly. The
    // consumer's health probe above already implies the response
    // round-trip completed, which guarantees exposer1 printed
    // `enable_camera handler finished` before we send shutdown.

    shut_down_test_node(&messenger, CONSUMER_NODE_NAME, CONSUMER_INSTANCE_ID).await;
    shut_down_test_node(&messenger, UVC_CAMERA_NODE_NAME, exposer1_instance_id).await;
    shut_down_test_node(&messenger, UVC_CAMERA_NODE_NAME, exposer2_instance_id).await;

    // Wait for all processes to exit
    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &consumer.dir,
    );
    let exposer_output1 = wait_for_child(
        &mut exposer1_child,
        Some(Duration::from_secs(10)),
        &exposer1.dir,
    );
    let exposer_output2 = wait_for_child(
        &mut exposer2_child,
        Some(Duration::from_secs(10)),
        &exposer2.dir,
    );

    let (consumer_stdout, consumer_stderr) =
        assert_exited_successfully("consumer cargo run", &consumer_output);

    let exposer1_stdout = String::from_utf8_lossy(&exposer_output1.stdout).into_owned();
    let exposer1_stderr = String::from_utf8_lossy(&exposer_output1.stderr).into_owned();
    let exposer2_stdout = String::from_utf8_lossy(&exposer_output2.stdout).into_owned();
    let exposer2_stderr = String::from_utf8_lossy(&exposer_output2.stderr).into_owned();

    // The consumer's slot is bound to exposer1, so exposer1 must serve
    // the request and exposer2 must NEVER run its handler; that's the
    // load-bearing safety guarantee of explicit bindings (no discovery
    // race, no wildcard fallback). Identify the responder by the response
    // marker the consumer printed (exposer1 emits `error=handled`,
    // exposer2 emits `error=handled_by_exposer2`).
    let expected_request_log = format!(
        "received enable_camera request for {}: true",
        CONSUMER_INSTANCE_ID
    );
    let consumer_saw_exposer2 = consumer_stdout.contains("error=handled_by_exposer2");
    let consumer_saw_exposer1 = consumer_stdout
        .lines()
        .any(|line| line.trim_end() == "enable_camera result: enabled=true error=handled");
    assert!(
        consumer_saw_exposer1 && !consumer_saw_exposer2,
        "consumer must have received its response from the bound exposer1 only.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    assert!(
        exposer1_stdout.contains(&expected_request_log)
            && exposer1_stdout.contains("enable_camera handler finished"),
        "exposer1 is the bound producer but did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        exposer1_stdout,
        exposer1_stderr
    );
    assert!(
        !exposer2_stdout.contains(&expected_request_log),
        "exposer2 must NOT process the enable_camera request: the slot is bound to the other exposer and there is no wildcard fallback.\nstdout:\n{}\nstderr:\n{}",
        exposer2_stdout,
        exposer2_stderr
    );
}
