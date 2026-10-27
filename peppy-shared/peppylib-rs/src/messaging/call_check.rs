//! The proof of the sender of the calls to a daemon, on both sides.
//!
//! A session that has a signer signs each user request that it sends to a
//! daemon (see [`daemon_receiver`]); a session that has a [`CallCheck`]
//! (the session of a daemon) runs it on each call that its endpoints receive,
//! before any handler reads the call. A refused call gets the failure of the
//! check as its reply, and its handler never runs; a call that the check takes
//! reaches its handler with the [`VerifiedCaller`] that the check gives.

use core_node_api::names;
use pmi::{CallProof, CallReceiver, Failure, SenderTarget, ServiceWireSender, SignedCall};

/// Verifies the sender of each call that a session receives.
pub trait CallCheck: Send + Sync {
    /// The verified caller of `call`, or the failure that refuses it.
    fn verify(&self, call: &ReceivedCall<'_>) -> Result<VerifiedCaller, Failure>;
}

/// A call as its receiver reads it.
#[derive(Clone, Copy, Debug)]
pub struct ReceivedCall<'a> {
    /// The core node that the key expression of the call gives as its sender.
    pub caller_core_node: &'a str,
    /// The instance id that the key expression of the call gives as its sender.
    pub caller_instance_id: &'a str,
    /// What the proof of the call signs.
    pub signed: SignedCall<'a>,
    /// The proof that the call carries, when it carries one.
    pub proof: Option<&'a CallProof>,
}

/// What a verified caller is to a daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallerKind {
    /// The command line of a machine.
    CommandLine,
    /// A daemon, under the instance id of its boot.
    Daemon,
    /// An instance of a node that a daemon started.
    Instance,
}

/// A caller whose call proved its identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VerifiedCaller {
    core_node: String,
    instance_id: String,
    kind: CallerKind,
}

impl VerifiedCaller {
    pub fn new(
        core_node: impl Into<String>,
        instance_id: impl Into<String>,
        kind: CallerKind,
    ) -> Self {
        Self {
            core_node: core_node.into(),
            instance_id: instance_id.into(),
            kind,
        }
    }

    /// The core node of the machine of the caller.
    pub fn core_node(&self) -> &str {
        &self.core_node
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn kind(&self) -> CallerKind {
        self.kind
    }
}

impl std::fmt::Display for VerifiedCaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` of `{}`", self.instance_id, self.core_node)
    }
}

/// The receiver of the call that `sender` sends, when the call goes to a
/// daemon: a raw service or action of a daemon, whose target is the node of
/// the daemon (`<core node>:core`), or a member of a daemon interface. `None`
/// for any other call.
pub(crate) fn daemon_receiver(sender: &ServiceWireSender) -> Option<CallReceiver> {
    let receiving_core_node = match sender.to_target() {
        SenderTarget::Node(node) if node.tag() == names::CORE_NODE_TAG => Some(node.name()),
        SenderTarget::Contract(contract)
            if names::is_daemon_interface(contract.name(), contract.tag()) =>
        {
            sender.target_core_node()
        }
        _ => return None,
    };
    Some(match receiving_core_node {
        Some(core_node) if core_node == sender.bound_core_node() => CallReceiver::ThisMachine,
        _ => CallReceiver::OtherMachine,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmi::{ProducerRef, ServiceKind};

    fn sender(target: SenderTarget, producer: Option<&ProducerRef>) -> ServiceWireSender {
        ServiceWireSender::new(
            "cn-desk",
            "arm_inst",
            producer,
            target,
            "member",
            ServiceKind::Service,
        )
        .unwrap()
    }

    #[test]
    fn a_raw_call_goes_to_the_daemon_of_the_core_node_that_its_target_names() {
        let local = SenderTarget::node("cn-desk", names::CORE_NODE_TAG).unwrap();
        let remote = SenderTarget::node("cn-arm", names::CORE_NODE_TAG).unwrap();
        assert_eq!(
            daemon_receiver(&sender(local, None)),
            Some(CallReceiver::ThisMachine)
        );
        assert_eq!(
            daemon_receiver(&sender(remote, None)),
            Some(CallReceiver::OtherMachine)
        );
    }

    #[test]
    fn a_daemon_interface_call_goes_to_the_daemon_that_it_pins() {
        let fleet = || SenderTarget::contract("fleet", "v1").unwrap();
        let here = ProducerRef::new("cn-desk", "brave-otter-1234");
        let there = ProducerRef::new("cn-arm", "calm-heron-5678");
        assert_eq!(
            daemon_receiver(&sender(fleet(), Some(&here))),
            Some(CallReceiver::ThisMachine)
        );
        assert_eq!(
            daemon_receiver(&sender(fleet(), Some(&there))),
            Some(CallReceiver::OtherMachine)
        );
        assert_eq!(
            daemon_receiver(&sender(fleet(), None)),
            Some(CallReceiver::OtherMachine),
            "an unpinned call may reach another machine, so it carries the machine proof"
        );
        let copies = SenderTarget::contract("stack_copies", "v1").unwrap();
        assert!(daemon_receiver(&sender(copies, Some(&here))).is_some());
    }

    #[test]
    fn a_call_to_a_node_or_to_another_contract_does_not_go_to_a_daemon() {
        let node = SenderTarget::node("arm", "v1").unwrap();
        let contract = SenderTarget::contract("manipulator", "v1").unwrap();
        let later_fleet = SenderTarget::contract("fleet", "v2").unwrap();
        for target in [node, contract, later_fleet] {
            assert_eq!(daemon_receiver(&sender(target, None)), None);
        }
    }
}
