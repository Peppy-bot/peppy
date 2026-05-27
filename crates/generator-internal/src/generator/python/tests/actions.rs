use super::*;
use config::node::{ConsumedAction, ExposedAction, MessageFormat};
use std::collections::HashMap;

// --- Exposes examples
const EXPOSED_ACTION_EXAMPLE: &str = r#"
{
  name: "move_arm",
  goal_service: {
    request_message_format: {
      arm_id: "u16",
      desired_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    },
    response_message_format: {
      accepted: "bool"
    }
  },
  feedback_topic: {
    qos_profile: "sensor_data",
    message_format: {
      new_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    }
  },
  result_service: {
    response_message_format: {
      success: "bool",
      error_msg: {
        $type: "string",
        $optional: true
      },
      final_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    }
  }
}
"#;

const EXPOSED_ACTION_EXAMPLE2: &str = r#"
{
  name: "rotate_servo_clockwise",
  goal_service: {
    response_message_format: {
      accepted: "bool"
    }
  },
  feedback_topic: {
    qos_profile: "sensor_data",
    message_format: {
      new_position: "i32",
      speed: "i32"
    }
  },
  result_service: {
    response_message_format: {
      success: "bool",
      error_msg: {
        $type: "string",
        $optional: true
      },
    }
  }
}
"#;

const EXPOSED_ACTION_WITH_NESTED_FEEDBACK_EXAMPLE: &str = r#"
{
  name: "move_gripper",
  goal_service: {
    response_message_format: {
      accepted: "bool"
    }
  },
  feedback_topic: {
    qos_profile: "sensor_data",
    message_format: {
      state: {
        $type: "object",
        position: "i32",
        note: "string"
      }
    }
  }
}
"#;

// --- Subscribes examples
pub(super) const SUBSCRIBED_ACTION_EXAMPLE1: &str = r#"
{
  link_id: "brain",
  name: "move_arm",
}
"#;

