//! The record of the state-changing calls an endpoint takes: the last
//! `keep` calls of every tool that is not read-only, tasks included, kept
//! in memory for the life of the process and answered by the record tool
//! newest first. Each entry says who called what, with what, and how it
//! ended, so a client that finds the robot in a state it did not command
//! can tell whether another client of the endpoint did it. A motion that a
//! teleoperation or another node commanded is not in it.
//!
//! The record keeps time on a wall clock of its own, also on an endpoint
//! whose clock is simulated time.

use crate::bridge::time_to_rfc3339;
use crate::clock::Clock;
use crate::failure::{CallFailure, FailureOutcome};
use crate::server::{read_only_annotations, serialized_len};
use crate::tasks::ActionExit;
use rmcp::model::{Implementation, JsonObject, Tool, object};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

/// The most bytes of a call's arguments an entry keeps as sent; larger
/// arguments are kept as their size alone.
const ARGUMENTS_KEPT_BYTES: u64 = 1024;

/// What a call is when its recording is dropped while it runs, which
/// decides what its entry says. The task manager drops the operation of a
/// task, and the recording with it, when the TTL of the task passes. The
/// runtime drops the handler of a call, and the operations of every task,
/// only when the endpoint stops, after which no client reads the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DroppedCall {
    /// A call: of a service tool, or of an action that runs inside it.
    Call,
    /// A task whose goal did not start: it waited for its confirmation.
    TaskBeforeItsGoal,
    /// A task whose goal runs. The bridge whose future is dropped stops
    /// following the goal and cancels it (see
    /// [`TaskHandler`](crate::TaskHandler)).
    TaskWithItsGoal,
}

impl DroppedCall {
    /// What the entry of a call dropped while it runs says.
    fn message(self) -> &'static str {
        match self {
            Self::Call => "the endpoint stopped before the call ended",
            Self::TaskBeforeItsGoal => "the task expired before its goal started",
            Self::TaskWithItsGoal => {
                "the task expired before its goal ended; the endpoint cancelled the goal"
            }
        }
    }
}

/// How a recorded call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Outcome {
    /// The call runs: a task that has not ended, or a goal that runs on
    /// after its client closed the call.
    Running,
    /// The goal completed, or the service answered with its response.
    Completed,
    /// The goal failed or the service gave an error reply, with the kind
    /// the provider gave; or the call did not reach an end that the
    /// provider gave: a deadline, a transport error, a goal that was
    /// abandoned, a task that expired.
    Failed,
    /// The goal ended cancelled, or the client cancelled the task before
    /// its goal started.
    Cancelled,
    /// The provider refused the goal, with the kind it gave; or the call
    /// never reached the provider: its arguments were refused, or the call
    /// could not be made.
    Refused,
}

impl From<FailureOutcome> for Outcome {
    fn from(outcome: FailureOutcome) -> Self {
        match outcome {
            FailureOutcome::Refused => Self::Refused,
            FailureOutcome::Failed => Self::Failed,
        }
    }
}

/// The client that made a call, as the request named it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ClientIdentity {
    pub(crate) name: String,
    pub(crate) version: String,
}

impl ClientIdentity {
    pub(crate) fn of(implementation: Option<Implementation>) -> Option<Self> {
        implementation.map(|implementation| Self {
            name: implementation.name,
            version: implementation.version,
        })
    }
}

/// One recorded call.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct RecordedCall {
    /// When the call started, RFC 3339 on the record's wall clock.
    started_at: String,
    client: Option<ClientIdentity>,
    tool: String,
    /// The arguments as sent, when their JSON is [`ARGUMENTS_KEPT_BYTES`]
    /// or fewer; else null.
    arguments: Option<Value>,
    /// The size of the arguments' JSON, bytes.
    arguments_bytes: u64,
    outcome: Outcome,
    /// The failure kind the provider gave a refused or failed call.
    failure_kind: Option<String>,
    /// What ended a call that did not complete: the provider's message of
    /// a refused or failed call, the reason of a cancelled one, or the
    /// endpoint's words.
    message: Option<String>,
    /// How long the call ran, once it ended.
    duration_ms: Option<u64>,
    #[serde(skip)]
    started_at_nanos: u64,
}

