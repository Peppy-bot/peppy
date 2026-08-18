//! Shared derivation for the generated test surfaces (`mock` + `fixtures`):
//! link grouping, mock-kind selection, identity/target derivation, and
//! seeding-call planning live here ONCE; the Rust and Python backends only
//! render these specs (one derivation, two renderers).
//!
//! Population is a one-line `record_*` call at the end of each backend's
//! existing `add_*` method, cloning the already-validated inputs. Renderers
//! re-derive schema keys with the same `naming` functions production used,
//! so codecs land on the identical capnp files (schema registries dedupe by
//! file stem — zero duplicated schemas).

use config::node::{
    Cardinality, ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat,
    NativeEmittedTopic, NativeExposedAction, NativeExposedService, QoSProfile,
};
use indexmap::IndexMap;

use super::types::{ConsumedActionMessage, ContractOrigin, DependencyContext, PeerContext};

/// Core-node segment every mock publishes/serves under.
pub(crate) const MOCK_CORE_NODE: &str = "mock-core";
/// The mock peer's own slot id: what the node's `PeerInfo.peer_link_id`
/// reports under the harness.
pub(crate) const MOCK_PEER_LINK_ID: &str = "mock_peer";
/// The producer-side link_id mock observed sources publish under.
pub(crate) const MOCK_SOURCE_LINK_ID: &str = "mock_source";
/// Wire identity of the fixtures' caller/observer session.
pub(crate) const FIXTURE_CORE_NODE: &str = "fixture-core";
pub(crate) const FIXTURE_INSTANCE_ID: &str = "fixture-observer";

/// Default instance id for the mock at `link_id` (single-instance slots).
pub(crate) fn mock_instance_id(link_id: &str) -> String {
    format!("mock-{link_id}")
}

/// The identity a mock's calls impersonate: the dependency's node or, for
/// contract-routed links, the contract itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetSpec {
    Node { name: String, tag: String },
    Contract { name: String, tag: String },
    Pairing { name: String, tag: String },
}

impl TargetSpec {
    pub fn from_dependency(dependency: &DependencyContext) -> Self {
        match &dependency.origin {
            Some(origin) => Self::Contract {
                name: origin.contract_name.clone(),
                tag: origin.contract_tag.clone(),
            },
            None => Self::Node {
                name: dependency.producer_name.clone(),
                tag: dependency.producer_tag.clone(),
            },
        }
    }
}

/// A consumed dep topic the mock publishes (typed over the production
/// `consumed_topics` `Message` struct; the serializer is the missing
/// direction, on the production schema key).
#[derive(Debug, Clone)]
pub(crate) struct DepTopicSpec {
    pub name: String,
    /// The topic's own consumer-side link_id: the production module nests
    /// under it (and the topic schema key derives from it). Usually equal to
    /// the dependency slot's link_id, but the manifest can bind several
    /// same-producer topics through distinct entries on one slot.
    pub module_link: String,
    pub format: MessageFormat,
}

/// A consumed dep service the mock serves. `request` is `None` for
/// request-less services (empty message format).
#[derive(Debug, Clone)]
pub(crate) struct DepServiceSpec {
    pub name: String,
    /// The service's own consumer-side link_id (module nesting).
    pub module_link: String,
    pub request: Option<MessageFormat>,
    pub response: Option<MessageFormat>,
}

/// A consumed dep action the mock serves on the real engine.
#[derive(Debug, Clone)]
pub(crate) struct DepActionSpec {
    pub name: String,
    /// The action's own consumer-side link_id (module nesting).
    pub module_link: String,
    pub messages: ConsumedActionMessage,
}

/// Everything mocked for one dependency link: one `Mock` per link, holding
/// one typed sub-surface per interface, all under one messaging session so
/// `stop()` is the realistic whole-producer loss.
#[derive(Debug, Clone)]
pub(crate) struct DepLinkSpec {
    /// The dependency's producer node name — feeds the schema-key naming
    /// functions even for contract-routed links (whose wire target is the
    /// contract).
    pub producer_name: String,
    pub target: TargetSpec,
    pub cardinality: Cardinality,
    pub topics: Vec<DepTopicSpec>,
    pub services: Vec<DepServiceSpec>,
    pub actions: Vec<DepActionSpec>,
}

/// One pairing-slot topic (either direction).
#[derive(Debug, Clone)]
pub(crate) struct PairTopicSpec {
    pub name: String,
    pub qos: QoSProfile,
    pub format: MessageFormat,
}

