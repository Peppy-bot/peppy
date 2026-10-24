use super::*;
use config::node::{ConsumedAction, MessageFormat, NativeExposedAction};
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

const SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT: &str = r#"
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
    let action: NativeExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();

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

    // GoalResponse mirrors response_message_format exactly. GoalDecision
    // controls framework admission independently: an accept carries the
    // declared payload, a refusal carries a failure of a declared kind. The
    // constructor takes no other pair, and parses the refusal again as a
    // failure of this member.
    assert_contains_all(
        &rendered,
        &[
            "class GoalResponse:",
            "accepted: bool",
            "class GoalDecision:",
            "def __init__(self, response: GoalResponse | None, refusal: Failure | None) -> None:",
            "if (response is None) == (refusal is None):",
            "raise ValueError(\"a goal decision is an accept with the declared GoalResponse, or a refusal with no response\")",
            "self.refusal = None if refusal is None else Failure(*refusal.args)",
            "def accept(response: GoalResponse) -> \"GoalDecision\":",
            "def refuse(kind: FailureKind, message: str) -> \"GoalDecision\":",
            "return GoalDecision(None, Failure(kind, message))",
        ],
    );

    // A native action gives the kinds of peppy only, and its `Failure` takes
    // no other kind.
    assert_contains_all(
        &rendered,
        &[
            "class FailureKind(str, Enum):",
            "ATTEMPT_FAILED = \"attempt_failed\"",
            "REJECTED = \"rejected\"",
            "class Failure(peppylib.FailureError):",
            "parsed = FailureKind(kind)",
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

    // handle_goal_next_request: recv → decode → accept/refuse → GoalContext.
    // A request that does not decode into the declared shape is refused with
    // the kind `rejected` before the handler runs, and the loop keeps
    // polling.
    assert_contains_all(
        &rendered,
        &[
            "async def handle_goal_next_request(self, handler: Callable[[GoalRequest], GoalDecision]) -> \"GoalContext | None\":",
            "A goal request that does not decode is refused with the kind `rejected`.",
            "pending = await self._inner.recv_next_goal()",
            "    request_data = _deserialize_goal_request(pending.request_bytes)",
            "except (ValueError, capnp.KjException) as error:",
            "await pending.refuse(FailureKind.REJECTED.value, f\"goal request does not decode: {error}\")",
            "continue",
            "request = GoalRequest(instance_id=pending.instance_id, core_node=pending.core_node, data=request_data)",
            "ctx = await pending.accept(answer)",
            "return GoalContext(ctx, request)",
        ],
    );

    // The answer of the handler: its decision is built again as a decision
    // of this member before anything of it reaches the wire, and an accept
    // gives the encoded response.
    assert_contains_all(
        &rendered,
        &[
            "async def _goal_answer(handler: Callable[[GoalRequest], GoalDecision], request: GoalRequest) -> \"Failure | bytes\":",
            "decision = handler(request)",
            "decision = await decision",
            "decision = GoalDecision(decision.response, decision.refusal)\n    if decision.refusal is not None:\n        return decision.refusal\n    response = decision.response\n",
            "return capnp_msg.to_bytes()",
        ],
    );

    // A handler that raises, or that gives a decision that this member
    // cannot send, refuses the goal with `attempt_failed`: the goal gets an
    // answer and the loop keeps polling.
    assert_contains_all(
        &rendered,
        &[
            "            try:\n                answer = await _goal_answer(handler, request)\n            except Exception as error:\n                answer = Failure(FailureKind.ATTEMPT_FAILED, f\"the goal handler raised {type(error).__name__}: {error}\")\n            if isinstance(answer, Failure):\n                await pending.refuse(answer.kind.value, answer.message)\n                continue\n",
        ],
    );

    // The sender grace of the action.
    assert_contains_all(
        &rendered,
        &[
            "async def set_sender_grace(self, seconds: float) -> None:",
            "await self._inner.set_sender_grace(seconds)",
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
            "def cancel_cause(self) -> Literal[\"caller\", \"sender_gone\"] | None:",
            "def cancel_reason(self) -> str | None:",
            "return self._inner.cancel_reason",
            "async def fail(self, kind: FailureKind, message: str) -> None:",
            "await self._inner.fail(failure.kind.value, failure.message)",
            "async def publish_feedback(self, new_position: list[int]) -> None:",
            "async def complete(self,",
            "async def complete_cancelled(self, reason: str,",
            "await self._inner.complete_cancelled(reason, payload)",
            "success: bool",
            "error_msg: Optional[str]",
            "final_position: list[int]",
        ],
    );
}

#[test]
fn expose_action_without_request_body() {
    let action: NativeExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_action(&action, None).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    // GoalRequest still exists (with metadata) even without request data. The
    // declared GoalResponse and framework GoalDecision remain present.
    assert_contains_all(
        &rendered,
        &[
            "class GoalRequest:",
            "class GoalResponse:",
            "class GoalDecision:",
            "def accept(response: GoalResponse) -> \"GoalDecision\":",
            "def refuse(kind: FailureKind, message: str) -> \"GoalDecision\":",
            "async def handle_goal_next_request(",
        ],
    );
    // No request data → no deserializer, no decode refusal, and a
    // GoalRequest without `data`.
    assert_rendered!(
        !rendered.contains("def _deserialize_goal_request"),
        &rendered,
        "expected no _deserialize_goal_request when there is no request body"
    );
    assert_rendered!(
        !rendered.contains("does not decode"),
        &rendered,
        "expected no decode refusal when there is no request body"
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

/// An action that declares no goal response accepts with no response: its
/// decision takes none, and raises for one.
#[test]
fn expose_action_without_goal_response_takes_no_response_in_its_decision() {
    let action: NativeExposedAction = serde_json5::from_str(r#"{ name: "home_arm" }"#).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_action(&action, None).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "def __init__(self, response: None, refusal: Failure | None) -> None:",
            "if response is not None:",
            "raise ValueError(\"this action declares no GoalResponse\")",
            "self.refusal = None if refusal is None else Failure(*refusal.args)",
            "def accept() -> \"GoalDecision\":",
            "return GoalDecision(None, None)",
            "decision = GoalDecision(decision.response, decision.refusal)\n    if decision.refusal is not None:\n        return decision.refusal\n    return b\"\"\n",
        ],
    );
    assert_rendered!(
        !rendered.contains("class GoalResponse"),
        &rendered,
        "expected no GoalResponse for an action that declares none"
    );
}

#[test]
fn exposed_action_feedback_emits_nested_types() {
    let action: NativeExposedAction =
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
    let action1: NativeExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let action2: NativeExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

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

/// A real manifest dep (link_id present) wires the runtime binding into
/// the module: this `one` slot's `bound_producer()` reads
/// `node_runner.bound_producer(<link_id>)`, and the caller-selected
/// `target` is membership-checked against the same slot before `send_goal`
/// pins it on the wire with no discovery probe.
#[test]
fn consumed_action_with_link_id_splices_runtime_binding_target() {
    let mut action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    // Production derives the wire link_id and the manifest link_id from the
    // same `depends_on` entry (see core-node `sync`); keep them identical
    // here so the fixture models a reachable input.
    action.link_id = "left_arm".to_owned();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let format = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT).unwrap()),
        feedback: None,
        result_response: None,
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(&action, &format, &native_dep("brain", "v1", "left_arm"))
        .unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "LINK_ID = \"left_arm\"",
            "return node_runner.bound_producer(\"left_arm\")",
            "node_runner.ensure_target_bound(LINK_ID, target)",
            "target,",
        ],
    );
}

