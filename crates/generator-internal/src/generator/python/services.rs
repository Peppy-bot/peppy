use super::PythonSchemaInfo;
use super::code_builder::PythonCodeBuilder;
use super::deserialization;
use super::serialization;
use super::topics::{capnp_loader_fn_name, emit_capnp_loader_fn, emit_capnp_preamble};
use super::type_mapping::{NestedDataclass, collect_fields_from_format, uses_optional};
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
pub fn build_exposed_service(
    service: &ExposedService,
    request_schema_info: Option<&PythonSchemaInfo>,
    response_schema_info: Option<&PythonSchemaInfo>,
) -> String {
    let mut builder = PythonCodeBuilder::new();

    let request_format = non_empty_message_format(service.request_message_format.as_ref());
    let response_format = non_empty_message_format(service.response_message_format.as_ref());

    // Emit capnp schema loaders
    if request_schema_info.is_some() || response_schema_info.is_some() {
        emit_capnp_preamble(&mut builder);
    }
    if let Some(info) = request_schema_info {
        emit_capnp_loader_fn(&mut builder, info);
    }
    if let Some(info) = response_schema_info {
        emit_capnp_loader_fn(&mut builder, info);
    }

    // Response dataclass
    if let Some(fmt) = response_format {
        let mut nested_classes = Vec::new();
        let fields = collect_fields_from_format(fmt, "Response", &mut nested_classes);
        if uses_optional(&fields, &nested_classes) {
            builder.add_import("from typing import Optional");
        }
        emit_nested_classes(&mut builder, &nested_classes);
        let field_refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_str.as_str()))
            .collect();
        builder.dataclass("Response", &field_refs);
    }

    // RequestData + Request dataclasses
    let has_request = request_format.is_some();
    if let Some(fmt) = request_format {
        let mut nested_classes = Vec::new();
        let fields = collect_fields_from_format(fmt, "RequestData", &mut nested_classes);
        if uses_optional(&fields, &nested_classes) {
            builder.add_import("from typing import Optional");
        }
        emit_nested_classes(&mut builder, &nested_classes);
        let field_refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_str.as_str()))
            .collect();
        builder.dataclass("RequestData", &field_refs);
        builder.dataclass(
            "Request",
            &[
                ("instance_id", "str"),
                ("master_node", "str"),
                ("data", "RequestData"),
            ],
        );
    } else {
        builder.dataclass("Request", &[("instance_id", "str"), ("master_node", "str")]);
    }

    // Service name constant
    builder.line(&format!("SERVICE_NAME = \"{}\"", service.name));
    builder.blank_line();

    // _deserialize_request helper (when request format exists)
    if let Some((fmt, info)) = service
        .request_message_format
        .as_ref()
        .filter(|f| !f.0.is_empty())
        .zip(request_schema_info)
    {
        let loader_fn_name = capnp_loader_fn_name(info);
        deserialization::build_deserialize_fn(
            &mut builder,
            info,
            fmt,
            "RequestData",
            &format!("{loader_fn_name}()"),
            "_deserialize_request",
        );
    }

    // Determine handler callable type
    let has_response = response_format.is_some();
    let handler_return_type = if has_response { "Response" } else { "None" };
    let handler_type = format!("Callable[[Request], {handler_return_type}]");
    builder.add_import("from typing import Callable");

    // _handle_request_payload helper
    builder.blank_line();
    if has_request {
        builder.line(&format!(
            "def _handle_request_payload(payload: bytes, handler: {handler_type}, master_node: str, instance_id: str) -> bytes:"
        ));
    } else {
        builder.line(&format!(
            "def _handle_request_payload(handler: {handler_type}, master_node: str, instance_id: str) -> bytes:"
        ));
    }
    builder.indent();

    if has_request {
        builder.line("request_data = _deserialize_request(payload)");
        builder.line(
            "request = Request(instance_id=instance_id, master_node=master_node, data=request_data)",
        );
    } else {
        builder.line("request = Request(instance_id=instance_id, master_node=master_node)");
    }

    if let Some((resp_fmt, resp_info)) = service
        .response_message_format
        .as_ref()
        .filter(|f| !f.0.is_empty())
        .zip(response_schema_info)
    {
        builder.line("response = handler(request)");
        let loader_fn_name = capnp_loader_fn_name(resp_info);
        builder.line(&format!(
            "capnp_msg = {loader_fn_name}().{}.new_message()",
            resp_info.struct_name
        ));
        let mut counter = 0u32;
        serialization::emit_capnp_assignments(
            &mut builder,
            "capnp_msg",
            resp_fmt,
            "response",
            &mut counter,
        );
        builder.line("return capnp_msg.to_bytes()");
    } else {
        builder.line("handler(request)");
        builder.line("return b\"\"");
    }
    builder.dedent();

    // handle_next_request async function
    builder.add_import("import peppylib");
    builder.blank_line();
    builder.line(&format!(
        "async def handle_next_request(node_runner: peppylib.NodeRunner, handler: {handler_type}) -> None:"
    ));
    builder.indent();
    builder.line("endpoint = await peppylib.ServiceMessenger.listen(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_master_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line("node_runner.node_name(),");
    builder.line("SERVICE_NAME,");
    builder.dedent();
    builder.line(")");

    // _on_request wrapper
    builder.line("async def _on_request(request_context):");
    builder.indent();
    builder.line("message = request_context.message");
    if has_request {
        builder.line("payload = message.payload");
    }
    builder.line("master_node = message.master_node");
    builder.line("instance_id = message.instance_id");
    if has_request {
        builder.line("return _handle_request_payload(payload, handler, master_node, instance_id)");
    } else {
        builder.line("return _handle_request_payload(handler, master_node, instance_id)");
    }
    builder.dedent();

    builder.line("await endpoint.handle_next_request(_on_request)");
    builder.dedent();

    builder.build()
}

