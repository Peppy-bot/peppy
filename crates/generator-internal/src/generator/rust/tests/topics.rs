use super::*;
use config::node::{ExposedTopic, SubscribedTopic};

const EXPOSED_TOPIC_EXAMPLE: &str = r#"
{
  name: "push_frame",
  qos_profile: "sensor_data",
  message_format: {
    header: {
    type: "object",
    stamp: "time",
    frame_id: "u32"
  },
  encoding: "string",
    width: "u32",
    height: "u32",
    image: {
      type: "array",
      items: "u8",
      length: 3
    }
  }
}
"#;

#[test]
fn exposed_topic_gen_calling_code() {
    let topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_topic(&topic).unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    let rendered = single_artifact(artifacts);

    assert_rendered!(
        rendered.contains("let mut message = capnp::message::Builder::new_default();"),
        &rendered,
        "expected capnp message builder"
    );
    assert_rendered!(
        rendered.contains("crate::capnp::push_frame_message_capnp::push_frame_message::Builder"),
        &rendered,
        "expected capnp root initialization"
    );
    assert_rendered!(
        rendered.contains("root.set_encoding(encoding.as_str());"),
        &rendered,
        "expected encoding setter"
    );
    assert_rendered!(
        rendered.contains("root.set_image(image.as_ref());"),
        &rendered,
        "expected fixed-size array setter"
    );
    assert_rendered!(
        rendered.contains("root.reborrow().init_header();"),
        &rendered,
        "expected nested header initialization"
    );
    assert_rendered!(
        rendered.contains("config::convert_time"),
        &rendered,
        "expected timestamp conversion helper"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::write_message"),
        &rendered,
        "expected serialization call"
    );
    assert_rendered!(
        rendered.contains("header: PushFrameHeader"),
        &rendered,
        "expected structured header argument"
    );
    assert_rendered!(
        rendered.contains("image: [u8; 3]"),
        &rendered,
        "expected fixed-size array argument"
    );
    assert_rendered!(
        rendered.contains("pub struct PushFrameHeader"),
        &rendered,
        "expected generated struct for nested object"
    );
    assert_rendered!(
        rendered.contains("pub struct PushFrame {"),
        &rendered,
        "expected topic messenger struct"
    );
    assert_rendered!(
        rendered.contains("pub fn new(host: &str, port: u16) -> Self"),
        &rendered,
        "expected blocking constructor"
    );
    assert_rendered!(
        rendered.contains("std::env::var(\"PEPPY_NAMESPACE\")"),
        &rendered,
        "expected namespace initialization from environment"
    );
    assert_rendered!(
        rendered.contains("pub fn emit("),
        &rendered,
        "expected sync emit method"
    );
    assert_rendered!(
        rendered.contains(") -> peppylib::PeppyResult<()>"),
        &rendered,
        "expected peppylib result type"
    );
    assert_rendered!(
        rendered.contains("pub async fn emit_async("),
        &rendered,
        "expected async emit method"
    );
    assert_rendered!(
        rendered.contains("runtime.block_on(self.emit_async"),
        &rendered,
        "expected sync emit to delegate to async variant"
    );
    assert_rendered!(
        rendered.contains("let topic_name = \"push_frame\";"),
        &rendered,
        "expected topic name literal"
    );
    assert_rendered!(
        rendered.contains("let qos = config::node::QoSProfile::SensorData;"),
        &rendered,
        "expected qos profile literal"
    );
    assert_rendered!(
        rendered.contains("self.messenger.emit"),
        &rendered,
        "expected messenger emit call"
    );
}

#[test]
fn subscribed_topic_returns_arguments() {
    let topic = r#"
        {
            node: "uvc_camera",
            name: "stream",
            tag: "0.1.0",
            callback: "on_video_frame_received"
        }
        "#;
    let topic: SubscribedTopic = serde_json5::from_str(topic).unwrap();
    let format = r#"
        {
            header: {
                type: "object",
                stamp: "time",
                frame_id: "u32"
            },
            encoding: "string",
            width: "u32",
            height: "u32",
            image: {
                type: "array",
                items: "u8",
                length: 3
            }
        }
        "#;
    let format: MessageFormat = serde_json5::from_str(format).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_topic(&topic, Some(&format))
        .unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    let rendered = single_artifact(artifacts);

    println!("generated subscribed topic code:\n{rendered}");

    assert_rendered!(
        rendered
            .contains("pub async fn on_video_frame_received() -> OnVideoFrameReceivedArguments"),
        &rendered,
        "expected async subscriber function with return type"
    );
    assert_rendered!(
        rendered.contains("pub struct OnVideoFrameReceivedArguments"),
        &rendered,
        "expected return struct definition"
    );
    assert_rendered!(
        rendered.contains("pub struct OnVideoFrameReceivedHeader"),
        &rendered,
        "expected nested struct definition"
    );
    assert_rendered!(
        rendered.contains("image: [u8; 3]"),
        &rendered,
        "expected array element type"
    );
}

