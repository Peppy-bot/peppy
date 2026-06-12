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

    // GoalResponse: framework-owned goal acknowledgement dataclass with
    // accept()/reject(reason) staticmethods the decider returns.
    assert_contains_all(
        &rendered,
        &[
            "class GoalResponse:",
            "accepted: bool",
            "error_message: Optional[str]",
            "def accept():",
            "def reject(reason):",
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

    // expose builds the concurrent-action engine.
    assert_contains_all(
        &rendered,
        &[
            "@classmethod",
            "async def expose(cls,",
            "node_runner: peppylib.NodeRunner) -> Self:",
            "peppylib.ConcurrentAction.expose(",
            "handle = cls()",
            "handle._inner = inner",
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

    // handle_goal_next_request: recv → decode → accept/reject → GoalContext.
    assert_contains_all(
        &rendered,
        &[
            "async def handle_goal_next_request(self, handler: Callable[[GoalRequest], GoalResponse]) -> \"GoalContext | None\":",
            "pending = await self._inner.recv_next_goal()",
            "request = GoalRequest(instance_id=pending.instance_id, core_node=pending.core_node, data=request_data)",
            "ctx = await pending.accept(response_bytes)",
            "return GoalContext(ctx, request)",
            "await pending.reject(response_bytes)",
        ],
    );

    // GoalContext class with per-goal methods.
    assert_contains_all(
        &rendered,
        &[
            "class GoalContext:",
            "def request(self) -> GoalRequest:",
            "def goal_id(self) -> str:",
            "async def cancel_signal(self) -> None:",
            "def is_cancelled(self) -> bool:",
            "async def publish_feedback(self, new_position: list[int]) -> None:",
            "async def complete(self,",
            "async def complete_cancelled(self,",
            "success: bool",
            "error_msg: Optional[str]",
            "final_position: list[int]",
        ],
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

    // GoalRequest still exists (with metadata) even without request data. The
    // framework GoalResponse (+ accept/reject) is always present.
    assert_contains_all(
        &rendered,
        &[
            "class GoalRequest:",
            "class GoalResponse:",
            "def accept():",
            "def reject(reason):",
            "async def handle_goal_next_request(",
        ],
    );
    // No request data → no deserializer and a GoalRequest without `data`.
    assert_rendered!(
        !rendered.contains("def _deserialize_goal_request"),
        &rendered,
        "expected no _deserialize_goal_request when there is no request body"
    );
    assert_contains_all(
        &rendered,
        &["request = GoalRequest(instance_id=pending.instance_id, core_node=pending.core_node)"],
    );

    // Result → GoalContext completion methods.
    assert_contains_all(
        &rendered,
        &[
            "success: bool",
            "error_msg: Optional[str]",
            "async def complete(self,",
            "async def complete_cancelled(self,",
        ],
    );

    // Feedback → GoalContext.publish_feedback.
    assert_contains_all(
        &rendered,
        &["async def publish_feedback(self, new_position: int, speed: int) -> None:"],
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
            "class GoalResponse:",
            "class GoalContext:",
            "class ActionHandle:",
            "@classmethod",
            "peppylib.ConcurrentAction.expose(",
            "async def handle_goal_next_request(self,",
            "async def complete(self,",
            "async def publish_feedback(self,",
        ],
    );

    // rotate_servo_clockwise checks
    assert_contains_all(
        rotate_servo,
        &[
            "\"rotate_servo_clockwise\"",
            "import capnp",
            "class GoalResponse:",
            "class GoalContext:",
            "class ActionHandle:",
            "@classmethod",
            "peppylib.ConcurrentAction.expose(",
            "async def handle_goal_next_request(self,",
            "async def complete(self,",
            "async def publish_feedback(self,",
        ],
    );
}

/// A real manifest dep (link_id present) splices the runtime binding
/// lookup as the single `target` argument of the generated `send_goal`:
/// `node_runner.pinned_producer_for(<link_id>)` resolves at runtime to
/// the bound producer's full `(core_node, instance_id)` tuple, so a
/// pinned slot addresses exactly one producer with no discovery probe.
#[test]
fn consumed_action_with_link_id_splices_runtime_binding_target() {
    let action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let format = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: None,
        result_request: None,
        result_response: None,
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(
            &action,
            &format,
            &crate::DependencyContext::native("brain", "v1")
                .with_link_id(crate::WireLinkId::from_link_id("left_arm", false)),
        )
        .unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_contains_all(
        &rendered,
        &["node_runner.pinned_producer_for(\"left_arm\"),"],
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

    // CancelState enum + CancelResponse dataclass (no per-action cancel data).
    assert_contains_all(
        &rendered,
        &[
            "class CancelState(IntEnum):",
            "SIGNALLED = 0",
            "ALREADY_TERMINAL = 1",
            "UNKNOWN = 2",
            "class CancelResponse:",
            "state: CancelState",
        ],
    );

    // ResultStatus enum, ResultResponseData, and ResultResponse dataclasses.
    assert_contains_all(
        &rendered,
        &[
            "class ResultStatus(IntEnum):",
            "COMPLETED = 0",
            "CANCELLED = 1",
            "ABANDONED = 2",
            "EXPIRED = 3",
            "class ResultResponseData:",
            "final_position: list[int]",
            "class ResultResponse:",
            "core_node: str",
            "instance_id: str",
            "status: ResultStatus",
            "data: Optional[ResultResponseData]",
        ],
    );

    // FeedbackMessage dataclass
    assert_contains_all(
        &rendered,
        &["class FeedbackMessage:", "new_position: list[int]"],
    );

    // Deserialization functions. The cancel reply is decoded by peppylib, so
    // there is no generated `_deserialize_cancel_response`.
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
    assert!(
        !rendered.contains("_deserialize_cancel_response"),
        "cancel reply is decoded by peppylib; no generated cancel deserializer expected"
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
    // `WireLinkId::wildcard()` (no manifest link_id), so the send_goal
    // call splices `None` at the single target slot and the user-facing
    // `target_instance_id` parameter is gone. `target_core_node` is never
    // exposed in the generated API, and the renamed `pinned_target_for`
    // accessor must never be emitted (the runtime helper is
    // `pinned_producer_for`).
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
            "TARGET_ACTION_NAME,\n            None,\n            user_goal_payload,",
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
    assert!(
        !rendered.contains("pinned_target_for"),
        "pinned_target_for should never be emitted; the runtime helper is pinned_producer_for; got:\n{rendered}"
    );

    // cancel_goal as self method, mapping the typed cancel reply's state tag.
    assert_contains_all(
        &rendered,
        &[
            "async def cancel_goal(self, timeout: float) -> CancelResponse:",
            "peppylib.ActionMessenger.cancel_goal(",
            "self._messenger,",
            "self._inner,",
            "state=CancelState(reply.state)",
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

    // get_result as self method, mapping the typed result reply's status + body.
    assert_contains_all(
        &rendered,
        &[
            "async def get_result(self, timeout: float) -> ResultResponse:",
            "peppylib.ActionMessenger.request_result(",
            "self._messenger,",
            "self._inner,",
            "status = ResultStatus(reply.status)",
            "data = _deserialize_result_response(reply.body)",
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

    // The goal acknowledgement is framework-owned, so goal_response
    // deserialization is always present even when the action declares none.
    assert_contains_all(&rendered, &["def _deserialize_goal_response("]);
    // result_response is NOT framework-owned, so it stays absent here.
    assert_rendered!(
        !rendered.contains("def _deserialize_result_response"),
        &rendered,
        "expected no _deserialize_result_response when result_response is None"
    );

    // Should still have feedback deserialization (cancel is decoded by peppylib).
    assert_contains_all(&rendered, &["def _deserialize_feedback_payload("]);
    assert!(
        !rendered.contains("_deserialize_cancel_response"),
        "cancel reply is decoded by peppylib; no generated cancel deserializer expected"
    );

    // ActionHandle class
    assert_contains_all(&rendered, &["class ActionHandle:"]);

    // fire_goal should have serialization (goal_request exists). The goal
    // acknowledgement is framework-owned, so GoalResponseData and handle.data
    // are always present even when the action declares no goal response.
    assert_contains_all(
        &rendered,
        &[
            "user_goal_payload = capnp_msg.to_bytes()",
            "handle = cls()",
            "handle._messenger = node_runner.messenger()",
            "handle._inner = action_handle",
            "handle.data = goal_response_data",
            "return handle",
            "class GoalResponseData:",
            "accepted: bool",
        ],
    );

    // get_result returns the typed status and no data (empty result format).
    assert_contains_all(
        &rendered,
        &[
            "status = ResultStatus(reply.status)",
            "return ResultResponse(core_node=reply.core_node, instance_id=reply.instance_id, status=status)",
        ],
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

    // Still has the capnp preamble (for the goal/result schemas). The cancel
    // reply is decoded by peppylib, so no generated cancel deserializer.
    assert_contains_all(rendered, &["import capnp"]);
    assert!(
        !rendered.contains("_deserialize_cancel_response"),
        "cancel reply is decoded by peppylib; no generated cancel deserializer expected"
    );
}
