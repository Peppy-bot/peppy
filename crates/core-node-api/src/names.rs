//! Wire identifiers used by the core node: the sender-target tag plus the
//! service/action names exposed to clients.

/// Sender-target tag used by the core node when emitting on the wire. The
/// core node is not declared via `manifest.tag` like regular nodes, so this
/// constant pins the tag on both publish and subscribe sides.
pub const CORE_NODE_TAG: &str = "core";

pub const CLOCK: &str = "clock";
pub const INFO: &str = "info";
pub const PING: &str = "ping";

pub const STACK_LAUNCH_ACTION: &str = "stack_launch";
pub const STACK_RESET: &str = "stack_reset";
pub const STACK_LIST: &str = "stack_list";

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