#[test]
fn create_lib_with_exposed_topic_artifact() {
    let temp_dir = TempDir::new().unwrap();
    let topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_topic(&topic).unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

    assert!(
        output_dir.join("Cargo.toml").exists(),
        "Expected Cargo.toml to be generated in the temporary crate directory"
    );
    assert!(
        !output_dir.join(PEPPY_NODE_CONFIG_FILE).exists(),
        "Generated crate should not keep a copy of the node configuration file"
    );

    let lib_rs = output_dir.join("src/lib.rs");
    assert!(
        lib_rs.exists(),
        "Expected lib.rs to exist so `peppygen::topics` is reachable"
    );
    let lib_contents = std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
    assert!(
        lib_contents.contains("pub mod topics;"),
        "Expected generated lib.rs to re-export the `topics` module, got:\n{}",
        lib_contents
    );

    let topics_mod = output_dir.join("src/topics.rs");
    assert!(
        topics_mod.exists(),
        "Expected topics module file to exist so `peppygen::topics::<module>` resolves"
    );
    let topics_contents =
        std::fs::read_to_string(&topics_mod).expect("failed to read topics module");
    assert!(
        topics_contents.contains("pub mod push_frame;"),
        "Expected topics module to expose generated `push_frame` module, got:\n{}",
        topics_contents
    );

    let push_frame_mod = output_dir.join("src/topics/push_frame.rs");
    assert!(
        push_frame_mod.exists(),
        "Expected generated topic module at {:?}",
        push_frame_mod
    );
    let push_frame_contents =
        std::fs::read_to_string(&push_frame_mod).expect("failed to read push_frame module");
    assert!(
        push_frame_contents.contains("pub struct PushFrame {"),
        "Expected generated module to define topic struct, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub fn emit("),
        "Expected generated module to expose sync emit method, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub async fn emit_async("),
        "Expected generated module to expose async emit method, got:\n{}",
        push_frame_contents
    );
}

#[test]
fn create_lib_with_exposed_double_topic_artifact() {
    let temp_dir = TempDir::new().unwrap();
    let topic2 = r#"
        {
            name: "push_lidar_object", // The name of the topic inside the `lidar_sensor` node
            qos_profile: "sensor_data",
            message_format: {
                header: {
                  type: "object",
                  stamp: "time",
                  frame_id: "u32",
                },
                x: "f32",
                y: "f32",
                z: "f32",
                intensity: "f32",
                return_type: "u8", // e.g. first return, last return
                classification: "u8", // type of object detected
            },
        }
        "#;
    let topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let topic2: ExposedTopic = serde_json5::from_str(topic2).unwrap();

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_topic(&topic).unwrap();
    generator.add_exposed_topic(&topic2).unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

    assert!(
        output_dir.join("Cargo.toml").exists(),
        "Expected Cargo.toml to be generated in the temporary crate directory"
    );

    let lib_rs = output_dir.join("src/lib.rs");
    assert!(
        lib_rs.exists(),
        "Expected lib.rs to exist so `peppygen::topics` is reachable"
    );
    let lib_contents = std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
    assert!(
        lib_contents.contains("pub mod topics;"),
        "Expected generated lib.rs to re-export the `topics` module, got:\n{}",
        lib_contents
    );

    let topics_mod = output_dir.join("src/topics.rs");
    assert!(
        topics_mod.exists(),
        "Expected topics module file to exist so `peppygen::topics::<module>` resolves"
    );
    let topics_contents =
        std::fs::read_to_string(&topics_mod).expect("failed to read topics module");
    assert!(
        topics_contents.contains("pub mod push_frame;"),
        "Expected topics module to expose generated `push_frame` module, got:\n{}",
        topics_contents
    );
    assert!(
        topics_contents.contains("pub mod push_lidar_object;"),
        "Expected topics module to expose generated `push_frame` module, got:\n{}",
        topics_contents
    );

    let push_frame_mod = output_dir.join("src/topics/push_frame.rs");
    assert!(
        push_frame_mod.exists(),
        "Expected generated topic module at {:?}",
        push_frame_mod
    );
    let push_frame_contents =
        std::fs::read_to_string(&push_frame_mod).expect("failed to read push_frame module");
    assert!(
        push_frame_contents.contains("pub struct PushFrame {"),
        "Expected generated module to define topic struct, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub fn emit("),
        "Expected generated module to expose sync emit method, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub async fn emit_async("),
        "Expected generated module to expose async emit method, got:\n{}",
        push_frame_contents
    );

    let push_lidar_mod = output_dir.join("src/topics/push_lidar_object.rs");
    assert!(
        push_lidar_mod.exists(),
        "Expected generated topic module at {:?}",
        push_lidar_mod
    );
    let push_lidar_contents =
        std::fs::read_to_string(&push_lidar_mod).expect("failed to read push_lidar_object module");
    assert!(
        push_lidar_contents.contains("pub struct PushLidarObject {"),
        "Expected generated module to define topic struct, got:\n{}",
        push_lidar_contents
    );
    assert!(
        push_lidar_contents.contains("pub fn emit("),
        "Expected generated module to expose sync emit method, got:\n{}",
        push_lidar_contents
    );
    assert!(
        push_lidar_contents.contains("pub async fn emit_async("),
        "Expected generated module to expose async emit method, got:\n{}",
        push_lidar_contents
    );
}