/// The record of one endpoint.
pub(crate) struct CallRecord {
    name: String,
    description: String,
    keep: usize,
    clock: Clock,
    calls: Mutex<VecDeque<Arc<Mutex<RecordedCall>>>>,
}

impl CallRecord {
    pub(crate) fn new(name: String, description: String, keep: u32, clock: Clock) -> Self {
        Self {
            name,
            description,
            keep: keep as usize,
            clock,
            calls: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The listing of the record tool: no argument, the calls as the
    /// output.
    pub(crate) fn tool(&self) -> Tool {
        let input =
            object(json!({ "type": "object", "properties": {}, "additionalProperties": false }));
        let output = object(json!({
            "type": "object",
            "properties": {
                "calls": {
                    "type": "array",
                    "description": "The recorded calls, newest first.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "started_at": { "type": "string", "description": "When the call started, RFC 3339, on the endpoint's wall clock." },
                            "client": {
                                "type": ["object", "null"],
                                "description": "The client that made the call, as its request named it; null when it named none.",
                                "properties": {
                                    "name": { "type": "string" },
                                    "version": { "type": "string" },
                                },
                                "required": ["name", "version"],
                            },
                            "tool": { "type": "string" },
                            "arguments": { "type": ["object", "null"], "description": format!("The arguments as sent, when their JSON is {ARGUMENTS_KEPT_BYTES} bytes or fewer; else null.") },
                            "arguments_bytes": { "type": "integer", "description": "The size of the arguments' JSON, bytes." },
                            "outcome": {
                                "type": "string",
                                "enum": ["running", "completed", "failed", "cancelled", "refused"],
                                "description": "How the call stands. `completed`: the goal completed or the service answered with its response. `failed`: the goal failed or the service gave an error reply, or the call did not reach an end that the provider gave. `refused`: the provider refused the goal, or the call never reached the provider. `cancelled`: the goal ended cancelled, or the task was cancelled before its goal started.",
                            },
                            "failure_kind": { "type": ["string", "null"], "description": "The failure kind the provider gave a refused or failed call: a kind of peppy or of the contract of the member. Null when the provider gave none." },
                            "message": { "type": ["string", "null"], "description": "What ended a call that did not complete: the provider's message of a refused or failed call, the reason of a cancelled call, or the endpoint's words. Null for a running or completed call." },
                            "duration_ms": { "type": ["integer", "null"], "description": "How long the call ran, once it ended." },
                        },
                        "required": ["started_at", "client", "tool", "arguments", "arguments_bytes", "outcome", "failure_kind", "message", "duration_ms"],
                    },
                },
            },
            "required": ["calls"],
            "additionalProperties": false,
        }));
        Tool::new(self.name.clone(), self.description.clone(), Arc::new(input))
            .with_annotations(read_only_annotations())
            .with_raw_output_schema(Arc::new(output))
    }

    /// Records the start of a call, which is `dropped` when its recording
    /// is dropped before it ends, and hands back what ends it. The oldest
    /// entry past `keep` is dropped.
    pub(crate) fn start(
        &self,
        client: Option<ClientIdentity>,
        tool: &str,
        arguments: &JsonObject,
        dropped: DroppedCall,
    ) -> Recording {
        let now = self.clock.now_nanos();
        let arguments_bytes = serialized_len(arguments);
        let arguments =
            (arguments_bytes <= ARGUMENTS_KEPT_BYTES).then(|| Value::Object(arguments.clone()));
        let call = Arc::new(Mutex::new(RecordedCall {
            started_at: time_to_rfc3339(UNIX_EPOCH + Duration::from_nanos(now)),
            client,
            tool: tool.to_string(),
            arguments,
            arguments_bytes,
            outcome: Outcome::Running,
            failure_kind: None,
            message: None,
            duration_ms: None,
            started_at_nanos: now,
        }));
        let mut calls = self.calls.lock().expect("record lock is never poisoned");
        calls.push_back(Arc::clone(&call));
        while calls.len() > self.keep {
            calls.pop_front();
        }
        Recording {
            kept: Some(Kept {
                call,
                clock: self.clock.clone(),
                dropped,
            }),
        }
    }