/// The mock peer for one pairing slot: publishes what the node consumes,
/// subscribes (triple-pinned) to what the node emits.
#[derive(Debug, Clone)]
pub(crate) struct PairingLinkSpec {
    pub pairing_name: String,
    pub pairing_tag: String,
    /// Topics the NODE emits on this slot (the mock subscribes).
    pub node_emits: Vec<PairTopicSpec>,
    /// Topics the NODE consumes on this slot (the mock publishes).
    pub node_consumes: Vec<PairTopicSpec>,
}

/// Mock sources for one observer slot: publish-only peers keyed by
/// `MOCK_SOURCE_LINK_ID`, 0..N instances per the slot's cardinality.
#[derive(Debug, Clone)]
pub(crate) struct ObservedLinkSpec {
    pub pairing_name: String,
    pub pairing_tag: String,
    pub cardinality: Cardinality,
    pub topics: Vec<PairTopicSpec>,
}

/// An emitted topic of the node itself: fixtures pre-subscribe (typed
/// deserializer is the missing direction) and the harness barriers on it.
#[derive(Debug, Clone)]
pub(crate) struct EmittedSpec {
    pub name: String,
    pub qos: QoSProfile,
    pub format: MessageFormat,
    pub origin: Option<ContractOrigin>,
}

/// An exposed service of the node: fixtures poll it as an identity-explicit
/// caller (request serializer + response deserializer are the missing
/// directions).
#[derive(Debug, Clone)]
pub(crate) struct ExposedServiceSpec {
    pub name: String,
    pub request: Option<MessageFormat>,
    pub response: Option<MessageFormat>,
    pub origin: Option<ContractOrigin>,
}

/// An exposed action of the node: fixtures drive the full goal lifecycle as
/// a caller.
#[derive(Debug, Clone)]
pub(crate) struct ExposedActionSpec {
    pub name: String,
    pub goal_request: Option<MessageFormat>,
    pub goal_response: Option<MessageFormat>,
    pub feedback: Option<MessageFormat>,
    pub result: Option<MessageFormat>,
    pub origin: Option<ContractOrigin>,
}

/// The node's own surface, observed by `fixtures`.
#[derive(Debug, Clone, Default)]
pub(crate) struct OwnSurfaceSpec {
    pub emitted: Vec<EmittedSpec>,
    pub services: Vec<ExposedServiceSpec>,
    pub actions: Vec<ExposedActionSpec>,
}

/// Accumulates test-surface specs while the backend's `add_*` methods run;
/// drained at the start of `build` by the mock/fixtures renderers.
#[derive(Debug, Clone, Default)]
pub(crate) struct TestGenRegistry {
    /// The node's manifest identity, set by `generate_peppygen_lib` from the
    /// parsed `peppy.json5`. The `fixtures` renderer needs it (the harness
    /// pins the node's own targets); when absent — a backend driven directly
    /// by `add_*` calls without it — `fixtures` is skipped and only `mock`
    /// renders.
    pub node_identity: Option<(String, String)>,
    pub deps: IndexMap<String, DepLinkSpec>,
    pub pairings: IndexMap<String, PairingLinkSpec>,
    pub observed: IndexMap<String, ObservedLinkSpec>,
    pub own: OwnSurfaceSpec,
}

/// `None` for an absent or empty message format: an empty struct has no
/// codec in production, and the veneer mirrors that (no typed argument).
fn non_empty(format: Option<&MessageFormat>) -> Option<MessageFormat> {
    format.filter(|format| !format.0.is_empty()).cloned()
}

impl TestGenRegistry {
    pub fn record_node_identity(&mut self, name: impl Into<String>, tag: impl Into<String>) {
        self.node_identity = Some((name.into(), tag.into()));
    }

    fn dep_entry(&mut self, dependency: &DependencyContext) -> &mut DepLinkSpec {
        self.deps
            .entry(dependency.link_id.clone())
            .or_insert_with(|| DepLinkSpec {
                producer_name: dependency.producer_name.clone(),
                target: TargetSpec::from_dependency(dependency),
                cardinality: dependency.cardinality,
                topics: Vec::new(),
                services: Vec::new(),
                actions: Vec::new(),
            })
    }

