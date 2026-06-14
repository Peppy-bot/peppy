//! Tests for `NodeStack`'s daemon-only add-log-path cache.

use std::path::PathBuf;

use node_stack::NodeStack;

use crate::helpers::config_common::core_node_config;

#[test]
fn add_log_path_round_trips_and_trims_keys() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));

    assert!(
        stack.add_log_path("sensor", "v1").is_none(),
        "no path recorded yet"
    );

    let path = PathBuf::from("/var/log/peppy/sensor_v1.add.log");
    stack.set_add_log_path("sensor", "v1", path.clone());
    assert_eq!(stack.add_log_path("sensor", "v1"), Some(path.clone()));

    // Keys are trimmed on both write and read, so surrounding whitespace
    // resolves to the same entry.
    assert_eq!(
        stack.add_log_path("  sensor ", " v1 "),
        Some(path),
        "lookup should trim the key"
    );
    stack.set_add_log_path(" sensor", "v1 ", PathBuf::from("/replaced.log"));
    assert_eq!(
        stack.add_log_path("sensor", "v1"),
        Some(PathBuf::from("/replaced.log")),
        "a whitespace-padded write should overwrite the trimmed entry"
    );
}
