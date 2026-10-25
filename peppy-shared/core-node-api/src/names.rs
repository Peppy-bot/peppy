//! The core node's sender-target tag: the one wire identifier that is not a
//! method. Method wire names live on the [`crate::registry`] id enums
//! ([`crate::ServiceId`], [`crate::ActionId`], [`crate::TopicId`]) as
//! `.name()`.

/// Sender-target tag used by the core node when emitting on the wire. The
/// core node is not declared via `manifest.tag` like regular nodes, so this
/// constant pins the tag on both publish and subscribe sides.
pub const CORE_NODE_TAG: &str = "core";

/// The instance id every process of the command line sends as. The presence
/// generation of each process makes two processes two senders. The daemon
/// reads it as the command line, which has no scope and acts for the person
/// or program that runs it.
pub const COMMAND_LINE_INSTANCE_ID: &str = "peppy-cli";

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag is part of every core-node wire key: publish and subscribe
    /// sides must agree byte-for-byte. Pin it so an accidental rename is
    /// caught here rather than as a silent runtime "service unreachable"
    /// against an older/newer peer.
    #[test]
    fn tag_matches_the_wire_contract() {
        assert_eq!(CORE_NODE_TAG, "core");
    }
}
