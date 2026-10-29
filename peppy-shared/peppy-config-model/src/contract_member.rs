//! The members of a contract document: each topic, service and action with
//! the shape of a native member, and what the document says of it beside
//! its shape: the failure kinds of the contract that a service or an action
//! gives ([`ContractKinds`]), and the marks of each member
//! ([`ContractMarks`]).

use crate::internal::description::Description;
use crate::internal::failure_kind::{
    ContractFailureKinds, ContractKinds, FailureKindName, InvalidMemberKinds, MemberSection,
};
use crate::internal::mark::Marks;
use crate::internal::node::{
    ActionTopicEndpoint, GoalServiceEndpoint, InterfaceKind, MessageFormat, NativeEmittedTopic,
    NativeExposedAction, NativeExposedService, QoSProfile, ResultServiceEndpoint, TopicRetention,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A topic of a contract document: the shape of a native topic, and its
/// marks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContractTopic {
    pub name: String,
    pub description: Description,
    #[serde(default)]
    pub qos_profile: QoSProfile,
    #[serde(default, skip_serializing_if = "TopicRetention::is_live_only")]
    pub retention: TopicRetention,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
    #[serde(default, skip_serializing_if = "Marks::is_empty")]
    pub marks: Marks,
}

/// A service of a contract document: the shape of a native service, the
/// kinds of the contract that it gives, and its marks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContractService {
    pub name: String,
    pub description: Description,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_message_format: Option<MessageFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_message_format: Option<MessageFormat>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_kinds: Vec<FailureKindName>,
    #[serde(default, skip_serializing_if = "Marks::is_empty")]
    pub marks: Marks,
}

/// An action of a contract document: the shape of a native action, the
/// kinds of the contract that it gives, and its marks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContractAction {
    pub name: String,
    pub description: Description,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_service: Option<GoalServiceEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_topic: Option<ActionTopicEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_service: Option<ResultServiceEndpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_kinds: Vec<FailureKindName>,
    #[serde(default, skip_serializing_if = "Marks::is_empty")]
    pub marks: Marks,
}

/// The marks of the members of a contract, by the kind and the name of each
/// member. A topic, a service and an action of one contract can share a
/// name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContractMarks(BTreeMap<(InterfaceKind, String), Marks>);

impl ContractMarks {
    /// The marks of the member `name` of `kind`: empty for a member with no
    /// mark.
    pub fn of(&self, kind: InterfaceKind, name: &str) -> Marks {
        self.0
            .get(&(kind, name.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    /// Whether the member `name` of `kind` has the mark `moves_robot`.
    pub fn moves_robot(&self, kind: InterfaceKind, name: &str) -> bool {
        self.0
            .get(&(kind, name.to_owned()))
            .is_some_and(Marks::moves_robot)
    }

    /// Each member with a mark, with its marks, in kind and name order.
    pub fn iter(&self) -> impl Iterator<Item = (InterfaceKind, &str, &Marks)> {
        self.0
            .iter()
            .map(|((kind, name), marks)| (*kind, name.as_str(), marks))
    }

    fn insert(&mut self, kind: InterfaceKind, name: &str, marks: Marks) {
        if !marks.is_empty() {
            self.0.insert((kind, name.to_owned()), marks);
        }
    }
}

/// The topics, services and actions of a contract, each with the shape of a
/// native member, the kinds of the contract that each service and action
/// gives, and the marks of each member.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContractMembers {
    pub topics: Vec<NativeEmittedTopic>,
    pub services: Vec<NativeExposedService>,
    pub actions: Vec<NativeExposedAction>,
    pub failure_kinds: ContractKinds,
    pub marks: ContractMarks,
}

impl ContractMembers {
    /// The members of a contract document that declares `declared` in its
    /// `failure_kinds` block and lists `topics`, `services` and `actions`.
    pub fn new(
        declared: ContractFailureKinds,
        topics: Vec<ContractTopic>,
        services: Vec<ContractService>,
        actions: Vec<ContractAction>,
    ) -> Result<Self, InvalidMemberKinds> {
        let mut marks = ContractMarks::default();
        let mut listed = Vec::with_capacity(services.len() + actions.len());
        let topics = topics
            .into_iter()
            .map(|topic| {
                let ContractTopic {
                    name,
                    description,
                    qos_profile,
                    retention,
                    message_format,
                    marks: topic_marks,
                } = topic;
                marks.insert(InterfaceKind::Topic, &name, topic_marks);
                NativeEmittedTopic {
                    name,
                    description,
                    qos_profile,
                    retention,
                    message_format,
                }
            })
            .collect();
        let services = services
            .into_iter()
            .map(|service| {
                let ContractService {
                    name,
                    description,
                    request_message_format,
                    response_message_format,
                    failure_kinds,
                    marks: service_marks,
                } = service;
                marks.insert(InterfaceKind::Service, &name, service_marks);
                listed.push((MemberSection::Service, name.clone(), failure_kinds));
                NativeExposedService {
                    name,
                    description,
                    request_message_format,
                    response_message_format,
                }
            })
            .collect();
        let actions = actions
            .into_iter()
            .map(|action| {
                let ContractAction {
                    name,
                    description,
                    goal_service,
                    feedback_topic,
                    result_service,
                    failure_kinds,
                    marks: action_marks,
                } = action;
                marks.insert(InterfaceKind::Action, &name, action_marks);
                listed.push((MemberSection::Action, name.clone(), failure_kinds));
                NativeExposedAction {
                    name,
                    description,
                    goal_service,
                    feedback_topic,
                    result_service,
                }
            })
            .collect();
        Ok(Self {
            topics,
            services,
            actions,
            failure_kinds: ContractKinds::new(declared, listed)?,
            marks,
        })
    }

    /// The topics of the document, each with its marks.
    pub fn documented_topics(&self) -> Vec<ContractTopic> {
        self.topics
            .iter()
            .map(|topic| ContractTopic {
                name: topic.name.clone(),
                description: topic.description.clone(),
                qos_profile: topic.qos_profile.clone(),
                retention: topic.retention,
                message_format: topic.message_format.clone(),
                marks: self.marks.of(InterfaceKind::Topic, &topic.name),
            })
            .collect()
    }

    /// The services of the document, each with the kinds it lists and its
    /// marks.
    pub fn documented_services(&self) -> Vec<ContractService> {
        self.services
            .iter()
            .map(|service| ContractService {
                name: service.name.clone(),
                description: service.description.clone(),
                request_message_format: service.request_message_format.clone(),
                response_message_format: service.response_message_format.clone(),
                failure_kinds: self
                    .failure_kinds
                    .listed(MemberSection::Service, &service.name),
                marks: self.marks.of(InterfaceKind::Service, &service.name),
            })
            .collect()
    }

    /// The actions of the document, each with the kinds it lists and its
    /// marks.
    pub fn documented_actions(&self) -> Vec<ContractAction> {
        self.actions
            .iter()
            .map(|action| ContractAction {
                name: action.name.clone(),
                description: action.description.clone(),
                goal_service: action.goal_service.clone(),
                feedback_topic: action.feedback_topic.clone(),
                result_service: action.result_service.clone(),
                failure_kinds: self
                    .failure_kinds
                    .listed(MemberSection::Action, &action.name),
                marks: self.marks.of(InterfaceKind::Action, &action.name),
            })
            .collect()
    }
}
