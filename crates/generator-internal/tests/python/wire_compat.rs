//! End-to-end tests that an exposing/emitting node and its consumer
//! agree on the capnp wire format for each interface kind (actions,
//! services, topics): spawn both, drive a single round of
//! communication, and check that the consumer decodes the payload the
//! producer sent.
//!
//! Each test is sensitive to producer/consumer schema mismatches via
//! two design choices applied uniformly across all three:
//!
//! 1. The exposed/emitted `MessageFormat` for the payload under test
//!    declares its two pointer-typed fields (one `Text` and one
//!    `List(Float64)`) in an order whose alphabetical sort swaps them.
//!    Any step that silently re-orders the MessageFormat on one side
//!    but not the other will land Text where the other side expects
//!    List, surfacing as a `Schema mismatch: Message contains list
//!    pointer of non-bytes where text was expected` -class decode
//!    error on the consumer.
//!
//! 2. Producer and consumer reach the generator through different
//!    pipelines, mirroring how a real deployment resolves them: the
//!    producer's `NodeConfig` is serialised to JSON5 and re-parsed
//!    before reaching `generate_peppygen_lib` (the variant-sync flow
//!    stages a merged config in a temp file the generator re-parses),
//!    while the consumer's `DeploymentInterface` is built directly
//!    from the in-memory parsed `NodeConfig` (a dependency's
//!    interfaces are normally resolved from the in-memory node stack).
//!
//! Python rather than Rust to avoid the per-spawn `cargo build` cost
//! -- the capnp schema generation under test is language-agnostic.

use crate::helpers::{
    DEFAULT_WAIT_TIMEOUT, WaitContext, init_python_project_venv, init_python_user_node,
    send_shutdown, spawn_python_run, test_peppy_dirs, wait_for_action_service_reachable_or_exit,
    wait_for_child, wait_for_health_service_reachable_or_exit, wait_for_service_reachable_or_exit,
};
use config::consts::{NODE_CONFIG_FILE, RUNTIME_CONFIG_VAR_NAME};
use config::launcher::Name;
use config::node::{
    ConsumedAction, ConsumedService, ConsumedTopic, NodeConfigParser, PeppygenLanguage,
};
use config::runtime::{NodeInstanceConfig, RuntimeConfig};
use generator::{
    ConsumedActionMessage, DeploymentInterface, InterfaceVariant, generate_peppygen_lib,
};
use std::path::Path;
use std::{fs, time::Duration};
use tempfile::TempDir;

const TEST_CORE_NODE: &str = "test_core";
const PRODUCER_NODE_NAME: &str = "producer";
const CONSUMER_NODE_NAME: &str = "consumer";
const PRODUCER_INSTANCE_ID: &str = "producer_instance";
const CONSUMER_INSTANCE_ID: &str = "consumer_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";

/// Mirror the variant-sync flow on the producer side: parse the source
/// config, serialise it to JSON5, then write the result to disk for the
/// generator to re-parse from a file.
fn write_producer_config_via_round_trip(producer_config_json5: &str, producer_dir: &Path) {
    let parsed =
        NodeConfigParser::from_content(producer_config_json5).expect("producer config parses");
    let pretty = json5_pretty::to_string_pretty(&parsed).expect("producer config pretty-prints");
    fs::write(producer_dir.join(NODE_CONFIG_FILE), pretty).expect("write producer peppy.json5");
}

/// Parse the producer's source JSON5 and clone the resolved
/// `NodeConfig`. Used by the consumer side to build its
/// `DeploymentInterface` without going through any JSON5 round-trip
/// -- mirroring how a dependency's interfaces are resolved from the
/// in-memory node stack.
fn parse_producer_config_in_memory(producer_config_json5: &str) -> config::node::NodeConfig {
    NodeConfigParser::from_content(producer_config_json5)
        .expect("producer config parses for consumer view")
}

