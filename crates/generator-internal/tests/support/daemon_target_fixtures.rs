//! The client of a daemon target that the Rust and Python generator tests
//! share: a lease holder of `robot_leases:v1`, whose consumed members take
//! their formats and failure kinds from the document compiled into peppy, as
//! node add resolves them for a `depends_on.daemon` slot.

use config::node::{ConsumedAction, ConsumedService, DaemonTargetDependency};
use daemon_config::daemon_interface::DaemonInterface;
use generator::{ConsumedActionMessage, DependencyContext, DeploymentInterface};

/// The link_id of the daemon target, and the name of its slot in the
/// generated modules.
pub const LINK_ID: &str = "fleet";

/// The services of `robot_leases:v1` that the lease holder calls.
pub const SERVICES: [&str; 2] = ["list", "take"];

/// The action of `robot_leases:v1` that the lease holder runs.
pub const ACTION: &str = "lease";

/// The manifest of a lease holder named `name`, whose `execution` block is
/// `execution`: its daemon target `fleet` of `robot_leases:v1` and the
/// members it consumes through it.
pub fn lease_holder_manifest(name: &str, execution: &str) -> String {
    format!(
        r#"{{
  peppy_schema: "node/v1",
  manifest: {{
    name: "{name}",
    tag: "v1",
    depends_on: {{ daemon: [{{ name: "robot_leases", tag: "v1", link_id: "{LINK_ID}" }}] }},
  }},
  interfaces: {{
    services: {{ consumes: [
      {{ link_id: "{LINK_ID}", name: "list" }},
      {{ link_id: "{LINK_ID}", name: "take" }},
    ] }},
    actions: {{ consumes: [{{ link_id: "{LINK_ID}", name: "{ACTION}" }}] }},
  }},
  execution: {{ {execution} }},
}}"#
    )
}

/// The consumed interfaces of the lease holder, read from the compiled-in
/// document of `robot_leases:v1`: each addresses the daemon as the provider
/// of the contract `robot_leases:v1` through the `one` slot of the daemon
/// target.
pub fn lease_holder_interfaces() -> Vec<DeploymentInterface> {
    let interface = DaemonInterface::RobotLeases;
    let document = interface.document();
    let dependency = DependencyContext::contract(
        interface.name(),
        interface.tag(),
        LINK_ID,
        DaemonTargetDependency::CARDINALITY,
        document.failure_kinds.clone(),
    );
    let services = SERVICES.into_iter().map(|name| {
        let service = document
            .interfaces
            .services
            .iter()
            .find(|service| service.name == name)
            .unwrap_or_else(|| panic!("robot_leases:v1 has no service `{name}`"));
        DeploymentInterface::consumed_service(
            ConsumedService {
                link_id: LINK_ID.to_owned(),
                name: name.to_owned(),
                refine: None,
            },
            service.request_message_format.clone().unwrap_or_default(),
            service.response_message_format.clone().unwrap_or_default(),
            dependency.clone(),
        )
    });
    let action = document
        .interfaces
        .actions
        .iter()
        .find(|action| action.name == ACTION)
        .expect("robot_leases:v1 has the action `lease`");
    let lease = DeploymentInterface::consumed_action(
        ConsumedAction {
            link_id: LINK_ID.to_owned(),
            name: ACTION.to_owned(),
            refine: None,
        },
        ConsumedActionMessage::from(action),
        dependency.clone(),
    );
    services.chain([lease]).collect()
}
