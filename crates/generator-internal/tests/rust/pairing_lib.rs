//! Generate-and-compile fixture for pairing peer modules (Rust): a synthetic
//! two-role pairing (`arm_link/v1`, roles controller/arm) generates
//! `paired_topics/<link_id>/<topic>` modules for both directions, and a user node
//! exercising the full generated surface — slot consts, `paired()` /
//! `wait_paired()`, the slot-scoped publisher, and the pin-following
//! subscription — must compile against the real peppylib. This is the
//! type-level proof that the generated seams line up with
//! `peppylib::runtime::{subscribe_peer, NodeRunner::peer}`.

use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::node::NativeEmittedTopic;
use generator::LanguageGenerator;
use generator::PeerContext;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

use crate::helpers::{
    STUB_NODE_CONFIG, compile_project, copy_config_to_output, init_cargo_user_node, init_test_env,
    test_peppy_dirs,
};

const JOINT_COMMANDS: &str = r#"
{
  name: "joint_commands",
  qos_profile: "reliable",
  message_format: {
    target_positions: { $type: "array", $items: "f64", $length: 3 },
    max_velocity: "f64"
  }
}
"#;

const JOINT_STATES: &str = r#"
{
  name: "joint_states",
  qos_profile: "sensor_data",
  message_format: {
    positions: { $type: "array", $items: "f64", $length: 3 },
    timestamp: "time"
  }
}
"#;

/// The controller side of `arm_link/v1`: emits `joint_commands`, consumes
/// `joint_states`, through one slot whose link_id is `arm`.
fn controller_peer_context() -> PeerContext {
    PeerContext {
        link_id: "arm".to_string(),
        pairing_name: "arm_link".to_string(),
        pairing_tag: "v1".to_string(),
    }
}

#[test]
fn generated_peer_modules_compile_against_peppylib() {
    let temp_dir = TempDir::new_in(crate::helpers::test_tmp_root()).unwrap();
    let commands: NativeEmittedTopic = serde_json5::from_str(JOINT_COMMANDS).unwrap();
    let states: NativeEmittedTopic = serde_json5::from_str(JOINT_STATES).unwrap();

    let (mut generator, output_dir, user_node, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir, STUB_NODE_CONFIG);
    let peer = controller_peer_context();
    generator.add_peer_emitted_topic(&commands, &peer).unwrap();
    generator.add_peer_consumed_topic(&states, &peer).unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(&output_dir, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    // Both directions of the slot nest under paired_topics/<link_id>/<topic>.
    let src_dir = user_node.join(PEPPYGEN_OUTPUT_PATH).join("src");
    let paired_topics_dir = src_dir.join("paired_topics");
    for module in ["arm/joint_commands.rs", "arm/joint_states.rs"] {
        assert!(
            paired_topics_dir.join(module).exists(),
            "expected generated module paired_topics/{module}"
        );
    }
    // Neither direction may leak into the flat consumed/emitted namespaces.
    // Asserted on directory contents so a misplaced-but-working module fails
    // loudly instead of passing by compiling.
    for category in ["consumed_topics", "emitted_topics"] {
        let dir = src_dir.join(category);
        // The scaffold creates every category dir, so a missing one means the
        // layout changed and this check would otherwise pass vacuously.
        assert!(
            dir.is_dir(),
            "expected category dir {}; if the scaffold stopped creating empty \
             categories, rewrite this assertion rather than deleting it",
            dir.display()
        );
        let entries: Vec<String> = fs::read_dir(&dir)
            .expect("category dir reads")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "mod.rs")
            .collect();
        assert!(
            entries.is_empty(),
            "pairing topics must not generate modules under {category}, found: {entries:?}"
        );
    }

    init_cargo_user_node(&user_node);
    // Exercise every generated seam; `compile_project` panics on any type
    // error, so this main is the assertion.
    let user_main = r#"
use peppygen::NodeBuilder;
use peppygen::Result;
use peppygen::paired_topics::arm::{joint_commands, joint_states};

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        // Slot consts are plain strings shared by both topic modules.
        assert_eq!(joint_commands::LINK_ID, "arm");
        assert_eq!(joint_states::PAIRING_NAME, "arm_link");
        assert_eq!(joint_states::PAIRING_TAG, "v1");

        // Pin-state surface: an optional probe and an awaitable identity.
        let currently: Option<peppygen::PeerInfo> = joint_commands::paired(&node_runner)?;
        if currently.is_none() {
            // A held subscription is legal while unpaired (stays silent).
            let mut subscription = joint_states::subscribe(&node_runner).await?;

            let peer: peppygen::PeerInfo = joint_states::wait_paired(&node_runner).await?;
            println!(
                "paired with {}/{} (their slot: {})",
                peer.producer.core_node, peer.producer.instance_id, peer.peer_link_id
            );

            // Slot-scoped publisher + message builder.
            let publisher = joint_commands::declare_publisher(&node_runner).await?;
            let payload = joint_commands::build_message([0.0, 0.5, 1.0], 0.25)?;
            publisher.publish(payload).await?;

            if let Some((producer, states)) = subscription.next().await? {
                println!(
                    "{} joints from {}/{}",
                    states.positions.len(),
                    producer.core_node,
                    producer.instance_id
                );
            }
        }
        Ok(())
    })
}
"#;
    fs::write(user_node.join("src").join("main.rs"), user_main).unwrap();

    compile_project(&user_node);
}
