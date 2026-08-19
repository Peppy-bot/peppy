//! Dev tool: generate a sample peppygen crate (both languages) covering every
//! interface kind, into /tmp/sample_node, for inspecting the emitted surface,
//! including the `mock`/`fixtures` test surfaces.
//!
//! ```sh
//! cargo run -p generator --example gen_sample
//! ```

use config::node::{
    Cardinality, ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, NativeEmittedTopic,
    NativeExposedAction, NativeExposedService,
};
use generator::{ConsumedActionMessage, DependencyContext, LanguageGenerator, PeerContext};
use std::fs;
use std::path::Path;

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: { name: "generated_node", tag: "v1" },
  execution: { language: "rust", run_cmd: ["./target/release/generated_node"] }
}
"#;

const EMITTED_TOPIC: &str = r#"{
  name: "video_stream",
  qos_profile: "sensor_data",
  message_format: { width: "u32", frame: { $type: "array", $items: "u8" } }
}"#;

const EXPOSED_SERVICE: &str = r#"{
  name: "enable_camera",
  request_message_format: { enable: "bool" },
  response_message_format: { enabled: "bool" }
}"#;

const EXPOSED_ACTION: &str = r#"{
  name: "move_arm",
  goal_service: {
    request_message_format: { arm_id: "u16" },
    response_message_format: { accepted: "bool" }
  },
  feedback_topic: {
    qos_profile: "sensor_data",
    message_format: { progress: "f64" }
  },
  result_service: { response_message_format: { success: "bool" } }
}"#;

const CONSUMED_TOPIC: &str = r#"{ link_id: "camera", name: "video_stream" }"#;
const CONSUMED_TOPIC_FORMAT: &str = r#"{ width: "u32", frame: { $type: "array", $items: "u8" } }"#;

const CONSUMED_SERVICE: &str = r#"{ link_id: "camera", name: "enable_camera" }"#;
const CONSUMED_SERVICE_REQ: &str = r#"{ enable: "bool" }"#;
const CONSUMED_SERVICE_RESP: &str = r#"{ enabled: "bool" }"#;

const CONSUMED_ACTION: &str = r#"{ link_id: "brain", name: "plan_motion" }"#;
const CA_GOAL: &str = r#"{ arm_id: "u16" }"#;
const CA_GOAL_RESP: &str = r#"{ accepted: "bool" }"#;
const CA_FEEDBACK: &str = r#"{ progress: "f64" }"#;
const CA_RESULT: &str = r#"{ success: "bool" }"#;

const PEER_COMMANDS: &str = r#"{
  name: "joint_commands",
  qos_profile: "reliable",
  message_format: { max_velocity: "f64" }
}"#;

const PEER_STATES: &str = r#"{
  name: "joint_states",
  qos_profile: "sensor_data",
  message_format: { positions: { $type: "array", $items: "f64", $length: 3 } }
}"#;

fn populate<G: LanguageGenerator>(generator: &mut G) {
    let emitted: NativeEmittedTopic = serde_json5::from_str(EMITTED_TOPIC).unwrap();
    generator.add_emitted_topic(&emitted, None).unwrap();

    let service: NativeExposedService = serde_json5::from_str(EXPOSED_SERVICE).unwrap();
    generator.add_exposed_service(&service, None).unwrap();

    let action: NativeExposedAction = serde_json5::from_str(EXPOSED_ACTION).unwrap();
    generator.add_exposed_action(&action, None).unwrap();

    let ctopic: ConsumedTopic = serde_json5::from_str(CONSUMED_TOPIC).unwrap();
    let cformat: MessageFormat = serde_json5::from_str(CONSUMED_TOPIC_FORMAT).unwrap();
    generator
        .add_consumed_topic(
            &ctopic,
            cformat,
            &DependencyContext::native("uvc_camera", "v1", "camera", Cardinality::One),
        )
        .unwrap();

    let cservice: ConsumedService = serde_json5::from_str(CONSUMED_SERVICE).unwrap();
    let req: MessageFormat = serde_json5::from_str(CONSUMED_SERVICE_REQ).unwrap();
    let resp: MessageFormat = serde_json5::from_str(CONSUMED_SERVICE_RESP).unwrap();
    generator
        .add_consumed_service(
            &cservice,
            &req,
            &resp,
            &DependencyContext::native("uvc_camera", "v1", "camera", Cardinality::One),
        )
        .unwrap();

    let caction: ConsumedAction = serde_json5::from_str(CONSUMED_ACTION).unwrap();
    let messages = ConsumedActionMessage {
        goal_request: Some(serde_json5::from_str(CA_GOAL).unwrap()),
        goal_response: Some(serde_json5::from_str(CA_GOAL_RESP).unwrap()),
        feedback: Some(serde_json5::from_str(CA_FEEDBACK).unwrap()),
        result_response: Some(serde_json5::from_str(CA_RESULT).unwrap()),
    };
    generator
        .add_consumed_action(
            &caction,
            &messages,
            &DependencyContext::native("brain", "v1", "brain", Cardinality::One),
        )
        .unwrap();

    let commands: NativeEmittedTopic = serde_json5::from_str(PEER_COMMANDS).unwrap();
    let states: NativeEmittedTopic = serde_json5::from_str(PEER_STATES).unwrap();
    let peer = PeerContext {
        link_id: "arm".to_string(),
        pairing_name: "arm_link".to_string(),
        pairing_tag: "v1".to_string(),
        optional: false,
    };
    generator.add_peer_emitted_topic(&commands, &peer).unwrap();
    generator.add_peer_consumed_topic(&states, &peer).unwrap();

    let observer = PeerContext {
        link_id: "observed_arm".to_string(),
        pairing_name: "arm_link".to_string(),
        pairing_tag: "v1".to_string(),
        optional: false,
    };
    generator
        .add_observed_topic(&states, &observer, Cardinality::One)
        .unwrap();
    let multi_observer = PeerContext {
        link_id: "fleet_arms".to_string(),
        pairing_name: "arm_link".to_string(),
        pairing_tag: "v1".to_string(),
        optional: false,
    };
    generator
        .add_observed_topic(&states, &multi_observer, Cardinality::ZeroOrMore)
        .unwrap();
}

fn prepare(root: &Path) -> std::path::PathBuf {
    let user_node = root.join("user_node");
    let output_dir = user_node.join(config::consts::PEPPYGEN_OUTPUT_PATH);
    let _ = fs::remove_dir_all(&user_node);
    fs::create_dir_all(&output_dir).unwrap();
    fs::write(
        user_node.join(config::consts::NODE_CONFIG_FILE),
        NODE_CONFIG,
    )
    .unwrap();
    fs::write(
        output_dir.join(config::consts::NODE_CONFIG_FILE),
        NODE_CONFIG,
    )
    .unwrap();
    output_dir
}

fn main() {
    let peppy_dirs = daemon_config::consts::PeppyDirs::default();

    let rust_out = prepare(Path::new("/tmp/sample_node/rust"));
    let mut rust_gen = generator::RustGenerator::new();
    rust_gen.set_node_identity("generated_node", "v1");
    populate(&mut rust_gen);
    rust_gen
        .build(&rust_out, &peppy_dirs, Default::default())
        .unwrap();
    println!("rust sample: {}", rust_out.display());

    let python_out = prepare(Path::new("/tmp/sample_node/python"));
    let mut python_gen = generator::PythonGenerator::new();
    python_gen.set_node_identity("generated_node", "v1");
    populate(&mut python_gen);
    python_gen
        .build(&python_out, &peppy_dirs, Default::default())
        .unwrap();
    println!("python sample: {}", python_out.display());
}
