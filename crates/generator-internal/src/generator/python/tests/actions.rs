use super::*;
use config::node::{ExposedAction, MessageFormat, SubscribedAction};
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
        note: {
          $type: "string",
          $optional: true
        }
      }
    }
  }
}
"#;

// --- Subscribes examples
const SUBSCRIBED_ACTION_EXAMPLE1: &str = r#"
{
  id: "brain_move_arm",
  node: "brain",
  name: "move_arm",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_ACTION_GOAL_FORMAT1: &str = r#"
{
  arm_id: "u16",
  desired_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1: &str = r#"
{
  accepted: "bool"
}
"#;

const SUBSCRIBED_ACTION_FEEDBACK_FORMAT1: &str = r#"
{
  new_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1: &str = r#"
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

const SUBSCRIBED_ACTION_EXAMPLE2: &str = r#"
{
  id: "controller_rotate_servo",
  node: "controller",
  name: "rotate_servo_clockwise",
  tag: "0.1.0"
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
    generator.add_exposed_action(&action).unwrap();
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
            "from pathlib import Path",
            "_PKG_DIR = Path(__file__).resolve().parent.parent",
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

    // Cancel request/response dataclasses
    assert_contains_all(
        &rendered,
        &[
            "class CancelRequest:",
            "class CancelResponse:",
            "error_message: Optional[str]",
        ],
    );

    // Result request/response dataclasses
    assert_contains_all(
        &rendered,
        &[
            "class ResultRequest:",
            "class ResultResponse:",
            "final_position: list[int]",
            "success: bool",
            "error_msg: Optional[str]",
        ],
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

    // expose @classmethod inside ActionHandle
    assert_contains_all(
        &rendered,
        &[
            "@classmethod",
            "async def expose(cls,",
            "node_runner: peppylib.NodeRunner) -> Self:",
            "peppylib.ActionMessenger.expose(",
            "handle = cls()",
            "handle.goal_service = action.goal_service",
            "handle.cancel_service = action.cancel_service",
            "handle.result_service = action.result_service",
            "handle.feedback_publisher = action.feedback_publisher",
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

    // _handle_goal_payload function with typed signature (module level)
    assert_contains_all(
        &rendered,
        &[
            "async def _handle_goal_payload(payload: bytes, handler: Callable[[GoalRequest], GoalResponse], core_node: str, instance_id: str) -> bytes:",
            "request_data = _deserialize_goal_request(payload)",
            "request = GoalRequest(instance_id=instance_id, core_node=core_node, data=request_data)",
            "response = handler(request)",
            "if hasattr(response, \"__await__\"):",
            "response = await response",
            "return capnp_msg.to_bytes()",
        ],
    );

    // handle_goal_next_request as method with self
    assert_contains_all(
        &rendered,
        &[
            "async def handle_goal_next_request(self, handler: Callable[[GoalRequest], GoalResponse]) -> None:",
            "async def _on_request(request_context):",
            "return await _handle_goal_payload(payload, handler, core_node, instance_id)",
            "await self.goal_service.handle_next_request(_on_request)",
        ],
    );

    // _handle_cancel_payload function (module level)
    assert_contains_all(
        &rendered,
        &[
            "async def _handle_cancel_payload(handler: Callable[[CancelRequest], CancelResponse], core_node: str, instance_id: str) -> bytes:",
            "request = CancelRequest(instance_id=instance_id, core_node=core_node)",
            "response = handler(request)",
            "if hasattr(response, \"__await__\"):",
            "response = await response",
        ],
    );

    // handle_cancel_next_request as method with self
    assert_contains_all(
        &rendered,
        &[
            "async def handle_cancel_next_request(self, handler: Callable[[CancelRequest], CancelResponse]) -> None:",
            "return await _handle_cancel_payload(handler, core_node, instance_id)",
            "await self.cancel_service.handle_next_request(_on_request)",
        ],
    );

    // _handle_result_payload function (module level)
    assert_contains_all(
        &rendered,
        &[
            "async def _handle_result_payload(handler: Callable[[ResultRequest], ResultResponse], core_node: str, instance_id: str) -> bytes:",
            "request = ResultRequest(instance_id=instance_id, core_node=core_node)",
        ],
    );

    // handle_result_next_request as method with self
    assert_contains_all(
        &rendered,
        &[
            "async def handle_result_next_request(self, handler: Callable[[ResultRequest], ResultResponse]) -> None:",
            "return await _handle_result_payload(handler, core_node, instance_id)",
            "await self.result_service.handle_next_request(_on_request)",
        ],
    );

    // emit_feedback as method with self
    assert_contains_all(
        &rendered,
        &[
            "async def emit_feedback(self, new_position: list[int]):",
            "payload = capnp_msg.to_bytes()",
            "await self.feedback_publisher.publish(payload)",
        ],
    );
}

#[test]
fn expose_action_without_request_body() {
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_action(&action).unwrap();
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
            "async def handle_goal_next_request(",
        ],
    );

    // Should NOT have _deserialize_goal_request (no request format)
    assert_rendered!(
        !rendered.contains("def _deserialize_goal_request"),
        &rendered,
        "expected no _deserialize_goal_request when there is no request body"
    );

    // _handle_goal_payload without payload parameter
    assert_contains_all(
        &rendered,
        &[
            "async def _handle_goal_payload(handler: Callable[[GoalRequest], GoalResponse], core_node: str, instance_id: str) -> bytes:",
            "request = GoalRequest(instance_id=instance_id, core_node=core_node)",
        ],
    );

    // handle_goal_next_request - _on_request without payload
    assert_contains_all(
        &rendered,
        &[
            "return await _handle_goal_payload(handler, core_node, instance_id)",
            "await self.goal_service.handle_next_request(_on_request)",
        ],
    );

    // Result handling
    assert_contains_all(
        &rendered,
        &[
            "class ResultResponse:",
            "success: bool",
            "error_msg: Optional[str]",
            "async def handle_result_next_request(",
        ],
    );

    // Cancel handling
    assert_contains_all(
        &rendered,
        &[
            "class CancelResponse:",
            "async def handle_cancel_next_request(",
            "async def _handle_cancel_payload(",
            "await self.cancel_service.handle_next_request(_on_request)",
        ],
    );

    // Result handler implementation
    assert_contains_all(
        &rendered,
        &[
            "async def _handle_result_payload(",
            "await self.result_service.handle_next_request(_on_request)",
        ],
    );

    // Feedback emitter as method with self
    assert_contains_all(
        &rendered,
        &[
            "async def emit_feedback(self, new_position: int, speed: int):",
            "await self.feedback_publisher.publish(payload)",
        ],
    );
}

#[test]
fn exposed_action_feedback_emits_nested_types() {
    let action: ExposedAction =
        serde_json5::from_str(EXPOSED_ACTION_WITH_NESTED_FEEDBACK_EXAMPLE).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_action(&action).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "from typing import Optional",
            "class FeedbackState:",
            "position: int",
            "note: Optional[str]",
            "async def emit_feedback(self, state: FeedbackState):",
        ],
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
    generator.add_exposed_action(&action1).unwrap();
    generator.add_exposed_action(&action2).unwrap();

    let artifacts = generator.into_artifacts();
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    let artifact_map: HashMap<_, _> = artifacts
        .into_iter()
        .map(|artifact| (artifact.node_name, artifact.code_output))
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
            "class ResultResponse:",
            "class ActionHandle:",
            "@classmethod",
            "async def handle_goal_next_request(self,",
            "async def _handle_goal_payload(",
            "async def _handle_cancel_payload(",
            "async def _handle_result_payload(",
            "await self.goal_service.handle_next_request(_on_request)",
            "await self.cancel_service.handle_next_request(_on_request)",
            "await self.result_service.handle_next_request(_on_request)",
            "async def emit_feedback(self,",
            "await self.feedback_publisher.publish(payload)",
        ],
    );

    // rotate_servo_clockwise checks
    assert_contains_all(
        rotate_servo,
        &[
            "\"rotate_servo_clockwise\"",
            "import capnp",
            "class GoalResponse:",
            "class ResultResponse:",
            "class ActionHandle:",
            "@classmethod",
            "async def handle_goal_next_request(self,",
            "async def _handle_goal_payload(",
            "async def _handle_cancel_payload(",
            "async def _handle_result_payload(",
            "await self.goal_service.handle_next_request(_on_request)",
            "await self.cancel_service.handle_next_request(_on_request)",
            "await self.result_service.handle_next_request(_on_request)",
            "async def emit_feedback(self,",
            "await self.feedback_publisher.publish(payload)",
        ],
    );
}

#[test]
fn subscribed_to_action() {
    let action: SubscribedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let format = SubscribedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };

    let mut generator = PythonGenerator::new();
    generator.add_subscribed_action(&action, &format).unwrap();
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
            "from pathlib import Path",
            "_PKG_DIR = Path(__file__).resolve().parent.parent",
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

    // Deserialization functions
    assert_contains_all(
        &rendered,
        &[
            "def _deserialize_goal_response(payload: bytes) -> GoalResponseData:",
            "return GoalResponseData(",
            "def _deserialize_cancel_response(payload: bytes) -> CancelResponseData:",
            "return CancelResponseData(",
            "def _deserialize_feedback_payload(payload: bytes) -> FeedbackMessage:",
            "return FeedbackMessage(",
            "def _deserialize_result_response(payload: bytes) -> ResultResponseData:",
            "return ResultResponseData(",
        ],
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

    // fire_goal @classmethod with typed signature, serialization, and ActionHandle construction
    assert_contains_all(
        &rendered,
        &[
            "@classmethod",
            "async def fire_goal(cls,",
            "node_runner: peppylib.NodeRunner",
            "request: GoalRequest",
            "timeout: float",
            "feedback_qos: peppylib.QoSProfile",
            "target_core_node: Optional[str] = None",
            "target_instance_id: Optional[str] = None",
            ") -> Self:",
            "goal_payload = capnp_msg.to_bytes()",
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

    // cancel_goal as self method with deserialization
    assert_contains_all(
        &rendered,
        &[
            "async def cancel_goal(self, timeout: float) -> CancelResponse:",
            "peppylib.ActionMessenger.cancel_goal(",
            "self._messenger,",
            "self._inner,",
            "cancel_response_data = _deserialize_cancel_response(payload)",
            "return CancelResponse(",
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
fn subscribed_to_two_actions_same_node() {
    let move_arm_action: SubscribedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let move_arm_goal_request: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let move_arm_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let move_arm_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let move_arm_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let move_arm_messages = SubscribedActionMessage {
        goal_request: Some(move_arm_goal_request),
        goal_response: Some(move_arm_goal_response),
        feedback: Some(move_arm_feedback),
        result_request: None,
        result_response: Some(move_arm_result_response),
    };

    let mut rotate_action: SubscribedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE2).unwrap();
    // Reuse the same upstream node so both subscriptions target the same source.
    rotate_action.node = move_arm_action.node.clone();
    let rotate_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT2).unwrap();
    let rotate_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT2).unwrap();
    let rotate_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT2).unwrap();
    let rotate_messages = SubscribedActionMessage {
        goal_request: None,
        goal_response: Some(rotate_goal_response),
        feedback: Some(rotate_feedback),
        result_request: None,
        result_response: Some(rotate_result_response),
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_subscribed_action(&move_arm_action, &move_arm_messages)
        .unwrap();
    generator
        .add_subscribed_action(&rotate_action, &rotate_messages)
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
            "def _deserialize_cancel_response(",
            "def _deserialize_feedback_payload(",
            "def _deserialize_result_response(",
            "request: GoalRequest",
            "feedback_qos: peppylib.QoSProfile",
            ") -> Self:",
            ") -> CancelResponse:",
            ") -> ResultResponse:",
            "goal_payload = capnp_msg.to_bytes()",
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
            "def _deserialize_cancel_response(",
            "def _deserialize_feedback_payload(",
            "def _deserialize_result_response(",
            "feedback_qos: peppylib.QoSProfile",
            ") -> Self:",
            ") -> CancelResponse:",
            ") -> ResultResponse:",
            "goal_payload = b\"\"",
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
fn subscribed_action_without_response_payload() {
    let action: SubscribedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();

    let format = SubscribedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: None,
        feedback: Some(feedback_format),
        result_request: None,
        result_response: None,
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_subscribed_action(&action, &format)
        .expect("generator should allow subscribed actions with empty response payloads");
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

    // Should still have cancel and feedback deserialization
    assert_contains_all(
        &rendered,
        &[
            "def _deserialize_cancel_response(",
            "def _deserialize_feedback_payload(",
        ],
    );

    // ActionHandle class
    assert_contains_all(&rendered, &["class ActionHandle:"]);

    // fire_goal should have serialization (goal_request exists) but no handle.data (no goal_response)
    assert_contains_all(
        &rendered,
        &[
            "goal_payload = capnp_msg.to_bytes()",
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
        &[
            "return ResultResponse(core_node=response.core_node, instance_id=response.instance_id)",
        ],
    );
}

#[test]
fn subscribed_action_without_feedback() {
    let action: SubscribedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();

    let format = SubscribedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: None,
        feedback: None,
        result_request: None,
        result_response: None,
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_subscribed_action(&action, &format)
        .expect("generator should allow subscribed actions without feedback payloads");
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

    // Should still have cancel deserialization (always present) and capnp preamble
    assert_contains_all(
        rendered,
        &["import capnp", "def _deserialize_cancel_response("],
    );
}