#[test]
fn consumed_action() {
    let action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let format = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT).unwrap()),
        feedback: Some(feedback_format),
        result_response: Some(result_response_format),
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(&action, &format, &native_dep("brain", "v1", "brain"))
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

    // GoalResponseData dataclass (no GoalResponse wrapper; data lives on ActionHandle)
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
            "FAILED = 4",
            "class ResultResponseData:",
            "final_position: list[int]",
            "class ResultResponse:",
            "core_node: str",
            "instance_id: str",
            "status: ResultStatus",
            "data: Optional[ResultResponseData]",
            "failure: Optional[Failure]",
            "cancel_reason: Optional[str]",
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
    // ActionHandle construction: the caller passes the selected member of
    // the slot's bound set explicitly (every cardinality, `one` included),
    // membership-checked before the goal reaches the wire. No
    // implicit-target overload is generated, and no `from_*` / `target_*`
    // core_node or instance_id string parameter appears (see
    // assert_no_producer_identity_params).
    assert_contains_all(
        &rendered,
        &[
            "@classmethod",
            "async def fire_goal(cls,",
            "node_runner: peppylib.NodeRunner",
            "target: peppylib.ProducerRef",
            "request: GoalRequest",
            "timeout: float",
            "feedback_bound: Optional[int] = None",
            ") -> Self:",
            "node_runner.ensure_target_bound(LINK_ID, target)",
            "user_goal_payload = capnp_msg.to_bytes()",
            "peppylib.ActionMessenger.send_goal(",
            "TARGET_ACTION_NAME,\n            target,\n            user_goal_payload,",
            "feedback_bound,",
            "handle = cls()",
            "handle._messenger = node_runner.messenger()",
            "handle._inner = action_handle",
            "handle.accepted = action_handle.accepted",
            "handle.refusal = None if action_handle.accepted else Failure(action_handle.refusal_kind, action_handle.refusal_message)",
            "handle.data = _deserialize_goal_response(action_handle.goal_reply_body) if action_handle.accepted else None",
            "return handle",
        ],
    );

    // The cardinality-typed module surface: the action module exposes the
    // same slot-level accessor as topics and services, singular here
    // because this is a `one` slot.
    assert_contains_all(
        &rendered,
        &[
            "LINK_ID = \"brain\"",
            "def bound_producer(node_runner: peppylib.NodeRunner) -> peppylib.ProducerRef:",
            "return node_runner.bound_producer(\"brain\")",
        ],
    );
    assert!(
        !rendered.contains("def bound_producers("),
        "a `one` slot must expose only the singular accessor; got:\n{rendered}"
    );
    super::topics::assert_no_producer_identity_params(&rendered);

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
            "if status is ResultStatus.FAILED:",
            "failure = Failure(reply.failure_kind, reply.failure_message)",
            "data = _deserialize_result_response(reply.body)",
            "failure=failure, cancel_reason=reply.cancel_reason)",
        ],
    );
}