    /// The answer of the record tool: every kept call, newest first.
    pub(crate) fn answer(&self) -> Value {
        let calls: Vec<RecordedCall> = self
            .calls
            .lock()
            .expect("record lock is never poisoned")
            .iter()
            .rev()
            .map(|call| call.lock().expect("call lock is never poisoned").clone())
            .collect();
        json!({ "calls": calls })
    }
}

/// A call from its start to its end: the entry it writes when the endpoint
/// records it, nothing otherwise. The first end recorded stands; a
/// recording dropped before one ends its call as failed, in the words of
/// its [`DroppedCall`].
pub(crate) struct Recording {
    kept: Option<Kept>,
}

struct Kept {
    call: Arc<Mutex<RecordedCall>>,
    clock: Clock,
    dropped: DroppedCall,
}

/// How a call ended, as its entry keeps it.
struct End {
    outcome: Outcome,
    failure_kind: Option<String>,
    message: Option<String>,
}

impl End {
    /// An end that the endpoint words: no failure kind.
    fn worded(outcome: Outcome, message: String) -> Self {
        Self {
            outcome,
            failure_kind: None,
            message: Some(message),
        }
    }
}

impl Recording {
    /// The recording of a call the endpoint does not record: a read-only
    /// tool, or an endpoint without a record.
    pub(crate) fn unkept() -> Self {
        Self { kept: None }
    }

    /// The call never reached the provider.
    pub(crate) fn refused(&mut self, message: String) {
        self.end(End::worded(Outcome::Refused, message));
    }

    /// The goal of the task starts: a recording dropped from now on says
    /// that the endpoint cancelled the goal.
    pub(crate) fn goal_started(&mut self) {
        if let Some(kept) = &mut self.kept {
            kept.dropped = DroppedCall::TaskWithItsGoal;
        }
    }

    /// The call did not reach an end that the provider gave.
    pub(crate) fn failed(&mut self, message: String) {
        self.end(End::worded(Outcome::Failed, message));
    }

    /// The goal ended cancelled for `reason`, or the task was before its
    /// goal started.
    pub(crate) fn cancelled(&mut self, reason: String) {
        self.end(End::worded(Outcome::Cancelled, reason));
    }

    /// The goal completed, or the service answered with its response.
    pub(crate) fn completed(&mut self) {
        self.end(End {
            outcome: Outcome::Completed,
            failure_kind: None,
            message: None,
        });
    }

    /// The provider refused or failed the call with a failure kind and a
    /// message.
    pub(crate) fn ended_with(&mut self, failure: &CallFailure<'_>) {
        self.end(End {
            outcome: failure.outcome.into(),
            failure_kind: Some(failure.failure.kind().to_string()),
            message: Some(failure.failure.message().to_owned()),
        });
    }

    /// The end of an action: completed, refused or failed with the
    /// provider's failure, cancelled with the goal's reason, or failed in
    /// the words of the exit.
    pub(crate) fn ended_by(&mut self, outcome: &Result<Value, ActionExit>) {
        let exit = match outcome {
            Ok(_) => return self.completed(),
            Err(exit) => exit,
        };
        if let Some(failure) = exit.call_failure() {
            return self.ended_with(&failure);
        }
        match exit {
            ActionExit::Cancelled(goal) => self.cancelled(goal.reason.clone()),
            other => self.failed(other.to_string()),
        }
    }

