//! The failure a provider gives: the failure kind and the message of a
//! refused goal, of a failed goal, or of the error reply of a service.
//!
//! The kind comes from the closed set of the member: a kind of peppy, or a
//! kind of its contract that the member lists. The bridge checks it
//! before it hands the failure to the runtime, so a [`ProviderFailure`]
//! always carries a kind that the member declares.
//!
//! A client reads a failure in one shape everywhere: the structured
//! content of the tool error of a call, the error data of a failed task,
//! and the entry of the call in the call record all hold the kind as
//! `failure_kind` and the message as `message`.

use peppy_config_model::failure_kind::FailureKindName;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};
use std::fmt;

/// The failure kind and the message that a provider gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    kind: FailureKindName,
    message: String,
}

impl ProviderFailure {
    pub fn new(kind: impl Into<FailureKindName>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }

    pub fn kind(&self) -> &FailureKindName {
        &self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// `kind: message`, as a readable text and a log line give it.
impl fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// How a call that a provider answered with a failure ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureOutcome {
    /// The provider refused the goal: the goal never ran.
    Refused,
    /// The goal ran and failed, or the service gave an error reply.
    Failed,
}

impl FailureOutcome {
    /// The name of the outcome, as the call record and the structured
    /// content of a failure give it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }
}

/// A failure that a provider gave, with how the call ended: what a tool
/// error and a failed task carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallFailure<'a> {
    pub(crate) outcome: FailureOutcome,
    pub(crate) failure: &'a ProviderFailure,
    /// The readable text that says what happened, the failure included.
    pub(crate) summary: String,
}

impl CallFailure<'_> {
    /// The structured form: `outcome`, `failure_kind` and `message`.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "outcome": self.outcome.name(),
            "failure_kind": self.failure.kind.as_str(),
            "message": self.failure.message,
        })
    }

    /// The tool error of a call that ended with the failure: `isError` is
    /// `true`, the text is the summary, and the structured content is
    /// [`Self::to_json`].
    pub(crate) fn into_tool_result(self) -> CallToolResult {
        let mut result = CallToolResult::structured_error(self.to_json());
        result.content = vec![ContentBlock::text(self.summary)];
        result
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// The failure of `kind`, a valid kind name, with `message`.
    pub(crate) fn failure(kind: &str, message: &str) -> ProviderFailure {
        ProviderFailure::new(
            FailureKindName::parse(kind).expect("a valid failure kind name"),
            message,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::failure;
    use super::*;

    #[test]
    fn a_failure_reads_as_its_kind_and_its_message() {
        let failure = failure("unreachable", "no plan reaches the pose");
        assert_eq!(failure.kind().as_str(), "unreachable");
        assert_eq!(failure.message(), "no plan reaches the pose");
        assert_eq!(failure.to_string(), "unreachable: no plan reaches the pose");
    }

    #[test]
    fn the_tool_error_of_a_failure_holds_the_summary_and_the_structured_failure() {
        let failure = failure("leader_streaming", "a leader streams the left arm");
        let result = CallFailure {
            outcome: FailureOutcome::Refused,
            failure: &failure,
            summary: "the provider refused the goal".to_owned(),
        }
        .into_tool_result();

        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content,
            Some(json!({
                "outcome": "refused",
                "failure_kind": "leader_streaming",
                "message": "a leader streams the left arm",
            }))
        );
        assert_eq!(
            result.content,
            vec![ContentBlock::text("the provider refused the goal")]
        );
    }
}
