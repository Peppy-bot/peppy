//! The daemon's authority for producer-binding delivery: when a join grows a
//! running consumer's producer set or a removal shrinks it, it pushes the slot's
//! whole set to the instance over the `binding_update` service and records the
//! set the instance holds on the node stack. A consumer's boot config carries
//! the set it starts with, so only changes are delivered.

use super::common::SlotUpdateClient;
use config::runtime::{BoundProducers, Name};
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use peppylib::encoding::binding_update::BindingUpdateRequest;
use peppylib::messaging::BINDING_UPDATE_SERVICE;
use std::sync::Arc;
use std::time::Duration;

/// How long a single `binding_update` delivery may take before it is abandoned.
pub(crate) const BINDING_UPDATE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct BindingCoordinator {
    updates: SlotUpdateClient,
}

impl BindingCoordinator {
    pub fn new(
        node_stack: Arc<NodeStack>,
        messenger: MessengerHandle,
        core_node_name: impl Into<String>,
        caller_instance_id: impl Into<String>,
    ) -> Self {
        Self {
            updates: SlotUpdateClient::new(
                node_stack,
                messenger,
                core_node_name,
                caller_instance_id,
            ),
        }
    }

    /// Delivers `producers` as the set slot `link_id` of `consumer_instance_id`
    /// holds from now on, and records it on the node stack once the instance
    /// holds it. Returns the reason when the set is not in place: this daemon
    /// does not track the instance, it refused or missed the delivery, or it
    /// stopped running as it took it.
    pub async fn replace_slot(
        &self,
        consumer_instance_id: &Name,
        link_id: &str,
        producers: BoundProducers,
    ) -> Result<(), String> {
        let payload = BindingUpdateRequest {
            link_id: link_id.to_string(),
            sequence: self.updates.next_sequence(),
            producers: producers.clone(),
        }
        .encode()
        .map_err(|error| error.to_string())?;
        self.updates
            .send(
                consumer_instance_id.as_str(),
                BINDING_UPDATE_SERVICE,
                payload,
                BINDING_UPDATE_TIMEOUT,
                "binding_update rejected",
            )
            .await?;
        if self.updates.node_stack().set_instance_slot_binding(
            consumer_instance_id,
            link_id,
            producers,
        ) {
            return Ok(());
        }
        Err("the instance stopped running as it took the set".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::runtime::ProducerRef;
    use pmi::{Messenger, MessengerAdapter, MockAdapter};

    const ROOT_CONFIG: &str = r#"{
        peppy_schema: "node/v1",
        manifest: { name: "core_a", tag: "v1" },
        interfaces: {},
        execution: {
            language: "rust",
            parameters: {},
            build_cmd: ["true"],
            run_cmd: ["true"],
        },
    }"#;

    #[tokio::test]
    async fn replacing_a_slot_of_an_instance_this_daemon_does_not_track_fails() {
        let directory = tempfile::tempdir().unwrap();
        let root_config = config::node::NodeConfigParser::from_content(ROOT_CONFIG)
            .expect("test root config parses");
        let stack = Arc::new(NodeStack::new(
            root_config,
            Some(Name::new("core_root_inst").unwrap()),
            directory.path(),
        ));
        let messenger = MessengerHandle::from_shared(Arc::new(tokio::sync::Mutex::new(
            Messenger::new(MessengerAdapter::Mock(MockAdapter::default())),
        )));
        let coordinator = BindingCoordinator::new(stack, messenger, "core_a", "core_root_inst");
        let error = coordinator
            .replace_slot(
                &Name::new("planner_inst").unwrap(),
                "robots",
                BoundProducers::from(ProducerRef::new("core_a", "alpha_arm_inst")),
            )
            .await
            .unwrap_err();
        assert!(error.contains("no longer tracked"), "{error}");
    }
}
