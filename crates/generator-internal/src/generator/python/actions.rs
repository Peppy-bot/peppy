use super::code_builder::PythonCodeBuilder;
use super::type_mapping::{NestedDataclass, collect_fields_from_format, uses_optional};
use crate::generator::types::{
    SubscribedActionMessage, cancel_action_response_format, non_empty_message_format,
};
use config::node::{ExposedAction, MessageFormat, SubscribedAction};

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

fn emit_format_as_dataclass(
    builder: &mut PythonCodeBuilder,
    class_name: &str,
    format: &MessageFormat,
) {
    let mut nested_classes = Vec::new();
    let fields = collect_fields_from_format(format, class_name, &mut nested_classes);
    if uses_optional(&fields, &nested_classes) {
        builder.add_import("from typing import Optional");
    }
    emit_nested_classes(builder, &nested_classes);
    let field_refs: Vec<(&str, &str)> = fields
        .iter()
        .map(|f| (f.name.as_str(), f.type_str.as_str()))
        .collect();
    builder.dataclass(class_name, &field_refs);
}

fn emit_format_as_class(builder: &mut PythonCodeBuilder, class_name: &str, format: &MessageFormat) {
    let mut nested_classes = Vec::new();
    let fields = collect_fields_from_format(format, class_name, &mut nested_classes);
    if uses_optional(&fields, &nested_classes) {
        builder.add_import("from typing import Optional");
    }
    emit_nested_classes(builder, &nested_classes);
    let field_refs: Vec<(&str, &str)> = fields
        .iter()
        .map(|f| (f.name.as_str(), f.type_str.as_str()))
        .collect();
    builder.class_def(class_name, &field_refs);
}

// ---------------------------------------------------------------------------
// Exposed actions
// ---------------------------------------------------------------------------

/// Generates Python code for an exposed (handler) action.
pub fn build_exposed_action(action: &ExposedAction) -> String {
    let mut builder = PythonCodeBuilder::new();

    // ACTION_NAME constant
    builder.line(&format!("ACTION_NAME = \"{}\"", action.name));
    builder.blank_line();

    // Goal service classes
    if let Some(goal) = &action.goal_service {
        let request_format = non_empty_message_format(goal.request_message_format.as_ref());
        let _response_format = non_empty_message_format(goal.response_message_format.as_ref());

        // GoalRequestData (only when request body exists)
        if let Some(fmt) = request_format {
            emit_format_as_dataclass(&mut builder, "GoalRequestData", fmt);
            builder.class_def(
                "GoalRequest",
                &[
                    ("instance_id", "str"),
                    ("master_node", "str"),
                    ("data", "GoalRequestData"),
                ],
            );
        } else {
            builder.class_def(
                "GoalRequest",
                &[("instance_id", "str"), ("master_node", "str")],
            );
        }

        // GoalResponse
        if let Some(fmt) = _response_format {
            emit_format_as_class(&mut builder, "GoalResponse", fmt);
        } else {
            builder.class_def("GoalResponse", &[]);
        }

        // Cancel classes (always present when goal exists)
        builder.class_def(
            "CancelRequest",
            &[("instance_id", "str"), ("master_node", "str")],
        );

        let cancel_format = cancel_action_response_format();
        emit_format_as_class(&mut builder, "CancelResponse", &cancel_format);
    }

    // Result service classes
    if let Some(result) = &action.result_service {
        builder.class_def(
            "ResultRequest",
            &[("instance_id", "str"), ("master_node", "str")],
        );

        let response_format = non_empty_message_format(result.response_message_format.as_ref());
        if let Some(fmt) = response_format {
            emit_format_as_class(&mut builder, "ResultResponse", fmt);
        } else {
            builder.class_def("ResultResponse", &[]);
        }
    }

    // ActionHandle class
    builder.class_def("ActionHandle", &[]);

    // expose method
    builder.add_import("import peppylib");
    builder.line("async def expose(node_runner: peppylib.NodeRunner):");
    builder.indent();
    builder.line("action = await peppylib.ActionMessenger.expose(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_master_node,");
    builder.line("node_runner.bound_instance_id,");
    builder.line("node_runner.node_name,");
    builder.line("ACTION_NAME,");
    builder.dedent();
    builder.line(")");
    builder.line("return ActionHandle()");
    builder.dedent();
    builder.blank_line();

    // Handler methods
    if action.goal_service.is_some() {
        builder.line("async def handle_goal_next_request(action_handle, handler):");
        builder.indent();
        builder.line("pass");
        builder.dedent();
        builder.blank_line();

        builder.line("async def handle_cancel_next_request(action_handle, handler):");
        builder.indent();
        builder.line("pass");
        builder.dedent();
        builder.blank_line();
    }

    if action.result_service.is_some() {
        builder.line("async def handle_result_next_request(action_handle, handler):");
        builder.indent();
        builder.line("pass");
        builder.dedent();
        builder.blank_line();
    }

    // Feedback emit
    if let Some(feedback) = &action.feedback_topic {
        let feedback_format = non_empty_message_format(feedback.message_format.as_ref());
        let mut param_parts = vec![String::from("action_handle")];
        if let Some(fmt) = feedback_format {
            let mut nested_classes = Vec::new();
            let fields = collect_fields_from_format(fmt, "Feedback", &mut nested_classes);
            for field in &fields {
                param_parts.push(format!("{}: {}", field.name, field.type_str));
            }
        }
        let params = param_parts.join(", ");
        builder.line(&format!("async def emit_feedback({params}):"));
        builder.indent();
        builder.line("pass");
        builder.dedent();
    }

    builder.build()
}

