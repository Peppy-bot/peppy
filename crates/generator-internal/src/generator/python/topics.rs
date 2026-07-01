use super::PythonSchemaInfo;
use super::code_builder::{PythonCodeBuilder, emit_nested_classes};
use super::deserialization;
use super::serialization;
use super::services::{consumed_target_python_expr, sender_target_python_expr};
use super::type_mapping::{collect_fields_from_format, qos_profile_python, uses_optional};
use crate::error::Result;
use config::node::{ConsumedTopic, EmittedTopic, MessageFormat};

pub(crate) fn capnp_loader_fn_name(schema_info: &PythonSchemaInfo) -> String {
    format!("_{}_capnp", schema_info.file_stem)
}

/// Emits the imports needed by capnp schema loaders. Call this once before
/// emitting any loaders via [`emit_capnp_loader_fn`].
///
/// Schemas are resolved via `importlib.resources.files("peppygen")` so the
/// lookup is independent of where the calling file lives in the package
/// tree — native artifacts at `peppygen/{category}/{leaf}.py` and conformed
/// ones nested under `peppygen/{category}/{iface}/{tag}/{leaf}.py` share
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

/// Generates Python code for an emitted (publishing) topic.
pub fn build_emitted_topic(
    topic: &EmittedTopic,
    schema_info: Option<&PythonSchemaInfo>,
    origin: Option<&crate::generator::types::InterfaceOrigin>,
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

    // Field params for build_message.
    let field_params: Vec<String> = fields
        .iter()
        .map(|field| format!("{}: {}", field.name, field.type_str))
        .collect();
    let field_params_str = field_params.join(", ");

    // Module-level topic constants, shared by build_message and
    // declare_publisher.
    builder.line(&format!("TOPIC_NAME = \"{}\"", topic.name));
    builder.line(&format!("QOS = {qos}"));
    builder.blank_line();

    // build_message: serialize the message to wire bytes. Pure (no node_runner,
    // no I/O), so a publish loop can build the payload off the asyncio event
    // loop thread (for example on a decoder worker) and hand only the finished
    // bytes to the loop, keeping per-message serialization off the loop's GIL
    // time.
    builder.line(&format!("def build_message({field_params_str}) -> bytes:"));
    builder.indent();
    if let (Some(info), Some(fmt)) = (schema_info, topic.message_format.as_ref()) {
        let loader_fn_name = capnp_loader_fn_name(info);
        builder.line(&format!(
            "capnp_msg = {loader_fn_name}().{}.new_message()",
            info.struct_name
        ));
        let mut counter = 0u32;
        serialization::emit_capnp_assignments(&mut builder, "capnp_msg", fmt, "", &mut counter);
        builder.line("return capnp_msg.to_bytes()");
    } else {
        builder.line("return b\"\"");
    }
    builder.dedent();
    builder.blank_line();

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

    // Generate the held-subscription consumer API. `subscribe()` returns a
    // `Subscription` whose buffer keeps every message in arrival order, so
    // looping on `next()` (or `async for`) never drops a message published
    // between calls.
    builder.add_import("import peppylib");

    builder.blank_line();
    builder.line("class Subscription:");
    builder.indent();
    builder
        .line("\"\"\"A held subscription whose buffer keeps every message in arrival order.\"\"\"");
    builder.blank_line();
    builder.line("def __init__(self, inner) -> None:");
    builder.indent();
    builder.line("self._inner = inner");
    builder.dedent();
    builder.blank_line();
    // `next()` mirrors the Rust `Subscription::next`: a message tuple, or
    // `None` once the subscription has closed.
    builder.line("async def next(self) -> Optional[Tuple[peppylib.ProducerRef, Message]]:");
    builder.indent();
    builder.line("raw_message = await self._inner.on_next_message()");
    builder.line("if raw_message is None:");
    builder.indent();
    builder.line("return None");
    builder.dedent();
    builder.line("producer = raw_message.producer");
    builder.line("message = _deserialize_payload(raw_message.payload)");
    builder.line("return producer, message");
    builder.dedent();
    builder.blank_line();
    builder.line("def __aiter__(self) -> \"Subscription\":");
    builder.indent();
    builder.line("return self");
    builder.dedent();
    builder.blank_line();
    builder.line("async def __anext__(self) -> Tuple[peppylib.ProducerRef, Message]:");
    builder.indent();
    builder.line("result = await self.next()");
    builder.line("if result is None:");
    builder.indent();
    builder.line("raise StopAsyncIteration");
    builder.dedent();
    builder.line("return result");
    builder.dedent();
    builder.dedent();

    builder.blank_line();
    builder.line("async def subscribe(node_runner: peppylib.NodeRunner) -> Subscription:");
    builder.indent();
    builder.line(&format!("topic_name = \"{}\"", topic_name));
    builder.line("inner = await peppylib.TopicMessenger.subscribe(");
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
    let from_producer = consumed_target_python_expr(dependency);
    builder.line(&format!("{from_producer},"));
    builder.line("peppylib.QoSProfile.Standard,");
    // `is_from_any: true` for `from_any: true` slots gates the
    // messenger's per-`(name, tag)` reservation. Pinned slots
    // (and the test-fixture wildcard with no manifest dep) pass
    // `false`.
    let is_from_any = matches!(
        dependency.link_id,
        crate::generator::types::WireLinkId::Wildcard { link_id: Some(_) }
    );
    builder.line(&format!(
        "is_from_any={},",
        if is_from_any { "True" } else { "False" }
    ));
    builder.dedent();
    builder.line(")");
    builder.line("return Subscription(inner)");
    builder.dedent();

    Ok(builder.build())
}
