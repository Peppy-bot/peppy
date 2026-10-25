//! `peppy node call <instance> <member>`: calls one service or action of a
//! running instance as a client with no node, and writes how the call ends
//! as JSON lines on standard output.
//!
//! The command reads the pinned documents of the instance from the daemon,
//! builds the request or the goal from `--args`, `--set` and `--file` (see
//! [`super::request`]), and sends it itself with the codec and the clients
//! of the MCP server, as the sender of its process: a provider cancels the
//! goals of a sender that is gone.
//!
//! The output is a `{"feedback": ...}` line for each feedback message of a
//! goal, then one line that says how the call ended, with an `outcome`:
//! `completed` with the `response` of a service or the `result` of a goal,
//! `refused` or `failed` with the `failure_kind` and the `message` of the
//! provider, `cancelled` with the `reason` and the `result`, `abandoned` or
//! `expired` with a `message`. An error reply of a service is a refusal by
//! its provider. The command exits with the code of the end (see
//! [`CallEnd`]).
//!
//! A Ctrl-C during a goal sends the provider a cancel, and the command
//! follows the goal to its end. A second Ctrl-C ends the command at once
//! with code 130: its process ends, so the provider cancels the goal of a
//! sender that is gone. A Ctrl-C while a service call waits for its response
//! ends the command at once with code 130.

use super::interfaces::{Member, MemberAddress, SelectedMember};
use super::json_lines::write_json_line;
use super::request::MessageArguments;
use crate::commands::action_poll::{CtrlC, Interrupts};
use crate::context::AppContext;
use crate::error::{CallEnd, Error, Result};
use config::node::{InterfaceKind, MessageFormat};
use message_codec::consumer::{
    ActionClient, ConsumerError, ConsumerIdentity, GoalHandle, GoalOutcome, MemberBinding,
    ServiceClient,
};
use message_codec::{ConversionError, payload_format};
use peppylib::PeppyError;
use peppylib::messaging::{
    CancelState, DEFAULT_FEEDBACK_BOUND, Failure, MessengerHandle, ProducerRef,
};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::warn;

/// How long a provider has to admit a goal.
const ADMISSION_TIMEOUT: Duration = crate::commands::GOAL_TIMEOUT;

/// How long a provider has to answer the cancel of a goal.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one request for the end of a goal waits before the command
/// asks again. The provider answers the request at the end of the goal, so
/// the command follows a goal of any length.
const RESULT_WAIT: Duration = Duration::from_secs(60);

/// What `peppy node call` calls.
pub struct CallParams {
    pub instance_id: String,
    pub member: String,
    pub message: MessageArguments,
}

pub fn call(ctx: &Arc<AppContext>, params: CallParams) -> Result<()> {
    crate::commands::block_on(call_writing(ctx, params, &mut std::io::stdout()))
}

/// `peppy node call`, writing its lines to `out`.
pub async fn call_writing(
    ctx: &Arc<AppContext>,
    params: CallParams,
    out: &mut impl Write,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;
    let interfaces = super::interfaces::fetch(&conn, &params.instance_id).await?;
    let selected = interfaces
        .select(
            &params.member,
            &[InterfaceKind::Service, InterfaceKind::Action],
        )
        .map_err(Error::ExecutionFailed)?;
    let call = MemberCall::new(&selected, &params.message)?;
    let caller = super::interfaces::command_line_caller(&conn);
    call.run(conn.messenger, &caller, &mut CtrlC::listen(), out)
        .await
}

/// One call of a service or an action of an instance, ready to send: the
/// client of the member, where it is served, and the message.
pub(crate) struct MemberCall {
    /// The member as `<qualifier>.<name>`, which the messages of the command
    /// name.
    name: String,
    binding: MemberBinding,
    producer: ProducerRef,
    message: Value,
    client: CallClient,
}

enum CallClient {
    Service(ServiceClient),
    /// `reports_feedback`: the action declares a feedback message, whose
    /// stream the provider closes at the end of a goal.
    Action {
        client: ActionClient,
        reports_feedback: bool,
    },
}

