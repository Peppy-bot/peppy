//! Public channel-address templates, pinned to the real wire builders.
//!
//! `platform-backend`'s AsyncAPI generator needs to render the zenoh
//! key-expression grammar with *placeholder* identity slots (the AsyncAPI
//! channel parameters `{coreNode}`, `{clientCoreNode}`, `{daemonInstance}`,
//! `{clientInstance}`, `{linkId}`, `{goalId}`). Nothing in [`super::zenoh_format`]
//! is public, so these thin helpers expose the grammar without letting the
//! generator hand-roll key expressions.
//!
//! The complex part of each shape — the service-root / action-root — is
//! delegated to the shared [`super::zenoh_format::service_root`] /
//! [`super::zenoh_format::action_root`] builders, so this module never
//! duplicates that grammar. The surrounding identity-slot arrangement is
//! formatted here and pinned, byte-for-byte, against the corresponding
//! `ZenohWireFormat` builder by the tests below.
//!
//! All slots are inserted verbatim: any string that is a valid keyexpr
//! segment (which the `{curly}` AsyncAPI parameters are — no `/`, `@`, and
//! not a reserved `*`/`_` sentinel) renders unchanged.

use super::zenoh_format::{action_root, service_root};
use super::{SenderTarget, ServiceKind};

/// Single-chunk wildcard, matching exactly one path segment. Kept as a local
/// literal (the canonical definition is private to [`super::zenoh_format`]).
const WILDCARD: &str = "*";

/// Fully-addressed service channel key expression, mirroring
/// [`super::zenoh_format::ZenohWireFormat::service_reply_keyexpr`]:
/// `{to_core}/{from_core}/{to_instance}/{from_instance}/{service_root}`.
///
/// * `to_core` / `to_instance` — the producer (daemon) identity the caller
///   addresses.
/// * `from_core` / `from_instance` — the caller's own identity.
/// * `target` — the producer's `(name, tag)` addressing (for a core-node
///   service this is `SenderTarget::node(core_node_name, "core")`).
/// * `link_id` — the link-id slot literal.
/// * `service_name`, `kind` — the service / action sub-service.
// The arguments are the wire's positional identity slots; grouping them into a
// struct would obscure that one-to-one mapping with the key expression.
#[allow(clippy::too_many_arguments)]
pub fn service_channel_address(
    to_core: &str,
    from_core: &str,
    to_instance: &str,
    from_instance: &str,
    target: &SenderTarget,
    link_id: &str,
    service_name: &str,
    kind: ServiceKind,
) -> String {
    format!(
        "{to_core}/{from_core}/{to_instance}/{from_instance}/{}",
        service_root(target, link_id, service_name, kind),
    )
}

/// Topic publish channel key expression for a non-pairing `target`, mirroring
/// [`super::zenoh_format::ZenohWireFormat::topic_publish`]:
/// `*/{as_core}/*/{as_instance}/topic/{discriminator}/{name}/{tag}/{link_id}/{topic_name}`.
///
/// The two leading wildcards are the subscriber identity slots, which this
/// shape always wildcards. `as_core` / `as_instance` are the publisher's
/// (daemon's) identity. A pairing publish names its recipient in a further
/// trailing segment that this grammar does not carry, so a pairing target
/// renders a key the wire never publishes on.
pub fn topic_channel_address(
    as_core: &str,
    as_instance: &str,
    target: &SenderTarget,
    link_id: &str,
    topic_name: &str,
) -> String {
    format!(
        "{WILDCARD}/{as_core}/{WILDCARD}/{as_instance}/topic/{}/{}/{}/{link_id}/{topic_name}",
        target.discriminator(),
        target.name(),
        target.tag(),
    )
}

