//! A provider that floods one goal with feedback, and a reader of a whole
//! feedback stream, for the tests of a caller that reads its feedback late or
//! never. Each feedback payload is its index, as text.

#![allow(dead_code)]

use std::time::Duration;

use peppylib::PeppyError;
use peppylib::messaging::{
    ActionGoalHandle, ConcurrentAction, MessengerHandle, NonEmptyPayload, SenderTarget,
};
use peppylib::types::Payload;
use tokio::sync::oneshot;

/// Turns a hang into a failure. No test passes or fails on it otherwise.
pub const HANG_GUARD: Duration = Duration::from_secs(30);

/// The result every flooded goal completes with.
pub const FLOOD_RESULT: &[u8] = b"done";

/// The action a flooding provider serves.
pub struct FloodedAction<'a> {
    pub core_node: &'a str,
    pub instance_id: &'a str,
    pub target: SenderTarget,
    pub action_name: &'a str,
}

/// A running flooding provider. `flooded` fires once the goal is completed,
/// all its feedback and the end of its stream published. Dropping the value
/// stops the provider.
pub struct Flood {
    pub flooded: oneshot::Receiver<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Flood {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Serves one goal of `action` on `provider`: accepts it, publishes
/// `feedback_count` feedback messages, then completes it with
/// [`FLOOD_RESULT`]. The action stays exposed, to answer result requests,
/// until the returned [`Flood`] drops.
pub async fn start_flooding_provider(
    provider: &MessengerHandle,
    action: FloodedAction<'_>,
    feedback_count: usize,
) -> Flood {
    let mut engine = ConcurrentAction::expose(
        provider,
        action.core_node,
        action.instance_id,
        action.target,
        action.action_name,
        true,
    )
    .await
    .expect("the action is exposed");
    let (flooded_tx, flooded) = oneshot::channel();
    let task = tokio::spawn(async move {
        let pending = engine
            .recv_next_goal()
            .await
            .expect("the goal is received")
            .expect("a goal arrives");
        let goal = pending
            .accept(Payload::from_static(b"accepted"))
            .await
            .expect("the goal is accepted");
        for index in 0..feedback_count {
            let payload = NonEmptyPayload::try_new(Payload::from(index.to_string().into_bytes()))
                .expect("the feedback is not empty");
            goal.publish_feedback(payload)
                .await
                .expect("the feedback is published");
        }
        goal.complete(Payload::from_static(FLOOD_RESULT))
            .await
            .expect("the goal completes");
        let _ = flooded_tx.send(());
        std::future::pending::<()>().await;
        drop((goal, engine));
    });
    Flood { flooded, task }
}

/// Reads `goal`'s feedback stream to its end and returns the feedback
/// indexes in the order they arrived.
pub async fn read_to_the_end(goal: &mut ActionGoalHandle) -> Vec<usize> {
    let mut indexes = Vec::new();
    loop {
        match tokio::time::timeout(HANG_GUARD, goal.on_next_feedback())
            .await
            .expect("the feedback stream must reach its end")
        {
            Ok(message) => indexes.push(feedback_index(message.payload_bytes().as_ref())),
            Err(PeppyError::ActionFeedbackChannelClosed) => return indexes,
            Err(error) => panic!("the stream must end cleanly: {error}"),
        }
    }
}

fn feedback_index(payload: &[u8]) -> usize {
    std::str::from_utf8(payload)
        .expect("the feedback is text")
        .parse()
        .expect("the feedback is an index")
}