impl MemberCall {
    /// The call of `selected` with the message `arguments` give, its codecs
    /// laid out from the formats of the pinned manifest.
    pub fn new(selected: &SelectedMember<'_>, arguments: &MessageArguments) -> Result<Self> {
        let name = selected.qualified_name.clone();
        let MemberAddress::Interface { target } = &selected.address else {
            return Err(topic_is_not_called(&name));
        };
        let codec = |side: &str, format: Option<&MessageFormat>| {
            payload_format(format)
                .map(|format| selected.codec(side, format))
                .transpose()
        };
        let (client, message_format) = match selected.member {
            Member::Service(service) => {
                let request = service.service.request_message_format.as_ref();
                let client = ServiceClient::new(
                    codec("request", request)?,
                    codec("response", service.service.response_message_format.as_ref())?,
                    service.failure_kinds.clone(),
                );
                (CallClient::Service(client), request)
            }
            Member::Action(action) => {
                let action_shape = &action.action;
                let goal = action_shape
                    .goal_service
                    .as_ref()
                    .and_then(|goal| goal.request_message_format.as_ref());
                let feedback = codec(
                    "feedback",
                    action_shape
                        .feedback_topic
                        .as_ref()
                        .map(|feedback| &feedback.message_format),
                )?;
                let reports_feedback = feedback.is_some();
                let client = ActionClient::new(
                    codec("goal", goal)?,
                    feedback,
                    codec(
                        "result",
                        action_shape
                            .result_service
                            .as_ref()
                            .and_then(|result| result.response_message_format.as_ref()),
                    )?,
                    action.failure_kinds.clone(),
                );
                let client = CallClient::Action {
                    client,
                    reports_feedback,
                };
                (client, goal)
            }
            Member::Topic(_) => return Err(topic_is_not_called(&name)),
        };
        let message = arguments
            .build(message_format)
            .map_err(|reason| Error::ExecutionFailed(format!("`{name}`: {reason}")))?;
        Ok(Self {
            binding: MemberBinding {
                target: target.clone(),
                member: selected.member.name().to_owned(),
            },
            producer: selected.producer.clone(),
            name,
            message,
            client,
        })
    }

    /// Sends the call as `caller` and follows it to its end, writing its
    /// lines to `out`; `interrupts` gives the Ctrl-C presses.
    pub async fn run(
        &self,
        messenger: &MessengerHandle,
        caller: &ConsumerIdentity,
        interrupts: &mut impl Interrupts,
        out: &mut impl Write,
    ) -> Result<()> {
        match &self.client {
            CallClient::Service(client) => {
                self.call_service(client, messenger, caller, interrupts, out)
                    .await
            }
            CallClient::Action {
                client,
                reports_feedback,
            } => {
                let mut goal = self.send_goal(client, messenger, caller, out).await?;
                let outcome = self
                    .follow(&mut goal, *reports_feedback, messenger, interrupts, out)
                    .await?;
                self.report_outcome(outcome, out)
            }
        }
    }

    /// Calls the service and waits for its response, or for a Ctrl-C.
    async fn call_service(
        &self,
        client: &ServiceClient,
        messenger: &MessengerHandle,
        caller: &ConsumerIdentity,
        interrupts: &mut impl Interrupts,
        out: &mut impl Write,
    ) -> Result<()> {
        let response = tokio::select! {
            biased;
            () = interrupts.next() => {
                return Err(Error::Interrupted(format!(
                    "Stopped waiting for the response of `{}`",
                    self.name
                )));
            }
            response = client.call(
                messenger,
                caller,
                &self.binding,
                &self.producer,
                &self.message,
                None,
            ) => response,
        };
        let error = match response {
            Ok(response) => {
                write_json_line(
                    out,
                    &json!({ "outcome": "completed", "response": response }),
                )?;
                return Ok(());
            }
            Err(error) => error,
        };
        if let Some(failure) = provider_failure(&error) {
            return Err(self.refusal(failure, out)?);
        }
        Err(Error::ExecutionFailed(format!(
            "The call of `{}` failed: {error}",
            self.name
        )))
    }

