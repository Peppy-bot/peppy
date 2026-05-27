use super::PythonSchemaInfo;
use super::code_builder::{PythonCodeBuilder, emit_format_as_dataclass, emit_nested_classes};
use super::deserialization;
use super::serialization;
use super::services::sender_target_python_expr;
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
///
/// The server wraps the concurrent `peppylib.actions.ActionServer`: each call
/// to `ActionHandle.handle_goal_next_request` accepts (or rejects) one goal and
/// returns a `GoalContext`. The context owns that goal's feedback stream
/// (`publish_feedback`), cancel signal (`cancel_signal` / `is_cancelled`), and
/// result delivery (`complete`), so a server can drive many goals concurrently
/// by moving each context into its own worker coroutine. Cancellation is an
/// SDK-driven signal with no user handler.
pub fn build_exposed_action(
    action: &ExposedAction,
    goal_request_schema_info: Option<&PythonSchemaInfo>,
    goal_response_schema_info: Option<&PythonSchemaInfo>,
    result_response_schema_info: Option<&PythonSchemaInfo>,
    feedback_schema_info: Option<&PythonSchemaInfo>,
    origin: Option<&InterfaceOrigin>,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();

    // Capnp preamble and loader functions
    let any_schema = goal_request_schema_info.is_some()
        || goal_response_schema_info.is_some()
        || result_response_schema_info.is_some()
        || feedback_schema_info.is_some();

    if any_schema {
        emit_capnp_preamble(&mut builder);
    }
    for info in [
        goal_request_schema_info,
        goal_response_schema_info,
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

    // An action with no goal service has no GoalContext-based server API.
    let Some(goal) = action.goal_service.as_ref() else {
        return Ok(builder.build());
    };

    // ---------------------------------------------------------------
    // Phase 1: Dataclass definitions (module level)
    // ---------------------------------------------------------------

    let has_goal_request = non_empty_message_format(goal.request_message_format.as_ref()).is_some();
    let has_goal_response =
        non_empty_message_format(goal.response_message_format.as_ref()).is_some();
    let result_response_format = action
        .result_service
        .as_ref()
        .and_then(|rs| non_empty_message_format(rs.response_message_format.as_ref()));
    let feedback_format = action
        .feedback_topic
        .as_ref()
        .and_then(|topic| non_empty_message_format(topic.message_format.as_ref()));

    // GoalRequest (+ GoalRequestData when the goal carries a body).
    if let Some(fmt) = non_empty_message_format(goal.request_message_format.as_ref()) {
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

    // GoalResponse only when the goal reply carries fields; otherwise the
    // decision closure returns `None` and the framework replies an empty body.
    if let Some(fmt) = non_empty_message_format(goal.response_message_format.as_ref()) {
        emit_format_as_dataclass(&mut builder, "GoalResponse", fmt)?;
    }
    let decide_return_type = if has_goal_response {
        "GoalResponse"
    } else {
        "None"
    };
    builder.add_import("from typing import Callable");

    // Nested dataclasses + Optional import for feedback / result field types,
    // emitted at module scope so the GoalContext method annotations resolve.
    let mut feedback_fields = Vec::new();
    if let Some(fmt) = feedback_format {
        let mut nested_classes = Vec::new();
        feedback_fields = collect_fields_from_format(fmt, "Feedback", &mut nested_classes)?;
        if uses_optional(&feedback_fields, &nested_classes) {
            builder.add_import("from typing import Optional");
        }
        emit_nested_classes(&mut builder, &nested_classes);
    }
    let mut result_fields = Vec::new();
    if let Some(fmt) = result_response_format {
        let mut nested_classes = Vec::new();
        result_fields = collect_fields_from_format(fmt, "Result", &mut nested_classes)?;
        if uses_optional(&result_fields, &nested_classes) {
            builder.add_import("from typing import Optional");
        }
        emit_nested_classes(&mut builder, &nested_classes);
    }

    // ---------------------------------------------------------------
    // Phase 2: Module-level helper functions
    // ---------------------------------------------------------------

    // _deserialize_goal_request helper
    if let Some((fmt, info)) =
        non_empty_message_format(goal.request_message_format.as_ref()).zip(goal_request_schema_info)
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
        builder.blank_line();
    }

    // ---------------------------------------------------------------
    // Phase 3: ActionHandle class
    // ---------------------------------------------------------------

    builder.add_import("import peppylib");
    // `Optional` is always needed for the `handle_goal_next_request` return
    // annotation; `Self` for `expose`.
    builder.add_import("from typing import Optional");
    builder.add_import("from typing import Self");
    builder.line("class ActionHandle:");
    builder.indent();

    // expose @classmethod
    builder.line("@classmethod");
    builder.line("async def expose(cls, node_runner: peppylib.NodeRunner) -> Self:");
    builder.indent();
    let expose_target_expr =
        sender_target_python_expr(origin, "node_runner.node_name()", "node_runner.node_tag()");
    builder.line("server = await peppylib.ActionMessenger.expose(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_core_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line(&format!("{expose_target_expr},"));
    builder.line("ACTION_NAME,");
    builder.dedent();
    builder.line(")");
    builder.line("handle = cls()");
    builder.line("handle._server = server");
    builder.line("return handle");
    builder.dedent();
    builder.blank_line();

    // handle_goal_next_request: accept-or-reject one goal, return its GoalContext.
    builder.line(&format!(
        "async def handle_goal_next_request(self, decide: Callable[[GoalRequest], {decide_return_type}]) -> Optional[\"GoalContext\"]:"
    ));
    builder.indent();
    builder.line("goal_request = await self._server.recv_next_goal()");
    builder.line("if goal_request is None:");
    builder.indent();
    builder.line("return None");
    builder.dedent();
    if has_goal_request {
        builder.line(
            "_goal_id, user_payload = peppylib.actions.unwrap_goal_payload(goal_request.payload)",
        );
        builder.line(
            "request = GoalRequest(instance_id=goal_request.instance_id, core_node=goal_request.core_node, data=_deserialize_goal_request(user_payload))",
        );
    } else {
        builder.line(
            "request = GoalRequest(instance_id=goal_request.instance_id, core_node=goal_request.core_node)",
        );
    }
    // Run the decision. Returning normally accepts; raising rejects (the
    // client's fire_goal then fails with the exception text).
    builder.line("try:");
    builder.indent();
    builder.line("response = decide(request)");
    builder.line("if hasattr(response, \"__await__\"):");
    builder.indent();
    builder.line("response = await response");
    builder.dedent();
    builder.dedent();
    builder.line("except BaseException as exc:");
    builder.indent();
    builder.line("await goal_request.reject(str(exc))");
    builder.line("return None");
    builder.dedent();
    // Serialize the goal response (empty when the goal reply carries no fields).
    if let Some((fmt, info)) = non_empty_message_format(goal.response_message_format.as_ref())
        .zip(goal_response_schema_info)
    {
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
            "response",
            &mut counter,
        );
        builder.line("response_payload = capnp_msg.to_bytes()");
    } else {
        builder.line("response_payload = b\"\"");
    }
    builder.line("inner = await goal_request.accept(response_payload)");
    builder.line("return GoalContext(inner, request)");
    builder.dedent();
    builder.dedent(); // end of class ActionHandle
    builder.blank_line();

    // ---------------------------------------------------------------
    // Phase 4: GoalContext class (per-goal handle)
    // ---------------------------------------------------------------

    build_goal_context_class(
        &mut builder,
        &feedback_fields,
        feedback_schema_info,
        feedback_format,
        &result_fields,
        result_response_schema_info,
        result_response_format,
        action.result_service.is_some(),
        &action.name,
    )?;

    Ok(builder.build())
}

/// Emits the per-goal `GoalContext` class: the accessors that are always
/// present plus `publish_feedback` (when the action has a feedback topic) and
/// `complete` (when it has a result service).
#[allow(clippy::too_many_arguments)]
fn build_goal_context_class(
    builder: &mut PythonCodeBuilder,
    feedback_fields: &[super::type_mapping::PythonField],
    feedback_schema_info: Option<&PythonSchemaInfo>,
    feedback_format: Option<&MessageFormat>,
    result_fields: &[super::type_mapping::PythonField],
    result_response_schema_info: Option<&PythonSchemaInfo>,
    result_response_format: Option<&MessageFormat>,
    has_result_service: bool,
    action_name: &str,
) -> Result<()> {
    builder.line("class GoalContext:");
    builder.indent();
    builder.line("def __init__(self, inner: peppylib.actions.GoalContext, request: GoalRequest):");
    builder.indent();
    builder.line("self._inner = inner");
    builder.line("self.request = request");
    builder.dedent();
    builder.blank_line();

    builder.line("@property");
    builder.line("def goal_id(self) -> str:");
    builder.indent();
    builder.line("return self._inner.goal_id");
    builder.dedent();
    builder.blank_line();

    builder.line("def is_cancelled(self) -> bool:");
    builder.indent();
    builder.line("return self._inner.is_cancelled()");
    builder.dedent();
    builder.blank_line();

    builder.line("async def cancel_signal(self) -> None:");
    builder.indent();
    builder.line("await self._inner.cancel_signal()");
    builder.dedent();

    // publish_feedback
    if feedback_format.is_some() {
        // ActionFeedbackPublisher.publish rejects empty payloads (reserved for
        // the end-of-stream sentinel), so a feedback_topic without any
        // message_format would emit code that fails at runtime on the first
        // call. Surface the misconfiguration at generation time instead.
        let (info, fmt) = match (feedback_schema_info, feedback_format) {
            (Some(info), Some(fmt)) => (info, fmt),
            _ => {
                return Err(Error::InvariantViolation {
                    context: format!(
                        "action `{action_name}` declares feedback_topic but no non-empty message_format; \
                         publish_feedback would publish an empty payload, which is reserved for the end-of-stream sentinel"
                    ),
                });
            }
        };
        builder.blank_line();
        let mut param_parts = vec![String::from("self")];
        for field in feedback_fields {
            param_parts.push(format!("{}: {}", field.name, field.type_str));
        }
        builder.line(&format!(
            "async def publish_feedback({}) -> None:",
            param_parts.join(", ")
        ));
        builder.indent();
        let loader_fn_name = capnp_loader_fn_name(info);
        builder.line(&format!(
            "capnp_msg = {loader_fn_name}().{}.new_message()",
            info.struct_name
        ));
        let mut counter = 0u32;
        serialization::emit_capnp_assignments(builder, "capnp_msg", fmt, "", &mut counter);
        builder.line("await self._inner.publish_feedback(capnp_msg.to_bytes())");
        builder.dedent();
    }

    // complete
    if has_result_service {
        builder.blank_line();
        match (result_response_schema_info, result_response_format) {
            (Some(info), Some(fmt)) => {
                let mut param_parts = vec![String::from("self")];
                for field in result_fields {
                    param_parts.push(format!("{}: {}", field.name, field.type_str));
                }
                builder.line(&format!(
                    "async def complete({}) -> None:",
                    param_parts.join(", ")
                ));
                builder.indent();
                let loader_fn_name = capnp_loader_fn_name(info);
                builder.line(&format!(
                    "capnp_msg = {loader_fn_name}().{}.new_message()",
                    info.struct_name
                ));
                let mut counter = 0u32;
                serialization::emit_capnp_assignments(builder, "capnp_msg", fmt, "", &mut counter);
                builder.line("await self._inner.complete(capnp_msg.to_bytes())");
                builder.dedent();
            }
            _ => {
                builder.line("async def complete(self) -> None:");
                builder.indent();
                builder.line("await self._inner.complete(b\"\")");
                builder.dedent();
            }
        }
    }

    builder.dedent(); // end of class GoalContext
    Ok(())
}

// ---------------------------------------------------------------------------
// Subscribed actions
// ---------------------------------------------------------------------------

pub struct ConsumedActionSchemaInfo<'a> {
    pub goal_request: Option<&'a PythonSchemaInfo>,
    pub goal_response: Option<&'a PythonSchemaInfo>,
    pub feedback: Option<&'a PythonSchemaInfo>,
    pub result_response: Option<&'a PythonSchemaInfo>,
}

