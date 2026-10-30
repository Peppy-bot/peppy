//! Interfaces of the implements scenarios that the Rust and Python generator
//! tests share: a node that offers one interface natively and again through
//! contract-backed slots, each copy carrying its own marker field so the
//! tests can tell the generated files apart.

use config::node::{
    GoalServiceEndpoint, MessageFormat, NativeEmittedTopic, NativeExposedAction,
    NativeExposedService, QoSProfile, SchemaType, TopicRetention, TypeToken,
};
use generator::{ContractOrigin, DeploymentInterface, InterfaceVariant};
use indexmap::IndexMap;

/// Minimal `video_stream`-shaped topic with one distinguishing field per
/// interface tag so we can tell the generated files apart at a glance.
pub fn make_video_stream_topic(distinguishing_field: &str) -> NativeEmittedTopic {
    let mut fields: IndexMap<String, SchemaType> = IndexMap::new();
    fields.insert(
        distinguishing_field.to_string(),
        SchemaType::Type(TypeToken::U32),
    );
    NativeEmittedTopic {
        name: "video_stream".to_string(),
        description: config::description::Description::new("A test topic.").unwrap(),
        qos_profile: QoSProfile::SensorData,
        retention: TopicRetention::LiveOnly,
        message_format: Some(MessageFormat(fields)),
    }
}

/// A `control` service whose request carries `marker` and whose response
/// carries `{marker}_ack`.
pub fn make_control_service(marker: &str) -> NativeExposedService {
    let mut request_fields: IndexMap<String, SchemaType> = IndexMap::new();
    request_fields.insert(marker.to_string(), SchemaType::Type(TypeToken::Bool));
    let mut response_fields: IndexMap<String, SchemaType> = IndexMap::new();
    response_fields.insert(format!("{marker}_ack"), SchemaType::Type(TypeToken::Bool));
    NativeExposedService {
        name: "control".to_string(),
        description: config::description::Description::new("A test service.").unwrap(),
        request_message_format: Some(MessageFormat(request_fields)),
        response_message_format: Some(MessageFormat(response_fields)),
    }
}

/// A `move_arm` action whose goal request carries `marker` and whose goal
/// response carries `{marker}_ack`.
pub fn make_move_arm_action(marker: &str) -> NativeExposedAction {
    let mut request_fields: IndexMap<String, SchemaType> = IndexMap::new();
    request_fields.insert(marker.to_string(), SchemaType::Type(TypeToken::I32));
    let mut response_fields: IndexMap<String, SchemaType> = IndexMap::new();
    response_fields.insert(format!("{marker}_ack"), SchemaType::Type(TypeToken::Bool));
    NativeExposedAction {
        name: "move_arm".to_string(),
        description: config::description::Description::new("A test action.").unwrap(),
        goal_service: Some(GoalServiceEndpoint {
            request_message_format: Some(MessageFormat(request_fields)),
            response_message_format: Some(MessageFormat(response_fields)),
        }),
        feedback_topic: None,
        result_service: None,
    }
}

fn contract_origin(link_id: &str, name: &str, tag: &str) -> Option<ContractOrigin> {
    Some(ContractOrigin {
        link_id: link_id.to_string(),
        contract_name: name.to_string(),
        contract_tag: tag.to_string(),
        failure_kinds: Default::default(),
    })
}

/// `topic` emitted through the `link_id` slot of contract `name:tag`.
pub fn contract_backed_topic(
    link_id: &str,
    name: &str,
    tag: &str,
    topic: NativeEmittedTopic,
) -> DeploymentInterface {
    DeploymentInterface::new(InterfaceVariant::EmittedTopic {
        topic,
        origin: contract_origin(link_id, name, tag),
    })
}

/// `service` exposed through the `link_id` slot of contract `name:tag`.
pub fn contract_backed_service(
    link_id: &str,
    name: &str,
    tag: &str,
    service: NativeExposedService,
) -> DeploymentInterface {
    DeploymentInterface::new(InterfaceVariant::ExposedService {
        service,
        origin: contract_origin(link_id, name, tag),
    })
}

/// `action` exposed through the `link_id` slot of contract `name:tag`.
pub fn contract_backed_action(
    link_id: &str,
    name: &str,
    tag: &str,
    action: NativeExposedAction,
) -> DeploymentInterface {
    DeploymentInterface::new(InterfaceVariant::ExposedAction {
        action,
        origin: contract_origin(link_id, name, tag),
    })
}