    fn pairing_entry(&mut self, peer: &PeerContext) -> &mut PairingLinkSpec {
        self.pairings
            .entry(peer.link_id.clone())
            .or_insert_with(|| PairingLinkSpec {
                pairing_name: peer.pairing_name.clone(),
                pairing_tag: peer.pairing_tag.clone(),
                node_emits: Vec::new(),
                node_consumes: Vec::new(),
            })
    }

    pub fn record_consumed_topic(
        &mut self,
        topic: &ConsumedTopic,
        format: &MessageFormat,
        dependency: &DependencyContext,
    ) {
        self.dep_entry(dependency).topics.push(DepTopicSpec {
            name: topic.name.clone(),
            module_link: topic.link_id.clone(),
            format: format.clone(),
        });
    }

    pub fn record_consumed_service(
        &mut self,
        service: &ConsumedService,
        request: &MessageFormat,
        response: &MessageFormat,
        dependency: &DependencyContext,
    ) {
        self.dep_entry(dependency).services.push(DepServiceSpec {
            name: service.name.clone(),
            module_link: service.link_id.clone(),
            request: non_empty(Some(request)),
            response: non_empty(Some(response)),
        });
    }

    pub fn record_consumed_action(
        &mut self,
        action: &ConsumedAction,
        messages: &ConsumedActionMessage,
        dependency: &DependencyContext,
    ) {
        self.dep_entry(dependency).actions.push(DepActionSpec {
            name: action.name.clone(),
            module_link: action.link_id.clone(),
            messages: ConsumedActionMessage {
                goal_request: non_empty(messages.goal_request.as_ref()),
                goal_response: non_empty(messages.goal_response.as_ref()),
                feedback: non_empty(messages.feedback.as_ref()),
                result_response: non_empty(messages.result_response.as_ref()),
            },
        });
    }

    pub fn record_peer_emitted_topic(&mut self, topic: &NativeEmittedTopic, peer: &PeerContext) {
        let spec = PairTopicSpec {
            name: topic.name.clone(),
            qos: topic.qos_profile.clone(),
            format: topic.message_format.clone().unwrap_or_default(),
        };
        self.pairing_entry(peer).node_emits.push(spec);
    }

    pub fn record_peer_consumed_topic(&mut self, topic: &NativeEmittedTopic, peer: &PeerContext) {
        let spec = PairTopicSpec {
            name: topic.name.clone(),
            qos: topic.qos_profile.clone(),
            format: topic.message_format.clone().unwrap_or_default(),
        };
        self.pairing_entry(peer).node_consumes.push(spec);
    }

    pub fn record_observed_topic(
        &mut self,
        topic: &NativeEmittedTopic,
        observer: &PeerContext,
        cardinality: Cardinality,
    ) {
        self.observed
            .entry(observer.link_id.clone())
            .or_insert_with(|| ObservedLinkSpec {
                pairing_name: observer.pairing_name.clone(),
                pairing_tag: observer.pairing_tag.clone(),
                cardinality,
                topics: Vec::new(),
            })
            .topics
            .push(PairTopicSpec {
                name: topic.name.clone(),
                qos: topic.qos_profile.clone(),
                format: topic.message_format.clone().unwrap_or_default(),
            });
    }

    pub fn record_emitted_topic(
        &mut self,
        topic: &NativeEmittedTopic,
        origin: Option<&ContractOrigin>,
    ) {
        self.own.emitted.push(EmittedSpec {
            name: topic.name.clone(),
            qos: topic.qos_profile.clone(),
            format: topic.message_format.clone().unwrap_or_default(),
            origin: origin.cloned(),
        });
    }

    pub fn record_exposed_service(
        &mut self,
        service: &NativeExposedService,
        origin: Option<&ContractOrigin>,
    ) {
        self.own.services.push(ExposedServiceSpec {
            name: service.name.clone(),
            request: non_empty(service.request_message_format.as_ref()),
            response: non_empty(service.response_message_format.as_ref()),
            origin: origin.cloned(),
        });
    }

    pub fn record_exposed_action(
        &mut self,
        action: &NativeExposedAction,
        origin: Option<&ContractOrigin>,
    ) {
        let messages = ConsumedActionMessage::from(action);
        self.own.actions.push(ExposedActionSpec {
            name: action.name.clone(),
            goal_request: non_empty(messages.goal_request.as_ref()),
            goal_response: non_empty(messages.goal_response.as_ref()),
            feedback: non_empty(messages.feedback.as_ref()),
            result: non_empty(messages.result_response.as_ref()),
            origin: origin.cloned(),
        });
    }
}
