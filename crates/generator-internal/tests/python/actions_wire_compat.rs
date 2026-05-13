//! End-to-end test that an exposing node and a consuming node agree on
//! the capnp wire format for an action's result response: spawn both,
//! drive the consumer through the action, and check that it can decode
//! the producer's payload.
//!
//! Two choices make the test sensitive to producer/consumer schema
//! mismatches:
//!
//! 1. The exposed action's `result_service.response_message_format`
//!    declares its two pointer-typed fields (`status: Text` and
//!    `measurements: List(Float64)`) in an order whose alphabetical sort
//!    swaps them. Any step that silently re-orders the MessageFormat on
//!    one side but not the other will land Text where the other side
//!    expects List, surfacing as a `Schema mismatch: Message contains
//!    list pointer of non-bytes where text was expected` -class decode
//!    error on the consumer.
//!
//! 2. Producer and consumer reach the generator through different
//!    pipelines, mirroring how a real deployment resolves them: the
//!    producer's `NodeConfig` is serialised to JSON5 and re-parsed before
//!    reaching `generate_peppygen_lib` (the variant-sync flow stages a
//!    merged config in a temp file the generator re-parses), while the
//!    consumer's `ConsumedActionMessage` is built directly from the
//!    in-memory parsed `NodeConfig` (a dependency's interfaces are
//!    normally resolved from the in-memory node stack).
//!
//! Python rather than Rust to avoid the per-spawn `cargo build` cost --
//! the capnp schema generation under test is language-agnostic.

use crate::helpers::{
    WaitContext, init_python_project_venv, init_python_user_node, send_shutdown, spawn_python_run,
    test_peppy_dirs, wait_for_action_service_reachable_or_exit, wait_for_child,
    wait_for_health_service_reachable_or_exit,
};
use config::consts::{NODE_CONFIG_FILE, RUNTIME_CONFIG_VAR_NAME};
use config::json5_pretty;
use config::launcher::Name;
use config::node::{ConsumedAction, NodeConfigParser, PeppygenLanguage};
use config::runtime::{NodeInstanceConfig, RuntimeConfig};
use generator::{
    ConsumedActionMessage, DeploymentInterface, InterfaceVariant, generate_peppygen_lib,
};
use std::{fs, time::Duration};
use tempfile::TempDir;

const TEST_CORE_NODE: &str = "test_core";
const CONSUMER_NODE_NAME: &str = "controller";
const PRODUCER_NODE_NAME: &str = "telemetry";
const CONSUMER_INSTANCE_ID: &str = "controller_instance";
const PRODUCER_INSTANCE_ID: &str = "telemetry_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const ACTION_NAME: &str = "perform_scan";

