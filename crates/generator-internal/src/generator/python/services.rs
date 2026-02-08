use super::code_builder::PythonCodeBuilder;
use super::type_mapping::{NestedDataclass, collect_fields_from_format};
use crate::generator::types::non_empty_message_format;
use config::node::{ExposedService, MessageFormat, SubscribedService};

/// Emits all nested dataclass definitions.
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

/// Generates Python code for an exposed (handler) service.
pub fn build_exposed_service(service: &ExposedService) -> String {
    let mut builder = PythonCodeBuilder::new();

    let request_format = non_empty_message_format(service.request_message_format.as_ref());
    let response_format = non_empty_message_format(service.response_message_format.as_ref());

    // Response dataclass
    if let Some(fmt) = response_format {
        let mut nested_classes = Vec::new();
        let fields = collect_fields_from_format(fmt, "Response", &mut nested_classes);
        emit_nested_classes(&mut builder, &nested_classes);
        let field_refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_str.as_str()))
            .collect();
        builder.dataclass("Response", &field_refs);
    }

    // RequestData + Request classes
    if let Some(fmt) = request_format {
        let mut nested_classes = Vec::new();
        let fields = collect_fields_from_format(fmt, "RequestData", &mut nested_classes);
        emit_nested_classes(&mut builder, &nested_classes);
        let field_refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_str.as_str()))
            .collect();
        builder.class_def("RequestData", &field_refs);
        builder.class_def(
            "Request",
            &[("instance_id", "str"), ("data", "RequestData")],
        );
    } else {
        builder.class_def("Request", &[("instance_id", "str")]);
    }

    // Service name constant
    builder.line(&format!("SERVICE_NAME = \"{}\"", service.name));
    builder.blank_line();

    // handle_next_request function
    builder.line("async def handle_next_request(node_runner):");
    builder.indent();
    builder.line("endpoint = await peppylib.ServiceMessenger.listen(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_master_node,");
    builder.line("node_runner.bound_instance_id,");
    builder.line("node_runner.node_name,");
    builder.line("SERVICE_NAME,");
    builder.dedent();
    builder.line(")");
    builder.line("await endpoint.handle_next_request(handler)");
    builder.dedent();

    builder.build()
}

/// Generates Python code for a subscribed (polling) service.
pub fn build_subscribed_service(
    service: &SubscribedService,
    request_arguments: &MessageFormat,
    response_arguments: &MessageFormat,
) -> String {
    let mut builder = PythonCodeBuilder::new();

    let request_format = non_empty_message_format(Some(request_arguments));
    let response_format = non_empty_message_format(Some(response_arguments));

    // Constants
    builder.line(&format!("NODE_NAME = \"{}\"", service.node));
    builder.line(&format!("SERVICE_NAME = \"{}\"", service.name));
    builder.blank_line();

    // Request class
    if let Some(fmt) = request_format {
        let mut nested_classes = Vec::new();
        let fields = collect_fields_from_format(fmt, "Request", &mut nested_classes);
        emit_nested_classes(&mut builder, &nested_classes);
        let field_refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_str.as_str()))
            .collect();
        builder.class_def("Request", &field_refs);
    }

    // ResponseData + Response classes
    if let Some(fmt) = response_format {
        let mut nested_classes = Vec::new();
        let fields = collect_fields_from_format(fmt, "ResponseData", &mut nested_classes);
        emit_nested_classes(&mut builder, &nested_classes);
        let field_refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_str.as_str()))
            .collect();
        builder.dataclass("ResponseData", &field_refs);

        builder.class_def(
            "Response",
            &[
                ("master_node", "str"),
                ("instance_id", "str"),
                ("data", "ResponseData"),
            ],
        );
    }

    // poll function
    builder.line(
        "async def poll(node_runner, timeout, target_master_node=None, target_instance_id=None):",
    );
    builder.indent();
    builder.line("response = await peppylib.ServiceMessenger.poll(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_master_node,");
    builder.line("node_runner.bound_instance_id,");
    builder.line("NODE_NAME,");
    builder.line("SERVICE_NAME,");
    builder.line("target_master_node,");
    builder.line("target_instance_id,");
    builder.line("request_payload,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");
    builder.dedent();

    builder.build()
}