    /// Sends the goal and reads its admission: a refused goal ends the call.
    async fn send_goal(
        &self,
        client: &ActionClient,
        messenger: &MessengerHandle,
        caller: &ConsumerIdentity,
        out: &mut impl Write,
    ) -> Result<GoalHandle> {
        let goal = client
            .fire_goal(
                messenger,
                caller,
                &self.binding,
                &self.producer,
                &self.message,
                DEFAULT_FEEDBACK_BOUND,
                ADMISSION_TIMEOUT,
            )
            .await
            .map_err(|error| {
                Error::ExecutionFailed(format!("The goal of `{}` was not sent: {error}", self.name))
            })?;
        if let Some(refusal) = goal.refusal() {
            return Err(self.refusal(refusal, out)?);
        }
        Ok(goal)
    }

    /// Follows the goal to its end: writes each feedback message, sends the
    /// provider a cancel at the first Ctrl-C, and stops at the second.
    async fn follow(
        &self,
        goal: &mut GoalHandle,
        reports_feedback: bool,
        messenger: &MessengerHandle,
        interrupts: &mut impl Interrupts,
        out: &mut impl Write,
    ) -> Result<std::result::Result<GoalOutcome, ConsumerError>> {
        let mut feedback_open = reports_feedback;
        let mut cancel: Option<JoinHandle<std::result::Result<CancelState, ConsumerError>>> = None;
        let mut cancel_sent = false;
        loop {
            let step = tokio::select! {
                biased;
                () = interrupts.next() => {
                    if cancel_sent {
                        return Err(Error::Interrupted(format!(
                            "Stopped waiting for the end of the goal of `{}`; its provider \
                             cancels the goal as this command ends",
                            self.name
                        )));
                    }
                    cancel_sent = true;
                    warn!(
                        "Cancelling the goal of `{}`; press Ctrl-C again to stop at once",
                        self.name
                    );
                    cancel = Some(tokio::spawn(goal.cancel_request(messenger, CANCEL_TIMEOUT)));
                    continue;
                }
                reply = async { cancel.as_mut().expect("a cancel is on its way").await },
                    if cancel.is_some() =>
                {
                    cancel = None;
                    if self.after_cancel(reply) == AfterCancel::ReadTheEnd {
                        feedback_open = false;
                    }
                    continue;
                }
                step = next_step(goal, &mut feedback_open, messenger) => step,
            };
            match step {
                Step::Feedback(feedback) => {
                    write_json_line(out, &json!({ "feedback": feedback }))?;
                }
                Step::Unconvertible(error) => warn!(
                    "A feedback message of `{}` does not convert, and is not shown: {error}",
                    self.name
                ),
                Step::FeedbackEnded => {}
                Step::FellBehind { bound } => {
                    return Err(Error::ExecutionFailed(format!(
                        "The command fell behind the feedback of `{}` by more than {bound} unread \
                         messages, so it cancelled the goal",
                        self.name
                    )));
                }
                Step::Ended(outcome) => return Ok(outcome),
            }
        }
    }

    /// Reads the provider's reply to the cancel: a goal that had already
    /// ended has its end read at once. A provider that does not know the
    /// goal, or a cancel that did not reach it, is a warning, and the command
    /// goes on following the goal.
    fn after_cancel(
        &self,
        reply: std::result::Result<
            std::result::Result<CancelState, ConsumerError>,
            tokio::task::JoinError,
        >,
    ) -> AfterCancel {
        match reply {
            Ok(Ok(CancelState::Signalled)) => AfterCancel::KeepFollowing,
            Ok(Ok(CancelState::AlreadyTerminal)) => AfterCancel::ReadTheEnd,
            Ok(Ok(CancelState::Unknown)) => {
                warn!(
                    "The provider of `{}` does not know the goal it was asked to cancel",
                    self.name
                );
                AfterCancel::KeepFollowing
            }
            Ok(Err(error)) => {
                warn!(
                    "The cancel of the goal of `{}` did not reach its provider: {error}",
                    self.name
                );
                AfterCancel::KeepFollowing
            }
            Err(error) => {
                warn!(
                    "The cancel of the goal of `{}` did not run: {error}",
                    self.name
                );
                AfterCancel::KeepFollowing
            }
        }
    }

