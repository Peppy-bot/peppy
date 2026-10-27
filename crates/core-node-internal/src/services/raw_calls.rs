//! The raw calls of the daemon from the nodes.
//!
//! A node reaches a daemon through its daemon targets, which its launcher
//! scopes. So the daemon refuses each of its raw actions, and each of its raw
//! services that changes the stack, a node, a repository or a launch of the
//! federation, from an instance: a sender that a call proves to be an
//! instance of a node, of this machine or of another one. A node keeps the
//! raw services that read or that it needs to run: the clock, the datastore,
//! health, info, the stack listing, the node information, the documents of an
//! instance and the repository listing. The command line and the daemons call
//! every raw service. The sender is the one that the check of the session of
//! the daemon verified ([`crate::services::credentials`]).

use crate::services::response::failure;
use config::failure_kind::PeppyFailureKind;
use futures::future::{Either, Ready, ready};
use peppylib::messaging::{
    CallerKind, Failure, ServiceAnswer, ServiceRequestContext, VerifiedCaller,
};
use std::future::Future;

/// Refuses the raw calls of the daemon from the instances of the nodes.
#[derive(Clone, Default)]
pub(crate) struct RawCallGate;

impl RawCallGate {
    pub(crate) fn new() -> Self {
        Self
    }

    /// The refusal of the raw call `call` from `caller`, the caller that the
    /// check of the session verified: when it is an instance, or when no
    /// check verified it.
    pub(crate) fn refusal(&self, call: &str, caller: Option<&VerifiedCaller>) -> Option<Failure> {
        let caller = match verified(caller) {
            Ok(caller) => caller,
            Err(refusal) => return Some(refusal),
        };
        (caller.kind() == CallerKind::Instance).then(|| {
            failure(
                PeppyFailureKind::Rejected,
                format!(
                    "the daemon refuses its raw call `{call}` from {caller}, an instance of a \
                     node: a node calls the daemon through its daemon targets"
                ),
            )
        })
    }

    /// `handler`, the handler of the raw service `service`, behind the
    /// refusal of each request from an instance.
    pub(crate) fn guard<F, Fut>(
        &self,
        service: &'static str,
        mut handler: F,
    ) -> impl FnMut(ServiceRequestContext) -> Either<Ready<ServiceAnswer>, Fut>
    where
        F: FnMut(ServiceRequestContext) -> Fut,
        Fut: Future<Output = ServiceAnswer>,
    {
        let gate = self.clone();
        move |request| match gate.refusal(service, request.verified_caller()) {
            Some(refusal) => Either::Left(ready(Err(refusal))),
            None => Either::Right(handler(request)),
        }
    }
}

/// The caller that the check of the session of the daemon verified. Every
/// endpoint of the daemon has the check, so a call with no verified caller
/// is refused rather than read as its sender says.
pub(crate) fn verified(caller: Option<&VerifiedCaller>) -> Result<&VerifiedCaller, Failure> {
    caller.ok_or_else(|| {
        failure(
            PeppyFailureKind::Rejected,
            "the daemon takes no call whose sender its check did not verify",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::failure_kind::FailureKindName;
    use core_node_api::names::COMMAND_LINE_INSTANCE_ID;

    #[test]
    fn an_instance_of_any_machine_is_refused_and_the_command_line_and_the_daemons_are_not() {
        let gate = RawCallGate::new();
        for core_node in ["cn-desk", "cn-arm"] {
            let instance = VerifiedCaller::new(core_node, "arm_inst", CallerKind::Instance);
            let refusal = gate
                .refusal("stack_join", Some(&instance))
                .expect("an instance");
            assert_eq!(
                refusal.kind(),
                &FailureKindName::from(PeppyFailureKind::Rejected)
            );
            assert!(
                refusal.message().contains(&format!(
                    "raw call `stack_join` from `arm_inst` of `{core_node}`"
                )),
                "{}",
                refusal.message()
            );
        }
        let command_line =
            VerifiedCaller::new("cn-arm", COMMAND_LINE_INSTANCE_ID, CallerKind::CommandLine);
        assert!(gate.refusal("stack_join", Some(&command_line)).is_none());
        let daemon = VerifiedCaller::new("cn-arm", "brave-otter-1234", CallerKind::Daemon);
        assert!(gate.refusal("stack_join", Some(&daemon)).is_none());
    }

    #[test]
    fn a_call_that_no_check_verified_is_refused() {
        let refusal = RawCallGate::new()
            .refusal("node_run", None)
            .expect("an unverified call");
        assert!(
            refusal.message().contains("did not verify"),
            "{}",
            refusal.message()
        );
    }
}