#[test]
fn consumed_two_actions_same_node() {
    let move_arm_action: ConsumedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let move_arm_goal_request: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let move_arm_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let move_arm_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let move_arm_messages = ConsumedActionMessage {
        goal_request: Some(move_arm_goal_request),
        goal_response: Some(serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT).unwrap()),
        feedback: Some(move_arm_feedback),
        result_response: Some(move_arm_result_response),
    };

    // Both actions target the same source node ("brain"), so link_id must match.
    let rotate_action: ConsumedAction =
        serde_json5::from_str(r#"{ link_id: "brain", name: "rotate_servo_clockwise" }"#).unwrap();
    let rotate_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT2).unwrap();
    let rotate_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT2).unwrap();
    let rotate_messages = ConsumedActionMessage {
        goal_request: None,
        goal_response: Some(serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT).unwrap()),
        feedback: Some(rotate_feedback),
        result_response: Some(rotate_result_response),
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(
            &move_arm_action,
            &move_arm_messages,
            &native_dep("brain", "v1", "brain"),
        )
        .unwrap();
    // Both actions target the same upstream node.
    generator
        .add_consumed_action(
            &rotate_action,
            &rotate_messages,
            &native_dep("brain", "v1", "brain"),
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
            "feedback_bound: Optional[int] = None",
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
            "feedback_bound: Optional[int] = None",
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

/// A consumed action pulled via a `depends_on.contracts` dependency
/// addresses the producer as a *contract* rather than a node: `fire_goal`
/// splices `peppylib.SenderTarget.contract(contract_name, contract_tag)`
/// instead of `peppylib.SenderTarget.node(...)`. Python twin of the Rust
/// backend's `consumed_action_via_contract_origin_targets_contract`.
#[test]
fn consumed_action_via_contract_origin_targets_contract() {
    let action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let format = ConsumedActionMessage {
        goal_request: Some(serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap()),
        goal_response: Some(serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT).unwrap()),
        feedback: Some(serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap()),
        result_response: Some(
            serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap(),
        ),
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(
            &action,
            &format,
            &contract_dep("arm_contract", "v2", "brain"),
        )
        .unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &["peppylib.SenderTarget.contract(\"arm_contract\", \"v2\")"],
    );
    assert!(
        !rendered.contains("SenderTarget.node("),
        "a contract-origin dep must address the producer as a contract, not a node: {rendered}"
    );
    // The action lifecycle surface is unchanged by the addressing shape.
    assert_contains_all(
        &rendered,
        &[
            "async def fire_goal(",
            "async def cancel_goal(",
            "async def on_next_feedback_message(",
            "async def get_result(",
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
        result_response: None,
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(&action, &format, &native_dep("brain", "v1", "brain"))
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
            "feedback_bound: Optional[int] = None",
            "async def get_result(",
            "peppylib.ActionMessenger.send_goal(",
            "peppylib.ActionMessenger.request_result(",
        ],
    );

    assert_rendered!(
        !rendered.contains("GoalResponseData")
            && !rendered.contains("_deserialize_goal_response")
            && !rendered.contains("handle.data"),
        &rendered,
        "no goal-response API should be generated without response_message_format"
    );
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

    // fire_goal still serializes the declared goal request and returns a handle
    // without response data.
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

    // get_result returns the typed status and no data (empty result format).
    assert_contains_all(
        &rendered,
        &[
            "status = ResultStatus(reply.status)",
            "return ResultResponse(core_node=reply.core_node, instance_id=reply.instance_id, status=status, failure=failure, cancel_reason=reply.cancel_reason)",
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
        goal_response: Some(serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT).unwrap()),
        feedback: None,
        result_response: None,
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_action(&action, &format, &native_dep("brain", "v1", "brain"))
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
            "feedback_bound: Optional[int] = None",
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