/// Generates Python code for a subscribed (client-side) action.
pub fn build_consumed_action(
    action: &ConsumedAction,
    messages: &ConsumedActionMessage,
    schema_info: ConsumedActionSchemaInfo<'_>,
    dependency: &crate::generator::types::DependencyContext,
) -> Result<String> {
    let dependency_node_name = dependency.producer_name.as_str();
    let mut builder = PythonCodeBuilder::new();

    let goal_request_format = non_empty_message_format(messages.goal_request.as_ref());
    let goal_response_format = non_empty_message_format(messages.goal_response.as_ref());
    let feedback_format = non_empty_message_format(messages.feedback.as_ref());
    let result_response_format = non_empty_message_format(messages.result_response.as_ref());

    // Capnp preamble and loader functions
    let any_schema = schema_info.goal_request.is_some()
        || schema_info.goal_response.is_some()
        || schema_info.feedback.is_some()
        || schema_info.result_response.is_some();

    if any_schema {
        emit_capnp_preamble(&mut builder);
    }
    for info in [
        schema_info.goal_request,
        schema_info.goal_response,
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
    builder.add_import("from typing import Self");
    builder.blank_line();

    builder.line("class ActionHandle:");
    builder.indent();

    // fire_goal @classmethod
    builder.line("@classmethod");
    if has_goal_request {
        builder.line("async def fire_goal(cls, node_runner: peppylib.NodeRunner, request: GoalRequest, timeout: float, feedback_qos: peppylib.QoSProfile) -> Self:");
    } else {
        builder.line("async def fire_goal(cls, node_runner: peppylib.NodeRunner, timeout: float, feedback_qos: peppylib.QoSProfile) -> Self:");
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

    let send_goal_target_expr = sender_target_python_expr(
        dependency.origin.as_ref(),
        "TARGET_NODE_NAME",
        &format!("{:?}", dependency.producer_tag),
    );
    let send_goal_target_instance_id_expr =
        crate::generator::python::services::consumed_from_instance_id_python_expr(dependency);
    builder.line("action_handle = await peppylib.ActionMessenger.send_goal(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_core_node(),");
    builder.line("node_runner.bound_instance_id(),");
    builder.line(&format!("{send_goal_target_expr},"));
    builder.line("TARGET_ACTION_NAME,");
    builder.line("None,");
    builder.line(&format!("{send_goal_target_instance_id_expr},"));
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

    // cancel_goal method. The cancel acknowledgement is an SDK-owned one-byte
    // value (no user handler runs on the server), decoded via
    // `decode_cancel_ack`. `error_message` is always None because the SDK only
    // reports whether the goal was in flight.
    builder.line("async def cancel_goal(self, timeout: float) -> CancelResponse:");
    builder.indent();
    builder.line("response = await peppylib.ActionMessenger.cancel_goal(");
    builder.indent();
    builder.line("self._messenger,");
    builder.line("self._inner,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");

    builder.line("accepted = peppylib.actions.decode_cancel_ack(response.payload)");
    builder.line("return CancelResponse(core_node=response.core_node, instance_id=response.instance_id, data=CancelResponseData(accepted=accepted, error_message=None))");

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