    /// Writes how the goal ended, and gives the end of the command.
    fn report_outcome(
        &self,
        outcome: std::result::Result<GoalOutcome, ConsumerError>,
        out: &mut impl Write,
    ) -> Result<()> {
        let name = &self.name;
        let outcome = outcome.map_err(|error| {
            Error::ExecutionFailed(format!(
                "Cannot read how the goal of `{name}` ended: {error}"
            ))
        })?;
        match outcome {
            GoalOutcome::Completed(result) => {
                write_json_line(out, &json!({ "outcome": "completed", "result": result }))?;
                Ok(())
            }
            GoalOutcome::Cancelled { reason, result } => {
                write_json_line(
                    out,
                    &json!({ "outcome": "cancelled", "reason": reason, "result": result }),
                )?;
                Err(Error::ended(
                    CallEnd::Cancelled,
                    format!("The goal of `{name}` was {reason}"),
                ))
            }
            GoalOutcome::Failed(failure) => {
                write_failure_line(out, "failed", &failure)?;
                Err(Error::ended(
                    CallEnd::Failed,
                    format!(
                        "The goal of `{name}` failed ({}): {}",
                        failure.kind(),
                        failure.message()
                    ),
                ))
            }
            GoalOutcome::Abandoned => {
                let message = "the provider abandoned the goal";
                write_json_line(out, &json!({ "outcome": "abandoned", "message": message }))?;
                Err(Error::ended(
                    CallEnd::Abandoned,
                    format!("The goal of `{name}` ended without a result: {message}"),
                ))
            }
            GoalOutcome::Expired => {
                let message = "the goal ended, and its result is no longer readable";
                write_json_line(out, &json!({ "outcome": "expired", "message": message }))?;
                Err(Error::ended(
                    CallEnd::Expired,
                    format!("The goal of `{name}` expired: {message}"),
                ))
            }
        }
    }

    /// Writes the refusal of the provider, and gives the end of the command.
    fn refusal(&self, failure: &Failure, out: &mut impl Write) -> Result<Error> {
        write_failure_line(out, "refused", failure)?;
        Ok(Error::ended(
            CallEnd::Refused,
            format!(
                "`{}` refused ({}): {}",
                self.name,
                failure.kind(),
                failure.message()
            ),
        ))
    }
}

/// What the provider's reply to a cancel leaves the command to do.
#[derive(Debug, PartialEq, Eq)]
enum AfterCancel {
    KeepFollowing,
    ReadTheEnd,
}

/// One step of a followed goal.
enum Step {
    Feedback(Value),
    /// A feedback message that does not convert: the provider sent it for
    /// the goal all the same, and the goal goes on.
    Unconvertible(ConversionError),
    /// The provider closed the feedback stream at the end of the goal, or
    /// is gone.
    FeedbackEnded,
    /// The command fell behind the feedback by more than `bound` unread
    /// messages, and the handle of the goal cancelled it.
    FellBehind {
        bound: usize,
    },
    /// The end of the goal, or why it cannot be read.
    Ended(std::result::Result<GoalOutcome, ConsumerError>),
}

/// The next step of `goal`: its next feedback message while the stream is
/// open, then its end. The provider answers a request for the end at the end
/// of the goal, so a request that times out is sent again. Cancel safe: a
/// dropped call leaves `feedback_open` as it was, and the next call asks
/// again.
async fn next_step(
    goal: &mut GoalHandle,
    feedback_open: &mut bool,
    messenger: &MessengerHandle,
) -> Step {
    if *feedback_open {
        return match goal.next_feedback().await {
            Ok(Some(feedback)) => Step::Feedback(feedback),
            Err(ConsumerError::Conversion(error)) => Step::Unconvertible(error),
            Err(ConsumerError::Messaging(PeppyError::ActionFeedbackOverflow { bound, .. })) => {
                Step::FellBehind { bound }
            }
            Ok(None) | Err(_) => {
                *feedback_open = false;
                Step::FeedbackEnded
            }
        };
    }
    loop {
        match goal.result(messenger, RESULT_WAIT).await {
            Err(ConsumerError::Messaging(PeppyError::ActionResultTimeout { .. })) => continue,
            outcome => return Step::Ended(outcome),
        }
    }
}

