//! Shared harness for the MCP end-to-end suites.
//!
//! Two test binaries drive the built-in MCP server end to end: the codec
//! wire test in generator-internal (the runtime codec against a generated
//! provider) and peppy's full-stack suite (a launcher-deployed stack). Both
//! need the same moves: compile a generated provider node offline in the
//! shared test target dir, connect the real MCP client over Streamable HTTP,
//! poll a task to a state, register a fixture contract's members on a
//! provider generator. Those moves live here.

use config::consts::PEPPYGEN_OUTPUT_PATH;
use daemon_config::contract::PeppyContract;
use generator::{ContractOrigin, LanguageGenerator};
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
/// to one exposure's Streamable HTTP endpoint, negotiating MCP `2026-07-28`.
pub async fn connect_with_tasks(endpoint_url: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(endpoint_url.to_owned()),
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
