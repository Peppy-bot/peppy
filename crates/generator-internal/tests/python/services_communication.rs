use crate::helpers::{
    CapturedChild, ConsumerExposerOutput, DEFAULT_WAIT_TIMEOUT, SOLE_CONSUMER_INSTANCE_ID,
    SOLE_EXPOSER_INSTANCE_ID, STUB_PYTHON_NODE_CONFIG, ServicePair, TEST_CORE_NODE,
    assert_exited_successfully, bind_slot, generate_node_project, init_python_project_venv,
    init_python_user_node, native_dep, python_consumer_stub_node_config, save_runtime_config,
    shut_down_test_node, spawn_python_run, test_node_runtime_config, test_wait_context,
    wait_for_child, wait_for_health_service_reachable_or_exit, wait_for_service_reachable_or_exit,
};
use config::consts::RUNTIME_CONFIG_VAR_NAME;
use generator::LanguageGenerator;
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const CONSUMER_NODE_NAME: &str = "consumer_node";
const CONSUMER_INSTANCE_ID: &str = "consumer_instance";
const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";

/// Runs one consumer of `service` against its one exposer. The consumer's
/// `uvc_camera` slot is bound to the exposer, the exposer starts first, and
/// both nodes are shut down once the consumer printed
/// `expected_consumer_log`. Returns what the two nodes printed; both exited
/// successfully.
async fn run_single_exposer_service_flow(
    topology: crate::helpers::LocalNodesTopology,
    service: ServicePair,
    consumer_main: &str,
    exposer_main: &str,
    expected_consumer_log: &str,
) -> ConsumerExposerOutput {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let temp_dir_consumer = TempDir::new_in(config_test_support::test_tmp_root()).unwrap();
    let user_node_consumer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_consumer,
        &python_consumer_stub_node_config("uvc_camera", "v1", "uvc_camera"),
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
    let consumer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        CONSUMER_NODE_NAME,
        SOLE_CONSUMER_INSTANCE_ID,
    );
    let consumer_runtime_config = bind_slot(
        consumer_runtime_config,
        "uvc_camera",
        TEST_CORE_NODE,
        SOLE_EXPOSER_INSTANCE_ID,
    );
    let consumer_runtime_config = crate::helpers::apply_topology(consumer_runtime_config, topology);
    let consumer_runtime_config_str =
        save_runtime_config(&consumer_runtime_config, temp_dir_consumer.path());
    init_python_user_node(&user_node_consumer);
    fs::write(user_node_consumer.join("main.py"), consumer_main)
        .expect("failed to write consumer main.py");

    // --- Exposer (server) project
    let temp_dir_exposer = TempDir::new_in(config_test_support::test_tmp_root()).unwrap();
    let user_node_exposer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_exposer,
        STUB_PYTHON_NODE_CONFIG,
        |generator| {
            generator
                .add_exposed_service(&service.exposed, None)
                .unwrap();
        },
    );
    let exposer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        UVC_CAMERA_NODE_NAME,
        SOLE_EXPOSER_INSTANCE_ID,
    );
    let exposer_runtime_config = crate::helpers::apply_topology(exposer_runtime_config, topology);
    let exposer_runtime_config_str =
        save_runtime_config(&exposer_runtime_config, temp_dir_exposer.path());
    init_python_user_node(&user_node_exposer);
    fs::write(user_node_exposer.join("main.py"), exposer_main)
        .expect("failed to write exposer main.py");

    init_python_project_venv(&user_node_consumer);
    init_python_project_venv(&user_node_exposer);

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = test_wait_context(&messenger);

    // Spawn exposer first so it's ready to handle requests
    let mut exposer_child = spawn_python_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        &service.exposed.name,
        Some(SOLE_EXPOSER_INSTANCE_ID),
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer = CapturedChild::new(spawn_python_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    ));

    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        SOLE_CONSUMER_INSTANCE_ID,
        &mut consumer.child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        SOLE_EXPOSER_INSTANCE_ID,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    // Consumer's poll runs as a background task created in `setup`. Wait
    // for the result line to appear in its stdout before sending shutdown,
    // otherwise the event loop can stop mid-poll and the print is lost.
    consumer.wait_for_stdout_contains(
        expected_consumer_log,
        Duration::from_secs(10),
        &user_node_consumer,
    );

    shut_down_test_node(&messenger, CONSUMER_NODE_NAME, SOLE_CONSUMER_INSTANCE_ID).await;
    shut_down_test_node(&messenger, UVC_CAMERA_NODE_NAME, SOLE_EXPOSER_INSTANCE_ID).await;

    let consumer_output = consumer.wait(Some(Duration::from_secs(10)), &user_node_consumer);
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer,
    );
    let (consumer_stdout, consumer_stderr) =
        assert_exited_successfully("consumer process", &consumer_output);
    let (exposer_stdout, exposer_stderr) =
        assert_exited_successfully("exposer process", &exposer_output);
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
import asyncio
from peppygen import NodeBuilder
from peppygen.consumed_services.uvc_camera import enable_camera as uvc_camera_enable_camera