    fn end(&mut self, end: End) {
        let Some(kept) = self.kept.take() else {
            return;
        };
        let now = kept.clock.now_nanos();
        let mut call = kept.call.lock().expect("call lock is never poisoned");
        call.outcome = end.outcome;
        call.failure_kind = end.failure_kind;
        call.message = end.message;
        call.duration_ms = Some(now.saturating_sub(call.started_at_nanos) / 1_000_000);
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if let Some(dropped) = self.kept.as_ref().map(|kept| kept.dropped) {
            self.failed(dropped.message().to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::test_support::{MS, manual_clock};
    use crate::failure::test_support::failure;
    use crate::tasks::CancelledGoal;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn record(keep: u32) -> (CallRecord, Arc<AtomicU64>) {
        let (clock, nanos) = manual_clock();
        let record = CallRecord::new(
            "robot.recent_calls".into(),
            "The record.".into(),
            keep,
            clock,
        );
        (record, nanos)
    }

    fn client(name: &str) -> Option<ClientIdentity> {
        Some(ClientIdentity {
            name: name.into(),
            version: "1.0".into(),
        })
    }

    #[test]
    fn a_call_is_recorded_from_its_start_to_its_end_newest_first() {
        let (record, nanos) = record(10);
        nanos.store(1_000 * MS, Ordering::SeqCst);
        let mut first = record.start(
            client("alice"),
            "robot.move_arm",
            &object(json!({ "arm_name": "left_arm" })),
            DroppedCall::Call,
        );
        nanos.store(1_400 * MS, Ordering::SeqCst);
        first.completed();
        nanos.store(2_000 * MS, Ordering::SeqCst);
        let mut second = record.start(
            None,
            "robot.stop",
            &object(json!({ "reason": "" })),
            DroppedCall::Call,
        );

        let answer = record.answer();
        let calls = answer["calls"].as_array().expect("calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            json!({
                "started_at": "1970-01-01T00:00:02.000000000Z",
                "client": null,
                "tool": "robot.stop",
                "arguments": { "reason": "" },
                "arguments_bytes": 13,
                "outcome": "running",
                "failure_kind": null,
                "message": null,
                "duration_ms": null,
            })
        );
        assert_eq!(
            calls[1],
            json!({
                "started_at": "1970-01-01T00:00:01.000000000Z",
                "client": { "name": "alice", "version": "1.0" },
                "tool": "robot.move_arm",
                "arguments": { "arm_name": "left_arm" },
                "arguments_bytes": 23,
                "outcome": "completed",
                "failure_kind": null,
                "message": null,
                "duration_ms": 400,
            })
        );

        nanos.store(2_500 * MS, Ordering::SeqCst);
        second.cancelled("stopped: the operator asked".into());
        // The first end stands.
        second.failed("too late".into());
        let answer = record.answer();
        assert_eq!(answer["calls"][0]["outcome"], "cancelled");
        assert_eq!(answer["calls"][0]["failure_kind"], Value::Null);
        assert_eq!(answer["calls"][0]["message"], "stopped: the operator asked");
        assert_eq!(answer["calls"][0]["duration_ms"], 500);
    }

    #[test]
    fn the_record_keeps_the_last_keep_calls_and_large_arguments_as_their_size() {
        let (record, _) = record(2);
        for index in 0..3 {
            record
                .start(
                    None,
                    &format!("tool_{index}"),
                    &object(json!({})),
                    DroppedCall::Call,
                )
                .refused("refused".into());
        }
        let answer = record.answer();
        let tools: Vec<&str> = answer["calls"]
            .as_array()
            .expect("calls")
            .iter()
            .map(|call| call["tool"].as_str().expect("tool"))
            .collect();
        assert_eq!(tools, ["tool_2", "tool_1"]);
        assert_eq!(answer["calls"][0]["outcome"], "refused");
        assert_eq!(answer["calls"][0]["failure_kind"], Value::Null);

        let large = object(json!({ "text": "x".repeat(2000) }));
        record
            .start(None, "tool_3", &large, DroppedCall::Call)
            .failed("deadline".into());
        let answer = record.answer();
        assert_eq!(answer["calls"][0]["arguments"], Value::Null);
        assert_eq!(answer["calls"][0]["arguments_bytes"], 2011);
        assert_eq!(answer["calls"][0]["outcome"], "failed");
        assert_eq!(answer["calls"][0]["message"], "deadline");
    }

    /// A recording dropped while its call runs ends the call failed, in the
    /// words of what the call was: a call, a task that waited for its
    /// confirmation, or a task whose goal ran.
    #[test]
    fn a_recording_dropped_while_running_ends_failed_saying_what_was_dropped() {
        let dropped_message = |dropped: DroppedCall, goal_started: bool| {
            let (record, _) = record(2);
            let mut recording = record.start(None, "tool", &object(json!({})), dropped);
            if goal_started {
                recording.goal_started();
            }
            drop(recording);
            let answer = record.answer();
            assert_eq!(answer["calls"][0]["outcome"], "failed");
            assert_eq!(answer["calls"][0]["failure_kind"], Value::Null);
            answer["calls"][0]["message"].clone()
        };

        assert_eq!(
            dropped_message(DroppedCall::Call, false),
            "the endpoint stopped before the call ended"
        );
        assert_eq!(
            dropped_message(DroppedCall::TaskBeforeItsGoal, false),
            "the task expired before its goal started"
        );
        assert_eq!(
            dropped_message(DroppedCall::TaskBeforeItsGoal, true),
            "the task expired before its goal ended; the endpoint cancelled the goal"
        );
    }

    /// The entry an action's exit leaves, newest first in the record.
    fn entry_of(exit: Result<Value, ActionExit>) -> Value {
        let (record, _) = record(1);
        record
            .start(
                None,
                "robot.move_arm",
                &object(json!({})),
                DroppedCall::Call,
            )
            .ended_by(&exit);
        record.answer()["calls"][0].clone()
    }

    /// The `outcome`, `failure_kind` and `message` of an entry.
    fn end_of(entry: &Value) -> Value {
        json!({
            "outcome": entry["outcome"],
            "failure_kind": entry["failure_kind"],
            "message": entry["message"],
        })
    }

    #[test]
    fn an_actions_exit_ends_its_recording_with_the_providers_kind_or_reason() {
        assert_eq!(
            end_of(&entry_of(Ok(json!({ "reached": true })))),
            json!({ "outcome": "completed", "failure_kind": null, "message": null })
        );
        assert_eq!(
            end_of(&entry_of(Err(ActionExit::Refused(failure(
                "leader_streaming",
                "a leader streams the left arm"
            ))))),
            json!({
                "outcome": "refused",
                "failure_kind": "leader_streaming",
                "message": "a leader streams the left arm",
            })
        );
        assert_eq!(
            end_of(&entry_of(Err(ActionExit::Failed(failure(
                "unreachable",
                "no plan reaches the pose"
            ))))),
            json!({
                "outcome": "failed",
                "failure_kind": "unreachable",
                "message": "no plan reaches the pose",
            })
        );
        assert_eq!(
            end_of(&entry_of(Err(ActionExit::Cancelled(CancelledGoal {
                result: json!({ "reached": false }),
                reason: "replaced by a newer goal".into(),
            })))),
            json!({
                "outcome": "cancelled",
                "failure_kind": null,
                "message": "replaced by a newer goal",
            })
        );
        assert_eq!(
            end_of(&entry_of(Err(ActionExit::Error(
                "the provider abandoned the goal".into()
            )))),
            json!({
                "outcome": "failed",
                "failure_kind": null,
                "message": "the action failed: the provider abandoned the goal",
            })
        );
    }

    #[test]
    fn a_completed_call_keeps_nothing_of_its_result() {
        // A result is data: a field named like a report of the call is no
        // part of how the call ended.
        let entry = entry_of(Ok(json!({ "success": false, "message": "no plan" })));
        assert_eq!(entry["outcome"], "completed");
        assert_eq!(entry["message"], Value::Null);
        assert!(entry.get("success").is_none(), "{entry}");
    }

    #[test]
    fn an_unkept_recording_writes_nothing() {
        let mut recording = Recording::unkept();
        recording.refused("nothing to write to".into());
        drop(recording);
    }
}
