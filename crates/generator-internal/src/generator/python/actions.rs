use super::PythonSchemaInfo;
use super::code_builder::{PythonCodeBuilder, emit_format_as_dataclass, emit_nested_classes};
use super::deserialization;
use super::serialization;
use super::services::sender_target_python_expr;
use super::topics::{capnp_loader_fn_name, emit_capnp_loader_fn, emit_capnp_preamble};
use super::type_mapping::{collect_fields_from_format, uses_optional};
use crate::error::Result;
use crate::generator::types::{ConsumedActionMessage, ContractOrigin, non_empty_message_format};
use config::node::{ConsumedAction, NativeExposedAction};

// ---------------------------------------------------------------------------
// Exposed actions
// ---------------------------------------------------------------------------

/// Generates Python code for an exposed (handler) action.
///
/// Mirrors the Rust codegen: `ActionHandle` wraps `peppylib.ConcurrentAction`,
/// `handle_goal_next_request` receives a goal, runs the user decider, and
/// accepts/rejects it, returning a per-goal `GoalContext` on accept. All
/// routing/cancel/result/feedback behavior lives in the shared Rust engine.
pub fn build_exposed_action(
    action: &NativeExposedAction,
    goal_request_schema_info: Option<&PythonSchemaInfo>,
    goal_response_schema_info: Option<&PythonSchemaInfo>,
    result_response_schema_info: Option<&PythonSchemaInfo>,
    feedback_schema_info: Option<&PythonSchemaInfo>,
    origin: Option<&ContractOrigin>,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();

    let has_feedback = action.feedback_topic.is_some();
    let has_result = action.result_service.is_some();

    // Capnp preamble and loader functions.
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

    builder.line(&format!("ACTION_NAME = \"{}\"", action.name));
    builder.blank_line();

    let goal_request_format = action
        .goal_service
        .as_ref()
        .and_then(|goal| non_empty_message_format(goal.request_message_format.as_ref()));
    let goal_response_format = action
        .goal_service
        .as_ref()
        .and_then(|goal| non_empty_message_format(goal.response_message_format.as_ref()));
    let result_response_format = action
        .result_service
        .as_ref()
        .and_then(|result| non_empty_message_format(result.response_message_format.as_ref()));
    // A declared `feedback_topic` always carries a non-empty message_format
    // (enforced when the config is parsed), so the presence of the block is
    // the only thing deciding whether `publish_feedback` is generated.
    let feedback_format = action
        .feedback_topic
        .as_ref()
        .map(|topic| &topic.message_format);

    let has_goal_request = goal_request_format.is_some();
    let has_goal_response = goal_response_format.is_some();

    // ---------------------------------------------------------------
    // Dataclasses: GoalRequest[Data], GoalResponse, GoalDecision
    // ---------------------------------------------------------------

    if let Some(fmt) = goal_request_format {
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

    if let Some(fmt) = goal_response_format {
        emit_format_as_dataclass(&mut builder, "GoalResponse", fmt)?;
    }

    // GoalDecision controls framework admission, carried by the goal-ack
    // envelope independently of the declared response payload. An accept
    // carries the declared GoalResponse when one exists; a reject carries an
    // optional human-readable reason and an optional response.
    builder.block("class GoalDecision:", |builder| {
        if has_goal_response {
            builder.py(r#"
def __init__(self, accepted: bool, reason=None, response=None):
    self.accepted = accepted
    self.reason = reason
    self.response = response
@staticmethod
def accept(response):
    if response is None:
        raise ValueError("GoalDecision.accept requires the declared GoalResponse")
    return GoalDecision(True, response=response)
@staticmethod
def reject(reason=None, response=None):
    return GoalDecision(False, reason=reason, response=response)
"#);
        } else {
            builder.py(r#"
def __init__(self, accepted: bool, reason=None):
    self.accepted = accepted
    self.reason = reason
@staticmethod
def accept():
    return GoalDecision(True)
@staticmethod
def reject(reason=None):
    return GoalDecision(False, reason=reason)
"#);
        }
    });
    builder.blank_line();

    // ---------------------------------------------------------------
    // Helpers: request deserializer + nested classes for feedback/result
    // ---------------------------------------------------------------

    if let Some((fmt, info)) = goal_request_format.zip(goal_request_schema_info) {
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

    // Feedback/result fields become method parameters on GoalContext; emit any
    // nested dataclasses they reference at module scope.
    let mut feedback_fields = Vec::new();
    if let Some(fmt) = feedback_format {
        let mut nested = Vec::new();
        feedback_fields = collect_fields_from_format(fmt, "Feedback", &mut nested)?;
        if uses_optional(&feedback_fields, &nested) {
            builder.add_import("from typing import Optional");
        }
        emit_nested_classes(&mut builder, &nested);
    }
    let mut result_fields = Vec::new();
    if let Some(fmt) = result_response_format {
        let mut nested = Vec::new();
        result_fields = collect_fields_from_format(fmt, "Result", &mut nested)?;
        if uses_optional(&result_fields, &nested) {
            builder.add_import("from typing import Optional");
        }
        emit_nested_classes(&mut builder, &nested);
    }

    // ---------------------------------------------------------------
    // ActionHandle class
    // ---------------------------------------------------------------

    builder.add_import("import peppylib");
    builder.add_import("from typing import Self");
    builder.add_import("from typing import Callable");
    let expose_target_expr =
        sender_target_python_expr(origin, "node_runner.node_name()", "node_runner.node_tag()");
    let has_feedback_py = if has_feedback { "True" } else { "False" };
    builder.block("class ActionHandle:", |builder| {
        // expose
        builder.line("@classmethod");
        builder.block(
            "async def expose(cls, node_runner: peppylib.NodeRunner) -> Self:",
            |builder| {
                builder.call(
                    "inner = await peppylib.ConcurrentAction.expose(",
                    &[
                        "node_runner.messenger(),",
                        "node_runner.bound_core_node(),",
                        "node_runner.bound_instance_id(),",
                        &format!("{expose_target_expr},"),
                        "ACTION_NAME,",
                        &format!("{has_feedback_py},"),
                    ],
                    ")",
                );
                builder.py(r#"
handle = cls()
handle._inner = inner
return handle
"#);
            },
        );
        builder.blank_line();

        // handle_goal_next_request
        builder.block(
            "async def handle_goal_next_request(self, handler: Callable[[GoalRequest], GoalDecision]) -> \"GoalContext | None\":",
            |builder| {
                builder.block("while True:", |builder| {
                    builder.py(r#"
pending = await self._inner.recv_next_goal()
if pending is None:
    return None  # goal stream closed (node shutting down)
"#);
                    if has_goal_request {
                        builder.py(r#"
request_data = _deserialize_goal_request(pending.request_bytes)
request = GoalRequest(instance_id=pending.instance_id, core_node=pending.core_node, data=request_data)
"#);
                    } else {
                        builder.line(
                            "request = GoalRequest(instance_id=pending.instance_id, core_node=pending.core_node)",
                        );
                    }
                    builder.py(r#"
decision = handler(request)
if hasattr(decision, "__await__"):
    decision = await decision
"#);
                    // Serialize the declared GoalResponse when the decision
                    // carries one; a reject without a response replies with an
                    // empty body.
                    if let Some((fmt, info)) = goal_response_format.zip(goal_response_schema_info) {
                        builder.line("response = decision.response");
                        builder.block("if response is not None:", |builder| {
                            let loader_fn_name = capnp_loader_fn_name(info);
                            builder.line(&format!(
                                "capnp_msg = {loader_fn_name}().{}.new_message()",
                                info.struct_name
                            ));
                            let mut counter = 0u32;
                            serialization::emit_capnp_assignments(
                                builder,
                                "capnp_msg",
                                fmt,
                                "response",
                                &mut counter,
                            );
                            builder.line("response_bytes = capnp_msg.to_bytes()");
                        });
                        builder.block("else:", |builder| {
                            builder.line("response_bytes = b\"\"");
                        });
                    } else {
                        builder.line("response_bytes = b\"\"");
                    }
                    builder.py(r#"
if decision.accepted:
    ctx = await pending.accept(response_bytes)
    return GoalContext(ctx, request)
"#);
                    // Rejected: answer the client and keep polling for the next
                    // goal (the accept branch above returns, so this runs only
                    // when not accepted).
                    builder.line("await pending.reject(decision.reason, response_bytes)");
                });
            },
        );
    });
    builder.blank_line();

    // ---------------------------------------------------------------
    // GoalContext class
    // ---------------------------------------------------------------

    builder.block("class GoalContext:", |builder| {
        builder.py(r#"
def __init__(self, inner, request: GoalRequest):
    self._inner = inner
    self._request = request
"#);
        builder.blank_line();
        builder.py(r#"
def request(self) -> GoalRequest:
    return self._request
def goal_id(self) -> str:
    return self._inner.goal_id
async def cancel_signal(self) -> None:
    await self._inner.cancel_signal()
def is_cancelled(self) -> bool:
    return self._inner.is_cancelled()
"#);

        // publish_feedback
        if let Some((fmt, info)) = feedback_format.zip(feedback_schema_info) {
            let mut params = vec![String::from("self")];
            for field in &feedback_fields {
                params.push(format!("{}: {}", field.name, field.type_str));
            }
            builder.block(
                &format!("async def publish_feedback({}) -> None:", params.join(", ")),
                |builder| {
                    let loader_fn_name = capnp_loader_fn_name(info);
                    builder.line(&format!(
                        "capnp_msg = {loader_fn_name}().{}.new_message()",
                        info.struct_name
                    ));
                    let mut counter = 0u32;
                    serialization::emit_capnp_assignments(
                        builder,
                        "capnp_msg",
                        fmt,
                        "",
                        &mut counter,
                    );
                    builder.py(r#"
payload = capnp_msg.to_bytes()
await self._inner.publish_feedback(payload)
"#);
                },
            );
        }

        // complete / complete_cancelled
        if has_result {
            for method in ["complete", "complete_cancelled"] {
                let mut params = vec![String::from("self")];
                for field in &result_fields {
                    params.push(format!("{}: {}", field.name, field.type_str));
                }
                builder.block(
                    &format!("async def {method}({}) -> None:", params.join(", ")),
                    |builder| {
                        if let Some((fmt, info)) =
                            result_response_format.zip(result_response_schema_info)
                        {
                            let loader_fn_name = capnp_loader_fn_name(info);
                            builder.line(&format!(
                                "capnp_msg = {loader_fn_name}().{}.new_message()",
                                info.struct_name
                            ));
                            let mut counter = 0u32;
                            serialization::emit_capnp_assignments(
                                builder,
                                "capnp_msg",
                                fmt,
                                "",
                                &mut counter,
                            );
                            builder.line("payload = capnp_msg.to_bytes()");
                        } else {
                            builder.line("payload = b\"\"");
                        }
                        builder.line(&format!("await self._inner.{method}(payload)"));
                    },
                );
            }
        }
    });

    Ok(builder.build())
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
    builder.lines([
        format!("TARGET_NODE_NAME = \"{dependency_node_name}\""),
        format!("TARGET_ACTION_NAME = \"{}\"", action.name),
        format!("LINK_ID = \"{}\"", dependency.link_id),
    ]);
    builder.blank_line();

    // GoalRequest class
    if let Some(fmt) = goal_request_format {
        emit_format_as_dataclass(&mut builder, "GoalRequest", fmt)?;
    }

    if let Some(fmt) = goal_response_format {
        emit_format_as_dataclass(&mut builder, "GoalResponseData", fmt)?;
    }

    // CancelState enum + CancelResponse. The cancel reply is the framework
    // cancel-ack, decoded Rust-side (`cancel_goal` returns a typed reply); Python
    // just maps the typed `state` tag. No per-action cancel payload schema.
    builder.add_import("from enum import IntEnum");
    builder.py(r#"
class CancelState(IntEnum):
    SIGNALLED = 0
    ALREADY_TERMINAL = 1
    UNKNOWN = 2
"#);
    builder.blank_line();
    builder.dataclass(
        "CancelResponse",
        &[
            ("core_node", "str"),
            ("instance_id", "str"),
            ("state", "CancelState"),
        ],
    );

    // ResultStatus enum + ResultResponseData + ResultResponse. The result reply
    // is framed by the engine as a status tag + body, stripped Rust-side; Python
    // maps the typed status and decodes the body only for Completed/Cancelled.
    builder.py(r#"
class ResultStatus(IntEnum):
    COMPLETED = 0
    CANCELLED = 1
    ABANDONED = 2
    EXPIRED = 3
"#);
    builder.blank_line();
    if let Some(fmt) = result_response_format {
        emit_format_as_dataclass(&mut builder, "ResultResponseData", fmt)?;
        builder.add_import("from typing import Optional");
        builder.dataclass(
            "ResultResponse",
            &[
                ("core_node", "str"),
                ("instance_id", "str"),
                ("status", "ResultStatus"),
                ("data", "Optional[ResultResponseData]"),
            ],
        );
    } else {
        builder.dataclass(
            "ResultResponse",
            &[
                ("core_node", "str"),
                ("instance_id", "str"),
                ("status", "ResultStatus"),
            ],
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
    crate::generator::python::services::emit_bound_producer_accessor_fn(&mut builder, dependency);
    builder.blank_line();

    let send_goal_target_expr = sender_target_python_expr(
        dependency.origin.as_ref(),
        "TARGET_NODE_NAME",
        &format!("{:?}", dependency.producer_tag),
    );
    // A body-less goal takes no `request` parameter at all, so the parameter
    // list is spliced rather than the whole signature written twice.
    let request_param = if has_goal_request {
        "request: GoalRequest, "
    } else {
        ""
    };
    builder.block("class ActionHandle:", |builder| {
        // fire_goal @classmethod
        builder.line("@classmethod");
        builder.block(
            &format!(
                "async def fire_goal(cls, node_runner: peppylib.NodeRunner, target: peppylib.ProducerRef, {request_param}timeout: float, feedback_qos: peppylib.QoSProfile) -> Self:"
            ),
            |builder| {
                // A multi-paragraph docstring, so it opens and closes by hand
                // rather than through `docstring`.
                builder.py(r#"
"""Fires this goal at `target`, a member of the slot's bound set.

A target outside the set fails before anything reaches the wire. The
handle retains the target: feedback, result, and cancel stay pinned
to it.
"#);
                builder.lines(dependency.target_selection_doc());
                builder.line("\"\"\"");
                crate::generator::python::services::emit_target_membership_check(builder);

                // Serialize request payload
                if let Some((fmt, info)) = goal_request_format.zip(schema_info.goal_request) {
                    let loader_fn_name = capnp_loader_fn_name(info);
                    builder.line(&format!(
                        "capnp_msg = {loader_fn_name}().{}.new_message()",
                        info.struct_name
                    ));
                    let mut counter = 0u32;
                    serialization::emit_capnp_assignments(
                        builder,
                        "capnp_msg",
                        fmt,
                        "request",
                        &mut counter,
                    );
                    builder.line("user_goal_payload = capnp_msg.to_bytes()");
                } else {
                    builder.line("user_goal_payload = b\"\"");
                }

                builder.call(
                    "action_handle = await peppylib.ActionMessenger.send_goal(",
                    &[
                        "node_runner.messenger(),",
                        "node_runner.bound_core_node(),",
                        "node_runner.bound_instance_id(),",
                        &format!("{send_goal_target_expr},"),
                        "TARGET_ACTION_NAME,",
                        "target,",
                        "user_goal_payload,",
                        "feedback_qos,",
                        "timeout,",
                    ],
                    ")",
                );

                // Construct ActionHandle instance. Admission and the optional
                // rejection reason come from the framework goal-ack envelope,
                // decoded engine-side.
                builder.py(r#"
handle = cls()
handle._messenger = node_runner.messenger()
handle._inner = action_handle
handle.accepted = action_handle.accepted
handle.reason = action_handle.reason
"#);
                if has_goal_response {
                    // An empty body means no response was supplied (a declared
                    // response serializes to a non-empty capnp message), which
                    // only a reject can produce.
                    builder.py(r#"
body = action_handle.goal_reply_body
handle.data = _deserialize_goal_response(body) if body else None
"#);
                }
                builder.line("return handle");
            },
        );
        builder.blank_line();

        // cancel_goal method
        builder.block(
            "async def cancel_goal(self, timeout: float) -> CancelResponse:",
            |builder| {
                builder.call(
                    "reply = await peppylib.ActionMessenger.cancel_goal(",
                    &["self._messenger,", "self._inner,", "timeout,"],
                    ")",
                );
                builder.line("return CancelResponse(core_node=reply.core_node, instance_id=reply.instance_id, state=CancelState(reply.state))");
            },
        );
        builder.blank_line();

        // on_next_feedback_message (only when feedback format exists)
        if feedback_format.is_some() {
            builder.block(
                "async def on_next_feedback_message(self) -> FeedbackMessage:",
                |builder| {
                    builder.py(r#"
"""Receive the next feedback message for this goal.

Raises RuntimeError when the producer closed the stream cleanly
(end-of-stream sentinel) and ConnectionError when the producer
instance disappeared without closing it (process killed, crashed);
in the latter case get_result resolves to ResultStatus.ABANDONED.
"""
feedback = await self._inner.on_next_feedback()
"#);
                    if schema_info.feedback.is_some() {
                        builder.py(r#"
payload = feedback.payload
return _deserialize_feedback_payload(payload)
"#);
                    } else {
                        builder.line("return feedback");
                    }
                },
            );
            builder.blank_line();
        }

        // get_result method
        builder.block(
            "async def get_result(self, timeout: float) -> ResultResponse:",
            |builder| {
                builder.call(
                    "reply = await peppylib.ActionMessenger.request_result(",
                    &["self._messenger,", "self._inner,", "timeout,"],
                    ")",
                );
                builder.line("status = ResultStatus(reply.status)");
                if has_result_response {
                    builder.py(r#"
data = None
if status in (ResultStatus.COMPLETED, ResultStatus.CANCELLED):
    data = _deserialize_result_response(reply.body)
return ResultResponse(core_node=reply.core_node, instance_id=reply.instance_id, status=status, data=data)
"#);
                } else {
                    builder.line(
                        "return ResultResponse(core_node=reply.core_node, instance_id=reply.instance_id, status=status)",
                    );
                }
            },
        );
    });

    Ok(builder.build())
}
