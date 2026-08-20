//! Shared harness for the MCP endpoint e2e suites.
//!
//! Two test binaries drive a generated MCP server node end to end:
//! generator-internal's endpoint test (one node against a stub provider)
//! and peppy's full-stack test (a launcher-deployed stack). Both need the
//! same moves — compile the generated node offline in the shared test
//! target dir, connect the real MCP client over Streamable HTTP, poll a
//! task to a state, derive the consumed interfaces the daemon would
//! resolve — so those moves live here.

use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::node::{Cardinality, ConsumedAction, ConsumedService, ConsumedTopic};
use daemon_config::contract::PeppyContract;
use generator::{
    ConsumedActionMessage, ContractOrigin, DependencyContext, DeploymentInterface, ExposureBundle,
    LanguageGenerator,
};
use rmcp::model::{
    ClientCapabilities, ClientInfo, DetailedTask, GetTaskParams, ProtocolVersion, UpdateTaskParams,
};
use rmcp::service::ServiceError;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The MCP client handle the e2e helpers operate on.
pub type Client = rmcp::service::RunningService<rmcp::RoleClient, ClientInfo>;

/// Compiles a generated node crate offline in the shared test target dir.
///
/// The generated `peppygen` package is renamed to `peppygen_{built_binary}`
/// (and the node manifest aliases the dependency back, so generated
/// `use peppygen::…` code is unchanged): per-node names keep the builds
/// distinct cargo units in the shared target dir. The built binary is
/// copied into the node's own `target/debug` as `installed_binary`, where
/// the node's `run_cmd` (or the test's spawn helper) looks for it; the
/// local copy's path is returned.
pub fn compile_node(node_dir: &Path, built_binary: &str, installed_binary: &str) -> PathBuf {
    let peppygen_cargo = node_dir.join(PEPPYGEN_OUTPUT_PATH).join("Cargo.toml");
    let contents = fs::read_to_string(&peppygen_cargo).expect("generated peppygen manifest exists");
    let unique = format!("peppygen_{built_binary}");
    let renamed = contents.replacen("name = \"peppygen\"", &format!("name = \"{unique}\""), 1);
    assert_ne!(renamed, contents, "the peppygen manifest names its package");
    fs::write(&peppygen_cargo, renamed).expect("rewrite peppygen package name");

    let manifest_path = node_dir.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path).expect("node manifest exists");
    // Prefix-based so it aliases every peppygen path entry: the main
    // dependency and the testing-featured dev-dependency sync writes.
    let aliased = manifest.replace(
        "peppygen = { path = \".peppy/libs/peppygen\"",
        &format!("peppygen = {{ package = \"{unique}\", path = \".peppy/libs/peppygen\""),
    );
    assert_ne!(
        aliased, manifest,
        "the node manifest declares the peppygen path dependency"
    );
    fs::write(&manifest_path, aliased).expect("rewrite node manifest");

    let target_dir = config_test_support::test_data_root().join("cache/rust/test-targets");
    fs::create_dir_all(&target_dir).expect("create shared target dir");
    let output = std::process::Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TARGET_DIR", &target_dir)
        .current_dir(node_dir)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("invoke cargo build on the node crate");
    assert!(
        output.status.success(),
        "cargo build failed for `{built_binary}`:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let built = target_dir
        .join("debug")
        .join(executable_filename(built_binary));
    let local_bin_dir = node_dir.join("target").join("debug");
    fs::create_dir_all(&local_bin_dir).expect("create local target dir");
    let local_binary = local_bin_dir.join(executable_filename(installed_binary));
    fs::copy(&built, &local_binary).expect("copy the built node binary");
    local_binary
}

/// The platform filename of a built executable.
pub fn executable_filename(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// An OS-assigned loopback port, released for the node to claim.
pub fn ephemeral_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("bound listener has an address")
        .port()
}

/// Connects a client that declares the SEP-2663 tasks extension capability
/// to the node's Streamable HTTP endpoint, negotiating MCP `2026-07-28`.
pub async fn connect_with_tasks(http_port: u16) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://127.0.0.1:{http_port}/mcp")),
    );
    let mut info = ClientInfo::default();
    info.capabilities = ClientCapabilities::builder().enable_tasks().build();
    info.serve_with_lifecycle(
        transport,
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    )
    .await
    .expect("the tasks-capable MCP client negotiates 2026-07-28")
}

