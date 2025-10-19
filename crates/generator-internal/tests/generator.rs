use config::node::Interfaces;
use config::peppy_config::PeppyConfig;
use generator::generate_lib_for_language;

#[test]
fn generate_interfaces_code_rust_success() {
    let config: PeppyConfig = serde_json5::from_str(
        r#"{
        deployments: [
            {
            name: 'uvc_camera',
            tag: '0.1.0',
            namespace: "/",
            instances: [
                {
                  namespace: '/camera/right',
                  parameters: {
                    device: {
                      physical: '/dev/video_right',
                      sim: 'mujoco:camera_right',
                      priority: 'physical',
                    },
                    video: {
                      frame_rate: 30,
                      resolution: {
                        width: 1920,
                        height: 1080,
                      },
                      encoding: 'yuyv',
                    },
                  },
                },
                {
                  namespace: '/camera/left',
                  parameters: {
                    device: {
                      physical: '/dev/video_left',
                      sim: 'mujoco:camera_left',
                      priority: 'physical',
                    },
                    video: {
                      frame_rate: 30,
                      resolution: {
                        width: 1920,
                        height: 1080,
                      },
                      encoding: 'yuyv',
                    },
                  },
                },
            ],
            },
        ],
    }"#,
    )
    .expect("valid JSON5 config structure");

    let interfaces: Interfaces = serde_json5::from_str(
        r#"{
        exposes: {
            topics: [
                {
                    name: "stream",
                    qos_profile: "sensor_data",
                    message_format: {
                        header: {
                            type: "object",
                            stamp: "time",
                            frame_id: "u32",
                        },
                        encoding: "string", // "rgb8", "bgr8", "yuyv", "mjpeg"
                        width: "u32",
                        height: "u32",
                        image: {
                            type: "array",
                            items: "u8",
                            length: 3
                        },
                    },
                }
            ],
        },
        subscribes_to: {
            topics: [
                {
                    node: "uvc_camera",
                    tag: "0.1.0",
                    name: "stream",
                    callback: "on_handle_video_frame",
                }
            ],
        },
        }"#,
    )
    .expect("valid JSON5 interfaces structure");
    // let lang = generator::Language::Rust;
    // let gene = InterfaceGenerator::language(lang)
    //     .interfaces(interfaces.iter().cloned())
    //     .build();
    todo!("Finish")
}

#[test]
fn generate_interfaces_code_rust_missing_topic_format() {
    let interfaces: Interfaces = serde_json5::from_str(
        r#"{
        exposes: {
            topics: [
                {
                    name: "stream",
                    qos_profile: "sensor_data",
                    message_format: {
                        header: {
                            type: "object",
                            stamp: "time",
                            frame_id: "u32",
                        },
                        encoding: "string",
                        width: "u32",
                        height: "u32",
                        image: {
                            type: "array",
                            items: "u8",
                            length: 3
                        },
                    },
                }
            ],
        },
        subscribes_to: {
            topics: [
                {
                    node: "uvc_camera",
                    tag: "0.1.0",
                    name: "orphan_topic",
                    callback: "on_handle_video_frame",
                }
            ],
        },
    }"#,
    )
    .expect("valid JSON5 interfaces structure");

    todo!("Finish")
}
