use super::PythonSchemaInfo;
use super::code_builder::{PythonCodeBuilder, emit_format_as_dataclass};
use super::deserialization;
use super::serialization;
use super::topics::{capnp_loader_fn_name, emit_capnp_loader_fn, emit_capnp_preamble};
use crate::error::Result;
use crate::generator::types::{ContractOrigin, non_empty_message_format};
use config::node::{Cardinality, ConsumedService, MessageFormat, NativeExposedService};

/// Returns the Python expression for the `SenderTarget` to splice into a
/// generated `listen` / `poll` / `subscribe` / `emit` call:
///   - native artifact         -> `peppylib.SenderTarget.node(<node_name_expr>, <node_tag_expr>)`
///   - contract-backed artifact -> `peppylib.SenderTarget.contract("<name>", "<tag>")`
///
/// Consumer-side wildcard (`None`) is passed directly at the call site that
/// needs it; this helper covers only the producer-known origin cases.
pub(crate) fn sender_target_python_expr(
    origin: Option<&ContractOrigin>,
    node_name_expr: &str,
    node_tag_expr: &str,
) -> String {
    match origin {
        Some(o) => format!(
            "peppylib.SenderTarget.contract({:?}, {:?})",
            o.contract_name, o.contract_tag
        ),
        None => format!("peppylib.SenderTarget.node({node_name_expr}, {node_tag_expr})"),
    }
}

/// Emits the module-level bound-producer accessor every consumed topic,
/// service, and action module exposes. Mirroring the Rust codegen, the
/// accessor encodes the slot's launch-validated cardinality: `one` generates
/// the singular `bound_producer()` returning the sole `ProducerRef` directly,
/// `zero_or_one` the same name returning an `Optional`, while `one_or_more` /
/// `zero_or_more` generate `bound_producers()` returning the ordered list
/// (documented never-empty or possibly-empty respectively; Python has no
/// non-empty list type, so the name flip is what surfaces a cardinality
/// change at call sites). The emitted name and the `NodeRunner` method it
/// calls are separate: the two scalar cardinalities share one emitted name
/// over two runtime accessors. The docstring prose comes from
/// `DependencyContext::bound_producers_doc` so both language generators
/// state the same guarantees; only the Python-API tail sentence is added
/// here. Callers need `import peppylib`, which every consumed module
/// already adds.
pub(crate) fn emit_bound_producer_accessor_fn(
    builder: &mut PythonCodeBuilder,
    dependency: &crate::generator::types::DependencyContext,
) {
    let (fn_name, runtime_method, return_type, typing_import, api_note) =
        match dependency.cardinality {
            Cardinality::One => (
                "bound_producer",
                "bound_producer",
                "peppylib.ProducerRef",
                None,
                None,
            ),
            Cardinality::ZeroOrOne => (
                "bound_producer",
                "optional_bound_producer",
                "Optional[peppylib.ProducerRef]",
                Some("from typing import Optional"),
                None,
            ),
            Cardinality::OneOrMore => (
                "bound_producers",
                "bound_producers",
                "List[peppylib.ProducerRef]",
                Some("from typing import List"),
                Some(crate::generator::types::DocLanguage::Python.never_empty_tail()),
            ),
            Cardinality::ZeroOrMore => (
                "bound_producers",
                "bound_producers",
                "List[peppylib.ProducerRef]",
                Some("from typing import List"),
                None,
            ),
        };
    if let Some(import) = typing_import {
        builder.add_import(import);
    }

    let doc = dependency.bound_producers_doc();
    builder.blank_line();
    builder.block(
        &format!("def {fn_name}(node_runner: peppylib.NodeRunner) -> {return_type}:"),
        |builder| {
            // A multi-paragraph docstring, so it opens and closes by hand
            // rather than through `docstring`.
            builder.line(&format!("\"\"\"{}", doc[0]));
            builder.blank_line();
            builder.lines(&doc[1..]);
            if let Some(note) = api_note {
                builder.line(note);
            }
            builder.line("\"\"\"");
            builder.line(&format!(
                "return node_runner.{runtime_method}({:?})",
                dependency.link_id
            ));
        },
    );
    builder.blank_line();
}

/// Emits the per-call membership check ahead of a consumed poll /
/// send_goal call: the caller-selected `target` must be a member of the
/// slot's own bound set, rejected before anything reaches the wire
/// otherwise. `LINK_ID` is a module constant every consumed service /
/// action module defines.
pub(crate) fn emit_target_membership_check(builder: &mut PythonCodeBuilder) {
    builder.line("node_runner.ensure_target_bound(LINK_ID, target)");
}