/// Unwraps the protocol-level error data of a failed client call.
pub fn protocol_error(error: ServiceError) -> rmcp::ErrorData {
    match error {
        ServiceError::McpError(data) => data,
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

/// Polls `tasks/get` until the task satisfies `accept`; bounded by
/// `timeout` and driven by server responses.
pub async fn poll_task_until(
    client: &Client,
    timeout: Duration,
    task_id: &str,
    description: &str,
    accept: impl Fn(&DetailedTask) -> bool,
) -> DetailedTask {
    tokio::time::timeout(timeout, async {
        loop {
            let result = client
                .get_task(GetTaskParams::new(task_id))
                .await
                .expect("tasks/get answers");
            if accept(&result.task) {
                return result.task;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("task `{task_id}` never reached: {description}"))
}

/// The `tasks/update` payload that accepts a confirmation elicitation.
pub fn confirmation_accept(task_id: &str) -> UpdateTaskParams {
    UpdateTaskParams::new(
        task_id,
        [("confirmation".to_string(), json!({ "action": "accept" }))]
            .into_iter()
            .collect(),
    )
}

/// Registers every member of `contract` on a producer-side generator, as a
/// stub provider implementing the whole contract.
pub fn register_contract_members(
    generator: &mut impl LanguageGenerator,
    contract: &PeppyContract,
    origin: &ContractOrigin,
) {
    for topic in &contract.interfaces.topics {
        generator
            .add_emitted_topic(topic, Some(origin))
            .expect("register emitted topic");
    }
    for service in &contract.interfaces.services {
        generator
            .add_exposed_service(service, Some(origin))
            .expect("register exposed service");
    }
    for action in &contract.interfaces.actions {
        generator
            .add_exposed_action(action, Some(origin))
            .expect("register exposed action");
    }
}

/// The consumed interfaces the daemon resolves for a generated exposure
/// node: exactly the members the bundle selects, in catalog order, resolved
/// against the fixture contract filling each `link_id` slot. This is what
/// the generated manifest consumes, so it is what the node's peppygen must
/// be built from.
pub fn consumed_interfaces(
    bundle: &ExposureBundle,
    slots: &[(&PeppyContract, &str)],
) -> Vec<DeploymentInterface> {
    let slot = |target: &str| -> (&PeppyContract, DependencyContext) {
        let (contract, link_id) = slots
            .iter()
            .find(|(_, link_id)| *link_id == target)
            .unwrap_or_else(|| panic!("no fixture contract fills slot `{target}`"));
        let dependency = DependencyContext::contract(
            contract.manifest.name.as_str(),
            contract.manifest.tag.as_str(),
            *link_id,
            Cardinality::One,
        );
        (contract, dependency)
    };

    let mut interfaces = Vec::new();
    for resource in &bundle.resources {
        let (contract, dependency) = slot(&resource.target);
        let topic = contract
            .interfaces
            .topics
            .iter()
            .find(|topic| topic.name == resource.member)
            .unwrap_or_else(|| panic!("the contract declares topic `{}`", resource.member));
        interfaces.push(DeploymentInterface::consumed_topic(
            ConsumedTopic {
                link_id: resource.target.clone(),
                name: topic.name.clone(),
                refine: None,
            },
            topic
                .message_format
                .clone()
                .expect("fixture topics carry formats"),
            dependency,
        ));
    }
    for tool in &bundle.tools {
        let (contract, dependency) = slot(&tool.target);
        let service = contract
            .interfaces
            .services
            .iter()
            .find(|service| service.name == tool.member)
            .unwrap_or_else(|| panic!("the contract declares service `{}`", tool.member));
        interfaces.push(DeploymentInterface::consumed_service(
            ConsumedService {
                link_id: tool.target.clone(),
                name: service.name.clone(),
                refine: None,
            },
            service.request_message_format.clone().unwrap_or_default(),
            service.response_message_format.clone().unwrap_or_default(),
            dependency,
        ));
    }
    for task in &bundle.tasks {
        let (contract, dependency) = slot(&task.target);
        let action = contract
            .interfaces
            .actions
            .iter()
            .find(|action| action.name == task.member)
            .unwrap_or_else(|| panic!("the contract declares action `{}`", task.member));
        interfaces.push(DeploymentInterface::consumed_action(
            ConsumedAction {
                link_id: task.target.clone(),
                name: action.name.clone(),
                refine: None,
            },
            ConsumedActionMessage::from(action),
            dependency,
        ));
    }
    interfaces
}
