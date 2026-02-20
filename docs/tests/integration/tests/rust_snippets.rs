use docs_integration_tests::snippet_runner::{run_snippet, run_snippet_with_deps};
use serial_test::serial;

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
            "device.physical=/dev/device1",
            "device.sim=the_camera",
            "device.priority=physical",
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
