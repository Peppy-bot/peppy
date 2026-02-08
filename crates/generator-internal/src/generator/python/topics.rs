use super::code_builder::PythonCodeBuilder;
use super::type_mapping::{
    NestedDataclass, collect_fields_from_format, qos_profile_python, uses_optional,
};
use crate::generator::naming::sanitize_component;
use config::node::{ExposedTopic, MessageFormat, SubscribedTopic};

/// Emits all nested dataclass definitions collected during field collection.
fn emit_nested_classes(builder: &mut PythonCodeBuilder, nested_classes: &[NestedDataclass]) {
    for class_def in nested_classes {
        let fields: Vec<(&str, &str)> = class_def
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_str.as_str()))
            .collect();
        builder.dataclass(&class_def.name, &fields);
    }
}

/// Generates Python code for an exposed (publishing) topic.
pub fn build_exposed_topic(topic: &ExposedTopic) -> String {
    let mut builder = PythonCodeBuilder::new();
    let mut nested_classes = Vec::new();

    // Collect fields from message format
    let fields = topic
        .message_format
        .as_ref()
        .map(|fmt| collect_fields_from_format(fmt, "Message", &mut nested_classes))
        .unwrap_or_default();

    if uses_optional(&fields, &nested_classes) {
        builder.add_import("from typing import Optional");
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
    builder.line(&format!("topic_name = \"{}\"", topic.name));
    builder.line(&format!("qos = {qos}"));
    builder.line("await peppylib.TopicMessenger.emit(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_master_node,");
    builder.line("node_runner.bound_instance_id,");
    builder.line("node_runner.node_name,");
    builder.line("topic_name,");
    builder.line("qos,");
    builder.line("payload,");
    builder.dedent();
    builder.line(")");
    builder.dedent();

    builder.build()
}

/// Generates Python code for a subscribed (receiving) topic.
pub fn build_subscribed_topic(topic: &SubscribedTopic, arguments: &MessageFormat) -> String {
    let mut builder = PythonCodeBuilder::new();
    let mut nested_classes = Vec::new();

    // Collect fields from the message format
    let fields = collect_fields_from_format(arguments, "Message", &mut nested_classes);

    if uses_optional(&fields, &nested_classes) {
        builder.add_import("from typing import Optional");
    }

    // Emit nested dataclasses first (dependency order)
    emit_nested_classes(&mut builder, &nested_classes);

    // Emit the main Message dataclass
    let field_refs: Vec<(&str, &str)> = fields
        .iter()
        .map(|f| (f.name.as_str(), f.type_str.as_str()))
        .collect();
    builder.dataclass("Message", &field_refs);

    // Generate on_next_message_received function
    builder.add_import("import peppylib");
    builder.line("async def on_next_message_received(node_runner: peppylib.NodeRunner):");
    builder.indent();
    builder.line(&format!("node_name = \"{}\"", topic.node));
    builder.line(&format!("topic_name = \"{}\"", topic.name));
    builder.line("subscription = await peppylib.TopicMessenger.subscribe(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_master_node,");
    builder.line("node_runner.bound_instance_id,");
    builder.line("node_name,");
    builder.line("topic_name,");
    builder.line("None,");
    builder.line("None,");
    builder.line("peppylib.QoSProfile.Standard,");
    builder.dedent();
    builder.line(")");
    builder.line("message = await subscription.on_next_message()");
    builder.dedent();

    builder.build()
}

/// Returns the module label for a subscribed topic artifact.
pub fn subscribed_topic_module_label(topic: &SubscribedTopic) -> String {
    let node_component = sanitize_component(&topic.node);
    let topic_component = sanitize_component(&topic.name);

    match (node_component.is_empty(), topic_component.is_empty()) {
        (false, false) => format!("{node_component}_{topic_component}"),
        (false, true) => node_component,
        (true, false) => topic_component,
        (true, true) => String::from("topic"),
    }
}