/// The failure of the provider that `error` carries: an error reply with a
/// kind the member declares.
fn provider_failure(error: &ConsumerError) -> Option<&Failure> {
    match error {
        ConsumerError::Messaging(messaging) => messaging.failure(),
        ConsumerError::Conversion(_) | ConsumerError::UndeclaredFailureKind { .. } => None,
    }
}

/// The line of a refused or a failed call: the `outcome`, and the
/// `failure_kind` and the `message` of the provider, as the MCP server gives
/// them.
fn write_failure_line(out: &mut impl Write, outcome: &str, failure: &Failure) -> Result<()> {
    write_json_line(
        out,
        &json!({
            "outcome": outcome,
            "failure_kind": failure.kind().as_str(),
            "message": failure.message(),
        }),
    )?;
    Ok(())
}

fn topic_is_not_called(name: &str) -> Error {
    Error::ExecutionFailed(format!(
        "`{name}` is a topic: `peppy node echo` prints its messages"
    ))
}

/// The ends of a call, each with its line and its exit code, against
/// providers of the instance of [`super::test_fixtures`] on the in-process
/// mock messaging. The presses come from the test, at fixed points.
#[cfg(test)]
mod tests {
    use super::super::interfaces::InstanceInterfaces;
    use super::super::test_fixtures::{
        CORE_NODE, INSTANCE, caller, codec, documents, mock_messenger, payload,
    };
    use super::*;
    use crate::commands::action_poll::test_support::{
        Presses, next_goal, presses, presses_on_demand,
    };
    use config::failure_kind::FailureKindName;
    use peppylib::Payload;
    use peppylib::messaging::{
        CancelCause, ConcurrentAction, GoalContext, NonEmptyPayload, SenderTarget, ServiceAnswer,
        ServiceMessenger,
    };
    use serde_json::json;

    const FEEDBACK: &str = r#"{ progress: "f64" }"#;
    const RESULT: &str = r#"{ reached: "bool" }"#;

    fn unreachable() -> Failure {
        Failure::new(
            FailureKindName::parse("unreachable").expect("a kind name"),
            "no plan to the target",
        )
    }

    /// The call of `member` with the fields `settings` give.
    fn member_call(member: &str, settings: &[&str]) -> MemberCall {
        let interfaces = InstanceInterfaces::from_documents(&documents()).expect("the interfaces");
        let selected = interfaces
            .select(member, &[InterfaceKind::Service, InterfaceKind::Action])
            .expect("the member");
        let arguments = MessageArguments {
            args: None,
            settings: settings
                .iter()
                .map(|s| s.parse().expect("a setting"))
                .collect(),
            files: Vec::new(),
        };
        MemberCall::new(&selected, &arguments).expect("the call builds")
    }

    /// The lines the command wrote.
    fn lines(out: &[u8]) -> Vec<Value> {
        std::str::from_utf8(out)
            .expect("UTF-8 output")
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON line"))
            .collect()
    }