// `result_service.response_message_format` is intentionally declared in
// an order whose alphabetical sort swaps its two pointer-typed fields
// (`status: Text`, `measurements: List(Float64)`); see module docstring.
const PRODUCER_NODE_CONFIG: &str = r#"{
  peppy_schema: "node_v1",
  manifest: {
    name: "telemetry",
    tag: "0.1.0"
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

const CONSUMER_NODE_CONFIG: &str = r#"{
  peppy_schema: "node_v1",
  manifest: {
    name: "controller",
    tag: "0.1.0",
    depends_on: {
      nodes: [
        { name: "telemetry", tag: "0.1.0", local_id: "telemetry" }
      ]
    }
  },
  interfaces: {
    actions: {
      consumes: [
        {
          local_node_id: "telemetry",
          name: "perform_scan"
        }
      ]
    }
  },
  execution: {
    language: "python",
    run_cmd: ["uv", "run", "python", "main.py"]
  }
}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_preserves_result_field_order() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Producer (server) project ----------------------------------------
    //
    // Serialise the parsed `NodeConfig` to JSON5 and re-parse it on the
    // way to `generate_peppygen_lib` -- this mirrors the variant-sync
    // flow that stages a merged config in a temp file the generator
    // re-parses.
    let temp_dir_producer = TempDir::new().unwrap();
    let producer_dir = temp_dir_producer.path().join("user_node");
    fs::create_dir_all(&producer_dir).unwrap();

    let producer_config_parsed = NodeConfigParser::from_content(PRODUCER_NODE_CONFIG)
        .expect("producer config parses")
        .into_resolved()
        .expect("producer config resolves");
    let producer_pretty = json5_pretty::to_string_pretty(&producer_config_parsed)
        .expect("producer config pretty-prints");
    fs::write(producer_dir.join(NODE_CONFIG_FILE), &producer_pretty)
        .expect("write producer peppy.json5");

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

    let producer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig {
            instance_id: Name::new(PRODUCER_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        PRODUCER_NODE_NAME,
        TEST_CORE_NODE,
    )
    .unwrap();
    let producer_runtime_config_path = temp_dir_producer.path().join("peppy_runtime.json5");
    producer_runtime_config
        .save_json5_launch_config(&producer_runtime_config_path)
        .unwrap();

    init_python_user_node(&producer_dir);
    // Construct ResultResponse with keyword arguments so the Python source
    // compiles regardless of how the generated constructor orders its
    // positional parameters. The bug we're catching is on the wire, not in
    // the class layout.
    let producer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import perform_scan

async def run_exposer(node_runner):
    action = await perform_scan.ActionHandle.expose(node_runner)

    def goal_handler(request):
        print(f"server received scan goal scan_id={request.data.scan_id}", flush=True)
        return perform_scan.GoalResponse(accepted=True)

    await action.handle_goal_next_request(goal_handler)

    await action.emit_feedback(7)
    print("server emitted feedback progress=7", flush=True)

    def result_handler(request):
        print("server preparing scan result", flush=True)
        return perform_scan.ResultResponse(
            success=True,
            status="completed",
            measurements=[1.5, 2.5, 3.5],
            duration=42.0,
        )

    await action.handle_result_next_request(result_handler)
    print("server handled scan result request", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    fs::write(producer_dir.join("main.py"), producer_main).expect("write producer main.py");

    // --- Consumer (client) project ----------------------------------------
    //
    // Build the `ConsumedActionMessage` directly from the parsed
    // `NodeConfig` (no JSON5 round-trip) -- a dependency's interfaces are
    // normally resolved from the in-memory node stack.
    let temp_dir_consumer = TempDir::new().unwrap();
    let consumer_dir = temp_dir_consumer.path().join("user_node");
    fs::create_dir_all(&consumer_dir).unwrap();
    fs::write(consumer_dir.join(NODE_CONFIG_FILE), CONSUMER_NODE_CONFIG)
        .expect("write consumer peppy.json5");

    let producer_view_for_consumer = NodeConfigParser::from_content(PRODUCER_NODE_CONFIG)
        .expect("producer config parses for consumer view")
        .into_resolved()
        .expect("producer config resolves for consumer view");
    let exposed_action = producer_view_for_consumer
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
        goal_response: exposed_action
            .goal_service
            .as_ref()
            .and_then(|s| s.response_message_format.clone()),
        feedback: exposed_action
            .feedback_topic
            .as_ref()
            .and_then(|t| t.message_format.clone()),
        result_request: None,
        result_response: exposed_action
            .result_service
            .as_ref()
            .and_then(|s| s.response_message_format.clone()),
    };
    let consumed_action: ConsumedAction = serde_json5::from_str(&format!(
        r#"{{ local_node_id: "{PRODUCER_NODE_NAME}", name: "{ACTION_NAME}" }}"#
    ))
    .unwrap();
    let consumed_interface = DeploymentInterface::new(InterfaceVariant::ConsumedAction {
        action: consumed_action,
        messages: consumed_action_messages,
        dependency_node_name: PRODUCER_NODE_NAME.to_string(),
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

    let consumer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig {
            instance_id: Name::new(CONSUMER_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        CONSUMER_NODE_NAME,
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_python_user_node(&consumer_dir);
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder, QoSProfile
from peppygen.consumed_actions import telemetry_perform_scan

async def run_consumer(node_runner):
    request = telemetry_perform_scan.GoalRequest(scan_id=7)
    goal = await telemetry_perform_scan.ActionHandle.fire_goal(
        node_runner, request, 5.0, QoSProfile.SensorData
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    feedback = await goal.on_next_feedback_message()
    print(f"feedback message received progress={feedback.progress}", flush=True)

    result = await goal.get_result(5.0)
    print(
        f"result success={result.data.success} status={result.data.status} "
        f"measurements={result.data.measurements} duration={result.data.duration}",
        flush=True,
    )

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_consumer(node_runner))]

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

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    // Spawn producer first so it's ready to handle requests.
    let mut producer_child = spawn_python_run(
        &producer_dir,
        &[(RUNTIME_CONFIG_VAR_NAME, &producer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
        PRODUCER_NODE_NAME,
        &format!("{ACTION_NAME}/goal"),
        None,
        &mut producer_child,
        &producer_dir,
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
        target_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        CONSUMER_INSTANCE_ID,
        &mut consumer_child,
        &consumer_dir,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        PRODUCER_NODE_NAME,
        PRODUCER_INSTANCE_ID,
        &mut producer_child,
        &producer_dir,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        Some(TEST_CORE_NODE),
        CONSUMER_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        PRODUCER_NODE_NAME,
        Some(TEST_CORE_NODE),
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
        "consumer process failed -- this typically means the producer's capnp wire \
         layout for `result_response` disagrees with the consumer's view of the same \
         action, producing 'list pointer of non-bytes where text was expected' or a \
         similar decode error.\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
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
