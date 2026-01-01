use config::peppy_config::{DeploymentInstance, Name};
use config::runtime::RuntimeConfig;
use peppylib::start_zenohd_process;
use tempfile::TempDir;

#[test]
fn shutdown_request_exits_node() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    // Start zenohd router
    let (_router, _router_dir, router_host, router_port) = rt
        .block_on(start_zenohd_process("127.0.0.1", None))
        .expect("failed to start zenoh router for test");

    // Create temp directory for config files
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let peppy_config_path = temp_dir.path().join("peppy.json5");

    // Create a minimal peppy.json5 config
    let peppy_config_content = r#"{
        schema_version: 1,
        manifest: {
            name: "test_shutdown_node",
            tag: "0.1.0"
        },
        parameters: {}
    }"#;
    std::fs::write(&peppy_config_path, peppy_config_content).expect("failed to write peppy config");

    // Create runtime config pointing to zenohd
    let instance_id = "test_shutdown_instance";
    let node_name = "test_shutdown_node";
    let master_node = "test_master";

    let runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(instance_id).unwrap(),
            arguments: Default::default(),
        },
        node_name,
        master_node,
    )
    .unwrap();

    let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
    runtime_config
        .save_json5_launch_config(&runtime_config_path)
        .expect("failed to save runtime config");

    // Spawn the runner in a separate thread
    todo!(
        "Spawn a node by invoking the `run` service on the master node and then and send it a shutdown signal"
    );
}
