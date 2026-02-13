use docs_integration_tests::snippet_runner::{run_snippet, run_snippet_with_deps};

const SNIPPETS_ROOT: &str = "docs/src/content/docs/guides/snippets/python";

#[test]
fn hello_world() {
    run_snippet(SNIPPETS_ROOT, "hello_world", &[]);
}

#[test]
fn first_node() {
    run_snippet(SNIPPETS_ROOT, "first_node", &[]);
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

// Combine both tests into one since they depend on each other and doing so avoids parallelism issues
#[test]
fn hello_world_param_and_hello_receiver() {
    run_snippet_with_deps(SNIPPETS_ROOT, "hello_receiver", &[], &["hello_world_param"]);
}
