//! Wire identifiers used by the core node: the sender-target tag plus the
//! service/action names exposed to clients.

/// Sender-target tag used by the core node when emitting on the wire. The
/// core node is not declared via `manifest.tag` like regular nodes, so this
/// constant pins the tag on both publish and subscribe sides.
pub const CORE_NODE_TAG: &str = "core";

pub const CLOCK: &str = "clock";
/// Topic the daemon publishes a periodic liveness beat on. Each spawned node
/// subscribes via its daemon-liveness watchdog and shuts itself down if the
/// beat goes silent past the configured grace period (uncatchable-death safety
/// net). Keyed by the core-node name (deterministic per machine), so a
/// restarted daemon resumes on the same key and nodes survive the restart.
pub const DAEMON_HEARTBEAT: &str = "daemon_heartbeat";
pub const INFO: &str = "info";
pub const PING: &str = "ping";

pub const DATASTORE_STORE: &str = "datastore_store";
pub const DATASTORE_GET: &str = "datastore_get";
pub const DATASTORE_LIST: &str = "datastore_list";
pub const DATASTORE_REMOVE: &str = "datastore_remove";

pub const STACK_LAUNCH_ACTION: &str = "stack_launch";
pub const STACK_RESET: &str = "stack_reset";
pub const STACK_LIST: &str = "stack_list";
pub const STACK_BENCHMARK_ACTION: &str = "stack_benchmark";

pub const NODE_ADD_ACTION: &str = "node_add";
pub const NODE_BUILD_ACTION: &str = "node_build";
pub const NODE_RUN_ACTION: &str = "node_run";
pub const NODE_REMOVE: &str = "node_remove";
pub const NODE_INIT: &str = "node_init";
pub const NODE_INFO: &str = "node_info";
pub const NODE_STOP: &str = "node_stop";
pub const NODE_SYNC: &str = "node_sync";

pub const REPO_ADD: &str = "repo_add";
pub const REPO_EXCLUDE: &str = "repo_exclude";
pub const REPO_LIST: &str = "repo_list";
pub const REPO_REMOVE: &str = "repo_remove";
pub const REPO_REFRESH_ACTION: &str = "repo_refresh";

#[cfg(test)]
mod tests {
    use super::*;

    /// These strings are the wire contract between the daemon and every client:
    /// publish and subscribe sides must agree byte-for-byte. Pin them so an
    /// accidental rename is caught here rather than as a silent runtime
    /// "service unreachable" against an older/newer peer.
    #[test]
    fn service_names_match_the_wire_contract() {
        assert_eq!(CORE_NODE_TAG, "core");
        assert_eq!(CLOCK, "clock");
        assert_eq!(DAEMON_HEARTBEAT, "daemon_heartbeat");
        assert_eq!(INFO, "info");
        assert_eq!(PING, "ping");

        assert_eq!(DATASTORE_STORE, "datastore_store");
        assert_eq!(DATASTORE_GET, "datastore_get");
        assert_eq!(DATASTORE_LIST, "datastore_list");
        assert_eq!(DATASTORE_REMOVE, "datastore_remove");

        assert_eq!(STACK_LAUNCH_ACTION, "stack_launch");
        assert_eq!(STACK_RESET, "stack_reset");
        assert_eq!(STACK_LIST, "stack_list");
        assert_eq!(STACK_BENCHMARK_ACTION, "stack_benchmark");

        assert_eq!(NODE_ADD_ACTION, "node_add");
        assert_eq!(NODE_BUILD_ACTION, "node_build");
        assert_eq!(NODE_RUN_ACTION, "node_run");
        assert_eq!(NODE_REMOVE, "node_remove");
        assert_eq!(NODE_INIT, "node_init");
        assert_eq!(NODE_INFO, "node_info");
        assert_eq!(NODE_STOP, "node_stop");
        assert_eq!(NODE_SYNC, "node_sync");

        assert_eq!(REPO_ADD, "repo_add");
        assert_eq!(REPO_EXCLUDE, "repo_exclude");
        assert_eq!(REPO_LIST, "repo_list");
        assert_eq!(REPO_REMOVE, "repo_remove");
        assert_eq!(REPO_REFRESH_ACTION, "repo_refresh");
    }
}
