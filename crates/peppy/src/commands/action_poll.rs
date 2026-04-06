use peppylib::messaging::ActionGoalHandle;
use peppylib::{ActionMessenger, MessengerHandle, PeppyError};

use super::node::TimeoutConfig;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

use std::time::Duration;

const FEEDBACK_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);
const RESULT_POLL_TIMEOUT: Duration = Duration::from_millis(200);
const SLEEP_BETWEEN_POLLS: Duration = Duration::from_millis(50);

/// Polls an action goal to completion, draining feedback and checking timeouts.
///
/// - `on_feedback` is called with each raw feedback payload to process feedback
///   (typically decode + feed to scrolling output).
/// - `decode_result` attempts to decode the final result from the raw payload.
///   It should return `Ok(Some(result))` on success, `Ok(None)` if the payload
///   is a "result pending" sentinel, or `Err` on decode failure.
///
/// On timeout or error, the scrolling output is cleared before returning.
pub(crate) async fn poll_action_to_completion<R>(
    messenger: &MessengerHandle,
    action_handle: &mut ActionGoalHandle,
    timeouts: &TimeoutConfig,
    scrolling_output: &mut ScrollingOutput,
    mut on_feedback: impl FnMut(&[u8], &mut ScrollingOutput),
    decode_result: impl Fn(&[u8]) -> std::result::Result<Option<R>, String>,
) -> Result<R> {
    let idle_timeout = Duration::from_secs(timeouts.idle_secs);
    let absolute_deadline = tokio::time::Instant::now() + Duration::from_secs(timeouts.max_secs);
    let mut last_activity = tokio::time::Instant::now();

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            check_timeouts(last_activity, idle_timeout, absolute_deadline, timeouts).inspect_err(
                |_| {
                    scrolling_output.clear();
                },
            )?;

            match tokio::time::timeout(FEEDBACK_DRAIN_TIMEOUT, action_handle.on_next_feedback())
                .await
            {
                Ok(Ok(msg)) => {
                    last_activity = tokio::time::Instant::now();
                    on_feedback(&msg.payload(), scrolling_output);
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }

        check_timeouts(last_activity, idle_timeout, absolute_deadline, timeouts).inspect_err(
            |_| {
                scrolling_output.clear();
            },
        )?;

        match ActionMessenger::request_result(messenger, action_handle, RESULT_POLL_TIMEOUT).await {
            Ok(msg) => {
                last_activity = tokio::time::Instant::now();
                let payload = msg.payload();
                match decode_result(&payload) {
                    Ok(Some(result)) => return Ok(result),
                    Ok(None) => {} // "result pending" — keep polling
                    Err(err) => {
                        scrolling_output.clear();
                        return Err(Error::ExecutionFailed(err));
                    }
                }
            }
            Err(PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => {
                scrolling_output.clear();
                return Err(Error::ExecutionFailed(format!(
                    "Failed to get action result: {err}"
                )));
            }
        }

        tokio::time::sleep(SLEEP_BETWEEN_POLLS).await;
    }
}

fn check_timeouts(
    last_activity: tokio::time::Instant,
    idle_timeout: Duration,
    absolute_deadline: tokio::time::Instant,
    timeouts: &TimeoutConfig,
) -> Result<()> {
    let now = tokio::time::Instant::now();
    if now >= absolute_deadline {
        return Err(Error::ExecutionFailed(format!(
            "Timeout: max timeout of {}s exceeded. \
             Use --max-timeout <seconds> to increase.",
            timeouts.max_secs
        )));
    }
    if now.duration_since(last_activity) >= idle_timeout {
        return Err(Error::ExecutionFailed(format!(
            "Timeout: no output received for {}s. \
             Use --idle-timeout <seconds> to increase.",
            timeouts.idle_secs
        )));
    }
    Ok(())
}
