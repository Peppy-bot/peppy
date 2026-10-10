//! The snippet scenarios that the Rust and the Python guides document alike.
//!
//! The snippets of the two languages mirror each other: the same node names,
//! instance ids, links, parameters and printed lines. Each scenario takes the
//! snippet root of one language, and the `rust_snippets` and `python_snippets`
//! test targets run every scenario against their own root, so a change to a
//! scenario is made once for both languages.

use crate::snippet_runner::{
    run_node_tests, run_snippet, run_snippet_with_contract_repo, run_snippet_with_deps,
    run_snippet_with_deps_asserting_output,
};

const PAIRINGS_ROOT: &str = "docs/src/content/docs/guides/snippets/pairings";

pub fn hello_world(snippets_root: &str) {
    run_snippet(snippets_root, "hello_world", &[]);
}

pub fn standalone_node(snippets_root: &str) {
    run_snippet(
        snippets_root,
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

/// One scenario covers both snippets because the receiver links to a running
/// `hello_world_param` instance.
pub fn hello_world_param_and_hello_receiver(snippets_root: &str) {
    run_snippet_with_deps(
        snippets_root,
        "hello_receiver",
        &["--link", "hello_world_param@hello_world_param_1"],
        &[("hello_world_param", &["name=planet"])],
    );
}

/// The testing guide's node-author loop: sync generates the `mock` and
/// `fixtures` modules of `peppygen` for the snippet, and its test
/// (`tests/hello.rs` in Rust, `tests/test_hello.py` in Python) boots the node
/// in-process through the generated harness and drives one message through the
/// mocked producer slot. The producer node is added to the stack (sync resolves
/// the slot's interfaces from its manifest) but never built or run: the harness
/// mocks it.
pub fn hello_receiver_node_tests(snippets_root: &str) {
    run_node_tests(snippets_root, "hello_receiver", &["hello_world_param"]);
}

// The paired duo from the "Pairing" guide. Each side declares one pairing slot
// of `arm_link/v1`, declared `cardinality: "zero_or_one"` because each node
// runs on its own. Running each node with the other absent entirely, under
// `--vacant-link <slot>=<why>`, proves the documented solo boot: the slot
// starts unpaired and silent, and the node still launches.

pub fn pairing_robot_arm(snippets_root: &str) {
    run_snippet_with_contract_repo(
        snippets_root,
        "robot_arm",
        &[
            "--vacant-link",
            "controller=docs snippet: this node boots solo",
        ],
        PAIRINGS_ROOT,
    );
}

pub fn pairing_arm_controller(snippets_root: &str) {
    run_snippet_with_contract_repo(
        snippets_root,
        "arm_controller",
        &["--vacant-link", "arm=docs snippet: this node boots solo"],
        PAIRINGS_ROOT,
    );
}

// The cardinality guide's `zero_or_one` producer slot, driven through both of
// its states. The snippet's `greeter` slot declares
// `cardinality: "zero_or_one"`, so its generated accessor is an `Option`
// (Rust) or an `Optional` (Python), and the node prints which of the two it
// got: the run log is therefore the node's own report of what the deployment
// wired.

pub fn optional_receiver_bound_to_a_producer(snippets_root: &str) {
    run_snippet_with_deps_asserting_output(
        snippets_root,
        "optional_receiver",
        &["--link", "greeter@hello_world_param_1"],
        &[("hello_world_param", &["name=planet"])],
        &["greeter bound: hello_world_param_1"],
    );
}

pub fn optional_receiver_declared_vacant(snippets_root: &str) {
    run_snippet_with_deps_asserting_output(
        snippets_root,
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
