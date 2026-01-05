mod helpers;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use config::consts::NODE_CONFIG_FILE;
use helpers::TestServeHandle;
use master_node::encoding::{NodeAddRequest, NodeListRequest};
use node_stack::SerializedNodeGraph;
use peppy::commands::service::serve::{CancellationToken, PID_FILE_ENV, PROMPT_ANSWER_ENV};
use peppy::commands::service::serve::ServeCommand;
use peppy::commands::Command;
use peppy::context::{AppContext, DaemonState};

const CALLER_INSTANCE_ID: &str = "peppy-test";

fn write_node_config(nodes_directory: &Path, node_name: &str, node_tag: &str) -> PathBuf {
    let node_dir = nodes_directory.join(node_name);
    fs::create_dir_all(&node_dir).expect("failed to create node directory");
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    fs::write(
        &node_config_path,
        format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{node_name}",
                    tag: "{node_tag}",
                    launch_cmd: ["sh", "-c", "exit 0"]
                }}
            }}"#
        ),
    )
    .expect("failed to write node config");
    node_config_path
}

#[test]
fn serve_command() {
    let _serial_guard = helpers::serve_test_lock().lock().unwrap();
    let _serve_env = helpers::TempServeEnvGuard::new();

    let ctx = Arc::new(AppContext::default());
    let log_capture = helpers::LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let shutdown_token = CancellationToken::new();
    let shutdown_token_clone = shutdown_token.clone();

    let shutdown_thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        shutdown_token_clone.cancel();
    });

    ServeCommand {
        messaging_engine: "mock".to_string(),
        master_name: Some("master-node".to_string()),
        shutdown_token: Some(shutdown_token),
    }
    .execute(&ctx)
    .expect("serve command executes with mock messaging engine");

    shutdown_thread
        .join()
        .expect("shutdown thread should complete without panic");

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    assert_eq!(
        daemon_state.master_node_name, "master-node",
        "daemon state should use the configured master name"
    );

    let logs = log_capture.logs();
    assert!(
        logs.contains("Serve command initialized!"),
        "serve command should log initialization message. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains("Shutdown signal received"),
        "serve command should log shutdown signal reception. Logs:\n{}",
        logs
    );
}

#[test]
fn serve_command_replace_existing_stack() {
    let _serial_guard = helpers::serve_test_lock().lock().unwrap();
    let serve = TestServeHandle::with_mock_messenger();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let node_name = "reset_test_node";
    let node_tag = "0.1.0";
    let node_config_path = write_node_config(nodes_dir.path(), node_name, node_tag);

    let ctx = Arc::new(AppContext::with_messenger(
        nodes_dir.path(),
        serve.messenger(),
    ));

    // Capture logs from the reset attempt (runs in this test thread).
    let log_capture = serve.log_capture().clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let node_config_content =
        fs::read_to_string(&node_config_path).expect("node config should be readable");

    let add_response = rt
        .block_on(NodeAddRequest::new(node_config_content, nodes_dir.path()).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_add request should complete");

    assert!(
        add_response.success,
        "node_add should succeed: {:?}",
        add_response.error_message
    );

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    assert_eq!(
        graph.nodes.len(),
        2,
        "graph should contain the master node + one added node before reset. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    // Simulate an already-running peppy daemon by writing a pid file containing this process PID.
    let pid_file = std::env::var_os(PID_FILE_ENV).expect("pid file env should be set");
    let pid_path = PathBuf::from(pid_file);
    fs::write(&pid_path, std::process::id().to_string()).expect("pid file should be writable");

    // Trigger reset via a second serve command invocation.
    let _prompt_guard = helpers::EnvVarGuard::set(PROMPT_ANSWER_ENV, "y");
    ServeCommand {
        messaging_engine: "mock".to_string(),
        master_name: Some("master-node".to_string()),
        shutdown_token: None,
    }
    .execute(&ctx)
    .expect("reset should succeed when user answers yes");

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete after reset");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse after reset");
    assert_eq!(
        graph.nodes.len(),
        1,
        "graph should only contain the master node after reset. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    let logs = log_capture.logs();
    assert!(
        logs.contains("Existing peppy instance detected"),
        "existing instance detection should be logged. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains("reset_requested=true"),
        "user response should be registered in logs. Logs:\n{}",
        logs
    );
}