async def poll_service(node_runner):
    request = uvc_camera_enable_camera.Request(enable=True)
    camera = uvc_camera_enable_camera.bound_producer(node_runner)
    response = await uvc_camera_enable_camera.poll(node_runner, camera, request, 5.0)
    error_msg = response.data.error_msg if response.data.error_msg is not None else "<none>"
    print(
        f"enable_camera result: service_id={response.instance_id} enabled={response.data.enabled} error={error_msg}",
        flush=True,
    )

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(poll_service(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import enable_camera

async def handle_requests(node_runner):
    def handler(request):
        print(
            f"received enable_camera request from {request.instance_id}: enable = {request.data.enable}",
            flush=True,
        )
        return enable_camera.Response(
            enabled=request.data.enable,
            error_msg="handled",
        )
    await enable_camera.handle_next_request(node_runner, handler)
    print("enable_camera handler finished", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(handle_requests(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let expected_consumer_log = format!(
        "enable_camera result: service_id={SOLE_EXPOSER_INSTANCE_ID} enabled=True error=handled"
    );
    let ConsumerExposerOutput {
        consumer_stdout,
        consumer_stderr,
        exposer_stdout,
        exposer_stderr,
    } = run_single_exposer_service_flow(
        topology,
        service,
        consumer_main,
        exposer_main,
        &expected_consumer_log,
    )
    .await;

    // `wait_for_stdout_contains` in the flow already proved the line was
    // printed, but we re-assert here so the failure points at the right
    // location if the captured-output flow ever regresses.
    assert!(
        consumer_stdout.contains(&expected_consumer_log),
        "consumer did not receive expected service response (expected log: {}).\nstdout:\n{}\nstderr:\n{}",
        expected_consumer_log,
        consumer_stdout,
        consumer_stderr
    );

    let expected_request_log =
        format!("received enable_camera request from {SOLE_CONSUMER_INSTANCE_ID}: enable = True");
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
import asyncio
from peppygen import NodeBuilder
from peppygen.consumed_services.uvc_camera import get_system_status as uvc_camera_get_system_status

async def poll_service(node_runner):
    camera = uvc_camera_get_system_status.bound_producer(node_runner)
    response = await uvc_camera_get_system_status.poll(node_runner, camera, 5.0)
    print(
        f"get_system_status result: service_id={response.instance_id} healthy={response.data.healthy}",
        flush=True,
    )

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(poll_service(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import get_system_status

async def handle_requests(node_runner):
    def handler(request):
        print(
            f"received get_system_status request from {request.instance_id}",
            flush=True,
        )
        return get_system_status.Response(healthy=True)
    await get_system_status.handle_next_request(node_runner, handler)
    print("get_system_status handler finished", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(handle_requests(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let expected_consumer_log =
        format!("get_system_status result: service_id={SOLE_EXPOSER_INSTANCE_ID} healthy=True");
    let ConsumerExposerOutput {
        consumer_stdout,
        consumer_stderr,
        exposer_stdout,
        exposer_stderr,
    } = run_single_exposer_service_flow(
        topology,
        service,
        consumer_main,
        exposer_main,
        &expected_consumer_log,
    )
    .await;

    // `wait_for_stdout_contains` in the flow already proved the line was
    // printed, but we re-assert here so the failure points at the right
    // location if the captured-output flow ever regresses.
    assert!(
        consumer_stdout.contains(&expected_consumer_log),
        "consumer did not receive expected service response (expected log: {}).\nstdout:\n{}\nstderr:\n{}",
        expected_consumer_log,
        consumer_stdout,
        consumer_stderr
    );

    let expected_request_log =
        format!("received get_system_status request from {SOLE_CONSUMER_INSTANCE_ID}");
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

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(config_test_support::test_tmp_root()).unwrap();
    let user_node_consumer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_consumer,
        &python_consumer_stub_node_config("uvc_camera", "v1", "uvc_camera"),
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

    let consumer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
    );
    // Bind the slot to exposer1: only the bound instance may serve the call.
    let consumer_runtime_config = bind_slot(
        consumer_runtime_config,
        "uvc_camera",
        TEST_CORE_NODE,
        "exposer1_instance",
    );
    let consumer_runtime_config = crate::helpers::apply_topology(consumer_runtime_config, topology);
    let consumer_runtime_config_str =
        save_runtime_config(&consumer_runtime_config, temp_dir_consumer.path());

    init_python_user_node(&user_node_consumer);
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.consumed_services.uvc_camera import enable_camera as uvc_camera_enable_camera

async def poll_service(node_runner):
    request = uvc_camera_enable_camera.Request(enable=True)
    camera = uvc_camera_enable_camera.bound_producer(node_runner)
    response = await uvc_camera_enable_camera.poll(node_runner, camera, request, 5.0)
    error_msg = response.data.error_msg if response.data.error_msg is not None else "<none>"
    print(
        f"enable_camera result: enabled={response.data.enabled} error={error_msg}",
        flush=True,
    )

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(poll_service(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let main_file = user_node_consumer.join("main.py");
    fs::write(main_file, consumer_main).expect("failed to write consumer main.py");

    // --- Exposer 1
    let exposer1_instance_id = "exposer1_instance";
    let temp_dir_exposer1 = TempDir::new_in(config_test_support::test_tmp_root()).unwrap();
    let user_node_exposer1 = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_exposer1,
        STUB_PYTHON_NODE_CONFIG,
        |generator| {
            generator
                .add_exposed_service(&service.exposed, None)
                .unwrap();
        },
    );

    let exposer1_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        UVC_CAMERA_NODE_NAME,
        exposer1_instance_id,
    );
    let exposer1_runtime_config = crate::helpers::apply_topology(exposer1_runtime_config, topology);
    let exposer1_runtime_config_str =
        save_runtime_config(&exposer1_runtime_config, temp_dir_exposer1.path());

    init_python_user_node(&user_node_exposer1);
    let exposer1_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import enable_camera

async def handle_requests(node_runner):
    def handler(request):
        print(
            f"received enable_camera request for {request.instance_id}: {request.data.enable}",
            flush=True,
        )
        return enable_camera.Response(
            enabled=request.data.enable,
            error_msg="handled",
        )
    await enable_camera.handle_next_request(node_runner, handler)
    print("enable_camera handler finished", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(handle_requests(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let main_file = user_node_exposer1.join("main.py");
    fs::write(main_file, exposer1_main).expect("failed to write exposer main 1");

    // --- Exposer 2
    let exposer2_instance_id = "exposer2_instance";
    let temp_dir_exposer2 = TempDir::new_in(config_test_support::test_tmp_root()).unwrap();
    let user_node_exposer2 = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_exposer2,
        STUB_PYTHON_NODE_CONFIG,
        |generator| {
            generator
                .add_exposed_service(&service.exposed, None)
                .unwrap();
        },
    );

    let exposer2_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        UVC_CAMERA_NODE_NAME,
        exposer2_instance_id,
    );
    let exposer2_runtime_config = crate::helpers::apply_topology(exposer2_runtime_config, topology);
    let exposer2_runtime_config_str =
        save_runtime_config(&exposer2_runtime_config, temp_dir_exposer2.path());

    init_python_user_node(&user_node_exposer2);
    let exposer2_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import enable_camera

async def handle_requests(node_runner):
    def handler(request):
        print(
            f"received enable_camera request for {request.instance_id}: {request.data.enable}",
            flush=True,
        )
        return enable_camera.Response(
            enabled=request.data.enable,
            error_msg="handled_by_exposer2",
        )
    await enable_camera.handle_next_request(node_runner, handler)
    print("enable_camera handler finished", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(handle_requests(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let main_file = user_node_exposer2.join("main.py");
    fs::write(main_file, exposer2_main).expect("failed to write exposer main 2");

    // Install dependencies for all projects
    init_python_project_venv(&user_node_consumer);
    init_python_project_venv(&user_node_exposer1);
    init_python_project_venv(&user_node_exposer2);

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = test_wait_context(&messenger);

    // Spawn both exposers first so they're ready to handle requests
    let mut exposer1_child = spawn_python_run(
        &user_node_exposer1,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer1_runtime_config_str)],
    );
    let mut exposer2_child = spawn_python_run(
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

    let mut consumer = CapturedChild::new(spawn_python_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    ));

    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer.child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    // Do NOT wait for health on the exposers here. Only the bound
    // exposer's `handle_next_request` returns and lets `setup` complete;
    // the unbound one stays parked on its queryable, so
    // `run_post_setup_services` (which registers the health endpoint)
    // never runs there. Probing health on it would panic with a
    // wait-timeout instead of hanging, but it's still the wrong question
    // to ask. The pre-setup shutdown queryable is up on both exposers, so
    // the shutdown requests below land cleanly. The
    // `wait_for_stdout_contains` below guarantees the bound exposer
    // already produced the response that the consumer's poll observed.

    // Consumer's poll runs as a background task created in `setup`. Wait
    // for the result line to appear in its stdout before sending shutdown,
    // otherwise the event loop can stop mid-poll and the print is lost.
    let response_prefix = "enable_camera result: enabled=True error=handled";
    consumer.wait_for_stdout_contains(
        response_prefix,
        Duration::from_secs(10),
        &user_node_consumer,
    );

    shut_down_test_node(&messenger, CONSUMER_NODE_NAME, consumer_instance_id).await;
    shut_down_test_node(&messenger, UVC_CAMERA_NODE_NAME, exposer1_instance_id).await;
    shut_down_test_node(&messenger, UVC_CAMERA_NODE_NAME, exposer2_instance_id).await;

    let consumer_output = consumer.wait(Some(Duration::from_secs(10)), &user_node_consumer);
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

    let (consumer_stdout, consumer_stderr) =
        assert_exited_successfully("consumer process", &consumer_output);

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
        "received enable_camera request for {}: True",
        consumer_instance_id
    );
    let consumer_saw_exposer2 = consumer_stdout.contains("error=handled_by_exposer2");
    let consumer_saw_exposer1 = consumer_stdout
        .lines()
        .any(|line| line.trim_end() == "enable_camera result: enabled=True error=handled");
    assert!(
        consumer_saw_exposer1 && !consumer_saw_exposer2,
        "consumer must have received its response from the bound exposer1 only.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    assert!(
        exposer1_stdout.contains(&expected_request_log),
        "exposer1 is the bound producer but did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        exposer1_stdout,
        exposer1_stderr
    );
    assert!(
        !exposer2_stdout.contains(&expected_request_log),
        "exposer2 must NOT process the enable_camera request: the slot is bound to exposer1 and there is no wildcard fallback.\nstdout:\n{}\nstderr:\n{}",
        exposer2_stdout,
        exposer2_stderr
    );
}
