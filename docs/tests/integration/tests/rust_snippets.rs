use docs_integration_tests::snippet_runner::{run_snippet, run_snippet_with_deps};

const SNIPPETS_ROOT: &str = "docs/src/content/docs/guides/snippets/rust";

#[test]
fn hello_world() {
    run_snippet(SNIPPETS_ROOT, "hello_world", &[]);
}

#[test]
fn standalone_node() {
    run_snippet(
        SNIPPETS_ROOT,
        "standalone",
        &[
            "device_path=/dev/device1",
            "video.encoding=rgb",
            "video.frame_rate=30",
            "video.resolution.width=1280",
            "video.resolution.height=720",
        ],
    );
}

#[test]
fn hello_world_param_and_hello_receiver() {
    run_snippet_with_deps(SNIPPETS_ROOT, "hello_receiver", &[], &["hello_world_param"]);
}

// Guards the external-consumed-topic signature documented in
// docs/src/content/docs/advanced_guides/bidirectional_communication.mdx.
// The generator emits three arguments for `on_next_message_received` on
// external slots (node_runner, from_core_node, from_instance_id); if that
// shape ever drifts, this snippet will fail to compile.
#[test]
fn robot_arm_external_consumed_topic() {
    run_snippet(SNIPPETS_ROOT, "robot_arm", &[]);
}
