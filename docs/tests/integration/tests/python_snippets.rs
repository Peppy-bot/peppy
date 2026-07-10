use docs_integration_tests::snippet_runner::{
    run_snippet, run_snippet_with_contract_repo, run_snippet_with_deps,
};

const SNIPPETS_ROOT: &str = "docs/src/content/docs/guides/snippets/python";
const PAIRINGS_ROOT: &str = "docs/src/content/docs/guides/snippets/pairings";

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

// Combine both tests into one since they depend on each other and doing so avoids parallelism issues
#[test]
fn hello_world_param_and_hello_receiver() {
    run_snippet_with_deps(SNIPPETS_ROOT, "hello_receiver", &[], &["hello_world_param"]);
}

// The paired duo from the "Pairing" guide. Each side declares one required
// pairing slot of `arm_link/v1`. Running each node on its own (the other
// absent entirely) with `--defer-pair <slot>` proves the documented solo
// boot: the slot starts unpaired and silent, and the node still launches.

#[test]
fn pairing_robot_arm() {
    run_snippet_with_contract_repo(
        SNIPPETS_ROOT,
        "robot_arm",
        &["--defer-pair", "controller"],
        PAIRINGS_ROOT,
    );
}

#[test]
fn pairing_arm_controller() {
    run_snippet_with_contract_repo(
        SNIPPETS_ROOT,
        "arm_controller",
        &["--defer-pair", "arm"],
        PAIRINGS_ROOT,
    );
}