fn build_runtime_config(
    router_host: &str,
    router_port: u16,
    instance_id: &str,
    node_name: &str,
    runtime_config_path: &Path,
) {
    let cfg = RuntimeConfig::new(
        router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(instance_id).unwrap()),
        node_name,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    cfg.save_json5_launch_config(runtime_config_path).unwrap();
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

const ACTION_NAME: &str = "perform_scan";
const ACTION_RESULT_RECEIVED_SERVICE: &str = "result_received";
// `result_service.response_message_format` is intentionally declared in
// an order whose alphabetical sort swaps its two pointer-typed fields
// (`status: Text`, `measurements: List(Float64)`); see module docstring.
const ACTION_PRODUCER_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "producer",
    tag: "v1"
  },
  interfaces: {
    actions: {
      exposes: [
        {
          name: "perform_scan",
          goal_service: {
            request_message_format: { scan_id: "u32" },
            response_message_format: { accepted: "bool" }
          },
          feedback_topic: {
            qos_profile: "sensor_data",
            message_format: { progress: "u8" }
          },
          result_service: {
            response_message_format: {
              success: "bool",
              status: "string",
              measurements: { $type: "array", $items: "f64" },
              duration: "f64"
            }
          }
        }
      ]
    }
  },
  execution: {
    language: "python",
    run_cmd: ["uv", "run", "python", "main.py"]
  }
}"#;

const ACTION_CONSUMER_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "consumer",
    tag: "v1",
    depends_on: {
      nodes: [
        { name: "producer", tag: "v1", link_id: "producer" }
      ]
    }
  },
  interfaces: {
    actions: {
      consumes: [
        { link_id: "producer", name: "perform_scan" }
      ]
    },
    services: {
      exposes: [
        { name: "result_received" }
      ]
    }
  },
  execution: {
    language: "python",
    run_cmd: ["uv", "run", "python", "main.py"]
  }
}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_preserve_result_field_order() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Producer (server) project
    let temp_dir_producer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for producer project");
    let producer_dir = temp_dir_producer.path().join("user_node");
    fs::create_dir_all(&producer_dir).unwrap();
    write_producer_config_via_round_trip(ACTION_PRODUCER_CONFIG, &producer_dir);

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        &producer_dir,
        Vec::new(),
        "test-hash",
        &test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("generate peppygen for producer");

    let producer_runtime_config_path = temp_dir_producer.path().join("peppy_runtime.json5");
    build_runtime_config(
        &router_host,
        router_port,
        PRODUCER_INSTANCE_ID,
        PRODUCER_NODE_NAME,
        &producer_runtime_config_path,
    );

    init_python_user_node(&producer_dir);
    // Construct `ResultResponse` with keyword arguments so this source
    // compiles regardless of how the generated constructor orders its
    // positional parameters. The bug we're catching is on the wire, not
    // in the class layout.
    let producer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import perform_scan

