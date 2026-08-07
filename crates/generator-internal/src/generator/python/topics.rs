use super::PythonSchemaInfo;
use super::code_builder::{PythonCodeBuilder, emit_nested_classes};
use super::deserialization;
use super::serialization;
use super::services::sender_target_python_expr;
use super::type_mapping::{collect_fields_from_format, qos_profile_python, uses_optional};
use crate::error::Result;
use crate::generator::types::{PairTopicConsumerKind, SubscriptionTag};
use config::node::{Cardinality, ConsumedTopic, MessageFormat, NativeEmittedTopic};

pub(crate) fn capnp_loader_fn_name(schema_info: &PythonSchemaInfo) -> String {
    format!("_{}_capnp", schema_info.file_stem)
}

/// Emits the imports needed by capnp schema loaders. Call this once before
/// emitting any loaders via [`emit_capnp_loader_fn`].
///
/// Schemas are resolved via `importlib.resources.files("peppygen")` so the
/// lookup is independent of where the calling file lives in the package
/// tree: native artifacts at `peppygen/{category}/{leaf}.py` and contract-backed
/// ones nested under `peppygen/{category}/{contract}/{tag}/{leaf}.py` share
/// the same loader body.
pub(crate) fn emit_capnp_preamble(builder: &mut PythonCodeBuilder) {
    builder.add_import("import capnp");
    builder.add_import("import types");
    builder.add_import("from functools import lru_cache");
    builder.add_import("from importlib.resources import files");
}

/// Emits a single `@lru_cache` loader function for a capnp schema.
/// Requires [`emit_capnp_preamble`] to have been called first.
pub(crate) fn emit_capnp_loader_fn(
    builder: &mut PythonCodeBuilder,
    schema_info: &PythonSchemaInfo,
) {
    let loader_fn_name = capnp_loader_fn_name(schema_info);
    builder.line("@lru_cache(maxsize=1)");
    builder.line(&format!("def {loader_fn_name}() -> types.ModuleType:"));
    builder.indent();
    builder.line(&format!(
        "return capnp.load(str(files(\"peppygen\") / \"capnp\" / \"{}.capnp\"))",
        schema_info.file_stem
    ));
    builder.dedent();
    builder.blank_line();
}

/// Convenience wrapper: emits preamble + a single loader function.
/// Use this when there is only one schema to load.
pub(crate) fn emit_capnp_schema_loader(
    builder: &mut PythonCodeBuilder,
    schema_info: &PythonSchemaInfo,
) {
    emit_capnp_preamble(builder);
    emit_capnp_loader_fn(builder, schema_info);
}

/// Emits the pure `build_message(...) -> bytes` serializer shared by the
/// plain and peer publisher modules. Pure (no node_runner, no I/O), so a
/// publish loop can build the payload off the asyncio event loop thread
/// and hand only the finished bytes to the loop, keeping per-message
/// serialization off the loop's GIL time.
fn emit_build_message_fn(
    builder: &mut PythonCodeBuilder,
    fields: &[super::type_mapping::PythonField],
    schema_info: Option<&PythonSchemaInfo>,
    message_format: Option<&MessageFormat>,
) {
    let field_params: Vec<String> = fields
        .iter()
        .map(|field| format!("{}: {}", field.name, field.type_str))
        .collect();
    builder.line(&format!(
        "def build_message({}) -> bytes:",
        field_params.join(", ")
    ));
    builder.indent();
    if let (Some(info), Some(fmt)) = (schema_info, message_format) {
        let loader_fn_name = capnp_loader_fn_name(info);
        builder.line(&format!(
            "capnp_msg = {loader_fn_name}().{}.new_message()",
            info.struct_name
        ));
        let mut counter = 0u32;
        serialization::emit_capnp_assignments(builder, "capnp_msg", fmt, "", &mut counter);
        builder.line("return capnp_msg.to_bytes()");
    } else {
        builder.line("return b\"\"");
    }
    builder.dedent();
    builder.blank_line();
}

/// The tag's Python type annotation, spliced into `next()`'s signature.
fn tag_annotation(tag: SubscriptionTag) -> &'static str {
    match tag {
        SubscriptionTag::Producer => "peppylib.ProducerRef",
        SubscriptionTag::Peer => "peppylib.PeerInfo",
        SubscriptionTag::ObservedSource => "peppylib.ObservedSource",
    }
}

