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

use crate::clock::Clock;
use crate::tasks::ActionExit;
use rmcp::model::{Implementation, JsonObject, Tool};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// The most bytes of a call's arguments an entry keeps as sent; larger
/// arguments are kept as their size alone.
const ARGUMENTS_KEPT_BYTES: usize = 1024;

/// What a recording that is dropped while its call runs says: the task
/// was cancelled or aborted, or the call's connection closed, before the
/// call reached an end of its own.
const CANCELLED_BEFORE_ITS_END: &str = "the call was cancelled before it ended";

/// How a recorded call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Outcome {
    /// The call runs: a task that has not ended.
    Running,
    /// The provider answered; `success` and `message` say what it
    /// answered.
    Completed,
    /// The call did not reach an answer: a deadline, a provider error, a
    /// goal that failed.
    Failed,
    /// The goal ended cancelled, or the task was cancelled.
    Cancelled,
    /// The call never reached the provider: its arguments were refused, or
    /// the call could not be made.
    Refused,
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

/// What a completed call reports: the `success` and `message` members of
/// its result, when the result has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Completion {
    success: Option<bool>,
    message: String,
}

impl Completion {
    pub(crate) fn of(result: &Value) -> Self {
        Self {
            success: result.get("success").and_then(Value::as_bool),
            message: result
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }
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
    /// The result's `success` member, when the call completed with one.
    success: Option<bool>,
    message: String,
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
        let Value::Object(input) =
            json!({ "type": "object", "properties": {}, "additionalProperties": false })
        else {
            unreachable!("the input schema is an object");
        };
        let Value::Object(output) = json!({
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
                            "arguments": { "type": ["object", "null"], "description": "The arguments as sent, when their JSON is 1024 bytes or fewer; else null." },
                            "arguments_bytes": { "type": "integer", "description": "The size of the arguments' JSON, bytes." },
                            "outcome": { "type": "string", "enum": ["running", "completed", "failed", "cancelled", "refused"] },
                            "success": { "type": ["boolean", "null"], "description": "The result's `success` member of a completed call that has one: a completed call with success false ran to its end and reports a failure." },
                            "message": { "type": "string" },
                            "duration_ms": { "type": ["integer", "null"], "description": "How long the call ran, once it ended." },
                        },
                        "required": ["started_at", "client", "tool", "arguments", "arguments_bytes", "outcome", "success", "message", "duration_ms"],
                    },
                },
            },
            "required": ["calls"],
            "additionalProperties": false,
        }) else {
            unreachable!("the output schema is an object");
        };
        Tool::new(self.name.clone(), self.description.clone(), Arc::new(input))
            .with_annotations(
                rmcp::model::ToolAnnotations::default()
                    .read_only(true)
                    .destructive(false),
            )
            .with_raw_output_schema(Arc::new(output))
    }

    /// Records the start of a call and hands back what ends it. The oldest
    /// entry past `keep` is dropped.
    pub(crate) fn start(
        &self,
        client: Option<ClientIdentity>,
        tool: &str,
        arguments: &JsonObject,
    ) -> Recording {
        let now = self.clock.now_nanos();
        let arguments_json = serde_json::to_string(arguments).expect("JSON object serializes");
        let arguments_bytes = arguments_json.len() as u64;
        let arguments = (arguments_json.len() <= ARGUMENTS_KEPT_BYTES)
            .then(|| Value::Object(arguments.clone()));
        let call = Arc::new(Mutex::new(RecordedCall {
            started_at: rfc3339(now),
            client,
            tool: tool.to_string(),
            arguments,
            arguments_bytes,
            outcome: Outcome::Running,
            success: None,
            message: String::new(),
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
/// recording dropped before one ends its call as cancelled, in the words
/// of [`CANCELLED_BEFORE_ITS_END`].
pub(crate) struct Recording {
    kept: Option<Kept>,
}

struct Kept {
    call: Arc<Mutex<RecordedCall>>,
    clock: Clock,
}

impl Recording {
    /// The recording of a call the endpoint does not record: a read-only
    /// tool, or an endpoint without a record.
    pub(crate) fn unkept() -> Self {
        Self { kept: None }
    }

    /// The call never reached the provider.
    pub(crate) fn refused(&mut self, message: String) {
        self.end(Outcome::Refused, None, message);
    }

    /// The call did not reach an answer.
    pub(crate) fn failed(&mut self, message: String) {
        self.end(Outcome::Failed, None, message);
    }

    /// The goal ended cancelled, or the task was.
    pub(crate) fn cancelled(&mut self, message: String) {
        self.end(Outcome::Cancelled, None, message);
    }

    /// The provider answered, with what `completion` reports.
    pub(crate) fn completed(&mut self, completion: Completion) {
        self.end(Outcome::Completed, completion.success, completion.message);
    }

    /// The end of an action: completed with the result's report, cancelled
    /// with the words a task's status message carries, or failed with the
    /// exit's words.
    pub(crate) fn ended_by(&mut self, outcome: &Result<Value, ActionExit>) {
        match outcome {
            Ok(value) => self.completed(Completion::of(value)),
            Err(ActionExit::Cancelled(goal)) => self.cancelled(goal.status_message()),
            Err(exit @ ActionExit::Failed(_)) => self.failed(exit.to_string()),
        }
    }

    fn end(&mut self, outcome: Outcome, success: Option<bool>, message: String) {
        let Some(kept) = self.kept.take() else {
            return;
        };
        let now = kept.clock.now_nanos();
        let mut call = kept.call.lock().expect("call lock is never poisoned");
        call.outcome = outcome;
        call.success = success;
        call.message = message;
        call.duration_ms = Some(now.saturating_sub(call.started_at_nanos) / 1_000_000);
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.cancelled(CANCELLED_BEFORE_ITS_END.to_string());
    }
}

/// Nanoseconds since the Unix epoch as an RFC 3339 instant in UTC, to the
/// millisecond.
fn rfc3339(nanos: u64) -> String {
    let seconds = nanos / 1_000_000_000;
    let millis = (nanos % 1_000_000_000) / 1_000_000;
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let of_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        of_day / 3600,
        (of_day % 3600) / 60,
        of_day % 60
    )
}

/// The proleptic Gregorian date of a day count since 1970-01-01, by the
/// civil-from-days algorithm of Howard Hinnant.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::test_support::manual_clock;
    use crate::tasks::CancelledGoal;
    use std::sync::atomic::{AtomicU64, Ordering};

    const MS: u64 = 1_000_000;

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

    fn arguments(value: Value) -> JsonObject {
        match value {
            Value::Object(fields) => fields,
            _ => panic!("arguments are an object"),
        }
    }

    fn client(name: &str) -> Option<ClientIdentity> {
        Some(ClientIdentity {
            name: name.into(),
            version: "1.0".into(),
        })
    }

    #[test]
    fn instants_are_rfc3339_in_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339(1_790_784_000 * 1_000_000_000 + 250 * MS),
            "2026-09-30T16:00:00.250Z"
        );
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn a_call_is_recorded_from_its_start_to_its_end_newest_first() {
        let (record, nanos) = record(10);
        nanos.store(1_000 * MS, Ordering::SeqCst);
        let mut first = record.start(
            client("alice"),
            "robot.move_arm",
            &arguments(json!({ "arm_name": "left_arm" })),
        );
        nanos.store(1_400 * MS, Ordering::SeqCst);
        first.completed(Completion::of(
            &json!({ "success": false, "message": "no plan reaches the pose" }),
        ));
        nanos.store(2_000 * MS, Ordering::SeqCst);
        let mut second = record.start(None, "robot.stop", &arguments(json!({ "reason": "" })));

        let answer = record.answer();
        let calls = answer["calls"].as_array().expect("calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["tool"], "robot.stop");
        assert_eq!(calls[0]["outcome"], "running");
        assert_eq!(calls[0]["client"], Value::Null);
        assert_eq!(calls[0]["success"], Value::Null);
        assert_eq!(calls[0]["duration_ms"], Value::Null);
        assert_eq!(calls[0]["started_at"], "1970-01-01T00:00:02.000Z");
        assert_eq!(calls[1]["tool"], "robot.move_arm");
        assert_eq!(calls[1]["outcome"], "completed");
        assert_eq!(calls[1]["success"], false);
        assert_eq!(calls[1]["message"], "no plan reaches the pose");
        assert_eq!(calls[1]["duration_ms"], 400);
        assert_eq!(calls[1]["client"]["name"], "alice");
        assert_eq!(calls[1]["arguments"]["arm_name"], "left_arm");
        assert_eq!(calls[1]["arguments_bytes"], 23);

        nanos.store(2_500 * MS, Ordering::SeqCst);
        second.cancelled("stopped: the operator asked".into());
        // The first end stands.
        second.failed("too late".into());
        let answer = record.answer();
        assert_eq!(answer["calls"][0]["outcome"], "cancelled");
        assert_eq!(answer["calls"][0]["message"], "stopped: the operator asked");
        assert_eq!(answer["calls"][0]["duration_ms"], 500);
    }

    #[test]
    fn the_record_keeps_the_last_keep_calls_and_large_arguments_as_their_size() {
        let (record, _) = record(2);
        for index in 0..3 {
            record
                .start(None, &format!("tool_{index}"), &arguments(json!({})))
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

        let large = arguments(json!({ "text": "x".repeat(2000) }));
        record
            .start(None, "tool_3", &large)
            .failed("deadline".into());
        let answer = record.answer();
        assert_eq!(answer["calls"][0]["arguments"], Value::Null);
        assert_eq!(answer["calls"][0]["arguments_bytes"], 2011);
        assert_eq!(answer["calls"][0]["outcome"], "failed");
    }

    #[test]
    fn a_recording_dropped_while_running_ends_as_cancelled() {
        let (record, _) = record(2);
        let recording = record.start(None, "tool", &arguments(json!({})));
        drop(recording);
        let answer = record.answer();
        assert_eq!(answer["calls"][0]["outcome"], "cancelled");
        assert_eq!(answer["calls"][0]["message"], CANCELLED_BEFORE_ITS_END);
    }

    #[test]
    fn an_actions_exit_ends_its_recording_in_the_words_of_the_exit() {
        let (record, _) = record(3);
        record
            .start(None, "done", &arguments(json!({})))
            .ended_by(&Ok(json!({ "success": true, "message": "at the pose" })));
        record
            .start(None, "cancelled", &arguments(json!({})))
            .ended_by(&Err(ActionExit::Cancelled(CancelledGoal {
                result: json!({ "success": false }),
                reason: Some("no progress within 2000 ms".into()),
            })));
        record
            .start(None, "failed", &arguments(json!({})))
            .ended_by(&Err(ActionExit::Failed(
                "the provider abandoned the goal".into(),
            )));
        let answer = record.answer();
        assert_eq!(answer["calls"][2]["outcome"], "completed");
        assert_eq!(answer["calls"][2]["success"], true);
        assert_eq!(answer["calls"][2]["message"], "at the pose");
        assert_eq!(answer["calls"][1]["outcome"], "cancelled");
        assert_eq!(
            answer["calls"][1]["message"],
            "the action was cancelled: no progress within 2000 ms: {\"success\":false}"
        );
        assert_eq!(answer["calls"][0]["outcome"], "failed");
        assert_eq!(
            answer["calls"][0]["message"],
            "the action failed: the provider abandoned the goal"
        );
    }

    #[test]
    fn an_unkept_recording_writes_nothing() {
        let mut recording = Recording::unkept();
        recording.refused("nothing to write to".into());
        drop(recording);
    }
}
