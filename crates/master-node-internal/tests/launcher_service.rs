mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::{LauncherRequest, LauncherResponse};
use peppylib::messaging::ServiceMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_launch_config_request() {
    let test_node = setup_test_master_node().await;
    // TODO create a working directory with the nodes of the config as subfolders (the command is supposed to find all the `peppy.json5` recursively. There is a feature like that available in the project)

    let launcher_config = r#"
    deployments: [
        {
        name: "uvc_camera",
        source: {
            repo: "https://github.com/Peppy/nodes.git",
            path: "uvc_camera"
        },
        tag: "0.1.0",
        instances: [
            {
            instance_id: "camera_front",
            parameters: {
                device: {
                physical: "/dev/video_right",
                sim: "mujoco:camera_right",
                priority: "physical"
                },
                video: {
                frame_rate: 30,
                resolution: {
                    width: 1920,
                    height: 1080,
                },
                encoding: "yuyv",
                },
            }
            },
            {
            instance_id: "camera_rear",
            parameters: {
                device: {
                physical: "/dev/video_left",
                sim: "mujoco:camera_left",
                priority: "physical"
                },
                video: {
                frame_rate: 30,
                resolution: {
                    width: 1920,
                    height: 1080,
                },
                encoding: "yuyv",
                },
            }
            }
        ]
        },
        {
        name: "web_video_stream",
        tag: "0.1.0",
        optional: true,
        instances: [
            {
            instance_id: "video_stream1",
            parameters: {
                camera_instances_ids: [
                "camera_front",
                "camera_rear"
                ],
                http: {
                host: "0.0.0.0",
                port: 8083,
                cors_enabled: false,
                cors_origins: "*",
                max_connections: "2000",
                request_timeout_ms: "3000",
                },
                video_stream: {
                format: "mjpeg",
                quality: 3,
                max_fps: 30,
                },
            }
            }
        ]
        },
        {
        name: "esp32_board",
        tag: "0.1.0",
        instances: [
            {
            instance_id: "esp32_1",
            env_vars: {
                ESP32_DEVICE: "/dev/tty.usbmodem585A0076841"
            },
            }
        ]
        },
    ],
    logging: {
        min_level: "info",
        file_name: "peppy_root.log",
        max_file_size_mb: 100,
        format: "text"
    }
    }
    "#;

    let request = LauncherRequest::new(launcher_config);
    let request_payload = request.encode().expect("failed to encode info request");

    let response = ServiceMessenger::poll(
        &test_node.caller_handle,
        &test_node.master_node_name,
        CALLER_INSTANCE_ID,
        &test_node.master_node_name,
        "info",
        None,
        Some(&test_node.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    let response = LauncherResponse::decode(&response.payload().to_bytes())
        .expect("should decode info response");

    todo!("Check the node_stack");
}