/// Generates Python code for a subscribed (polling) service.
pub fn build_subscribed_service(
    service: &SubscribedService,
    request_arguments: &MessageFormat,
    response_arguments: &MessageFormat,
    request_schema_info: Option<&PythonSchemaInfo>,
    response_schema_info: Option<&PythonSchemaInfo>,
) -> String {
    let mut builder = PythonCodeBuilder::new();

    let request_format = non_empty_message_format(Some(request_arguments));
    let response_format = non_empty_message_format(Some(response_arguments));

    // Capnp schema loaders
    if request_schema_info.is_some() || response_schema_info.is_some() {
        emit_capnp_preamble(&mut builder);
    }
    if let Some(info) = request_schema_info {
        emit_capnp_loader_fn(&mut builder, info);
    }
    if let Some(info) = response_schema_info {
        emit_capnp_loader_fn(&mut builder, info);
    }

    // Constants
    builder.line(&format!("NODE_NAME = \"{}\"", service.node));
    builder.line(&format!("SERVICE_NAME = \"{}\"", service.name));
    builder.blank_line();

    // Request dataclass
    if let Some(fmt) = request_format {
        let mut nested_classes = Vec::new();
        let fields = collect_fields_from_format(fmt, "Request", &mut nested_classes);
        if uses_optional(&fields, &nested_classes) {
            builder.add_import("from typing import Optional");
        }
        emit_nested_classes(&mut builder, &nested_classes);
        let field_refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_str.as_str()))
            .collect();
        builder.dataclass("Request", &field_refs);
    }

    // ResponseData + Response dataclasses
    if let Some(fmt) = response_format {
        let mut nested_classes = Vec::new();
        let fields = collect_fields_from_format(fmt, "ResponseData", &mut nested_classes);
        if uses_optional(&fields, &nested_classes) {
            builder.add_import("from typing import Optional");
        }
        emit_nested_classes(&mut builder, &nested_classes);
        let field_refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_str.as_str()))
            .collect();
        builder.dataclass("ResponseData", &field_refs);

        builder.dataclass(
            "Response",
            &[
                ("master_node", "str"),
                ("instance_id", "str"),
                ("data", "ResponseData"),
            ],
        );
    }

    // _deserialize_response helper (when response schema exists)
    if let Some((fmt, info)) = response_format.zip(response_schema_info) {
        let loader_fn_name = capnp_loader_fn_name(info);
        deserialization::build_deserialize_fn(
            &mut builder,
            info,
            fmt,
            "ResponseData",
            &format!("{loader_fn_name}()"),
            "_deserialize_response",
        );
    }

    // poll async function
    builder.add_import("import peppylib");
    builder.add_import("from typing import Optional");
    builder.blank_line();

    let has_request = request_format.is_some();
    let has_response = response_format.is_some();
    let return_type = if has_response {
        " -> Response"
    } else {
        " -> None"
    };

    let signature = if has_request {
        format!(
            "async def poll(node_runner: peppylib.NodeRunner, request: Request, timeout: float, target_master_node: Optional[str] = None, target_instance_id: Optional[str] = None){return_type}:"
        )
    } else {
        format!(
            "async def poll(node_runner: peppylib.NodeRunner, timeout: float, target_master_node: Optional[str] = None, target_instance_id: Optional[str] = None){return_type}:"
        )
    };
    builder.line(&signature);
    builder.indent();

    // Serialize the request payload
    if let Some((fmt, info)) = request_format.zip(request_schema_info) {
        let loader_fn_name = capnp_loader_fn_name(info);
        builder.line(&format!(
            "capnp_msg = {loader_fn_name}().{}.new_message()",
            info.struct_name
        ));
        let mut counter = 0u32;
        serialization::emit_capnp_assignments(
            &mut builder,
            "capnp_msg",
            fmt,
            "request",
            &mut counter,
        );
        builder.line("request_payload = capnp_msg.to_bytes()");
    } else {
        builder.line("request_payload = b\"\"");
    }

    // Call peppylib.ServiceMessenger.poll
    if has_response {
        builder.line("response_message = await peppylib.ServiceMessenger.poll(");
    } else {
        builder.line("await peppylib.ServiceMessenger.poll(");
    }
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_master_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line("NODE_NAME,");
    builder.line("SERVICE_NAME,");
    builder.line("target_master_node,");
    builder.line("target_instance_id,");
    builder.line("request_payload,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");

    // Deserialize response and return
    if has_response {
        builder.line("payload = response_message.payload");
        builder.line("response_data = _deserialize_response(payload)");
        builder.line("return Response(master_node=response_message.master_node, instance_id=response_message.instance_id, data=response_data)");
    }

    builder.dedent();

    builder.build()
}