/// Emits the held-`Subscription` class shared by the bound-set, peer, and
/// observer consumer modules; the docstring and the identity tag differ.
/// `next()` mirrors the Rust `Subscription::next`: a `(tag, message)` tuple,
/// or `None` once the subscription has closed.
fn emit_subscription_class(builder: &mut PythonCodeBuilder, docstring: &str, tag: SubscriptionTag) {
    let annotation = tag_annotation(tag);
    let binding = tag.binding();
    builder.line("class Subscription:");
    builder.indent();
    builder.line(&format!("\"\"\"{docstring}\"\"\""));
    builder.blank_line();
    builder.line("def __init__(self, inner) -> None:");
    builder.indent();
    builder.line("self._inner = inner");
    builder.dedent();
    builder.blank_line();
    builder.line(&format!(
        "async def next(self) -> Optional[Tuple[{annotation}, Message]]:"
    ));
    builder.indent();
    builder.line("item = await self._inner.on_next_message()");
    builder.line("if item is None:");
    builder.indent();
    builder.line("return None");
    builder.dedent();
    builder.line(&format!("{binding}, raw_message = item"));
    builder.line("message = _deserialize_payload(raw_message.payload)");
    builder.line(&format!("return {binding}, message"));
    builder.dedent();
    builder.blank_line();
    builder.line("def __aiter__(self) -> \"Subscription\":");
    builder.indent();
    builder.line("return self");
    builder.dedent();
    builder.blank_line();
    builder.line(&format!(
        "async def __anext__(self) -> Tuple[{annotation}, Message]:"
    ));
    builder.indent();
    builder.line("result = await self.next()");
    builder.line("if result is None:");
    builder.indent();
    builder.line("raise StopAsyncIteration");
    builder.dedent();
    builder.line("return result");
    builder.dedent();
    builder.dedent();
}

