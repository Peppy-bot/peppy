use super::PythonSchemaInfo;
use super::code_builder::{PythonCodeBuilder, emit_format_as_dataclass};
use super::deserialization;
use super::serialization;
use super::topics::{capnp_loader_fn_name, emit_capnp_loader_fn, emit_capnp_preamble};
use crate::error::Result;
use crate::generator::types::{InterfaceOrigin, non_empty_message_format};
use config::node::{ConsumedService, ExposedService, MessageFormat};

/// Returns the Python expression for the `SenderTarget` to splice into a
/// generated `listen` / `poll` / `subscribe` / `emit` call:
///   - non-`conforms_to` artifact → `peppylib.SenderTarget.node(<node_name_expr>, <node_tag_expr>)`
///   - `interfaces.conforms_to`   → `peppylib.SenderTarget.interface("<name>", "<tag>")`
///
/// Consumer-side wildcard (`None`) is passed directly at the call site that
/// needs it; this helper covers only the producer-known origin cases.
pub(crate) fn sender_target_python_expr(
    origin: Option<&InterfaceOrigin>,
    node_name_expr: &str,
    node_tag_expr: &str,
) -> String {
    match origin {
        Some(o) => format!(
            "peppylib.SenderTarget.interface({:?}, {:?})",
            o.iface_name, o.iface_tag
        ),
        None => format!("peppylib.SenderTarget.node({node_name_expr}, {node_tag_expr})"),
    }
}

/// Returns the Python expression for the consumer's
/// `target_instance_id` argument at a consumed service / action call
/// site. Calls `node_runner.pinned_target_for(<link_id>)` when the
/// dependency carries a manifest link_id; `None` for synthetic test
/// fixtures that don't model a manifest dep. Pinned slots resolve to
/// the bound producer's `instance_id`; `from_any` slots (bound or not)
/// resolve to `None` and fall back to wildcard discover-then-pin at
/// the call site.
pub(crate) fn consumed_from_instance_id_python_expr(
    dependency: &crate::generator::types::DependencyContext,
) -> String {
    match dependency.wire_link_id() {
        Some(link_id) => format!("node_runner.pinned_target_for({link_id:?})"),
        None => "None".to_string(),
    }
}

/// Generates Python code for an exposed (handler) service.
pub fn build_exposed_service(
    service: &ExposedService,
    request_schema_info: Option<&PythonSchemaInfo>,
    response_schema_info: Option<&PythonSchemaInfo>,
    origin: Option<&InterfaceOrigin>,
) -> Result<String> {
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
        emit_format_as_dataclass(&mut builder, "Response", fmt)?;
    }

    // RequestData + Request dataclasses
    let has_request = request_format.is_some();
    if let Some(fmt) = request_format {
        emit_format_as_dataclass(&mut builder, "RequestData", fmt)?;
        builder.dataclass(
            "Request",
            &[
                ("instance_id", "str"),
                ("core_node", "str"),
                ("data", "RequestData"),
            ],
        );
    } else {
        builder.dataclass("Request", &[("instance_id", "str"), ("core_node", "str")]);
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
            "async def _handle_request_payload(payload: bytes, handler: {handler_type}, core_node: str, instance_id: str) -> bytes:"
        ));
    } else {
        builder.line(&format!(
            "async def _handle_request_payload(handler: {handler_type}, core_node: str, instance_id: str) -> bytes:"
        ));
    }
    builder.indent();

    if has_request {
        builder.line("request_data = _deserialize_request(payload)");
        builder.line(
            "request = Request(instance_id=instance_id, core_node=core_node, data=request_data)",
        );
    } else {
        builder.line("request = Request(instance_id=instance_id, core_node=core_node)");
    }

    if let Some((resp_fmt, resp_info)) = service
        .response_message_format
        .as_ref()
        .filter(|f| !f.0.is_empty())
        .zip(response_schema_info)
    {
        builder.line("response = handler(request)");
        builder.line("if hasattr(response, \"__await__\"):");
        builder.indent();
        builder.line("response = await response");
        builder.dedent();
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
        builder.line("maybe_response = handler(request)");
        builder.line("if hasattr(maybe_response, \"__await__\"):");
        builder.indent();
        builder.line("await maybe_response");
        builder.dedent();
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
    let target_expr =
        sender_target_python_expr(origin, "node_runner.node_name()", "node_runner.node_tag()");
    builder.line("endpoint = await peppylib.ServiceMessenger.listen(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_core_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line(&format!("{target_expr},"));
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
    builder.line("core_node = message.core_node");
    builder.line("instance_id = message.instance_id");
    if has_request {
        builder
            .line("return await _handle_request_payload(payload, handler, core_node, instance_id)");
    } else {
        builder.line("return await _handle_request_payload(handler, core_node, instance_id)");
    }
    builder.dedent();

    builder.line("await endpoint.handle_next_request(_on_request)");
    builder.dedent();

    Ok(builder.build())
}

/// Generates Python code for a subscribed (polling) service.
pub fn build_consumed_service(
    service: &ConsumedService,
    request_arguments: &MessageFormat,
    response_arguments: &MessageFormat,
    request_schema_info: Option<&PythonSchemaInfo>,
    response_schema_info: Option<&PythonSchemaInfo>,
    dependency: &crate::generator::types::DependencyContext,
) -> Result<String> {
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
    builder.line(&format!("NODE_NAME = \"{}\"", dependency.producer_name));
    builder.line(&format!("SERVICE_NAME = \"{}\"", service.name));
    builder.blank_line();

    // Request dataclass
    if let Some(fmt) = request_format {
        emit_format_as_dataclass(&mut builder, "Request", fmt)?;
    }

    // ResponseData + Response dataclasses
    if let Some(fmt) = response_format {
        emit_format_as_dataclass(&mut builder, "ResponseData", fmt)?;

        builder.dataclass(
            "Response",
            &[
                ("core_node", "str"),
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
            "async def poll(node_runner: peppylib.NodeRunner, request: Request, timeout: float){return_type}:"
        )
    } else {
        format!("async def poll(node_runner: peppylib.NodeRunner, timeout: float){return_type}:")
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

    if has_response {
        builder.line("response_message = await peppylib.ServiceMessenger.poll(");
    } else {
        builder.line("await peppylib.ServiceMessenger.poll(");
    }
    let target_expr = sender_target_python_expr(
        dependency.origin.as_ref(),
        "NODE_NAME",
        &format!("{:?}", dependency.producer_tag),
    );
    let target_instance_id_expr = consumed_from_instance_id_python_expr(dependency);
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_core_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line(&format!("{target_expr},"));
    builder.line("SERVICE_NAME,");
    builder.line("None,");
    builder.line(&format!("{target_instance_id_expr},"));
    builder.line("request_payload,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");

    // Deserialize response and return
    if has_response {
        builder.line("payload = response_message.payload");
        builder.line("response_data = _deserialize_response(payload)");
        builder.line("return Response(core_node=response_message.core_node, instance_id=response_message.instance_id, data=response_data)");
    }

    builder.dedent();

    Ok(builder.build())
}