/// Per-goal action feedback publish channel key expression, mirroring
/// [`super::zenoh_format::ZenohWireFormat::action_feedback_publish`]:
/// `*/{bound_core}/*/{as_instance}/{action_root}/feedback/{as_instance}/{goal_id}`.
///
/// `bound_core` / `as_instance` are the producer's (daemon's) identity;
/// `action_name` and `goal_id` identify the action and the specific goal.
pub fn action_feedback_channel_address(
    bound_core: &str,
    as_instance: &str,
    target: &SenderTarget,
    link_id: &str,
    action_name: &str,
    goal_id: &str,
) -> String {
    format!(
        "{WILDCARD}/{bound_core}/{WILDCARD}/{as_instance}/{}/feedback/{as_instance}/{goal_id}",
        action_root(target, link_id, action_name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::zenoh_format::ZenohWireFormat;
    use crate::wire::{ActionWireReceiver, ServiceWireReceiver, TopicWireSender};

    fn node(name: &str, tag: &str) -> SenderTarget {
        SenderTarget::node(name, tag).expect("valid node target")
    }

    /// The service helper must equal both a hand-written golden key expression
    /// and the real `service_reply_keyexpr` builder, for each `ServiceKind`
    /// (plain service + the goal/cancel/result action sub-services).
    #[test]
    fn service_channel_address_matches_golden_and_reply_builder() {
        let target = node("dstcore", "core");
        // (kind, expected service-root tail)
        let cases = [
            (ServiceKind::Service, "service/node/dstcore/core/_/svc"),
            (
                ServiceKind::ActionGoal,
                "action/node/dstcore/core/_/svc/goal",
            ),
            (
                ServiceKind::ActionCancel,
                "action/node/dstcore/core/_/svc/cancel",
            ),
            (
                ServiceKind::ActionResult,
                "action/node/dstcore/core/_/svc/result",
            ),
        ];
        for (kind, tail) in cases {
            let rendered = service_channel_address(
                "dstcore", "srccore", "dstinst", "srcinst", &target, "_", "svc", kind,
            );
            let golden = format!("dstcore/srccore/dstinst/srcinst/{tail}");
            assert_eq!(rendered, golden, "golden mismatch for {kind:?}");

            let receiver =
                ServiceWireReceiver::new("srccore", "srcinst", target.clone(), "svc", kind)
                    .unwrap();
            assert_eq!(
                rendered,
                ZenohWireFormat::service_reply_keyexpr(&receiver, "_", "dstcore", "dstinst"),
                "reply-builder mismatch for {kind:?}",
            );
        }
    }

    /// The topic helper must equal the golden and the real `topic_publish` builder.
    #[test]
    fn topic_channel_address_matches_golden_and_publish_builder() {
        let target = node("daemoncore", "core");
        let rendered = topic_channel_address("daemoncore", "daemoninst", &target, "_", "clock");
        assert_eq!(
            rendered,
            "*/daemoncore/*/daemoninst/topic/node/daemoncore/core/_/clock",
        );

        let sender = TopicWireSender::new(
            "daemoncore",
            "daemoninst",
            target.clone(),
            Some("_"),
            "clock",
        )
        .unwrap();
        assert_eq!(rendered, ZenohWireFormat::topic_publish(&sender));
    }

    /// The feedback helper must equal the golden and the real
    /// `action_feedback_publish` builder.
    #[test]
    fn action_feedback_channel_address_matches_golden_and_publish_builder() {
        let target = node("daemoncore", "core");
        let rendered = action_feedback_channel_address(
            "daemoncore",
            "daemoninst",
            &target,
            "_",
            "stack_launch",
            "goal-123",
        );
        assert_eq!(
            rendered,
            "*/daemoncore/*/daemoninst/action/node/daemoncore/core/_/stack_launch/feedback/daemoninst/goal-123",
        );

        let receiver =
            ActionWireReceiver::new("daemoncore", "daemoninst", target.clone(), "stack_launch")
                .unwrap();
        assert_eq!(
            rendered,
            ZenohWireFormat::action_feedback_publish(&receiver, "_", "goal-123"),
        );
    }

    /// The `{curly}` AsyncAPI parameters the `platform-backend` generator passes
    /// are valid keyexpr segments, so they render verbatim. This is the shape
    /// the generator pins its hardcoded templates against, so it doubles as the
    /// contract for that cross-repo test.
    #[test]
    fn asyncapi_placeholder_slots_render_verbatim() {
        let target = node("{coreNode}", "core");

        assert_eq!(
            service_channel_address(
                "{coreNode}",
                "{clientCoreNode}",
                "{daemonInstance}",
                "{clientInstance}",
                &target,
                "{linkId}",
                "health",
                ServiceKind::Service,
            ),
            "{coreNode}/{clientCoreNode}/{daemonInstance}/{clientInstance}/service/node/{coreNode}/core/{linkId}/health",
        );

        assert_eq!(
            topic_channel_address(
                "{coreNode}",
                "{daemonInstance}",
                &target,
                "{linkId}",
                "clock"
            ),
            "*/{coreNode}/*/{daemonInstance}/topic/node/{coreNode}/core/{linkId}/clock",
        );

        assert_eq!(
            action_feedback_channel_address(
                "{coreNode}",
                "{daemonInstance}",
                &target,
                "{linkId}",
                "stack_launch",
                "{goalId}",
            ),
            "*/{coreNode}/*/{daemonInstance}/action/node/{coreNode}/core/{linkId}/stack_launch/feedback/{daemonInstance}/{goalId}",
        );
    }
}
