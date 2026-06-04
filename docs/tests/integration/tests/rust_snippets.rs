use docs_integration_tests::snippet_runner::{
    run_snippet, run_snippet_with_deps, run_snippet_with_interface_repo,
};

const SNIPPETS_ROOT: &str = "docs/src/content/docs/guides/snippets/rust";
const INTERFACES_ROOT: &str = "docs/src/content/docs/guides/snippets/interfaces";

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

// The bidirectional pair from the "Bidirectional communication" guide. Each
// side consumes the other's interface through a `from_any: true` slot, so it
// builds and launches with NO `--bind` and zero producers present. Running
// each node on its own (the other absent entirely) proves the consumed slot
// is optional: launch must succeed with nothing bound, in any order.

#[test]
fn bidirectional_robot_arm() {
    run_snippet_with_interface_repo(SNIPPETS_ROOT, "robot_arm", &[], INTERFACES_ROOT);
}

#[test]
fn bidirectional_arm_controller() {
    run_snippet_with_interface_repo(SNIPPETS_ROOT, "arm_controller", &[], INTERFACES_ROOT);
}
