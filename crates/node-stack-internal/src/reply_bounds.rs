//! How long the daemon waits for the answer to each request it sends a node
//! of the stack or a peer daemon about one, before it gives up on it.

use std::time::Duration;

/// The bound on the answer to each kind of request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyBounds {
    /// A node's answer to its shutdown request.
    pub shutdown: Duration,
    /// A daemon's acknowledgement of a lifecycle notification. Short: the
    /// receiver only records what already happened, and the notification
    /// runs on lifecycle paths that must not stall behind a slow peer.
    pub notify: Duration,
    /// A peer daemon's answer to a pair commit. Strictly longer than
    /// [`Self::peer_update`] because it contains one: the receiving daemon
    /// commits to its registry, pins its own node over `peer_update`, and
    /// answers. Reusing the inner bound for the outer call would let the
    /// requester give up on a peer that is still inside a delivery it was
    /// granted the full time for, and then revert a pair the peer goes on to
    /// make.
    pub pair_commit: Duration,
    /// A node's answer to a `peer_update` delivery, after which the operation
    /// is failed and reverted. The service is pre-setup (registered before
    /// the node's ready signal), so a healthy endpoint answers promptly.
    pub peer_update: Duration,
    /// A node's answer to a `binding_update` delivery, after which it is
    /// abandoned.
    pub binding_update: Duration,
    /// A node's answer to an `observation_update` delivery, after which it is
    /// failed. The service is pre-setup (registered before the node's ready
    /// signal), so a healthy observer answers promptly.
    pub observation_update: Duration,
}

impl ReplyBounds {
    /// The bounds of a daemon: every released daemon runs with them, and the
    /// bounds the CLI and a launch's coordinator put on a daemon's work
    /// follow from them.
    pub const DEFAULT: Self = Self {
        shutdown: Duration::from_secs(5),
        notify: Duration::from_secs(10),
        pair_commit: Duration::from_secs(15),
        peer_update: Duration::from_secs(5),
        binding_update: Duration::from_secs(5),
        observation_update: Duration::from_secs(5),
    };
}

impl Default for ReplyBounds {
    fn default() -> Self {
        Self::DEFAULT
    }
}

const _: () = assert!(
    ReplyBounds::DEFAULT.pair_commit.as_millis() > ReplyBounds::DEFAULT.peer_update.as_millis(),
    "a pair commit contains the peer update of its receiver"
);
