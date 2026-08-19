use docs_integration_tests::snippet_runner::{
    run_node_tests, run_snippet, run_snippet_with_contract_repo, run_snippet_with_deps,
    run_snippet_with_deps_asserting_output,
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
    run_snippet_with_deps(
        SNIPPETS_ROOT,
        "hello_receiver",
        &["--link", "hello_world_param@hello_world_param_1"],
        &[("hello_world_param", &["name=planet"])],
    );
}

// The testing guide's node-author loop: sync generates `peppygen.mock` /
// `peppygen.fixtures` for the snippet, and its `tests/test_hello.py` boots
// the node in-process through the generated harness and drives one message
// through the mocked producer slot. The producer node is added to the stack
// (sync resolves the slot's interfaces from its manifest) but never built or
// run: the harness mocks it.
#[test]
fn hello_receiver_node_tests() {
    run_node_tests(SNIPPETS_ROOT, "hello_receiver", &["hello_world_param"]);
}

// The paired duo from the "Pairing" guide. Each side declares one pairing slot
// of `arm_link/v1`, declared `optional: true` because each node runs on its
// own. Running each node with the other absent entirely, under
// `--vacant-link <slot>=<why>`, proves the documented solo boot: the slot
// starts unpaired and silent, and the node still launches.

#[test]
fn pairing_robot_arm() {
    run_snippet_with_contract_repo(
        SNIPPETS_ROOT,
        "robot_arm",
        &[
            "--vacant-link",
            "controller=docs snippet: this node boots solo",
        ],
        PAIRINGS_ROOT,
    );
}

#[test]
fn pairing_arm_controller() {
    run_snippet_with_contract_repo(
        SNIPPETS_ROOT,
        "arm_controller",
        &["--vacant-link", "arm=docs snippet: this node boots solo"],
        PAIRINGS_ROOT,
    );
}

// The cardinality guide's `zero_or_one` producer slot, driven through both of
// its states. The snippet's `greeter` slot declares
// `cardinality: "zero_or_one"`, so its generated accessor is an `Optional`,
// and the node prints which of the two it got: the run log is therefore the
// node's own report of what the deployment wired.

#[test]
fn optional_receiver_bound_to_a_producer() {
    run_snippet_with_deps_asserting_output(
        SNIPPETS_ROOT,
        "optional_receiver",
        &["--link", "greeter@hello_world_param_1"],
        &[("hello_world_param", &["name=planet"])],
        &["greeter bound: hello_world_param_1"],
    );
}

#[test]
fn optional_receiver_declared_vacant() {
    run_snippet_with_deps_asserting_output(
        SNIPPETS_ROOT,
        "optional_receiver",
        &[
            "--vacant-link",
            "greeter=docs snippet: this rig ships without a greeter",
        ],
        // The producer is in the stack (a `depends_on.nodes` slot resolves its
        // interfaces from the producer's manifest at sync time, bound or not)
        // and running. This instance still declares the slot empty, which is
        // the point: emptiness is the deployment's decision, not an accident of
        // what happens to be up.
        &[("hello_world_param", &["name=planet"])],
        &["no greeter bound"],
    );
}
