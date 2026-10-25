//! The raw calls of the daemon from the nodes of its stack.
//!
//! A node of the stack reaches the daemon through its daemon targets, which
//! its launcher scopes. So the daemon refuses each of its raw actions, and
//! each of its raw services that changes the stack, a node, a repository or
//! a launch of the federation, from an instance of the stack: an instance
//! that the stack of this daemon runs, or an instance of the launch that
//! this daemon coordinates. A node keeps the raw services that read or that
//! it needs to run: the clock, the datastore, health, info, the stack
//! listing, the node information, the documents of an instance and the
//! repository listing.

use crate::services::federation::SliceOwnership;
use crate::services::response::failure;
use config::failure_kind::PeppyFailureKind;
use config::runtime::Name;
use futures::future::{Either, Ready, ready};
use node_stack::NodeStack;
use peppylib::messaging::{Failure, ServiceAnswer, ServiceRequestContext};
use std::future::Future;
use std::sync::Arc;

/// Refuses the raw calls of the daemon from the instances of its stack.
#[derive(Clone)]
pub(crate) struct RawCallGate {
    node_stack: Arc<NodeStack>,
    ownership: Arc<SliceOwnership>,
}

impl RawCallGate {
    pub(crate) fn new(node_stack: Arc<NodeStack>, ownership: Arc<SliceOwnership>) -> Self {
        Self {
            node_stack,
            ownership,
        }
    }

    /// The refusal of the raw call `call` from the sender `instance_id`,
    /// when the sender is an instance of the stack.
    pub(crate) fn refusal(&self, call: &str, instance_id: &str) -> Option<Failure> {
        self.is_stack_instance(instance_id).then(|| {
            failure(
                PeppyFailureKind::Rejected,
                format!(
                    "the daemon refuses its raw call `{call}` from `{instance_id}`, an instance \
                     of the stack: a node calls the daemon through its daemon targets"
                ),
            )
        })
    }

    /// `handler`, the handler of the raw service `service`, behind the
    /// refusal of each request from an instance of the stack.
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
        move |request| match gate.refusal(service, request.message().instance_id()) {
            Some(refusal) => Either::Left(ready(Err(refusal))),
            None => Either::Right(handler(request)),
        }
    }

    /// Whether `instance_id` is an instance of the stack: an instance that
    /// the stack of this daemon tracks in any state, or an instance of the
    /// launch that this daemon coordinates.
    fn is_stack_instance(&self, instance_id: &str) -> bool {
        let of_the_launch = self
            .ownership
            .active
            .lock()
            .as_ref()
            .is_some_and(|active| active.has_instance(instance_id));
        of_the_launch
            || Name::new(instance_id)
                .ok()
                .is_some_and(|name| self.node_stack.find_instance_any_state(&name).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stack::fixtures::active_launch_running;
    use config::failure_kind::FailureKindName;
    use core_node_api::names::COMMAND_LINE_INSTANCE_ID;

    const ROOT_CONFIG: &str = r#"{
        peppy_schema: "node/v1",
        manifest: { name: "cn_desk", tag: "v1" },
        interfaces: {},
        execution: {
            language: "rust",
            parameters: {},
            build_cmd: ["true"],
            run_cmd: ["true"],
        },
    }"#;

    /// A gate on a stack that tracks the instance `tracked`, and the
    /// ownership whose launch the gate reads.
    fn gate_tracking(tracked: &str) -> (RawCallGate, Arc<SliceOwnership>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let root_config = config::node::NodeConfigParser::from_content(ROOT_CONFIG)
            .expect("the root config parses");
        let node_stack = Arc::new(NodeStack::new(
            root_config,
            Some(Name::new(tracked).unwrap()),
            directory.path(),
        ));
        let ownership = SliceOwnership::new("cn-desk");
        (
            RawCallGate::new(node_stack, Arc::clone(&ownership)),
            ownership,
            directory,
        )
    }

    #[test]
    fn an_instance_of_the_stack_or_of_the_launch_is_refused_and_any_other_sender_is_not() {
        let (gate, ownership, _directory) = gate_tracking("camera_inst");
        assert!(
            gate.refusal("node_run", "camera_inst").is_some(),
            "an instance that the stack tracks"
        );
        assert!(
            gate.refusal("stack_join", "arm_inst").is_none(),
            "no launch, so no instance of a launch"
        );

        *ownership.active.lock() = Some(active_launch_running(&[
            ("planner_inst", "cn-desk"),
            ("arm_inst", "cn-arm"),
        ]));
        let refusal = gate
            .refusal("stack_join", "arm_inst")
            .expect("an instance of the launch on another machine");
        assert_eq!(
            refusal.kind(),
            &FailureKindName::from(PeppyFailureKind::Rejected)
        );
        assert!(
            refusal
                .message()
                .contains("raw call `stack_join` from `arm_inst`"),
            "{}",
            refusal.message()
        );
        assert!(
            gate.refusal("stack_join", COMMAND_LINE_INSTANCE_ID)
                .is_none(),
            "the command line"
        );
        assert!(
            gate.refusal("stack_join", "brave-otter-1234").is_none(),
            "a daemon, under its per-boot instance id"
        );
    }
}