async def run_exposer(node_runner):
    action = await perform_scan.ActionHandle.expose(node_runner)

    def goal_handler(request):
        print(f"server received scan goal scan_id={request.data.scan_id}", flush=True)
        return perform_scan.GoalResponse.accept()

    while True:
        ctx = await action.handle_goal_next_request(goal_handler)
        if ctx is None:
            break

        await ctx.publish_feedback(7)
        print("server emitted feedback progress=7", flush=True)

        # Keyword args so this source compiles regardless of the generated
        # parameter order; the bug we're catching is on the wire.
        print("server preparing scan result", flush=True)
        await ctx.complete(
            success=True,
            status="completed",
            measurements=[1.5, 2.5, 3.5],
            duration=42.0,
        )
        print("server handled scan result request", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    fs::write(producer_dir.join("main.py"), producer_main).expect("write producer main.py");

    // --- Consumer (client) project
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let consumer_dir = temp_dir_consumer.path().join("user_node");
    fs::create_dir_all(&consumer_dir).unwrap();
    fs::write(consumer_dir.join(NODE_CONFIG_FILE), ACTION_CONSUMER_CONFIG)
        .expect("write consumer peppy.json5");

    let exposed_action = parse_producer_config_in_memory(ACTION_PRODUCER_CONFIG)
        .interfaces
        .actions
        .as_ref()
        .and_then(|a| a.exposes.as_ref())
        .and_then(|v| v.iter().find(|a| a.name == ACTION_NAME))
        .cloned()
        .expect("exposed action present in producer config");

    let consumed_action_messages = ConsumedActionMessage {
        goal_request: exposed_action
            .goal_service
            .as_ref()
            .and_then(|s| s.request_message_format.clone()),
        feedback: exposed_action
            .feedback_topic
            .as_ref()
            .and_then(|t| t.message_format.clone()),
        result_response: exposed_action
            .result_service
            .as_ref()
            .and_then(|s| s.response_message_format.clone()),
    };
    let consumed_action: ConsumedAction = serde_json5::from_str(&format!(
        r#"{{ link_id: "{PRODUCER_NODE_NAME}", name: "{ACTION_NAME}" }}"#
    ))
    .unwrap();
    let consumed_interface = DeploymentInterface::new(InterfaceVariant::ConsumedAction {
        action: consumed_action,
        messages: consumed_action_messages,
        dependency: generator::DependencyContext::native(PRODUCER_NODE_NAME, "v1"),
    });

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        &consumer_dir,
        vec![consumed_interface],
        "test-hash",
        &test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("generate peppygen for consumer");

    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    build_runtime_config(
        &router_host,
        router_port,
        CONSUMER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        &consumer_runtime_config_path,
    );

    init_python_user_node(&consumer_dir);
    // The consumer drives the full goal/feedback/result cycle, then
    // exposes `result_received` to signal the test that all three decodes
    // succeeded. Without this handshake, the test would race shutdown
    // against the consumer's three sequential awaits and intermittently
    // tear down a pending task under parallel load.
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder, QoSProfile
from peppygen.exposed_services import result_received
from peppygen.consumed_actions import producer_perform_scan

async def consume_action(node_runner, done):
    request = producer_perform_scan.GoalRequest(scan_id=7)
    goal = await producer_perform_scan.ActionHandle.fire_goal(
        node_runner, request, 5.0, QoSProfile.SensorData
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    feedback = await goal.on_next_feedback_message()
    print(f"feedback message received progress={feedback.progress}", flush=True)

    result = await goal.get_result(5.0)
    assert result.status == producer_perform_scan.ResultStatus.COMPLETED, f"unexpected status {result.status}"
    print(
        f"result success={result.data.success} status={result.data.status} "
        f"measurements={result.data.measurements} duration={result.data.duration}",
        flush=True,
    )
    done.set()

async def ack_when_done(node_runner, done):
    await done.wait()
    await result_received.handle_next_request(node_runner, lambda _: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    done = asyncio.Event()
    return [
        asyncio.create_task(consume_action(node_runner, done)),
        asyncio.create_task(ack_when_done(node_runner, done)),
    ]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    fs::write(consumer_dir.join("main.py"), consumer_main).expect("write consumer main.py");

    init_python_project_venv(&producer_dir);
    init_python_project_venv(&consumer_dir);

    let producer_runtime_config_str = producer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut producer_child = spawn_python_run(
        &producer_dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &producer_runtime_config_str)],
    );

    wait_for_action_service_reachable_or_exit(
        &WaitContext {
            messenger: &messenger,
            bound_core_node: TEST_CORE_NODE,
            caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
            target_core_node: TEST_CORE_NODE,
        },
        PRODUCER_NODE_NAME,
        ACTION_NAME,
        None,
        &mut producer_child,
        &producer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer_child = spawn_python_run(
        &consumer_dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: TEST_CORE_NODE,
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        CONSUMER_INSTANCE_ID,
        &mut consumer_child,
        &consumer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        PRODUCER_NODE_NAME,
        PRODUCER_INSTANCE_ID,
        &mut producer_child,
        &producer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    // Block until the consumer has decoded goal, feedback, and result and
    // exposed its ack service. This converts a shutdown-vs-workflow race
    // into a deterministic handshake.
    wait_for_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        ACTION_RESULT_RECEIVED_SERVICE,
        Some(CONSUMER_INSTANCE_ID),
        &mut consumer_child,
        &consumer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        TEST_CORE_NODE,
        CONSUMER_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        PRODUCER_NODE_NAME,
        TEST_CORE_NODE,
        PRODUCER_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;

    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &consumer_dir,
    );
    let producer_output = wait_for_child(
        &mut producer_child,
        Some(Duration::from_secs(10)),
        &producer_dir,
    );

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer process failed -- likely producer/consumer capnp wire layout \
         disagreement for the action's `result_response`.\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    assert!(
        consumer_stdout.contains("goal accepted=True")
            && consumer_stdout.contains("feedback message received progress=7")
            && consumer_stdout.contains(
                "result success=True status=completed measurements=[1.5, 2.5, 3.5] duration=42.0"
            ),
        "consumer did not decode the action result correctly.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    let producer_stdout = String::from_utf8_lossy(&producer_output.stdout).into_owned();
    let producer_stderr = String::from_utf8_lossy(&producer_output.stderr).into_owned();
    assert!(
        producer_output.status.success(),
        "producer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        producer_output.status.code(),
        producer_stdout,
        producer_stderr
    );
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

const SERVICE_NAME: &str = "report_status";
const SERVICE_RESPONSE_RECEIVED_SERVICE: &str = "response_received";
// `response_message_format` is intentionally declared in an order whose
// alphabetical sort swaps its two pointer-typed fields (`status: Text`,
// `measurements: List(Float64)`); see module docstring.
const SERVICE_PRODUCER_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "producer",
    tag: "v1"
  },
  interfaces: {
    services: {
      exposes: [
        {
          name: "report_status",
          request_message_format: { detail: "bool" },
          response_message_format: {
            ok: "bool",
            status: "string",
            measurements: { $type: "array", $items: "f64" },
            elapsed: "f64"
          }
        }
      ]
    }
  },
  execution: {
    language: "python",
    run_cmd: ["uv", "run", "python", "main.py"]
  }
}"#;