pub(super) const SUBSCRIBED_ACTION_GOAL_FORMAT1: &str = r#"
{
  arm_id: "u16",
  desired_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

pub(super) const SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1: &str = r#"
{
  accepted: "bool"
}
"#;

pub(super) const SUBSCRIBED_ACTION_FEEDBACK_FORMAT1: &str = r#"
{
  new_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

pub(super) const SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1: &str = r#"
{
  success: "bool",
  error_msg: {
    $type: "string",
    $optional: true
  },
  final_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT2: &str = r#"
{
  accepted: "bool"
}
"#;

const SUBSCRIBED_ACTION_FEEDBACK_FORMAT2: &str = r#"
{
  new_position: "i32",
  speed: "i32"
}
"#;

const SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT2: &str = r#"
{
  success: "bool",
  error_msg: {
    $type: "string",
    $optional: true
  },
}
"#;

#[test]
fn exposed_action() {
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_action(&action, None).unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Capnp preamble imports
    assert_contains_all(
        &rendered,
        &[
            "import capnp",
            "import types",
            "from functools import lru_cache",
            "from importlib.resources import files",
        ],
    );

    // ACTION_NAME constant and GoalRequestData dataclass
    assert_contains_all(
        &rendered,
        &[
            "\"move_arm\"",
            "@dataclass",
            "class GoalRequestData:",
            "arm_id: int",
            "desired_position: list[int]",
        ],
    );

    // GoalRequest dataclass
    assert_contains_all(
        &rendered,
        &[
            "class GoalRequest:",
            "instance_id: str",
            "core_node: str",
            "data: GoalRequestData",
        ],
    );

    // GoalResponse dataclass
    assert_contains_all(&rendered, &["class GoalResponse:"]);

    // Cancel is an SDK-driven signal and the result is delivered via
    // GoalContext.complete, so no server-side cancel/result request/response
    // dataclasses are emitted.
    assert_rendered!(
        !rendered.contains("class CancelRequest:")
            && !rendered.contains("class CancelResponse:")
            && !rendered.contains("class ResultRequest:")
            && !rendered.contains("class ResultResponse:"),
        &rendered,
        "exposed action server should not emit cancel/result request/response dataclasses"
    );

    // ActionHandle class
    assert_contains_all(&rendered, &["class ActionHandle:"]);

    // Imports
    assert_contains_all(
        &rendered,
        &[
            "import peppylib",
            "from dataclasses import dataclass",
            "from typing import Callable",
            "from typing import Optional",
            "from typing import Self",
        ],
    );

    // expose @classmethod stores the ActionServer
    assert_contains_all(
        &rendered,
        &[
            "@classmethod",
            "async def expose(cls, node_runner: peppylib.NodeRunner) -> Self:",
            "peppylib.ActionMessenger.expose(",
            "handle = cls()",
            "handle._server = server",
            "return handle",
        ],
    );

    // _deserialize_goal_request function (module level)
    assert_contains_all(
        &rendered,
        &[
            "def _deserialize_goal_request(payload: bytes) -> GoalRequestData:",
            "return GoalRequestData(",
        ],
    );

    // handle_goal_next_request: accept/reject one goal, returning its GoalContext
    assert_contains_all(
        &rendered,
        &[
            "async def handle_goal_next_request(self, decide: Callable[[GoalRequest], GoalResponse]) -> Optional[\"GoalContext\"]:",
            "goal_request = await self._server.recv_next_goal()",
            "if goal_request is None:",
            "_goal_id, user_payload = peppylib.actions.unwrap_goal_payload(goal_request.payload)",
            "request = GoalRequest(instance_id=goal_request.instance_id, core_node=goal_request.core_node, data=_deserialize_goal_request(user_payload))",
            "response = decide(request)",
            "if hasattr(response, \"__await__\"):",
            "response = await response",
            "except BaseException as exc:",
            "await goal_request.reject(str(exc))",
            "response_payload = capnp_msg.to_bytes()",
            "inner = await goal_request.accept(response_payload)",
            "return GoalContext(inner, request)",
        ],
    );

    // GoalContext class with the always-present accessors
    assert_contains_all(
        &rendered,
        &[
            "class GoalContext:",
            "def __init__(self, inner: peppylib.actions.GoalContext, request: GoalRequest):",
            "self._inner = inner",
            "self.request = request",
            "def goal_id(self) -> str:",
            "return self._inner.goal_id",
            "def is_cancelled(self) -> bool:",
            "return self._inner.is_cancelled()",
            "async def cancel_signal(self) -> None:",
            "await self._inner.cancel_signal()",
        ],
    );

    // publish_feedback method takes the typed feedback fields
    assert_contains_all(
        &rendered,
        &[
            "async def publish_feedback(self, new_position: list[int]) -> None:",
            "await self._inner.publish_feedback(capnp_msg.to_bytes())",
        ],
    );

    // complete method takes the typed result fields
    assert_contains_all(
        &rendered,
        &[
            "async def complete(self, success: bool, error_msg: Optional[str], final_position: list[int]) -> None:",
            "await self._inner.complete(capnp_msg.to_bytes())",
        ],
    );

    // The old single-goal server API is gone.
    assert_rendered!(
        !rendered.contains("_handle_goal_payload")
            && !rendered.contains("handle_cancel_next_request")
            && !rendered.contains("handle_result_next_request")
            && !rendered.contains("emit_feedback")
            && !rendered.contains("current_goal"),
        &rendered,
        "exposed action server should not emit the old single-goal handler API"
    );
}

#[test]
fn expose_action_without_request_body() {
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_action(&action, None).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    // GoalRequest still exists (with metadata) even without request data
    assert_contains_all(
        &rendered,
        &[
            "class GoalRequest:",
            "class GoalResponse:",
            "async def handle_goal_next_request(self, decide: Callable[[GoalRequest], GoalResponse]) -> Optional[\"GoalContext\"]:",
        ],
    );

    // Should NOT have _deserialize_goal_request (no request format), and the
    // goal request is built directly without unwrapping a user payload.
    assert_rendered!(
        !rendered.contains("def _deserialize_goal_request")
            && !rendered.contains("unwrap_goal_payload"),
        &rendered,
        "expected no goal request decoding when there is no request body"
    );
    assert_contains_all(
        &rendered,
        &[
            "request = GoalRequest(instance_id=goal_request.instance_id, core_node=goal_request.core_node)",
            "inner = await goal_request.accept(response_payload)",
            "return GoalContext(inner, request)",
        ],
    );

    // Result is delivered via GoalContext.complete with the typed result
    // fields; there is no server-side ResultResponse dataclass.
    assert_contains_all(
        &rendered,
        &["async def complete(self, success: bool, error_msg: Optional[str]) -> None:"],
    );

    // Feedback is published via GoalContext.publish_feedback.
    assert_contains_all(
        &rendered,
        &[
            "async def publish_feedback(self, new_position: int, speed: int) -> None:",
            "await self._inner.publish_feedback(capnp_msg.to_bytes())",
        ],
    );

    // The old single-goal server API is gone.
    assert_rendered!(
        !rendered.contains("handle_cancel_next_request")
            && !rendered.contains("handle_result_next_request")
            && !rendered.contains("class CancelResponse:")
            && !rendered.contains("class ResultResponse:"),
        &rendered,
        "exposed action server should not emit the old single-goal handler API"
    );
}

#[test]
fn exposed_action_feedback_emits_nested_types() {
    let action: ExposedAction =
        serde_json5::from_str(EXPOSED_ACTION_WITH_NESTED_FEEDBACK_EXAMPLE).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_action(&action, None).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "class FeedbackState:",
            "position: int",
            "note: str",
            "async def publish_feedback(self, state: FeedbackState) -> None:",
        ],
    );

    // This action has no result service, so GoalContext exposes no `complete`.
    assert_rendered!(
        !rendered.contains("async def complete("),
        &rendered,
        "expected no complete method when the action has no result service"
    );

    let nested_pos = rendered
        .find("class FeedbackState:")
        .expect("FeedbackState class should be generated");
    let handle_pos = rendered
        .find("class ActionHandle:")
        .expect("ActionHandle class should be generated");
    assert_rendered!(
        nested_pos < handle_pos,
        &rendered,
        "expected feedback nested classes to be defined before ActionHandle"
    );
}

#[test]
fn expose_two_actions() {
    let action1: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let action2: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_action(&action1, None).unwrap();
    generator.add_exposed_action(&action2, None).unwrap();

    let artifacts = generator.into_artifacts();
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    let artifact_map: HashMap<_, _> = artifacts
        .into_iter()
        .map(|artifact| (artifact.leaf_name().to_string(), artifact.code_output))
        .collect();

    let move_arm = artifact_map
        .get("move_arm")
        .expect("move_arm artifact is present");
    let rotate_servo = artifact_map
        .get("rotate_servo_clockwise")
        .expect("rotate_servo_clockwise artifact is present");

    // move_arm checks
    assert_contains_all(
        move_arm,
        &[
            "\"move_arm\"",
            "import capnp",
            "class GoalRequest:",
            "class ActionHandle:",
            "@classmethod",
            "async def handle_goal_next_request(self,",
            "goal_request = await self._server.recv_next_goal()",
            "inner = await goal_request.accept(response_payload)",
            "class GoalContext:",
            "async def publish_feedback(self,",
            "async def complete(self,",
            "await self._inner.complete(capnp_msg.to_bytes())",
        ],
    );

    // rotate_servo_clockwise checks
    assert_contains_all(
        rotate_servo,
        &[
            "\"rotate_servo_clockwise\"",
            "import capnp",
            "class GoalResponse:",
            "class ActionHandle:",
            "@classmethod",
            "async def handle_goal_next_request(self,",
            "goal_request = await self._server.recv_next_goal()",
            "inner = await goal_request.accept(response_payload)",
            "class GoalContext:",
            "async def publish_feedback(self,",
            "async def complete(self,",
            "await self._inner.complete(capnp_msg.to_bytes())",
        ],
    );
}

#[test]
fn consumed_action() {
    let action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let format = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(
            &action,
            &format,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Capnp preamble imports
    assert_contains_all(
        &rendered,
        &[
            "import capnp",
            "import types",
            "from functools import lru_cache",
            "from importlib.resources import files",
        ],
    );

    // Constants
    assert_contains_all(&rendered, &["\"brain\"", "\"move_arm\""]);

    // GoalRequest dataclass
    assert_contains_all(
        &rendered,
        &[
            "class GoalRequest:",
            "arm_id: int",
            "desired_position: list[int]",
        ],
    );

    // GoalResponseData dataclass (no GoalResponse wrapper — data lives on ActionHandle)
    assert_contains_all(&rendered, &["class GoalResponseData:", "accepted: bool"]);

    // CancelResponseData and CancelResponse dataclasses
    assert_contains_all(
        &rendered,
        &[
            "class CancelResponseData:",
            "error_message: Optional[str]",
            "class CancelResponse:",
        ],
    );

    // ResultResponseData and ResultResponse dataclasses
    assert_contains_all(
        &rendered,
        &[
            "class ResultResponseData:",
            "final_position: list[int]",
            "class ResultResponse:",
            "core_node: str",
            "instance_id: str",
        ],
    );

    // FeedbackMessage dataclass
    assert_contains_all(
        &rendered,
        &["class FeedbackMessage:", "new_position: list[int]"],
    );

    // Deserialization functions. The cancel ack is decoded via the SDK
    // helper, not a generated capnp deserializer.
    assert_contains_all(
        &rendered,
        &[
            "def _deserialize_goal_response(payload: bytes) -> GoalResponseData:",
            "return GoalResponseData(",
            "def _deserialize_feedback_payload(payload: bytes) -> FeedbackMessage:",
            "return FeedbackMessage(",
            "def _deserialize_result_response(payload: bytes) -> ResultResponseData:",
            "return ResultResponseData(",
        ],
    );
    assert_rendered!(
        !rendered.contains("_deserialize_cancel_response"),
        &rendered,
        "cancel ack is decoded via decode_cancel_ack, not a capnp deserializer"
    );

    // Imports
    assert_contains_all(
        &rendered,
        &[
            "import peppylib",
            "from typing import Optional",
            "from typing import Self",
        ],
    );

    // ActionHandle class
    assert_contains_all(&rendered, &["class ActionHandle:"]);

    // fire_goal @classmethod with typed signature, serialization, and
    // ActionHandle construction. The fixture defaults to
    // `WireLinkId::wildcard()` (no manifest link_id), so the binding
    // lookup splices `None` and the user-facing `target_instance_id`
    // parameter is gone. `target_core_node` is never exposed in the
    // generated API.
    assert_contains_all(
        &rendered,
        &[
            "@classmethod",
            "async def fire_goal(cls,",
            "node_runner: peppylib.NodeRunner",
            "request: GoalRequest",
            "timeout: float",
            "feedback_qos: peppylib.QoSProfile",
            ") -> Self:",
            "user_goal_payload = capnp_msg.to_bytes()",
            "peppylib.ActionMessenger.send_goal(",
            "feedback_qos,",
            "handle = cls()",
            "handle._messenger = node_runner.messenger()",
            "handle._inner = action_handle",
            "goal_response_data = _deserialize_goal_response(payload)",
            "handle.data = goal_response_data",
            "return handle",
        ],
    );
    assert!(
        !rendered.contains("target_instance_id: Optional[str] = None"),
        "target_instance_id should no longer appear as a generated parameter; got:\n{rendered}"
    );
    assert!(
        !rendered.contains("target_core_node"),
        "target_core_node should not appear in the generated API; got:\n{rendered}"
    );

    // cancel_goal as self method with deserialization
    assert_contains_all(
        &rendered,
        &[
            "async def cancel_goal(self, timeout: float) -> CancelResponse:",
            "peppylib.ActionMessenger.cancel_goal(",
            "self._messenger,",
            "self._inner,",
            "accepted = peppylib.actions.decode_cancel_ack(response.payload)",
            "return CancelResponse(core_node=response.core_node, instance_id=response.instance_id, data=CancelResponseData(accepted=accepted, error_message=None))",
        ],
    );

    // on_next_feedback_message as self method with deserialization
    assert_contains_all(
        &rendered,
        &[
            "async def on_next_feedback_message(self) -> FeedbackMessage:",
            "await self._inner.on_next_feedback()",
            "return _deserialize_feedback_payload(payload)",
        ],
    );

    // get_result as self method with deserialization
    assert_contains_all(
        &rendered,
        &[
            "async def get_result(self, timeout: float) -> ResultResponse:",
            "peppylib.ActionMessenger.request_result(",
            "self._messenger,",
            "self._inner,",
            "result_response_data = _deserialize_result_response(payload)",
            "return ResultResponse(",
        ],
    );
}

#[test]
fn consumed_two_actions_same_node() {
    let move_arm_action: ConsumedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let move_arm_goal_request: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let move_arm_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let move_arm_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let move_arm_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let move_arm_messages = ConsumedActionMessage {
        goal_request: Some(move_arm_goal_request),
        goal_response: Some(move_arm_goal_response),
        feedback: Some(move_arm_feedback),
        result_request: None,
        result_response: Some(move_arm_result_response),
    };

    // Both actions target the same source node ("brain"), so link_id must match.
    let rotate_action: ConsumedAction =
        serde_json5::from_str(r#"{ link_id: "brain", name: "rotate_servo_clockwise" }"#).unwrap();
    let rotate_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT2).unwrap();
    let rotate_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT2).unwrap();
    let rotate_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT2).unwrap();
    let rotate_messages = ConsumedActionMessage {
        goal_request: None,
        goal_response: Some(rotate_goal_response),
        feedback: Some(rotate_feedback),
        result_request: None,
        result_response: Some(rotate_result_response),
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(
            &move_arm_action,
            &move_arm_messages,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    // Both actions target the same upstream node.
    generator
        .add_consumed_action(
            &rotate_action,
            &rotate_messages,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .unwrap();

    let artifacts: Vec<_> = generator.into_artifacts();
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Find each artifact by checking which one contains the action name
    let (move_arm, rotate_servo) = if artifacts[0].code_output.contains("\"move_arm\"") {
        (&artifacts[0].code_output, &artifacts[1].code_output)
    } else {
        (&artifacts[1].code_output, &artifacts[0].code_output)
    };

    // move_arm - constants and dataclass hierarchy
    assert_contains_all(
        move_arm,
        &[
            "\"brain\"",
            "\"move_arm\"",
            "import capnp",
            "class GoalRequest:",
            "class GoalResponseData:",
            "class ActionHandle:",
            "class ResultResponse:",
            "class FeedbackMessage:",
            "arm_id: int",
            "desired_position: list[int]",
        ],
    );

    // move_arm - deserialization functions and serialization
    assert_contains_all(
        move_arm,
        &[
            "def _deserialize_goal_response(",
            "def _deserialize_feedback_payload(",
            "def _deserialize_result_response(",
            "request: GoalRequest",
            "feedback_qos: peppylib.QoSProfile",
            ") -> Self:",
            ") -> CancelResponse:",
            ") -> ResultResponse:",
            "user_goal_payload = capnp_msg.to_bytes()",
        ],
    );

    // move_arm - API calls
    assert_contains_all(
        move_arm,
        &[
            "peppylib.ActionMessenger.send_goal(",
            "peppylib.ActionMessenger.cancel_goal(",
            "peppylib.ActionMessenger.request_result(",
        ],
    );

    // rotate_servo_clockwise - constants and dataclass hierarchy
    assert_contains_all(
        rotate_servo,
        &[
            "\"brain\"",
            "\"rotate_servo_clockwise\"",
            "import capnp",
            "class GoalResponseData:",
            "class ActionHandle:",
            "class ResultResponse:",
            "class FeedbackMessage:",
        ],
    );

    // rotate_servo_clockwise - deserialization functions (no goal request → empty payload)
    assert_contains_all(
        rotate_servo,
        &[
            "def _deserialize_goal_response(",
            "def _deserialize_feedback_payload(",
            "def _deserialize_result_response(",
            "feedback_qos: peppylib.QoSProfile",
            ") -> Self:",
            ") -> CancelResponse:",
            ") -> ResultResponse:",
            "user_goal_payload = b\"\"",
        ],
    );

    // rotate_servo_clockwise - API calls
    assert_contains_all(
        rotate_servo,
        &[
            "peppylib.ActionMessenger.send_goal(",
            "peppylib.ActionMessenger.cancel_goal(",
            "peppylib.ActionMessenger.request_result(",
        ],
    );
}

#[test]
fn consumed_action_without_response_payload() {
    let action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();

    let format = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: None,
        feedback: Some(feedback_format),
        result_request: None,
        result_response: None,
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(
            &action,
            &format,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .expect("generator should allow consumed actions with empty response payloads");
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "async def fire_goal(",
            "feedback_qos: peppylib.QoSProfile",
            "async def get_result(",
            "peppylib.ActionMessenger.send_goal(",
            "peppylib.ActionMessenger.request_result(",
        ],
    );

    // Should NOT have deserialization for goal_response or result_response
    assert_rendered!(
        !rendered.contains("def _deserialize_goal_response"),
        &rendered,
        "expected no _deserialize_goal_response when goal_response is None"
    );
    assert_rendered!(
        !rendered.contains("def _deserialize_result_response"),
        &rendered,
        "expected no _deserialize_result_response when result_response is None"
    );

    // Cancel uses the SDK ack decoder; feedback still has a capnp deserializer.
    assert_contains_all(
        &rendered,
        &[
            "accepted = peppylib.actions.decode_cancel_ack(response.payload)",
            "def _deserialize_feedback_payload(",
        ],
    );

    // ActionHandle class
    assert_contains_all(&rendered, &["class ActionHandle:"]);

    // fire_goal should have serialization (goal_request exists) but no handle.data (no goal_response)
    assert_contains_all(
        &rendered,
        &[
            "user_goal_payload = capnp_msg.to_bytes()",
            "handle = cls()",
            "handle._messenger = node_runner.messenger()",
            "handle._inner = action_handle",
            "return handle",
        ],
    );
    assert_rendered!(
        !rendered.contains("handle.data"),
        &rendered,
        "expected no handle.data when goal_response is None"
    );

    // get_result should return without data
    assert_contains_all(
        &rendered,
        &["return ResultResponse(core_node=response.core_node, instance_id=response.instance_id)"],
    );
}

#[test]
fn consumed_action_without_feedback() {
    let action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();

    let format = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: None,
        feedback: None,
        result_request: None,
        result_response: None,
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(
            &action,
            &format,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .expect("generator should allow consumed actions without feedback payloads");
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(artifacts.len(), 1, "expected single generated artifact");

    let rendered = &artifacts[0];

    // ActionHandle class with goal and result methods
    assert_contains_all(
        rendered,
        &[
            "class ActionHandle:",
            "async def fire_goal(",
            "feedback_qos: peppylib.QoSProfile",
            "async def get_result(",
            "peppylib.ActionMessenger.send_goal(",
            "peppylib.ActionMessenger.request_result(",
        ],
    );

    // Make sure the related functions for feedback are not present
    assert_rendered!(
        !rendered.contains("async def on_next_feedback_message"),
        rendered,
        "expected generator to skip feedback listener when no feedback payload is provided"
    );
    assert_rendered!(
        !rendered.contains("def _deserialize_feedback_payload"),
        rendered,
        "expected no _deserialize_feedback_payload when feedback is None"
    );

    // Cancel ack is always decoded via the SDK helper; capnp preamble present
    // for the goal request serializer.
    assert_contains_all(
        rendered,
        &[
            "import capnp",
            "accepted = peppylib.actions.decode_cancel_ack(response.payload)",
        ],
    );
}