/// Generates Python code for an emitted (publishing) topic.
pub fn build_emitted_topic(
    topic: &NativeEmittedTopic,
    schema_info: Option<&PythonSchemaInfo>,
    origin: Option<&crate::generator::types::ContractOrigin>,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();
    let mut nested_classes = Vec::new();

    // Collect fields from message format
    let fields = topic
        .message_format
        .as_ref()
        .map(|fmt| collect_fields_from_format(fmt, "Message", &mut nested_classes))
        .transpose()?
        .unwrap_or_default();

    if uses_optional(&fields, &nested_classes) {
        builder.add_import("from typing import Optional");
    }

    // Add capnp imports and a lazy, cached schema loader.
    if let Some(info) = schema_info {
        emit_capnp_schema_loader(&mut builder, info);
    }

    // Emit nested dataclasses (e.g., MessageHeader)
    emit_nested_classes(&mut builder, &nested_classes);

    builder.add_import("import peppylib");

    let qos = qos_profile_python(&topic.qos_profile);
    let target_expr =
        sender_target_python_expr(origin, "node_runner.node_name()", "node_runner.node_tag()");

    // Module-level topic constants, shared by build_message and
    // declare_publisher.
    builder.line(&format!("TOPIC_NAME = \"{}\"", topic.name));
    builder.line(&format!("QOS = {qos}"));
    builder.blank_line();

    emit_build_message_fn(
        &mut builder,
        &fields,
        schema_info,
        topic.message_format.as_ref(),
    );

    // declare_publisher: take the central messenger lock ONCE and return a
    // lock-free publisher whose publish(payload) never re-takes that lock.
    // Declare once, then publish per message (a camera streaming frames, a
    // sensor at rate), paired with build_message.
    builder.line(
        "async def declare_publisher(node_runner: peppylib.NodeRunner) -> peppylib.TopicPublisher:",
    );
    builder.indent();
    builder.line("return await peppylib.TopicMessenger.declare_publisher(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_core_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line(&format!("{target_expr},"));
    builder.line("TOPIC_NAME,");
    builder.line("QOS,");
    builder.dedent();
    builder.line(")");
    builder.dedent();

    Ok(builder.build())
}

/// Emits the module-level slot constants plus `paired()`/`wait_paired()`
/// shared by both directions of a peer topic module.
fn emit_peer_module_header(
    builder: &mut PythonCodeBuilder,
    topic_name: &str,
    qos: &str,
    peer: &crate::generator::types::PeerContext,
) {
    builder.add_import("import peppylib");
    builder.add_import("from typing import Optional");
    builder.line(&format!("TOPIC_NAME = \"{topic_name}\""));
    builder.line(&format!("LINK_ID = \"{}\"", peer.link_id));
    builder.line(&format!("PAIRING_NAME = \"{}\"", peer.pairing_name));
    builder.line(&format!("PAIRING_TAG = \"{}\"", peer.pairing_tag));
    builder.line(&format!("QOS = {qos}"));
    builder.blank_line();

    builder.line("def paired(node_runner: peppylib.NodeRunner) -> Optional[peppylib.PeerInfo]:");
    builder.indent();
    builder.line("\"\"\"The peer currently paired on this slot, or None while unpaired.\"\"\"");
    builder.line("return node_runner.peer(LINK_ID).paired()");
    builder.dedent();
    builder.blank_line();

    builder.line("async def wait_paired(node_runner: peppylib.NodeRunner) -> peppylib.PeerInfo:");
    builder.indent();
    builder.line("\"\"\"Wait until a peer is paired on this slot and return its identity.\"\"\"");
    builder.line("return await node_runner.peer(LINK_ID).wait_paired()");
    builder.dedent();
    builder.blank_line();
}

/// Generates Python code for a pairing topic this node's role emits:
/// `build_message` plus a slot-scoped `declare_publisher` (pairing wire
/// target, producer-side link_id = this node's own slot link_id).
pub fn build_peer_emitted_topic(
    topic: &NativeEmittedTopic,
    schema_info: Option<&PythonSchemaInfo>,
    peer: &crate::generator::types::PeerContext,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();
    let mut nested_classes = Vec::new();

    let fields = topic
        .message_format
        .as_ref()
        .map(|fmt| collect_fields_from_format(fmt, "Message", &mut nested_classes))
        .transpose()?
        .unwrap_or_default();

    if uses_optional(&fields, &nested_classes) {
        builder.add_import("from typing import Optional");
    }
    if let Some(info) = schema_info {
        emit_capnp_schema_loader(&mut builder, info);
    }
    emit_nested_classes(&mut builder, &nested_classes);

    let qos = qos_profile_python(&topic.qos_profile);
    emit_peer_module_header(&mut builder, &topic.name, qos, peer);

    emit_build_message_fn(
        &mut builder,
        &fields,
        schema_info,
        topic.message_format.as_ref(),
    );

    // Slot-scoped publisher: publishing while unpaired is a legal no-op (the
    // mesh drops it); the paired peer's triple-pinned subscription receives
    // every publish made while the pair is live.
    builder.line(
        "async def declare_publisher(node_runner: peppylib.NodeRunner) -> peppylib.TopicPublisher:",
    );
    builder.indent();
    builder.line("return await peppylib.TopicMessenger.declare_publisher(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_core_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line("peppylib.SenderTarget.pairing(PAIRING_NAME, PAIRING_TAG),");
    builder.line("TOPIC_NAME,");
    builder.line("QOS,");
    builder.line("link_id=LINK_ID,");
    builder.dedent();
    builder.line(")");
    builder.dedent();

    Ok(builder.build())
}

/// Emits the module-level slot constants plus the observer slot's source
/// accessor. An observer plays no role, so there is no
/// `paired()`/`wait_paired()`.
///
/// The accessor is cardinality-typed the way a producer slot's is: `one` gets
/// `source() -> peppylib.ObservedSource`, `zero_or_one` gets
/// `source() -> Optional[ObservedSource]`, and both multi cardinalities get
/// `sources() -> List[ObservedSource]` in plan order. Python has no non-empty
/// list type, so the name flip is what surfaces a change between the scalar and
/// multi halves, and `one_or_more` states its never-empty guarantee in the
/// docstring rather than the annotation, exactly as `bound_producers` does.
///
/// Like the bound-producer accessor, the emitted name and the runtime method it
/// calls are separate: the two scalar cardinalities share one emitted name over
/// two runtime methods. The docstring prose comes from `observed_sources_doc` so
/// both language generators state the same guarantees.
fn emit_observer_module_header(
    builder: &mut PythonCodeBuilder,
    topic_name: &str,
    qos: &str,
    observer: &crate::generator::types::PeerContext,
    cardinality: Cardinality,
) {
    builder.add_import("import peppylib");
    builder.line(&format!("TOPIC_NAME = \"{topic_name}\""));
    builder.line(&format!("LINK_ID = \"{}\"", observer.link_id));
    builder.line(&format!("PAIRING_NAME = \"{}\"", observer.pairing_name));
    builder.line(&format!("PAIRING_TAG = \"{}\"", observer.pairing_tag));
    builder.line(&format!("QOS = {qos}"));
    builder.blank_line();

    let (fn_name, runtime_method, return_type, typing_import) = match cardinality {
        Cardinality::One => ("source", "sole_source", "peppylib.ObservedSource", None),
        Cardinality::ZeroOrOne => (
            "source",
            "source",
            "Optional[peppylib.ObservedSource]",
            Some("from typing import Optional"),
        ),
        // One arm for both multi cardinalities, because Python cannot spell a
        // non-empty list: the signatures are identical and the never-empty
        // guarantee of `one_or_more` lives in the docstring below.
        Cardinality::OneOrMore | Cardinality::ZeroOrMore => (
            "sources",
            "sources",
            "List[peppylib.ObservedSource]",
            Some("from typing import List"),
        ),
    };
    if let Some(import) = typing_import {
        builder.add_import(import);
    }
    // The handle split IS the scalar/multi split: the two scalar cardinalities
    // read an `ObservationSlot`, the two multi ones an `ObservationSlotSet`.
    let handle = if cardinality.is_scalar() {
        "observation_slot"
    } else {
        "observation_slot_set"
    };

    let doc = crate::generator::types::observed_sources_doc(
        cardinality,
        crate::generator::types::DocLanguage::Python,
    );
    builder.line(&format!(
        "def {fn_name}(node_runner: peppylib.NodeRunner) -> {return_type}:"
    ));
    builder.indent();
    builder.line(&format!("\"\"\"{}", doc.summary));
    builder.blank_line();
    for line in doc.body_lines() {
        builder.line(line);
    }
    builder.line("\"\"\"");
    builder.line(&format!(
        "return node_runner.{handle}(LINK_ID).{runtime_method}()"
    ));
    builder.dedent();
    builder.blank_line();
}

/// Generates Python code for a consume-side pairing topic. A
/// [`PairTopicConsumerKind::Peer`] slot gets a `subscribe_peer`-backed
/// subscription that follows the slot's live pin (silent while unpaired, only
/// the paired peer while paired); a [`PairTopicConsumerKind::Observed`] slot
/// gets a `subscribe_observed`-backed one that follows each observed source
/// instance's lifecycle and yields `(source, message)`. An observer plays no
/// role, so it has no publisher.
pub fn build_pair_topic_consumer(
    topic: &NativeEmittedTopic,
    arguments: &MessageFormat,
    schema_info: &PythonSchemaInfo,
    peer: &crate::generator::types::PeerContext,
    kind: PairTopicConsumerKind,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();
    let mut nested_classes = Vec::new();

    let fields = collect_fields_from_format(arguments, "Message", &mut nested_classes)?;

    builder.add_import("from typing import Optional, Tuple");
    emit_capnp_schema_loader(&mut builder, schema_info);
    emit_nested_classes(&mut builder, &nested_classes);

    let field_refs: Vec<(&str, &str)> = fields
        .iter()
        .map(|f| (f.name.as_str(), f.type_str.as_str()))
        .collect();
    builder.dataclass("Message", &field_refs);

    let loader_fn_name = capnp_loader_fn_name(schema_info);
    deserialization::build_deserialize_fn(
        &mut builder,
        schema_info,
        arguments,
        "Message",
        &format!("{loader_fn_name}()"),
        "_deserialize_payload",
    );

    let qos = qos_profile_python(&topic.qos_profile);
    let (subscription_doc, subscription_tag, subscribe_doc, subscribe_method) = match kind {
        PairTopicConsumerKind::Peer => {
            emit_peer_module_header(&mut builder, &topic.name, qos, peer);
            (
                "A held subscription that follows the slot's live pin: silent while unpaired, only the paired peer while paired. Each message is tagged with the PeerInfo of the paired peer, the same identity paired() returns.",
                SubscriptionTag::Peer,
                "\"\"\"Subscribe to this pairing topic. Legal while unpaired: the subscription stays silent until a peer pairs.\"\"\"",
                "subscribe_peer",
            )
        }
        PairTopicConsumerKind::Observed(cardinality) => {
            emit_observer_module_header(&mut builder, &topic.name, qos, peer, cardinality);
            (
                "A held subscription fanned in across the observer slot's whole member set: silent until a member is live and emitting; a live stream, not a mailbox. Each message is tagged with the ObservedSource that published it, the same identity the slot's accessors enumerate, so members stay distinct even when they share one instance.",
                SubscriptionTag::ObservedSource,
                "\"\"\"Subscribe to this observed pairing topic. Legal before any source is resolved or live: the subscription stays silent until a member emits.\"\"\"",
                "subscribe_observed",
            )
        }
    };

    emit_subscription_class(&mut builder, subscription_doc, subscription_tag);

    builder.blank_line();
    builder.line("async def subscribe(node_runner: peppylib.NodeRunner) -> Subscription:");
    builder.indent();
    builder.line(subscribe_doc);
    builder.line(&format!("inner = await node_runner.{subscribe_method}("));
    builder.indent();
    builder.line("LINK_ID,");
    builder.line("PAIRING_NAME,");
    builder.line("PAIRING_TAG,");
    builder.line("TOPIC_NAME,");
    builder.line("QOS,");
    builder.dedent();
    builder.line(")");
    builder.line("return Subscription(inner)");
    builder.dedent();

    Ok(builder.build())
}

/// Generates Python code for a consumed (receiving) topic.
pub fn build_consumed_topic(
    topic: &ConsumedTopic,
    arguments: &MessageFormat,
    schema_info: &PythonSchemaInfo,
    dependency: &crate::generator::types::DependencyContext,
) -> Result<String> {
    let topic_name = topic.name.as_str();
    let mut builder = PythonCodeBuilder::new();
    let mut nested_classes = Vec::new();

    // Collect fields from the message format
    let fields = collect_fields_from_format(arguments, "Message", &mut nested_classes)?;

    // Optional covers any Optional fields in the dataclasses and the closed
    // sentinel of `Subscription.next`; Tuple wraps the `(producer, message)`
    // pair each message yields (the producer itself is a structured
    // `peppylib.ProducerRef`).
    builder.add_import("from typing import Optional, Tuple");

    // Add capnp imports and a lazy, cached schema loader.
    emit_capnp_schema_loader(&mut builder, schema_info);

    // Emit nested dataclasses first (dependency order)
    emit_nested_classes(&mut builder, &nested_classes);

    // Emit the main Message dataclass
    let field_refs: Vec<(&str, &str)> = fields
        .iter()
        .map(|f| (f.name.as_str(), f.type_str.as_str()))
        .collect();
    builder.dataclass("Message", &field_refs);

    // Generate deserialize_payload helper function
    let loader_fn_name = capnp_loader_fn_name(schema_info);
    deserialization::build_deserialize_fn(
        &mut builder,
        schema_info,
        arguments,
        "Message",
        &format!("{loader_fn_name}()"),
        "_deserialize_payload",
    );

    // Generate the held-subscription consumer API. `subscribe()` covers the
    // slot's complete bound producer set (one pinned wire subscription per
    // member, merged behind one object) and yields `(producer, message)`
    // pairs; the shape is identical for every cardinality, only the size of
    // the set changes.
    builder.add_import("import peppylib");

    crate::generator::python::services::emit_bound_producer_accessor_fn(&mut builder, dependency);

    builder.blank_line();
    emit_subscription_class(
        &mut builder,
        "A merged subscription covering every producer bound to this slot. \
Per-producer order is preserved (no total order across producers), ready \
producers are merged fairly, and the bound set is fixed at startup; filter \
on the yielded producer to follow a single member.",
        SubscriptionTag::Producer,
    );

    builder.blank_line();
    builder.line("async def subscribe(node_runner: peppylib.NodeRunner) -> Subscription:");
    builder.indent();
    builder.line(&format!("topic_name = \"{}\"", topic_name));
    builder.line("inner = await peppylib.TopicMessenger.subscribe_bound_set(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_core_node(),");
    builder.line("node_runner.bound_instance_id(),");
    let from_target = sender_target_python_expr(
        dependency.origin.as_ref(),
        &format!("{:?}", dependency.producer_name),
        &format!("{:?}", dependency.producer_tag),
    );
    builder.line(&format!("{from_target},"));
    builder.line("topic_name,");
    // The slot's complete bound producer set: sized per the declared
    // cardinality at launch, re-validated at node startup. It is empty on a
    // zero_or_more slot bound to nothing and on a vacant zero_or_one slot,
    // where the subscription yields nothing until shutdown.
    builder.line(&format!(
        "node_runner.bound_producers({:?}),",
        dependency.link_id
    ));
    builder.line("peppylib.QoSProfile.Standard,");
    builder.line("node_runner.cancellation_token(),");
    builder.dedent();
    builder.line(")");
    builder.line("return Subscription(inner)");
    builder.dedent();

    Ok(builder.build())
}
