//! Feedback-stream plumbing shared by the add/build/run goal handlers.
//!
//! `FeedbackLine`/`FeedbackStream` are re-exported from
//! `node-stack-internal::build_io` so that `NodeEntity::build` can stream
//! apptainer output without depending on core-node-internal. The
//! forwarder consumes those lines from an in-process mpsc channel and
//! republishes each one onto a peppylib topic.

pub(crate) use node_stack::{FeedbackLine, FeedbackStream};

/// Spawns a task that consumes `FeedbackLine` values from `feedback_rx`,
/// converts each one via `encode` and publishes the resulting payload. Shared
/// by the add/build/start goal handlers, which all run the same
/// consumer-side forwarder over differently-typed feedback encoders.
pub(crate) fn spawn_feedback_forwarder<F>(
    mut feedback_rx: tokio::sync::mpsc::UnboundedReceiver<FeedbackLine>,
    publisher: peppylib::messaging::TopicPublisher,
    encode: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(FeedbackLine) -> core_node_api::Result<Vec<u8>> + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(line) = feedback_rx.recv().await {
            if let Ok(payload) = encode(line) {
                let _ = publisher.publish(payload.into()).await;
            }
        }
    })
}
