use super::PythonSchemaInfo;
use super::code_builder::{PythonCodeBuilder, emit_format_as_dataclass, emit_nested_classes};
use super::deserialization;
use super::failures::emit_failure_items;
use super::serialization;
use super::services::sender_target_python_expr;
use super::topics::emit_capnp_schema_loader;
use super::type_mapping::{PythonField, collect_fields_from_format, uses_optional};
use crate::error::Result;
use crate::generator::naming::cancel_reason_parameter;
use crate::generator::types::{
    ConsumedActionMessage, ContractOrigin, member_failure_kinds, non_empty_message_format,
};
use config::node::{ConsumedAction, NativeExposedAction};

// ---------------------------------------------------------------------------
// Exposed actions
// ---------------------------------------------------------------------------

/// Generates Python code for an exposed (handler) action.
///
/// Mirrors the Rust codegen: `ActionHandle` wraps `peppylib.ConcurrentAction`,
/// `handle_goal_next_request` receives a goal, runs the user decider, and
/// accepts/refuses it, returning a per-goal `GoalContext` on accept. All
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

    // Capnp loader functions.
    for info in [
        goal_request_schema_info,
        goal_response_schema_info,
        result_response_schema_info,
        feedback_schema_info,
    ]
    .into_iter()
    .flatten()
    {
        emit_capnp_schema_loader(&mut builder, info);
    }

    builder.line(&format!("ACTION_NAME = \"{}\"", action.name));
    builder.blank_line();
    emit_failure_items(&mut builder, &member_failure_kinds(origin), "ACTION_NAME");

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
    // carries the declared GoalResponse when one exists; a refusal carries a
    // failure of a kind the member declares, and no response. The
    // constructor takes no other pair, so a decision that the server cannot
    // send raises where it is built.
    builder.block("class GoalDecision:", |builder| {
        builder.docstring(
            "The decision of a goal handler: accept the goal, or refuse it with a failure.",
        );
        builder.blank_line();
        if has_goal_response {
            builder.py(r#"
def __init__(self, response: GoalResponse | None, refusal: Failure | None) -> None:
    """An accept with the declared `response`, or a refusal with no response.

    Raises ValueError for both or neither. The refusal is parsed again as a
    Failure of this member, which raises for a kind that this member does
    not declare.
    """
    if (response is None) == (refusal is None):
        raise ValueError("a goal decision is an accept with the declared GoalResponse, or a refusal with no response")
    self.response = response
    self.refusal = None if refusal is None else Failure(*refusal.args)
"#);
        } else {
            builder.py(r#"
def __init__(self, response: None, refusal: Failure | None) -> None:
    """An accept when `refusal` is None, else a refusal.

    Raises ValueError for a response, which this action does not declare.
    The refusal is parsed again as a Failure of this member, which raises
    for a kind that this member does not declare.
    """
    if response is not None:
        raise ValueError("this action declares no GoalResponse")
    self.response = None
    self.refusal = None if refusal is None else Failure(*refusal.args)
"#);
        }
        builder.blank_line();
        builder.line("@staticmethod");
        if has_goal_response {
            builder.py(r#"
def accept(response: GoalResponse) -> "GoalDecision":
    """Accepts the goal and replies with the declared response."""
    return GoalDecision(response, None)
"#);
        } else {
            builder.py(r#"
def accept() -> "GoalDecision":
    """Accepts the goal."""
    return GoalDecision(None, None)
"#);
        }
        builder.blank_line();
        builder.py(r#"
@staticmethod
def refuse(kind: FailureKind, message: str) -> "GoalDecision":
    """Refuses the goal with a failure kind that this member declares and a message.

    Raises ValueError for any other kind, and TypeError for a message that
    is not a str. A refusal carries no goal response.
    """
    return GoalDecision(None, Failure(kind, message))
"#);
    });
    builder.blank_line();

    // ---------------------------------------------------------------
    // Helpers: request deserializer + nested classes for feedback/result
    // ---------------------------------------------------------------

    if let Some((fmt, info)) = goal_request_format.zip(goal_request_schema_info) {
        deserialization::build_deserialize_fn(
            &mut builder,
            info,
            fmt,
            "GoalRequestData",
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

    // The answer of the handler to one goal. A handler shared between
    // members can give the GoalDecision of another member: building it
    // again as a decision of this member parses its refusal as a failure of
    // this member, so a kind that this member does not declare raises
    // ValueError before the wire.
    builder.block(
        "async def _goal_answer(handler: Callable[[GoalRequest], GoalDecision], request: GoalRequest) -> \"Failure | bytes\":",
        |builder| {
            builder.multiline_docstring(
                "The answer of `handler` to the goal `request`: a refusal, or the encoded response of an accept.",
                [
                    "The decision of `handler` is built again as a decision of this member,",
                    "so a decision that this member cannot send raises.",
                ],
            );
            builder.py(r#"
decision = handler(request)
if hasattr(decision, "__await__"):
    decision = await decision
decision = GoalDecision(decision.response, decision.refusal)
if decision.refusal is not None:
    return decision.refusal
"#);
            if let Some((fmt, info)) = goal_response_format.zip(goal_response_schema_info) {
                builder.line("response = decision.response");
                serialization::emit_new_capnp_message(builder, info, fmt, "response");
                builder.line("return capnp_msg.to_bytes()");
            } else {
                builder.line("return b\"\"");
            }
        },
    );
    builder.blank_line();

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

        builder.py(r#"
async def set_sender_grace(self, seconds: float) -> None:
    """Sets the sender grace of the action, in seconds.

    How long the action keeps the goals of a sender that is gone before they
    receive their cancel signal, with the cause "sender_gone". Zero by
    default. It applies to each sender found gone after the call.
    """
    await self._inner.set_sender_grace(seconds)
"#);
        builder.blank_line();

        // handle_goal_next_request
        builder.block(
            "async def handle_goal_next_request(self, handler: Callable[[GoalRequest], GoalDecision]) -> \"GoalContext | None\":",
            |builder| {
                let mut doc = vec![
                    "Gives the context of that goal, or None when the goal stream closes.",
                    "Each refused goal is answered and skipped.",
                ];
                if has_goal_request {
                    doc.push("A goal request that does not decode is refused with the kind `rejected`.");
                }
                doc.extend([
                    "A handler that raises, or that gives a decision that this action cannot",
                    "send, refuses the goal with the kind `attempt_failed` and the text of the",
                    "error.",
                ]);
                builder.multiline_docstring(
                    "Answers the goals of this action with `handler` up to the next goal that it accepts.",
                    doc,
                );
                builder.block("while True:", |builder| {
                    builder.py(r#"
pending = await self._inner.recv_next_goal()
if pending is None:
    return None  # goal stream closed (node shutting down)
"#);
                    if has_goal_request {
                        // A request that does not fit this server's declared
                        // shape is refused with the kind `rejected` and the
                        // decode error, and skipped; the loop keeps polling.
                        builder.py(r#"
try:
    request_data = _deserialize_goal_request(pending.request_bytes)
except (ValueError, capnp.KjException) as error:
    await pending.refuse(FailureKind.REJECTED.value, f"goal request does not decode: {error}")
    continue
request = GoalRequest(instance_id=pending.instance_id, core_node=pending.core_node, data=request_data)
"#);
                    } else {
                        builder.line(
                            "request = GoalRequest(instance_id=pending.instance_id, core_node=pending.core_node)",
                        );
                    }
                    // A refusal answers the client, and the loop polls for
                    // the next goal: a refusal never ends the accept loop. A
                    // handler that raises, or that gives a decision that
                    // this member cannot send, refuses the goal with
                    // `attempt_failed` and the error, as a service handler
                    // that raises gives an error reply of that kind.
                    builder.py(r#"
try:
    answer = await _goal_answer(handler, request)
except Exception as error:
    answer = Failure(FailureKind.ATTEMPT_FAILED, f"the goal handler raised {type(error).__name__}: {error}")
if isinstance(answer, Failure):
    await pending.refuse(answer.kind.value, answer.message)
    continue
ctx = await pending.accept(answer)
return GoalContext(ctx, request)
"#);
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
        builder.add_import("from typing import Literal");
        builder.py(r#"
def request(self) -> GoalRequest:
    return self._request
def goal_id(self) -> str:
    return self._inner.goal_id
async def cancel_signal(self) -> None:
    """Waits for the cancel signal of this goal.

    A cancel request of its caller, or the end of its sender, sends it (see
    cancel_cause). React by calling complete_cancelled.
    """
    await self._inner.cancel_signal()
def is_cancelled(self) -> bool:
    return self._inner.is_cancelled()
def cancel_cause(self) -> Literal["caller", "sender_gone"] | None:
    """Why this goal received its cancel signal; None before it did."""
    return self._inner.cancel_cause
def cancel_reason(self) -> str | None:
    """The reason that the cause of the cancel signal gives, for complete_cancelled.

    "cancelled by its caller" or "its sender is gone"; None before the goal
    received its cancel signal.
    """
    return self._inner.cancel_reason
async def fail(self, kind: FailureKind, message: str) -> None:
    """Ends the goal with the status failed, with no result.

    A failure kind that this member declares and a message: raises
    ValueError for any other kind, and TypeError for a message that is not
    a str. The first of complete, fail and complete_cancelled ends the goal;
    later calls do nothing.
    """
    failure = Failure(kind, message)
    await self._inner.fail(failure.kind.value, failure.message)
"#);

        // publish_feedback
        if let Some((fmt, info)) = feedback_format.zip(feedback_schema_info) {
            builder.block(
                &format!(
                    "async def publish_feedback({}) -> None:",
                    method_params(&[], &feedback_fields)
                ),
                |builder| {
                    serialization::emit_new_capnp_message(builder, info, fmt, "");
                    builder.py(r#"
payload = capnp_msg.to_bytes()
await self._inner.publish_feedback(payload)
"#);
                },
            );
        }

        // complete / complete_cancelled. An action that declares no result
        // ends its goals with an empty one.
        let reason =
            cancel_reason_parameter(|name| result_fields.iter().any(|field| field.name == name));
        let reason_param = format!("{reason}: str");
        for (signature, doc, delivery) in [
            (
                format!(
                    "async def complete({}) -> None:",
                    method_params(&[], &result_fields)
                ),
                "Ends the goal with the status completed and its result: a success.".to_string(),
                "await self._inner.complete(payload)".to_string(),
            ),
            (
                format!(
                    "async def complete_cancelled({}) -> None:",
                    method_params(&[&reason_param], &result_fields)
                ),
                format!(
                    "Ends the goal with the status cancelled, for `{reason}`, with its result."
                ),
                format!("await self._inner.complete_cancelled({reason}, payload)"),
            ),
        ] {
            builder.block(&signature, |builder| {
                builder.docstring(&doc);
                if let Some((fmt, info)) = result_response_format.zip(result_response_schema_info) {
                    serialization::emit_new_capnp_message(builder, info, fmt, "");
                    builder.line("payload = capnp_msg.to_bytes()");
                } else {
                    builder.line("payload = b\"\"");
                }
                builder.line(&delivery);
            });
        }
    });

    Ok(builder.build())
}

/// The parameter list of a `GoalContext` method that sends `fields`: `self`,
/// the `leading` parameters, then one annotated parameter per field.
fn method_params(leading: &[&str], fields: &[PythonField]) -> String {
    let mut params = vec![String::from("self")];
    params.extend(leading.iter().map(|param| param.to_string()));
    params.extend(
        fields
            .iter()
            .map(|field| format!("{}: {}", field.name, field.type_str)),
    );
    params.join(", ")
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

    // Capnp loader functions
    for info in [
        schema_info.goal_request,
        schema_info.goal_response,
        schema_info.feedback,
        schema_info.result_response,
    ]
    .into_iter()
    .flatten()
    {
        emit_capnp_schema_loader(&mut builder, info);
    }

    // Constants
    builder.lines([
        format!("TARGET_NODE_NAME = \"{dependency_node_name}\""),
        format!("TARGET_ACTION_NAME = \"{}\"", action.name),
        format!("LINK_ID = \"{}\"", dependency.link_id),
    ]);
    builder.blank_line();
    emit_failure_items(
        &mut builder,
        &dependency.member_failure_kinds(),
        "TARGET_ACTION_NAME",
    );

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
    // is framed by the engine as a status tag + body, decoded Rust-side; Python
    // maps the typed status, decodes the body only for Completed/Cancelled, and
    // parses the failure of a failed goal.
    builder.py(r#"
class ResultStatus(IntEnum):
    COMPLETED = 0
    CANCELLED = 1
    ABANDONED = 2
    EXPIRED = 3
    FAILED = 4
"#);
    builder.blank_line();
    builder.add_import("from typing import Optional");
    let mut result_fields = vec![
        ("core_node", "str"),
        ("instance_id", "str"),
        ("status", "ResultStatus"),
    ];
    if let Some(fmt) = result_response_format {
        emit_format_as_dataclass(&mut builder, "ResultResponseData", fmt)?;
        result_fields.push(("data", "Optional[ResultResponseData]"));
    }
    result_fields.extend([
        ("failure", "Optional[Failure]"),
        ("cancel_reason", "Optional[str]"),
    ]);
    builder.dataclass("ResultResponse", &result_fields);

    // FeedbackMessage (only when feedback format exists)
    if let Some(fmt) = feedback_format {
        emit_format_as_dataclass(&mut builder, "FeedbackMessage", fmt)?;
    }

    // ---------------------------------------------------------------
    // Deserialization helper functions
    // ---------------------------------------------------------------

    // _deserialize_goal_response
    if let Some((fmt, info)) = goal_response_format.zip(schema_info.goal_response) {
        deserialization::build_deserialize_fn(
            &mut builder,
            info,
            fmt,
            "GoalResponseData",
            "_deserialize_goal_response",
        );
    }

    // _deserialize_feedback_payload
    if let Some((fmt, info)) = feedback_format.zip(schema_info.feedback) {
        deserialization::build_deserialize_fn(
            &mut builder,
            info,
            fmt,
            "FeedbackMessage",
            "_deserialize_feedback_payload",
        );
    }

    // _deserialize_result_response
    if let Some((fmt, info)) = result_response_format.zip(schema_info.result_response) {
        deserialization::build_deserialize_fn(
            &mut builder,
            info,
            fmt,
            "ResultResponseData",
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
                "async def fire_goal(cls, node_runner: peppylib.NodeRunner, target: peppylib.ProducerRef, {request_param}timeout: float, feedback_bound: Optional[int] = None) -> Self:"
            ),
            |builder| {
                builder.multiline_docstring(
                    "Fires this goal at `target`, a member of the slot's bound set.",
                    [
                        "A target outside the set fails before anything reaches the wire. The",
                        "handle retains the target: feedback, result, and cancel stay pinned",
                        "to it. The handle keeps every feedback message until it is read, up to",
                        "`feedback_bound` unread messages (None takes the default of peppylib,",
                        "1024); a caller that falls behind by more cancels the goal and reads",
                        "peppylib.FeedbackOverflowError. `refusal` holds the failure of a",
                        "refused goal; a kind that this member does not declare raises",
                        "ValueError.",
                    ]
                    .into_iter()
                    .chain(dependency.target_selection_doc().iter().copied()),
                );
                crate::generator::python::services::emit_target_membership_check(builder);

                // Serialize request payload
                if let Some((fmt, info)) = goal_request_format.zip(schema_info.goal_request) {
                    serialization::emit_new_capnp_message(builder, info, fmt, "request");
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
                        "feedback_bound,",
                        "timeout,",
                    ],
                    ")",
                );

                // Construct ActionHandle instance. Admission and the failure
                // of a refusal come from the framework goal-ack envelope,
                // decoded engine-side; the failure is parsed into this
                // member's `Failure`.
                builder.py(r#"
handle = cls()
handle._messenger = node_runner.messenger()
handle._inner = action_handle
handle.accepted = action_handle.accepted
handle.refusal = None if action_handle.accepted else Failure(action_handle.refusal_kind, action_handle.refusal_message)
"#);
                if has_goal_response {
                    // An accepted goal carries the declared response; a refused
                    // one carries none.
                    builder.py(r#"
handle.data = _deserialize_goal_response(action_handle.goal_reply_body) if action_handle.accepted else None
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
(end-of-stream sentinel), ConnectionError when the producer
instance disappeared without closing it (process killed, crashed),
in which case get_result resolves to ResultStatus.ABANDONED, and
peppylib.FeedbackOverflowError when this caller fell behind by more
unread messages than its bound, in which case the handle cancelled
the goal.
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
                builder.py(r#"
status = ResultStatus(reply.status)
failure = None
if status is ResultStatus.FAILED:
    failure = Failure(reply.failure_kind, reply.failure_message)
"#);
                if has_result_response {
                    builder.py(r#"
data = None
if status in (ResultStatus.COMPLETED, ResultStatus.CANCELLED):
    data = _deserialize_result_response(reply.body)
return ResultResponse(core_node=reply.core_node, instance_id=reply.instance_id, status=status, data=data, failure=failure, cancel_reason=reply.cancel_reason)
"#);
                } else {
                    builder.line(
                        "return ResultResponse(core_node=reply.core_node, instance_id=reply.instance_id, status=status, failure=failure, cancel_reason=reply.cancel_reason)",
                    );
                }
            },
        );
    });

    Ok(builder.build())
}
