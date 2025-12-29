mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use config::peppy_config::BuildSystem;
use master_node::encoding::NodeSyncRequest;
use std::time::Duration;
use tempfile::Builder;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_node_sync_success() {
    let (client, _server) = setup_test_master_node().await;

    let temp_dir = Builder::new()
        .prefix("node_sync")
        .tempdir()
        .expect("failed to create tempdir");
    let node_root_dir = temp_dir.path().to_path_buf();

    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "sensor_node",
                tag: "1.0.0"
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                            name: "sensor_data",
                            qos_profile: "sensor_data",
                            message_format: {
                                value: "f32"
                            }
                        }
                    ]
                }
            }
        }"#;

    std::fs::write(
        node_root_dir.join(config::consts::NODE_CONFIG_FILE),
        peppy_json5,
    )
    .expect("failed to write node config");

    let request = NodeSyncRequest::new(&node_root_dir).with_build_system(BuildSystem::Rust);

    let node_sync_response = request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(node_sync_response.success);
    assert!(
        node_sync_response.error_message.is_empty(),
        "expected empty error message, got: {}",
        node_sync_response.error_message
    );

    let peppygen_dir = node_root_dir.join(".peppy/libs/peppygen");
    assert!(
        peppygen_dir.join("Cargo.toml").exists(),
        "expected generated peppygen Cargo.toml at {}",
        peppygen_dir.display()
    );
    assert!(
        peppygen_dir.join("src/lib.rs").exists(),
        "expected generated peppygen src/lib.rs at {}",
        peppygen_dir.display()
    );

    assert!(
        peppygen_dir.join("src/exposed_topics.rs").exists(),
        "expected generated exposed_topics module at {}",
        peppygen_dir.display()
    );
    assert!(
        peppygen_dir
            .join("src/exposed_topics/sensor_data.rs")
            .exists(),
        "expected generated sensor_data topic module at {}",
        peppygen_dir.display()
    );

    assert!(
        peppygen_dir
            .join(config::consts::NODE_CONFIG_FINGERPRINT_FILE)
            .exists(),
        "expected node config fingerprint at {}",
        peppygen_dir.display()
    );
    assert!(
        !peppygen_dir.join(config::consts::NODE_CONFIG_FILE).exists(),
        "peppy.json5 should not be copied into the generated crate"
    );
}