const SERVICE_CONSUMER_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "consumer",
    tag: "v1",
    depends_on: {
      nodes: [
        { name: "producer", tag: "v1", link_id: "producer" }
      ]
    }
  },
  interfaces: {
    services: {
      consumes: [
        { link_id: "producer", name: "report_status" }
      ],
      exposes: [
        { name: "response_received" }
      ]
    }
  },
  execution: {
    language: "python",
    run_cmd: ["uv", "run", "python", "main.py"]
  }
}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_preserve_response_field_order() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Producer (server) project
    let temp_dir_producer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for producer project");
    let producer_dir = temp_dir_producer.path().join("user_node");
    fs::create_dir_all(&producer_dir).unwrap();
    write_producer_config_via_round_trip(SERVICE_PRODUCER_CONFIG, &producer_dir);

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        &producer_dir,
        Vec::new(),
        "test-hash",
        &test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("generate peppygen for producer");

    let producer_runtime_config_path = temp_dir_producer.path().join("peppy_runtime.json5");
    build_runtime_config(
        &router_host,
        router_port,
        PRODUCER_INSTANCE_ID,
        PRODUCER_NODE_NAME,
        &producer_runtime_config_path,
    );

    init_python_user_node(&producer_dir);
    let producer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import report_status

async def handle_requests(node_runner):
    def handler(request):
        print(f"server received report_status request detail={request.data.detail}", flush=True)
        return report_status.Response(
            ok=True,
            status="nominal",
            measurements=[0.1, 0.2, 0.3],
            elapsed=12.5,
        )
    await report_status.handle_next_request(node_runner, handler)
    print("server handled report_status request", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(handle_requests(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    fs::write(producer_dir.join("main.py"), producer_main).expect("write producer main.py");

    // --- Consumer (client) project
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let consumer_dir = temp_dir_consumer.path().join("user_node");
    fs::create_dir_all(&consumer_dir).unwrap();
    fs::write(consumer_dir.join(NODE_CONFIG_FILE), SERVICE_CONSUMER_CONFIG)
        .expect("write consumer peppy.json5");

    let exposed_service = parse_producer_config_in_memory(SERVICE_PRODUCER_CONFIG)
        .interfaces
        .services
        .as_ref()
        .and_then(|s| s.exposes.as_ref())
        .and_then(|v| v.iter().find(|s| s.name == SERVICE_NAME))
        .cloned()
        .expect("exposed service present in producer config");

    let consumed_service: ConsumedService = serde_json5::from_str(&format!(
        r#"{{ link_id: "{PRODUCER_NODE_NAME}", name: "{SERVICE_NAME}" }}"#
    ))
    .unwrap();
    let consumed_interface = DeploymentInterface::new(InterfaceVariant::ConsumedService {
        service: consumed_service,
        request_format: exposed_service
            .request_message_format
            .clone()
            .unwrap_or_default(),
        response_format: exposed_service
            .response_message_format
            .clone()
            .unwrap_or_default(),
        dependency: generator::DependencyContext::native(PRODUCER_NODE_NAME, "v1"),
    });

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        &consumer_dir,
        vec![consumed_interface],
        "test-hash",
        &test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("generate peppygen for consumer");

    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    build_runtime_config(
        &router_host,
        router_port,
        CONSUMER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        &consumer_runtime_config_path,
    );

    init_python_user_node(&consumer_dir);
    // The consumer polls once, then exposes `response_received` to signal
    // the test that the decode succeeded. The single poll has a narrow
    // race window vs. shutdown but the handshake removes it entirely.
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import response_received
from peppygen.consumed_services import producer_report_status

async def poll_service(node_runner, done):
    request = producer_report_status.Request(detail=True)
    response = await producer_report_status.poll(node_runner, request, 5.0)
    print(
        f"response ok={response.data.ok} status={response.data.status} "
        f"measurements={response.data.measurements} elapsed={response.data.elapsed}",
        flush=True,
    )
    done.set()

async def ack_when_done(node_runner, done):
    await done.wait()
    await response_received.handle_next_request(node_runner, lambda _: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    done = asyncio.Event()
    return [
        asyncio.create_task(poll_service(node_runner, done)),
        asyncio.create_task(ack_when_done(node_runner, done)),
    ]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    fs::write(consumer_dir.join("main.py"), consumer_main).expect("write consumer main.py");

    init_python_project_venv(&producer_dir);
    init_python_project_venv(&consumer_dir);

    let producer_runtime_config_str = producer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut producer_child = spawn_python_run(
        &producer_dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &producer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: TEST_CORE_NODE,
    };
    wait_for_service_reachable_or_exit(
        &ctx,
        PRODUCER_NODE_NAME,
        SERVICE_NAME,
        Some(PRODUCER_INSTANCE_ID),
        &mut producer_child,
        &producer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer_child = spawn_python_run(
        &consumer_dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        CONSUMER_INSTANCE_ID,
        &mut consumer_child,
        &consumer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        PRODUCER_NODE_NAME,
        PRODUCER_INSTANCE_ID,
        &mut producer_child,
        &producer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    // Block until the consumer has decoded the response and exposed its
    // ack service. This converts a shutdown-vs-poll race into a
    // deterministic handshake.
    wait_for_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        SERVICE_RESPONSE_RECEIVED_SERVICE,
        Some(CONSUMER_INSTANCE_ID),
        &mut consumer_child,
        &consumer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        TEST_CORE_NODE,
        CONSUMER_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        PRODUCER_NODE_NAME,
        TEST_CORE_NODE,
        PRODUCER_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;

    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &consumer_dir,
    );
    let producer_output = wait_for_child(
        &mut producer_child,
        Some(Duration::from_secs(10)),
        &producer_dir,
    );

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer process failed -- likely producer/consumer capnp wire layout \
         disagreement for the service's `response_message_format`.\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    assert!(
        consumer_stdout
            .contains("response ok=True status=nominal measurements=[0.1, 0.2, 0.3] elapsed=12.5"),
        "consumer did not decode the service response correctly.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    let producer_stdout = String::from_utf8_lossy(&producer_output.stdout).into_owned();
    let producer_stderr = String::from_utf8_lossy(&producer_output.stderr).into_owned();
    assert!(
        producer_output.status.success(),
        "producer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        producer_output.status.code(),
        producer_stdout,
        producer_stderr
    );
}

// ---------------------------------------------------------------------------
// Topics
// ---------------------------------------------------------------------------

const TOPIC_NAME: &str = "telemetry_feed";
const TOPIC_RECEIVED_SERVICE: &str = "received_ack";
// `message_format` is intentionally declared in an order whose
// alphabetical sort swaps its two pointer-typed fields (`status: Text`,
// `readings: List(Float64)`); see module docstring.
const TOPIC_PRODUCER_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "producer",
    tag: "v1"
  },
  interfaces: {
    topics: {
      emits: [
        {
          name: "telemetry_feed",
          qos_profile: "sensor_data",
          message_format: {
            sequence: "u32",
            status: "string",
            readings: { $type: "array", $items: "f64" },
            timestamp: "f64"
          }
        }
      ]
    }
  },
  execution: {
    language: "python",
    run_cmd: ["uv", "run", "python", "main.py"]
  }
}"#;

const TOPIC_CONSUMER_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "consumer",
    tag: "v1",
    depends_on: {
      nodes: [
        { name: "producer", tag: "v1", link_id: "producer" }
      ]
    }
  },
  interfaces: {
    topics: {
      consumes: [
        { link_id: "producer", name: "telemetry_feed" }
      ]
    },
    services: {
      exposes: [
        { name: "received_ack" }
      ]
    }
  },
  execution: {
    language: "python",
    run_cmd: ["uv", "run", "python", "main.py"]
  }
}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topics_preserve_message_field_order() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Producer (emitter) project
    let temp_dir_producer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for producer project");
    let producer_dir = temp_dir_producer.path().join("user_node");
    fs::create_dir_all(&producer_dir).unwrap();
    write_producer_config_via_round_trip(TOPIC_PRODUCER_CONFIG, &producer_dir);

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        &producer_dir,
        Vec::new(),
        "test-hash",
        &test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("generate peppygen for producer");

    let producer_runtime_config_path = temp_dir_producer.path().join("peppy_runtime.json5");
    build_runtime_config(
        &router_host,
        router_port,
        PRODUCER_INSTANCE_ID,
        PRODUCER_NODE_NAME,
        &producer_runtime_config_path,
    );

    init_python_user_node(&producer_dir);
    let producer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.emitted_topics import telemetry_feed

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    async def emit_loop():
        publisher = await telemetry_feed.declare_publisher(node_runner)
        while True:
            await publisher.publish(
                telemetry_feed.build_message(
                    sequence=1,
                    status="nominal",
                    readings=[1.0, 2.0, 3.0],
                    timestamp=12345.6,
                )
            )
            await asyncio.sleep(0.1)
    return [asyncio.create_task(emit_loop())]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    fs::write(producer_dir.join("main.py"), producer_main).expect("write producer main.py");

    // --- Consumer (receiver) project
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let consumer_dir = temp_dir_consumer.path().join("user_node");
    fs::create_dir_all(&consumer_dir).unwrap();
    fs::write(consumer_dir.join(NODE_CONFIG_FILE), TOPIC_CONSUMER_CONFIG)
        .expect("write consumer peppy.json5");

    let emitted_topic = parse_producer_config_in_memory(TOPIC_PRODUCER_CONFIG)
        .interfaces
        .topics
        .as_ref()
        .and_then(|t| t.emits.as_ref())
        .and_then(|v| v.iter().find(|t| t.name == TOPIC_NAME))
        .cloned()
        .expect("emitted topic present in producer config");

    let consumed_topic: ConsumedTopic = serde_json5::from_str(&format!(
        r#"{{ link_id: "{PRODUCER_NODE_NAME}", name: "{TOPIC_NAME}" }}"#
    ))
    .unwrap();
    let consumed_interface = DeploymentInterface::new(InterfaceVariant::ConsumedTopic {
        topic: consumed_topic,
        message_format: emitted_topic
            .message_format
            .clone()
            .expect("emitted topic has a message format"),
        dependency: generator::DependencyContext::native(PRODUCER_NODE_NAME, "v1"),
    });

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        &consumer_dir,
        vec![consumed_interface],
        "test-hash",
        &test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("generate peppygen for consumer");

    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    build_runtime_config(
        &router_host,
        router_port,
        CONSUMER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        &consumer_runtime_config_path,
    );

    init_python_user_node(&consumer_dir);
    // The receiver subscribes, prints the first decoded message, and
    // then exposes `received_ack` to signal the test that decode
    // succeeded. If decode raises, `os._exit(1)` fast-fails so the
    // test sees the exit and panics with the captured stderr instead
    // of hanging on the unreachable ack service.
    let consumer_main = r#"
