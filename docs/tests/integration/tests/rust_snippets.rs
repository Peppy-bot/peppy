use docs_integration_tests::snippet_suite;

const SNIPPETS_ROOT: &str = "docs/src/content/docs/guides/snippets/rust";

#[test]
fn hello_world() {
    snippet_suite::hello_world(SNIPPETS_ROOT);
}

#[test]
fn standalone_node() {
    snippet_suite::standalone_node(SNIPPETS_ROOT);
}

#[test]
fn hello_world_param_and_hello_receiver() {
    snippet_suite::hello_world_param_and_hello_receiver(SNIPPETS_ROOT);
}

#[test]
fn hello_receiver_node_tests() {
    snippet_suite::hello_receiver_node_tests(SNIPPETS_ROOT);
}

#[test]
fn pairing_robot_arm() {
    snippet_suite::pairing_robot_arm(SNIPPETS_ROOT);
}

#[test]
fn pairing_arm_controller() {
    snippet_suite::pairing_arm_controller(SNIPPETS_ROOT);
}

#[test]
fn optional_receiver_bound_to_a_producer() {
    snippet_suite::optional_receiver_bound_to_a_producer(SNIPPETS_ROOT);
}

#[test]
fn optional_receiver_declared_vacant() {
    snippet_suite::optional_receiver_declared_vacant(SNIPPETS_ROOT);
}