/// Generates Python code for an exposed (handler) service.
pub fn build_exposed_service(
    service: &NativeExposedService,
    request_schema_info: Option<&PythonSchemaInfo>,
    response_schema_info: Option<&PythonSchemaInfo>,
    origin: Option<&ContractOrigin>,
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

    // _handle_request_payload helper. A body-less service takes no `payload`
    // parameter at all, so the parameter list is spliced rather than the whole
    // signature written twice.
    builder.blank_line();
    let payload_param = if has_request { "payload: bytes, " } else { "" };
    builder.block(
        &format!(
            "async def _handle_request_payload({payload_param}handler: {handler_type}, core_node: str, instance_id: str) -> bytes:"
        ),
        |builder| {
            if has_request {
                builder.py(r#"
request_data = _deserialize_request(payload)
request = Request(instance_id=instance_id, core_node=core_node, data=request_data)
"#);
            } else {
                builder.line("request = Request(instance_id=instance_id, core_node=core_node)");
            }

            let Some((resp_fmt, resp_info)) = service
                .response_message_format
                .as_ref()
                .filter(|f| !f.0.is_empty())
                .zip(response_schema_info)
            else {
                builder.py(r#"
maybe_response = handler(request)
if hasattr(maybe_response, "__await__"):
    await maybe_response
return b""
"#);
                return;
            };

            builder.py(r#"
response = handler(request)
if hasattr(response, "__await__"):
    response = await response
"#);
            let loader_fn_name = capnp_loader_fn_name(resp_info);
            builder.line(&format!(
                "capnp_msg = {loader_fn_name}().{}.new_message()",
                resp_info.struct_name
            ));
            let mut counter = 0u32;
            serialization::emit_capnp_assignments(
                builder,
                "capnp_msg",
                resp_fmt,
                "response",
                &mut counter,
            );
            builder.line("return capnp_msg.to_bytes()");
        },
    );

    // handle_next_request async function
    builder.add_import("import peppylib");
    builder.blank_line();
    let target_expr =
        sender_target_python_expr(origin, "node_runner.node_name()", "node_runner.node_tag()");
    builder.block(
        &format!(
            "async def handle_next_request(node_runner: peppylib.NodeRunner, handler: {handler_type}) -> None:"
        ),
        |builder| {
            builder.call(
                "endpoint = await peppylib.ServiceMessenger.listen(",
                &[
                    "node_runner.messenger(),",
                    "node_runner.bound_core_node(),",
                    "node_runner.bound_instance_id(),",
                    &format!("{target_expr},"),
                    "SERVICE_NAME,",
                ],
                ")",
            );

            // _on_request wrapper
            builder.block("async def _on_request(request_context):", |builder| {
                builder.line("message = request_context.message");
                if has_request {
                    builder.line("payload = message.payload");
                }
                builder.lines([
                    "core_node = message.core_node",
                    "instance_id = message.instance_id",
                ]);
                let payload_argument = if has_request { "payload, " } else { "" };
                builder.line(&format!(
                    "return await _handle_request_payload({payload_argument}handler, core_node, instance_id)"
                ));
            });

            builder.line("await endpoint.handle_next_request(_on_request)");
        },
    );

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

    // Capnp schema loaders.
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
    builder.line(&format!("LINK_ID = \"{}\"", dependency.link_id));
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

    emit_bound_producer_accessor_fn(&mut builder, dependency);

    let signature = if has_request {
        format!(
            "async def poll(node_runner: peppylib.NodeRunner, target: peppylib.ProducerRef, request: Request, timeout: float){return_type}:"
        )
    } else {
        format!(
            "async def poll(node_runner: peppylib.NodeRunner, target: peppylib.ProducerRef, timeout: float){return_type}:"
        )
    };
    let target_expr = sender_target_python_expr(
        dependency.origin.as_ref(),
        "NODE_NAME",
        &format!("{:?}", dependency.producer_tag),
    );
    builder.block(&signature, |builder| {
        // A multi-paragraph docstring, so it opens and closes by hand rather
        // than through `docstring`.
        builder.line("\"\"\"Polls this service on `target`, a member of the slot's bound set.");
        builder.blank_line();
        builder.line("A target outside the set fails before anything reaches the wire.");
        builder.lines(dependency.target_selection_doc());
        builder.line("\"\"\"");
        emit_target_membership_check(builder);

        // Serialize the request payload
        if let Some((fmt, info)) = request_format.zip(request_schema_info) {
            let loader_fn_name = capnp_loader_fn_name(info);
            builder.line(&format!(
                "capnp_msg = {loader_fn_name}().{}.new_message()",
                info.struct_name
            ));
            let mut counter = 0u32;
            serialization::emit_capnp_assignments(builder, "capnp_msg", fmt, "request", &mut counter);
            builder.line("request_payload = capnp_msg.to_bytes()");
        } else {
            builder.line("request_payload = b\"\"");
        }

        builder.call(
            if has_response {
                "response_message = await peppylib.ServiceMessenger.poll("
            } else {
                "await peppylib.ServiceMessenger.poll("
            },
            &[
                "node_runner.messenger(),",
                "node_runner.bound_core_node(),",
                "node_runner.bound_instance_id(),",
                &format!("{target_expr},"),
                "SERVICE_NAME,",
                "target,",
                "request_payload,",
                "timeout,",
            ],
            ")",
        );

        // Deserialize response and return
        if has_response {
            builder.py(r#"
payload = response_message.payload
response_data = _deserialize_response(payload)
return Response(core_node=response_message.core_node, instance_id=response_message.instance_id, data=response_data)
"#);
        }
    });

    Ok(builder.build())
}
