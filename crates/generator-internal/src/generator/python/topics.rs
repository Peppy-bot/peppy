use super::PythonSchemaInfo;
use super::code_builder::{PythonCodeBuilder, emit_nested_classes};
use super::deserialization;
use super::serialization;
use super::services::{consumed_from_instance_id_python_expr, sender_target_python_expr};
use super::type_mapping::{collect_fields_from_format, qos_profile_python, uses_optional};
use crate::error::Result;
use config::node::{ConsumedTopic, EmittedTopic, MessageFormat};

pub(crate) fn capnp_loader_fn_name(schema_info: &PythonSchemaInfo) -> String {
    format!("_{}_capnp", schema_info.file_stem)
}

/// Emits the shared `_PKG_DIR` constant and related imports needed by capnp schema loaders.
/// Call this once before emitting any loaders via [`emit_capnp_loader_fn`].
pub(crate) fn emit_capnp_preamble(builder: &mut PythonCodeBuilder) {
    builder.add_import("import capnp");
    builder.add_import("import types");
    builder.add_import("from functools import lru_cache");
    builder.add_import("from pathlib import Path");
    builder.blank_line();
    builder.line("_PKG_DIR = Path(__file__).resolve().parent.parent");
    builder.blank_line();
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
        "return capnp.load(str(_PKG_DIR / \"capnp/{}.capnp\"))",
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

    // Build parameter list for emit function
    builder.add_import("import peppylib");
    let mut param_parts = vec![String::from("node_runner: peppylib.NodeRunner")];
    for field in &fields {
        param_parts.push(format!("{}: {}", field.name, field.type_str));
    }
    let params_str = param_parts.join(", ");

    let qos = qos_profile_python(&topic.qos_profile);

    // Generate the emit function
    builder.line(&format!("async def emit({params_str}):"));
    builder.indent();
    builder.line(&format!("TOPIC_NAME = \"{}\"", topic.name));
    builder.line(&format!("qos = {qos}"));

    // Generate payload serialization
    if let (Some(info), Some(fmt)) = (schema_info, topic.message_format.as_ref()) {
        let loader_fn_name = capnp_loader_fn_name(info);
        builder.line(&format!(
            "capnp_msg = {loader_fn_name}().{}.new_message()",
            info.struct_name
        ));
        let mut counter = 0u32;
        serialization::emit_capnp_assignments(&mut builder, "capnp_msg", fmt, "", &mut counter);
        builder.line("payload = capnp_msg.to_bytes()");
    } else {
        builder.line("payload = b\"\"");
    }

    let target_expr =
        sender_target_python_expr(origin, "node_runner.node_name()", "node_runner.node_tag()");
    builder.line("await peppylib.TopicMessenger.emit(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_core_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line(&format!("{target_expr},"));
    builder.line("TOPIC_NAME,");
    builder.line("qos,");
    builder.line("payload,");
    builder.dedent();
    builder.line(")");
    builder.dedent();

    Ok(builder.build())
}

/// Generates Python code for an expected (receiving) topic.
pub fn build_consumed_topic(
    topic: &ConsumedTopic,
    arguments: &MessageFormat,
    schema_info: &PythonSchemaInfo,
    dependency: &crate::generator::types::DependencyContext,
) -> Result<String> {
    build_consumed_topic_inner(topic.name(), arguments, schema_info, Some(dependency))
}

pub fn build_external_consumed_topic(
    topic_name: &str,
    arguments: &MessageFormat,
    schema_info: &PythonSchemaInfo,
) -> Result<String> {
    build_consumed_topic_inner(topic_name, arguments, schema_info, None)
}

fn build_consumed_topic_inner(
    topic_name: &str,
    arguments: &MessageFormat,
    schema_info: &PythonSchemaInfo,
    dependency: Option<&crate::generator::types::DependencyContext>,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();
    let mut nested_classes = Vec::new();

    // Collect fields from the message format
    let fields = collect_fields_from_format(arguments, "Message", &mut nested_classes)?;

    // Always need Optional for the function parameters (from_core_node, from_instance_id),
    // plus any Optional fields in the dataclasses.
    // Tuple is used for the return type of on_next_message_received.
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

    // Generate on_next_message_received function
    builder.add_import("import peppylib");
    builder.blank_line();
    if dependency.is_some() {
        builder.line("async def on_next_message_received(node_runner: peppylib.NodeRunner, from_core_node: Optional[str] = None) -> Tuple[str, Message]:");
    } else {
        builder.line("async def on_next_message_received(node_runner: peppylib.NodeRunner, from_core_node: Optional[str] = None, from_instance_id: Optional[str] = None) -> Tuple[str, Message]:");
    }
    builder.indent();
    builder.line(&format!("topic_name = \"{}\"", topic_name));
    if let Some(dep) = dependency {
        builder.line("subscription = await peppylib.TopicMessenger.subscribe(");
        builder.indent();
        builder.line("node_runner.messenger(),");
        builder.line("node_runner.bound_core_node(),");
        builder.line("node_runner.bound_instance_id(),");
        let from_target = sender_target_python_expr(
            dep.origin.as_ref(),
            &format!("{:?}", dep.producer_name),
            &format!("{:?}", dep.producer_tag),
        );
        builder.line(&format!("{from_target},"));
        builder.line("topic_name,");
        builder.line("from_core_node,");
        let from_instance_id = consumed_from_instance_id_python_expr(dep);
        builder.line(&format!("{from_instance_id},"));
        builder.line("peppylib.QoSProfile.Standard,");
        builder.line("from_link_id=None,");
        builder.dedent();
        builder.line(")");
    } else {
        builder.line("subscription = await peppylib.TopicMessenger.consume_external(");
        builder.indent();
        builder.line("node_runner.messenger(),");
        builder.line("node_runner.bound_core_node(),");
        builder.line("node_runner.bound_instance_id(),");
        builder.line("topic_name,");
        builder.line("from_core_node,");
        builder.line("from_instance_id,");
        builder.line("peppylib.QoSProfile.Standard,");
        builder.dedent();
        builder.line(")");
    }
    builder.line("raw_message = await subscription.on_next_message()");
    builder.line("payload = raw_message.payload");
    builder.line("instance_id = raw_message.instance_id");
    builder.line("message = _deserialize_payload(payload)");
    builder.line("return instance_id, message");

    builder.dedent();

    Ok(builder.build())
}