    /// The instance's `check_move` service, which answers each request with
    /// `answer`.
    async fn serve_check_move(
        messenger: &MessengerHandle,
        answer: impl Fn(Value) -> ServiceAnswer + Send + Sync + 'static,
    ) {
        let mut endpoint = ServiceMessenger::listen(
            messenger,
            CORE_NODE,
            INSTANCE,
            SenderTarget::contract("limb_motion", "v1").expect("a target"),
            "check_move",
        )
        .await
        .expect("the service listens");
        let request = codec(r#"{ joints: { $type: "array", $items: "f64" } }"#);
        tokio::spawn(async move {
            let _ = endpoint
                .handle_requests(|context| {
                    let decoded = request
                        .decode(context.message().payload_bytes().as_ref())
                        .expect("the request decodes");
                    let answered = answer(decoded);
                    async move { answered }
                })
                .await;
        });
    }

    /// The instance's `move_to` action, on its goal engine.
    async fn move_to_provider(messenger: &MessengerHandle) -> ConcurrentAction {
        ConcurrentAction::expose(
            messenger,
            CORE_NODE,
            INSTANCE,
            SenderTarget::contract("limb_motion", "v1").expect("a target"),
            "move_to",
            true,
        )
        .await
        .expect("the action is exposed")
    }

    /// Accepts the next goal of `engine` after it checks its request.
    async fn accept_move_to(engine: &mut ConcurrentAction) -> GoalContext {
        let goal = next_goal(engine).await;
        let request = codec(r#"{ joints: { $type: "array", $items: "f64", $length: 3 } }"#)
            .decode(goal.request_bytes())
            .expect("the goal decodes");
        assert_eq!(request, json!({"joints": [0.1, 0.2, 0.3]}));
        goal.accept(Payload::new())
            .await
            .expect("the goal is accepted")
    }

    async fn run(
        call: &MemberCall,
        messenger: &MessengerHandle,
        presses: &mut Presses,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        call.run(messenger, &caller(), presses, out).await
    }

    const JOINTS: &str = "joints=[0.1, 0.2, 0.3]";

    #[tokio::test]
    async fn a_service_that_responds_gives_its_response_and_exit_code_0() {
        let messenger = mock_messenger().await;
        serve_check_move(&messenger, |request| {
            assert_eq!(request, json!({"joints": [0.1, 0.2, 0.3]}));
            Ok(payload(
                r#"{ reachable: "bool" }"#,
                json!({"reachable": true}),
            ))
        })
        .await;
        let mut out = Vec::new();

        run(
            &member_call("check_move", &[JOINTS]),
            &messenger,
            &mut presses(0),
            &mut out,
        )
        .await
        .expect("the service responds");

        assert_eq!(
            lines(&out),
            [json!({"outcome": "completed", "response": {"reachable": true}})]
        );
    }

    /// A service of the node's own is served under the node's name and tag,
    /// with the formats its manifest declares.
    #[tokio::test]
    async fn a_service_of_the_nodes_own_gives_its_response() {
        let messenger = mock_messenger().await;
        let mut endpoint = ServiceMessenger::listen(
            &messenger,
            CORE_NODE,
            INSTANCE,
            SenderTarget::node("arm", "v1").expect("a target"),
            "set_gain",
        )
        .await
        .expect("the service listens");
        tokio::spawn(async move {
            let _ = endpoint
                .handle_requests(|context| {
                    let request = codec(r#"{ value: "i32" }"#)
                        .decode(context.message().payload_bytes().as_ref())
                        .expect("the request decodes");
                    let answer = Ok(payload(
                        r#"{ current_value: "i32" }"#,
                        json!({"current_value": request["value"]}),
                    ));
                    async move { answer }
                })
                .await;
        });
        let mut out = Vec::new();

        run(
            &member_call("arm.set_gain", &["value=-4"]),
            &messenger,
            &mut presses(0),
            &mut out,
        )
        .await
        .expect("the service responds");

        assert_eq!(
            lines(&out),
            [json!({"outcome": "completed", "response": {"current_value": -4}})]
        );
    }

    /// An error reply of a service is a refusal by its provider.
    #[tokio::test]
    async fn an_error_reply_is_a_refusal_with_its_kind_and_exit_code_3() {
        let messenger = mock_messenger().await;
        serve_check_move(&messenger, |_| Err(unreachable())).await;
        let mut out = Vec::new();

        let error = run(
            &member_call("check_move", &[JOINTS]),
            &messenger,
            &mut presses(0),
            &mut out,
        )
        .await
        .expect_err("the provider refuses");

        assert_eq!(error.exit_code(), CallEnd::Refused.exit_code());
        assert_eq!(
            error.to_string(),
            "`limb_motion.check_move` refused (unreachable): no plan to the target"
        );
        assert_eq!(
            lines(&out),
            [json!({
                "outcome": "refused",
                "failure_kind": "unreachable",
                "message": "no plan to the target",
            })]
        );
    }

    /// A goal that completes gives each feedback message, then its result.
    #[tokio::test]
    async fn a_goal_that_completes_gives_its_feedback_then_its_result_and_exit_code_0() {
        let messenger = mock_messenger().await;
        let mut engine = move_to_provider(&messenger).await;
        let call = member_call("move_to", &[JOINTS]);
        let mut out = Vec::new();

        let mut no_press = presses(0);
        let (ended, ()) = tokio::join!(run(&call, &messenger, &mut no_press, &mut out), async {
            let context = accept_move_to(&mut engine).await;
            for progress in [0.5, 1.0] {
                let feedback =
                    NonEmptyPayload::try_new(payload(FEEDBACK, json!({"progress": progress})))
                        .expect("a feedback message");
                context
                    .publish_feedback(feedback)
                    .await
                    .expect("the feedback goes out");
            }
            context
                .complete(payload(RESULT, json!({"reached": true})))
                .await
                .expect("the goal completes");
        });

        ended.expect("the goal completed");
        assert_eq!(
            lines(&out),
            [
                json!({"feedback": {"progress": 0.5}}),
                json!({"feedback": {"progress": 1.0}}),
                json!({"outcome": "completed", "result": {"reached": true}}),
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_goal_gives_its_kind_and_exit_code_3() {
        let messenger = mock_messenger().await;
        let mut engine = move_to_provider(&messenger).await;
        let call = member_call("move_to", &[JOINTS]);
        let mut out = Vec::new();

        let mut no_press = presses(0);
        let (ended, ()) = tokio::join!(run(&call, &messenger, &mut no_press, &mut out), async {
            next_goal(&mut engine)
                .await
                .refuse(unreachable())
                .await
                .expect("the goal is refused");
        });

        let error = ended.expect_err("the provider refuses");
        assert_eq!(error.exit_code(), CallEnd::Refused.exit_code());
        assert_eq!(lines(&out)[0]["outcome"], "refused");
        assert_eq!(lines(&out)[0]["failure_kind"], "unreachable");
    }

    #[tokio::test]
    async fn a_failed_goal_gives_its_kind_and_exit_code_4() {
        let messenger = mock_messenger().await;
        let mut engine = move_to_provider(&messenger).await;
        let call = member_call("move_to", &[JOINTS]);
        let mut out = Vec::new();

        let mut no_press = presses(0);
        let (ended, ()) = tokio::join!(run(&call, &messenger, &mut no_press, &mut out), async {
            let context = accept_move_to(&mut engine).await;
            context.fail(unreachable()).await.expect("the goal fails");
        });

        let error = ended.expect_err("the goal failed");
        assert_eq!(error.exit_code(), CallEnd::Failed.exit_code());
        assert_eq!(
            error.to_string(),
            "The goal of `limb_motion.move_to` failed (unreachable): no plan to the target"
        );
        assert_eq!(
            lines(&out),
            [json!({
                "outcome": "failed",
                "failure_kind": "unreachable",
                "message": "no plan to the target",
            })]
        );
    }

    /// A Ctrl-C sends the provider a cancel, and the command follows the
    /// goal to the end the provider gives it: a cancel, with its result.
    #[tokio::test]
    async fn a_ctrl_c_cancels_the_goal_and_gives_exit_code_5() {
        let messenger = mock_messenger().await;
        let mut engine = move_to_provider(&messenger).await;
        let call = member_call("move_to", &[JOINTS]);
        let mut out = Vec::new();
        let (press, mut presses) = presses_on_demand();

        let (ended, ()) = tokio::join!(run(&call, &messenger, &mut presses, &mut out), async {
            let context = accept_move_to(&mut engine).await;
            press.send(()).expect("the command reads the press");
            context.cancel_signal().await;
            assert_eq!(context.cancel_cause(), Some(CancelCause::Caller));
            context
                .complete_cancelled(
                    CancelCause::Caller.reason(),
                    payload(RESULT, json!({"reached": false})),
                )
                .await
                .expect("the goal ends cancelled");
        });

        let error = ended.expect_err("the goal was cancelled");
        assert_eq!(error.exit_code(), CallEnd::Cancelled.exit_code());
        assert_eq!(
            error.to_string(),
            "The goal of `limb_motion.move_to` was cancelled by its caller"
        );
        assert_eq!(
            lines(&out),
            [json!({
                "outcome": "cancelled",
                "reason": "cancelled by its caller",
                "result": {"reached": false},
            })]
        );
    }

    /// A second Ctrl-C ends the command at once with code 130, the end of
    /// the goal unknown.
    #[tokio::test]
    async fn a_second_ctrl_c_stops_at_once_with_exit_code_130() {
        let messenger = mock_messenger().await;
        let mut engine = move_to_provider(&messenger).await;
        let call = member_call("move_to", &[JOINTS]);
        let mut out = Vec::new();
        let (press, mut presses) = presses_on_demand();

        let (ended, _context) =
            tokio::join!(run(&call, &messenger, &mut presses, &mut out), async {
                let context = accept_move_to(&mut engine).await;
                press.send(()).expect("the command reads the first press");
                press.send(()).expect("the command reads the second press");
                context
            });

        let error = ended.expect_err("the second press stops the command");
        assert_eq!(error.exit_code(), 130);
        assert!(out.is_empty(), "no end to print");
    }

    /// A provider that drops the goal before it ends abandons it.
    #[tokio::test]
    async fn an_abandoned_goal_gives_exit_code_6() {
        let messenger = mock_messenger().await;
        let mut engine = move_to_provider(&messenger).await;
        let call = member_call("move_to", &[JOINTS]);
        let mut out = Vec::new();

        let mut no_press = presses(0);
        let (ended, ()) = tokio::join!(run(&call, &messenger, &mut no_press, &mut out), async {
            drop(accept_move_to(&mut engine).await);
        });

        let error = ended.expect_err("the goal was abandoned");
        assert_eq!(error.exit_code(), CallEnd::Abandoned.exit_code());
        assert_eq!(
            lines(&out),
            [json!({"outcome": "abandoned", "message": "the provider abandoned the goal"})]
        );
    }

    /// A provider that holds the end of a goal for no time gives an end that
    /// expired before the command asked for it.
    #[tokio::test]
    async fn a_goal_whose_end_expired_gives_exit_code_7() {
        let messenger = mock_messenger().await;
        let mut engine = move_to_provider(&messenger)
            .await
            .with_result_retention_grace(Duration::ZERO);
        let call = member_call("move_to", &[JOINTS]);
        let mut out = Vec::new();

        let mut no_press = presses(0);
        let (ended, ()) = tokio::join!(run(&call, &messenger, &mut no_press, &mut out), async {
            let context = accept_move_to(&mut engine).await;
            context
                .complete(payload(RESULT, json!({"reached": true})))
                .await
                .expect("the goal completes");
        });

        let error = ended.expect_err("the end expired");
        assert_eq!(error.exit_code(), CallEnd::Expired.exit_code());
        assert_eq!(lines(&out)[0]["outcome"], "expired");
    }

    /// The goal of the refined member takes three joints: the command
    /// refuses another length before it sends anything.
    #[tokio::test]
    async fn a_goal_is_encoded_with_the_format_of_the_pinned_manifest() {
        let messenger = mock_messenger().await;
        let _engine = move_to_provider(&messenger).await;
        let mut out = Vec::new();

        let error = run(
            &member_call("move_to", &["joints=[0.1, 0.2]"]),
            &messenger,
            &mut presses(0),
            &mut out,
        )
        .await
        .expect_err("two joints do not fit the refined format");

        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("joints"), "{error}");
    }

    #[test]
    fn a_topic_is_not_called() {
        let interfaces = InstanceInterfaces::from_documents(&documents()).expect("the interfaces");
        let selected = interfaces
            .select("heartbeat", &[InterfaceKind::Topic])
            .expect("the topic");
        let Err(error) = MemberCall::new(&selected, &MessageArguments::default()) else {
            panic!("a topic has no call");
        };
        assert_eq!(
            error.to_string(),
            "`arm.heartbeat` is a topic: `peppy node echo` prints its messages"
        );
    }
}