import asyncio
import os
import sys
from peppygen import NodeBuilder
from peppygen.exposed_services import received_ack
from peppygen.consumed_topics import producer_telemetry_feed

async def receive_one(node_runner, msg_received):
    try:
        producer, msg = await producer_telemetry_feed.on_next_message_received(node_runner)
        print(
            f"received message status={msg.status} readings={list(msg.readings)} "
            f"sequence={msg.sequence} timestamp={msg.timestamp} from {producer.core_node}/{producer.instance_id}",
            flush=True,
        )
        msg_received.set()
    except BaseException as e:
        print(f"receive failed: {type(e).__name__}: {e}", file=sys.stderr, flush=True)
        os._exit(1)

async def ack_when_ready(node_runner, msg_received):
    await msg_received.wait()
    await received_ack.handle_next_request(node_runner, lambda _: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    msg_received = asyncio.Event()
    return [
        asyncio.create_task(receive_one(node_runner, msg_received)),
        asyncio.create_task(ack_when_ready(node_runner, msg_received)),
    ]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    fs::write(consumer_dir.join("main.py"), consumer_main).expect("write consumer main.py");

    init_python_project_venv(&producer_dir);
    init_python_project_venv(&consumer_dir);

    let producer_runtime_config_str = producer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut producer_child = spawn_python_run(
        &producer_dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &producer_runtime_config_str)],
    );
    let mut consumer_child = spawn_python_run(
        &consumer_dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: TEST_CORE_NODE,
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        CONSUMER_INSTANCE_ID,
        &mut consumer_child,
        &consumer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        PRODUCER_NODE_NAME,
        PRODUCER_INSTANCE_ID,
        &mut producer_child,
        &producer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    // Wait for the receiver to ack one successfully decoded message.
    // If decoding fails, the receiver process exits with code 1 and
    // this helper panics with the captured diagnostics.
    wait_for_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        TOPIC_RECEIVED_SERVICE,
        Some(CONSUMER_INSTANCE_ID),
        &mut consumer_child,
        &consumer_dir,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        TEST_CORE_NODE,
        CONSUMER_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        PRODUCER_NODE_NAME,
        TEST_CORE_NODE,
        PRODUCER_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;

    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &consumer_dir,
    );
    let producer_output = wait_for_child(
        &mut producer_child,
        Some(Duration::from_secs(10)),
        &producer_dir,
    );

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer process failed -- likely producer/consumer capnp wire layout \
         disagreement for the topic's `message_format`.\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    assert!(
        consumer_stdout.contains(&format!(
            "received message status=nominal readings=[1.0, 2.0, 3.0] sequence=1 timestamp=12345.6 from {TEST_CORE_NODE}/{PRODUCER_INSTANCE_ID}"
        )),
        "consumer did not decode the topic message correctly.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    let producer_stdout = String::from_utf8_lossy(&producer_output.stdout).into_owned();
    let producer_stderr = String::from_utf8_lossy(&producer_output.stderr).into_owned();
    assert!(
        producer_output.status.success(),
        "producer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        producer_output.status.code(),
        producer_stdout,
        producer_stderr
    );
}