// ---------------------------------------------------------------------------
// Subscribed actions
// ---------------------------------------------------------------------------

/// Generates Python code for a subscribed (client-side) action.
pub fn build_subscribed_action(
    action: &SubscribedAction,
    messages: &SubscribedActionMessage,
) -> String {
    let mut builder = PythonCodeBuilder::new();

    let goal_request_format = non_empty_message_format(messages.goal_request.as_ref());
    let goal_response_format = non_empty_message_format(messages.goal_response.as_ref());
    let feedback_format = non_empty_message_format(messages.feedback.as_ref());
    let result_response_format = non_empty_message_format(messages.result_response.as_ref());

    // Constants
    builder.line(&format!("TARGET_NODE_NAME = \"{}\"", action.node));
    builder.line(&format!("TARGET_ACTION_NAME = \"{}\"", action.name));
    builder.blank_line();

    // GoalRequest class
    if let Some(fmt) = goal_request_format {
        emit_format_as_class(&mut builder, "GoalRequest", fmt);
    }

    // GoalResponseData + GoalResponse
    if let Some(fmt) = goal_response_format {
        emit_format_as_dataclass(&mut builder, "GoalResponseData", fmt);
        builder.class_def("GoalResponse", &[("data", "GoalResponseData")]);
    } else {
        builder.class_def("GoalResponse", &[]);
    }

    // CancelResponseData + CancelResponse
    let cancel_format = cancel_action_response_format();
    emit_format_as_dataclass(&mut builder, "CancelResponseData", &cancel_format);
    builder.class_def(
        "CancelResponse",
        &[
            ("master_node", "str"),
            ("instance_id", "str"),
            ("data", "CancelResponseData"),
        ],
    );

    // ResultResponseData + ResultResponse
    if let Some(fmt) = result_response_format {
        emit_format_as_dataclass(&mut builder, "ResultResponseData", fmt);
        builder.class_def(
            "ResultResponse",
            &[
                ("master_node", "str"),
                ("instance_id", "str"),
                ("data", "ResultResponseData"),
            ],
        );
    } else {
        builder.class_def(
            "ResultResponse",
            &[("master_node", "str"), ("instance_id", "str")],
        );
    }

    // FeedbackMessage (only when feedback format exists)
    if let Some(fmt) = feedback_format {
        emit_format_as_class(&mut builder, "FeedbackMessage", fmt);
    }

    // fire_goal method
    builder.add_import("import peppylib");
    builder.line("async def fire_goal(node_runner: peppylib.NodeRunner, timeout, target_master_node=None, target_instance_id=None):");
    builder.indent();
    builder.line("goal_payload = b\"\"");
    builder.line("action_handle = await peppylib.ActionMessenger.send_goal(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("node_runner.bound_master_node,");
    builder.line("node_runner.bound_instance_id,");
    builder.line("TARGET_NODE_NAME,");
    builder.line("TARGET_ACTION_NAME,");
    builder.line("target_master_node,");
    builder.line("target_instance_id,");
    builder.line("goal_payload,");
    builder.line("peppylib.QoSProfile.Standard,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");
    builder.line("return action_handle");
    builder.dedent();
    builder.blank_line();

    // cancel_goal method
    builder
        .line("async def cancel_goal(node_runner: peppylib.NodeRunner, action_handle, timeout):");
    builder.indent();
    builder.line("response = await peppylib.ActionMessenger.cancel_goal(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("action_handle,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");
    builder.line("return response");
    builder.dedent();
    builder.blank_line();

    // on_next_feedback_message (only when feedback format exists)
    if feedback_format.is_some() {
        builder.line("async def on_next_feedback_message(action_handle):");
        builder.indent();
        builder.line("feedback = await action_handle.on_next_feedback()");
        builder.line("return feedback");
        builder.dedent();
        builder.blank_line();
    }

    // get_result method
    builder.line("async def get_result(node_runner: peppylib.NodeRunner, action_handle, timeout):");
    builder.indent();
    builder.line("response = await peppylib.ActionMessenger.request_result(");
    builder.indent();
    builder.line("node_runner.messenger(),");
    builder.line("action_handle,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");
    builder.line("return response");
    builder.dedent();

    builder.build()
}
