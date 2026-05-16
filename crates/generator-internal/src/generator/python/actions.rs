use super::PythonSchemaInfo;
use super::code_builder::{PythonCodeBuilder, emit_format_as_dataclass, emit_nested_classes};
use super::deserialization;
use super::serialization;
use super::services::iface_segment_python_literals;
use super::topics::{capnp_loader_fn_name, emit_capnp_loader_fn, emit_capnp_preamble};
use super::type_mapping::{collect_fields_from_format, uses_optional};
use crate::error::{Error, Result};
use crate::generator::types::{
    ConsumedActionMessage, InterfaceOrigin, cancel_action_response_format, non_empty_message_format,
};
use config::node::{ConsumedAction, ExposedAction, MessageFormat};

// ---------------------------------------------------------------------------
// Exposed actions
// ---------------------------------------------------------------------------

/// Generates Python code for an exposed (handler) action.
pub fn build_exposed_action(
    action: &ExposedAction,
    goal_request_schema_info: Option<&PythonSchemaInfo>,
    goal_response_schema_info: Option<&PythonSchemaInfo>,
    cancel_response_schema_info: Option<&PythonSchemaInfo>,
    result_response_schema_info: Option<&PythonSchemaInfo>,
    feedback_schema_info: Option<&PythonSchemaInfo>,
    origin: Option<&InterfaceOrigin>,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();

    let has_goal = action.goal_service.is_some();
    let has_result = action.result_service.is_some();
    let has_feedback = action.feedback_topic.is_some();

    // Capnp preamble and loader functions
    let any_schema = goal_request_schema_info.is_some()
        || goal_response_schema_info.is_some()
        || cancel_response_schema_info.is_some()
        || result_response_schema_info.is_some()
        || feedback_schema_info.is_some();

    if any_schema {
        emit_capnp_preamble(&mut builder);
    }
    for info in [
        goal_request_schema_info,
        goal_response_schema_info,
        cancel_response_schema_info,
        result_response_schema_info,
        feedback_schema_info,
    ]
    .into_iter()
    .flatten()
    {
        emit_capnp_loader_fn(&mut builder, info);
    }

    // ACTION_NAME constant
    builder.line(&format!("ACTION_NAME = \"{}\"", action.name));
    builder.blank_line();

    // ---------------------------------------------------------------
    // Phase 1: Dataclass definitions (module level)
    // ---------------------------------------------------------------

    let has_goal_request = action
        .goal_service
        .as_ref()
        .and_then(|gs| non_empty_message_format(gs.request_message_format.as_ref()))
        .is_some();
    let has_goal_response = action
        .goal_service
        .as_ref()
        .and_then(|gs| non_empty_message_format(gs.response_message_format.as_ref()))
        .is_some();
    let has_result_response = action
        .result_service
        .as_ref()
        .and_then(|rs| non_empty_message_format(rs.response_message_format.as_ref()))
        .is_some();
    let feedback_format = action
        .feedback_topic
        .as_ref()
        .and_then(|topic| non_empty_message_format(topic.message_format.as_ref()));
    let mut feedback_fields = Vec::new();

    // Compute goal handler type early (needed by both helpers and class methods)
    let goal_handler_type = if let Some(goal) = &action.goal_service {
        let request_format = non_empty_message_format(goal.request_message_format.as_ref());
        let response_format = non_empty_message_format(goal.response_message_format.as_ref());

        if let Some(fmt) = request_format {
            emit_format_as_dataclass(&mut builder, "GoalRequestData", fmt)?;
            builder.dataclass(
                "GoalRequest",
                &[
                    ("instance_id", "str"),
                    ("core_node", "str"),
                    ("data", "GoalRequestData"),
                ],
            );
        } else {
            builder.dataclass(
                "GoalRequest",
                &[("instance_id", "str"), ("core_node", "str")],
            );
        }

        if let Some(fmt) = response_format {
            emit_format_as_dataclass(&mut builder, "GoalResponse", fmt)?;
        } else {
            builder.dataclass("GoalResponse", &[]);
        }

        builder.dataclass(
            "CancelRequest",
            &[("instance_id", "str"), ("core_node", "str")],
        );

        let cancel_format = cancel_action_response_format();
        emit_format_as_dataclass(&mut builder, "CancelResponse", &cancel_format)?;

        let return_type = if has_goal_response {
            "GoalResponse"
        } else {
            "None"
        };
        builder.add_import("from typing import Callable");
        Some(format!("Callable[[GoalRequest], {return_type}]"))
    } else {
        None
    };

    let result_handler_type = if let Some(result) = &action.result_service {
        builder.dataclass(
            "ResultRequest",
            &[("instance_id", "str"), ("core_node", "str")],
        );

        let result_resp_format = non_empty_message_format(result.response_message_format.as_ref());
        if let Some(fmt) = result_resp_format {
            emit_format_as_dataclass(&mut builder, "ResultResponse", fmt)?;
        } else {
            builder.dataclass("ResultResponse", &[]);
        }

        let return_type = if has_result_response {
            "ResultResponse"
        } else {
            "None"
        };
        builder.add_import("from typing import Callable");
        Some(format!("Callable[[ResultRequest], {return_type}]"))
    } else {
        None
    };

    // Feedback field type annotations are used by ActionHandle.emit_feedback().
    // Emit any nested dataclasses/imports at module scope so annotations resolve.
    if let Some(fmt) = feedback_format {
        let mut nested_classes = Vec::new();
        feedback_fields = collect_fields_from_format(fmt, "Feedback", &mut nested_classes)?;
        if uses_optional(&feedback_fields, &nested_classes) {
            builder.add_import("from typing import Optional");
        }
        emit_nested_classes(&mut builder, &nested_classes);
    }

    // ---------------------------------------------------------------
    // Phase 2: Module-level helper functions
    // ---------------------------------------------------------------

    if let Some(goal) = &action.goal_service {
        // _deserialize_goal_request helper
        if let Some((fmt, info)) = goal
            .request_message_format
            .as_ref()
            .filter(|f| !f.0.is_empty())
            .zip(goal_request_schema_info)
        {
            let loader_fn_name = capnp_loader_fn_name(info);
            deserialization::build_deserialize_fn(
                &mut builder,
                info,
                fmt,
                "GoalRequestData",
                &format!("{loader_fn_name}()"),
                "_deserialize_goal_request",
            );
        }

        // _handle_goal_payload helper
        let ght = goal_handler_type
            .as_ref()
            .ok_or_else(|| Error::InvariantViolation {
                context: String::from(
                    "goal handler type should exist when goal service is present",
                ),
            })?;
        builder.blank_line();
        if has_goal_request {
            builder.line(&format!(
                "async def _handle_goal_payload(payload: bytes, handler: {ght}, core_node: str, instance_id: str) -> bytes:"
            ));
        } else {
            builder.line(&format!(
                "async def _handle_goal_payload(handler: {ght}, core_node: str, instance_id: str) -> bytes:"
            ));
        }
        builder.indent();

        if has_goal_request {
            builder.line("request_data = _deserialize_goal_request(payload)");
            builder.line(
                "request = GoalRequest(instance_id=instance_id, core_node=core_node, data=request_data)",
            );
        } else {
            builder.line("request = GoalRequest(instance_id=instance_id, core_node=core_node)");
        }

        emit_handler_response_body(
            &mut builder,
            goal.response_message_format
                .as_ref()
                .filter(|f| !f.0.is_empty())
                .zip(goal_response_schema_info),
        );
        builder.dedent();
        builder.blank_line();

        // _handle_cancel_payload helper
        let cancel_format = cancel_action_response_format();
        let cancel_handler_type = "Callable[[CancelRequest], CancelResponse]";
        builder.line(&format!(
            "async def _handle_cancel_payload(handler: {cancel_handler_type}, core_node: str, instance_id: str) -> bytes:"
        ));
        builder.indent();
        builder.line("request = CancelRequest(instance_id=instance_id, core_node=core_node)");

        emit_handler_response_body(
            &mut builder,
            cancel_response_schema_info.map(|info| (&cancel_format as &MessageFormat, info)),
        );
        builder.dedent();
        builder.blank_line();
    }

    if let Some(result) = &action.result_service {
        // _handle_result_payload helper
        let rht = result_handler_type
            .as_ref()
            .ok_or_else(|| Error::InvariantViolation {
                context: String::from(
                    "result handler type should exist when result service is present",
                ),
            })?;
        builder.line(&format!(
            "async def _handle_result_payload(handler: {rht}, core_node: str, instance_id: str) -> bytes:"
        ));
        builder.indent();
        builder.line("request = ResultRequest(instance_id=instance_id, core_node=core_node)");

        emit_handler_response_body(
            &mut builder,
            result
                .response_message_format
                .as_ref()
                .filter(|f| !f.0.is_empty())
                .zip(result_response_schema_info),
        );
        builder.dedent();
        builder.blank_line();
    }

    // ---------------------------------------------------------------
    // Phase 3: ActionHandle class with all methods
    // ---------------------------------------------------------------

    builder.add_import("import peppylib");
    builder.line("class ActionHandle:");
    builder.indent();

    // expose @classmethod
    builder.line("@classmethod");
    builder.add_import("from typing import Self");
    builder.line("async def expose(cls, node_runner: peppylib.NodeRunner) -> Self:");
    builder.indent();
    let (expose_iface_name_lit, expose_iface_tag_lit) = iface_segment_python_literals(origin);
    builder.line("action = await peppylib.ActionMessenger.expose(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_core_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line("node_runner.node_name(),");
    builder.line(&format!("{expose_iface_name_lit},"));
    builder.line(&format!("{expose_iface_tag_lit},"));
    builder.line("ACTION_NAME,");
    builder.dedent();
    builder.line(")");
    builder.line("handle = cls()");
    if has_goal {
        builder.line("handle.goal_service = action.goal_service");
        builder.line("handle.cancel_service = action.cancel_service");
    }
    if has_result {
        builder.line("handle.result_service = action.result_service");
    }
    if has_feedback {
        builder.line("handle.feedback_publisher_factory = action.feedback_publisher_factory");
        builder.line("handle.current_goal = None");
    }
    builder.line("return handle");
    builder.dedent();
    builder.blank_line();

    // handle_goal_next_request method
    if action.goal_service.is_some() {
        let ght = goal_handler_type
            .as_ref()
            .ok_or_else(|| Error::InvariantViolation {
                context: String::from(
                    "goal handler type should exist when goal service is present",
                ),
            })?;

        builder.line(&format!(
            "async def handle_goal_next_request(self, handler: {ght}) -> None:"
        ));
        builder.indent();
        builder.line("async def _on_request(request_context):");
        builder.indent();
        builder.line("message = request_context.message");
        if has_feedback {
            builder.line(
                "publisher, goal_id, payload = await self.feedback_publisher_factory.declare_from_wire(message.payload)",
            );
            // Assign self.current_goal BEFORE awaiting the user handler so
            // emit_feedback() called from within an async handler sees the
            // active publisher (regression fix for the in-handler deadlock).
            builder.line("self.current_goal = (goal_id, publisher)");
        } else if has_goal_request {
            builder.line("payload = message.payload");
        }
        builder.line("core_node = message.core_node");
        builder.line("instance_id = message.instance_id");
        if has_feedback {
            builder.line("try:");
            builder.indent();
        }
        if has_goal_request {
            builder.line(
                "outcome = await _handle_goal_payload(payload, handler, core_node, instance_id)",
            );
        } else {
            builder.line("outcome = await _handle_goal_payload(handler, core_node, instance_id)");
        }
        if has_feedback {
            builder.dedent();
            builder.line("except BaseException:");
            builder.indent();
            builder.line("self.current_goal = None");
            builder.line("raise");
            builder.dedent();
        }
        builder.line("return outcome");
        builder.dedent();
        builder.line("await self.goal_service.handle_next_request(_on_request)");
        builder.dedent();
        builder.blank_line();

        // handle_cancel_next_request method
        let cancel_handler_type = "Callable[[CancelRequest], CancelResponse]";
        builder.line(&format!(
            "async def handle_cancel_next_request(self, handler: {cancel_handler_type}) -> None:"
        ));
        builder.indent();
        if has_feedback {
            builder.line("close_decision = [None]");
            // CancelResponse.accepted decides whether to publish end-of-stream.
            // Inspecting it requires running the user handler ourselves rather
            // than letting the helper serialize directly. The handler may be
            // sync or async; await + hasattr(__await__) covers both.
            builder.line("async def _wrapped_handler(request):");
            builder.indent();
            builder.line("try:");
            builder.indent();
            builder.line("response = handler(request)");
            builder.line("if hasattr(response, \"__await__\"):");
            builder.indent();
            builder.line("response = await response");
            builder.dedent();
            builder.dedent();
            builder.line("except Exception:");
            builder.indent();
            builder.line("close_decision[0] = True");
            builder.line("raise");
            builder.dedent();
            builder.line("close_decision[0] = bool(response.accepted)");
            builder.line("return response");
            builder.dedent();
        }
        builder.line("async def _on_request(request_context):");
        builder.indent();
        builder.line("message = request_context.message");
        builder.line("core_node = message.core_node");
        builder.line("instance_id = message.instance_id");
        if has_feedback {
            // Resolve the publisher when the cancel request actually arrives,
            // not when handle_cancel_next_request was called.
            builder.line(
                "publisher = self.current_goal[1] if self.current_goal is not None else None",
            );
            builder.line(
                "outcome = await _handle_cancel_payload(_wrapped_handler, core_node, instance_id)",
            );
            builder.line("if close_decision[0] is True and publisher is not None:");
            builder.indent();
            builder.line("try:");
            builder.indent();
            builder.line("await publisher.publish_end()");
            builder.dedent();
            builder.line("except Exception:");
            builder.indent();
            builder.line("pass");
            builder.dedent();
            builder.dedent();
            builder.line("return outcome");
        } else {
            builder.line("return await _handle_cancel_payload(handler, core_node, instance_id)");
        }
        builder.dedent();
        builder.line("await self.cancel_service.handle_next_request(_on_request)");
        if has_feedback {
            builder.line("if close_decision[0] is True:");
            builder.indent();
            builder.line("self.current_goal = None");
            builder.dedent();
        }
        builder.dedent();
        builder.blank_line();
    }

    // handle_result_next_request method
    if action.result_service.is_some() {
        let rht = result_handler_type
            .as_ref()
            .ok_or_else(|| Error::InvariantViolation {
                context: String::from(
                    "result handler type should exist when result service is present",
                ),
            })?;

        builder.line(&format!(
            "async def handle_result_next_request(self, handler: {rht}) -> None:"
        ));
        builder.indent();
        if has_feedback {
            // Publish end-of-stream BEFORE awaiting the next result request
            // so the client's drain loop can break out and actually send the
            // request. Matches the Rust codegen's `ActionHandleRole::Result`
            // setup block. Inverting this ordering deadlocks any client that
            // drains feedback before calling get_result.
            builder.line("if self.current_goal is not None:");
            builder.indent();
            builder.line("_, publisher = self.current_goal");
            builder.line("try:");
            builder.indent();
            builder.line("await publisher.publish_end()");
            builder.dedent();
            builder.line("except Exception:");
            builder.indent();
            builder.line("pass");
            builder.dedent();
            builder.dedent();
        }
        builder.line("async def _on_request(request_context):");
        builder.indent();
        builder.line("message = request_context.message");
        builder.line("core_node = message.core_node");
        builder.line("instance_id = message.instance_id");
        builder.line("return await _handle_result_payload(handler, core_node, instance_id)");
        builder.dedent();
        builder.line("await self.result_service.handle_next_request(_on_request)");
        if has_feedback {
            builder.line("self.current_goal = None");
        }
        builder.dedent();
        builder.blank_line();
    }

    // emit_feedback method
    if action.feedback_topic.is_some() {
        // ActionFeedbackPublisher.publish rejects empty payloads (reserved
        // for the publish_end sentinel), so a feedback_topic without any
        // message_format would emit code that fails at runtime on the first
        // call. Surface the misconfiguration at generation time instead.
        let (info, fmt) = match (feedback_schema_info, feedback_format) {
            (Some(info), Some(fmt)) => (info, fmt),
            _ => {
                return Err(Error::InvariantViolation {
                    context: format!(
                        "action `{}` declares feedback_topic but no non-empty message_format; \
                         emit_feedback would publish an empty payload, which is reserved for publish_end()",
                        action.name
                    ),
                });
            }
        };

        let mut param_parts = vec![String::from("self")];
        for field in &feedback_fields {
            param_parts.push(format!("{}: {}", field.name, field.type_str));
        }
        let params = param_parts.join(", ");

        builder.line(&format!("async def emit_feedback({params}):"));
        builder.indent();
        // Per-goal publisher; user must call handle_goal_next_request first.
        builder.line("if self.current_goal is None:");
        builder.indent();
        builder.line(
            "raise RuntimeError('emit_feedback called with no active goal; call handle_goal_next_request first')",
        );
        builder.dedent();
        builder.line("_, publisher = self.current_goal");

        let loader_fn_name = capnp_loader_fn_name(info);
        builder.line(&format!(
            "capnp_msg = {loader_fn_name}().{}.new_message()",
            info.struct_name
        ));
        let mut counter = 0u32;
        serialization::emit_capnp_assignments(&mut builder, "capnp_msg", fmt, "", &mut counter);
        builder.line("payload = capnp_msg.to_bytes()");

        builder.line("await publisher.publish(payload)");
        builder.dedent();
    }

    builder.dedent(); // end of class ActionHandle

    Ok(builder.build())
}

/// Emits the handler-call + response-serialization block that appears in
/// `_handle_goal_payload`, `_handle_cancel_payload`, and `_handle_result_payload`.
///
/// When `response_info` is `Some`, the handler result is serialized via Cap'n Proto;
/// otherwise the handler is called for its side-effect and `b""` is returned.
fn emit_handler_response_body(
    builder: &mut PythonCodeBuilder,
    response_info: Option<(&MessageFormat, &PythonSchemaInfo)>,
) {
    if let Some((resp_fmt, resp_info)) = response_info {
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
            builder,
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
}

// ---------------------------------------------------------------------------
// Subscribed actions
// ---------------------------------------------------------------------------

pub struct ConsumedActionSchemaInfo<'a> {
    pub goal_request: Option<&'a PythonSchemaInfo>,
    pub goal_response: Option<&'a PythonSchemaInfo>,
    pub cancel_response: Option<&'a PythonSchemaInfo>,
    pub feedback: Option<&'a PythonSchemaInfo>,
    pub result_response: Option<&'a PythonSchemaInfo>,
}

/// Generates Python code for a subscribed (client-side) action.
pub fn build_consumed_action(
    action: &ConsumedAction,
    messages: &ConsumedActionMessage,
    schema_info: ConsumedActionSchemaInfo<'_>,
    dependency_node_name: &str,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();

    let goal_request_format = non_empty_message_format(messages.goal_request.as_ref());
    let goal_response_format = non_empty_message_format(messages.goal_response.as_ref());
    let feedback_format = non_empty_message_format(messages.feedback.as_ref());
    let result_response_format = non_empty_message_format(messages.result_response.as_ref());

    // Capnp preamble and loader functions
    let any_schema = schema_info.goal_request.is_some()
        || schema_info.goal_response.is_some()
        || schema_info.cancel_response.is_some()
        || schema_info.feedback.is_some()
        || schema_info.result_response.is_some();

    if any_schema {
        emit_capnp_preamble(&mut builder);
    }
    for info in [
        schema_info.goal_request,
        schema_info.goal_response,
        schema_info.cancel_response,
        schema_info.feedback,
        schema_info.result_response,
    ]
    .into_iter()
    .flatten()
    {
        emit_capnp_loader_fn(&mut builder, info);
    }

    // Constants
    builder.line(&format!("TARGET_NODE_NAME = \"{}\"", dependency_node_name));
    builder.line(&format!("TARGET_ACTION_NAME = \"{}\"", action.name));
    builder.blank_line();

    // GoalRequest class
    if let Some(fmt) = goal_request_format {
        emit_format_as_dataclass(&mut builder, "GoalRequest", fmt)?;
    }

    if let Some(fmt) = goal_response_format {
        emit_format_as_dataclass(&mut builder, "GoalResponseData", fmt)?;
    }

    // CancelResponseData + CancelResponse
    let cancel_format = cancel_action_response_format();
    emit_format_as_dataclass(&mut builder, "CancelResponseData", &cancel_format)?;
    builder.dataclass(
        "CancelResponse",
        &[
            ("core_node", "str"),
            ("instance_id", "str"),
            ("data", "CancelResponseData"),
        ],
    );

    // ResultResponseData + ResultResponse
    if let Some(fmt) = result_response_format {
        emit_format_as_dataclass(&mut builder, "ResultResponseData", fmt)?;
        builder.dataclass(
            "ResultResponse",
            &[
                ("core_node", "str"),
                ("instance_id", "str"),
                ("data", "ResultResponseData"),
            ],
        );
    } else {
        builder.dataclass(
            "ResultResponse",
            &[("core_node", "str"), ("instance_id", "str")],
        );
    }

    // FeedbackMessage (only when feedback format exists)
    if let Some(fmt) = feedback_format {
        emit_format_as_dataclass(&mut builder, "FeedbackMessage", fmt)?;
    }

    // ---------------------------------------------------------------
    // Deserialization helper functions
    // ---------------------------------------------------------------

    // _deserialize_goal_response
    if let Some((fmt, info)) = goal_response_format.zip(schema_info.goal_response) {
        let loader_fn_name = capnp_loader_fn_name(info);
        deserialization::build_deserialize_fn(
            &mut builder,
            info,
            fmt,
            "GoalResponseData",
            &format!("{loader_fn_name}()"),
            "_deserialize_goal_response",
        );
    }

    // _deserialize_cancel_response
    if let Some(info) = schema_info.cancel_response {
        let loader_fn_name = capnp_loader_fn_name(info);
        deserialization::build_deserialize_fn(
            &mut builder,
            info,
            &cancel_format,
            "CancelResponseData",
            &format!("{loader_fn_name}()"),
            "_deserialize_cancel_response",
        );
    }

    // _deserialize_feedback_payload
    if let Some((fmt, info)) = feedback_format.zip(schema_info.feedback) {
        let loader_fn_name = capnp_loader_fn_name(info);
        deserialization::build_deserialize_fn(
            &mut builder,
            info,
            fmt,
            "FeedbackMessage",
            &format!("{loader_fn_name}()"),
            "_deserialize_feedback_payload",
        );
    }

    // _deserialize_result_response
    if let Some((fmt, info)) = result_response_format.zip(schema_info.result_response) {
        let loader_fn_name = capnp_loader_fn_name(info);
        deserialization::build_deserialize_fn(
            &mut builder,
            info,
            fmt,
            "ResultResponseData",
            &format!("{loader_fn_name}()"),
            "_deserialize_result_response",
        );
    }

    // ---------------------------------------------------------------
    // ActionHandle class
    // ---------------------------------------------------------------

    let has_goal_request = goal_request_format.is_some();
    let has_goal_response = goal_response_format.is_some();
    let has_result_response = result_response_format.is_some();

    builder.add_import("import peppylib");
    builder.add_import("from typing import Optional");
    builder.add_import("from typing import Self");
    builder.blank_line();

    builder.line("class ActionHandle:");
    builder.indent();

    // fire_goal @classmethod
    builder.line("@classmethod");
    if has_goal_request {
        builder.line("async def fire_goal(cls, node_runner: peppylib.NodeRunner, request: GoalRequest, timeout: float, feedback_qos: peppylib.QoSProfile, target_core_node: Optional[str] = None, target_instance_id: Optional[str] = None) -> Self:");
    } else {
        builder.line("async def fire_goal(cls, node_runner: peppylib.NodeRunner, timeout: float, feedback_qos: peppylib.QoSProfile, target_core_node: Optional[str] = None, target_instance_id: Optional[str] = None) -> Self:");
    }
    builder.indent();

    // Serialize request payload
    if let Some((fmt, info)) = goal_request_format.zip(schema_info.goal_request) {
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
        builder.line("user_goal_payload = capnp_msg.to_bytes()");
    } else {
        builder.line("user_goal_payload = b\"\"");
    }

    // Consumer-side discovery of the producer's interface namespace is the
    // follow-up PR; for now we use native (None).
    let (send_goal_iface_name_lit, send_goal_iface_tag_lit) = iface_segment_python_literals(None);
    builder.line("action_handle = await peppylib.ActionMessenger.send_goal(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_core_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line("TARGET_NODE_NAME,");
    builder.line(&format!("{send_goal_iface_name_lit},"));
    builder.line(&format!("{send_goal_iface_tag_lit},"));
    builder.line("TARGET_ACTION_NAME,");
    builder.line("target_core_node,");
    builder.line("target_instance_id,");
    builder.line("user_goal_payload,");
    builder.line("feedback_qos,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");

    // Construct ActionHandle instance
    builder.line("handle = cls()");
    builder.line("handle._messenger = node_runner.messenger()");
    builder.line("handle._inner = action_handle");
    if has_goal_response {
        builder.line("payload = action_handle.goal_response.payload");
        builder.line("goal_response_data = _deserialize_goal_response(payload)");
        builder.line("handle.data = goal_response_data");
    }
    builder.line("return handle");

    builder.dedent();
    builder.blank_line();

    // cancel_goal method
    builder.line("async def cancel_goal(self, timeout: float) -> CancelResponse:");
    builder.indent();
    builder.line("response = await peppylib.ActionMessenger.cancel_goal(");
    builder.indent();
    builder.line("self._messenger,");
    builder.line("self._inner,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");

    builder.line("payload = response.payload");
    builder.line("cancel_response_data = _deserialize_cancel_response(payload)");
    builder.line("return CancelResponse(core_node=response.core_node, instance_id=response.instance_id, data=cancel_response_data)");

    builder.dedent();
    builder.blank_line();

    // on_next_feedback_message (only when feedback format exists)
    if feedback_format.is_some() {
        builder.line("async def on_next_feedback_message(self) -> FeedbackMessage:");
        builder.indent();
        builder.line("feedback = await self._inner.on_next_feedback()");
        if schema_info.feedback.is_some() {
            builder.line("payload = feedback.payload");
            builder.line("return _deserialize_feedback_payload(payload)");
        } else {
            builder.line("return feedback");
        }
        builder.dedent();
        builder.blank_line();
    }

    // get_result method
    builder.line("async def get_result(self, timeout: float) -> ResultResponse:");
    builder.indent();
    builder.line("response = await peppylib.ActionMessenger.request_result(");
    builder.indent();
    builder.line("self._messenger,");
    builder.line("self._inner,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");

    if has_result_response {
        builder.line("payload = response.payload");
        builder.line("result_response_data = _deserialize_result_response(payload)");
        builder.line("return ResultResponse(core_node=response.core_node, instance_id=response.instance_id, data=result_response_data)");
    } else {
        builder.line(
            "return ResultResponse(core_node=response.core_node, instance_id=response.instance_id)",
        );
    }

    builder.dedent();

    builder.dedent(); // end of class ActionHandle

    Ok(builder.build())
}
