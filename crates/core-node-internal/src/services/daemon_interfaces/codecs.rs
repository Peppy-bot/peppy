//! The codecs of the daemon interfaces: one for each side of each member,
//! laid out from the compiled-in document of its interface the first time a
//! message of that side travels.

use crate::services::response::failure;
use config::failure_kind::PeppyFailureKind;
use config::node::MessageFormat;
use daemon_config::daemon_interface::DaemonInterface;
use message_codec::MessageCodec;
use peppylib::messaging::Failure;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// One side of a member: the message a client sends or the provider gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Side {
    /// The request of a service.
    Request,
    /// The response of a service.
    Response,
    /// The goal of an action.
    Goal,
    /// The feedback of an action.
    Feedback,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Request => "request",
            Self::Response => "response",
            Self::Goal => "goal",
            Self::Feedback => "feedback",
        })
    }
}

type CodecKey = (DaemonInterface, String, Side);

/// The codecs laid out so far.
#[derive(Default)]
pub(super) struct Codecs(parking_lot::Mutex<HashMap<CodecKey, Arc<MessageCodec>>>);

impl Codecs {
    /// The codec of the `side` of `member` of `interface`.
    pub(super) fn codec(
        &self,
        interface: DaemonInterface,
        member: &str,
        side: Side,
    ) -> Result<Arc<MessageCodec>, Failure> {
        let key = (interface, member.to_owned(), side);
        if let Some(codec) = self.0.lock().get(&key) {
            return Ok(Arc::clone(codec));
        }
        let format = format_of(interface, member, side).ok_or_else(|| {
            failure(
                PeppyFailureKind::AttemptFailed,
                format!("`{member}` of {} has no {side} message", interface.label()),
            )
        })?;
        let schema = format!("{}_{}_{member}_{side}", interface.name(), interface.tag());
        let codec = MessageCodec::new(&schema, format.clone()).map_err(|error| {
            failure(
                PeppyFailureKind::AttemptFailed,
                format!("the codec of the {side} of `{member}` is not laid out: {error}"),
            )
        })?;
        let codec = Arc::new(codec);
        self.0.lock().insert(key, Arc::clone(&codec));
        Ok(codec)
    }
}

/// The message format of the `side` of `member` of `interface`, as its
/// compiled-in document declares it.
fn format_of(
    interface: DaemonInterface,
    member: &str,
    side: Side,
) -> Option<&'static MessageFormat> {
    let interfaces = &interface.document().interfaces;
    match side {
        Side::Request | Side::Response => {
            let service = interfaces
                .services
                .iter()
                .find(|service| service.name == member)?;
            match side {
                Side::Request => service.request_message_format.as_ref(),
                _ => service.response_message_format.as_ref(),
            }
        }
        Side::Goal | Side::Feedback => {
            let action = interfaces
                .actions
                .iter()
                .find(|action| action.name == member)?;
            match side {
                Side::Goal => action
                    .goal_service
                    .as_ref()
                    .and_then(|goal| goal.request_message_format.as_ref()),
                _ => action
                    .feedback_topic
                    .as_ref()
                    .map(|feedback| &feedback.message_format),
            }
        }
    }
}
