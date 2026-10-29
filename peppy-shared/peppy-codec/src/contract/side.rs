//! The message sides of the members of a contract.

/// One message side of a member, as its document names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MessageSide {
    /// `message_format` of a topic.
    Message,
    /// `request_message_format` of a service.
    Request,
    /// `response_message_format` of a service.
    Response,
    /// `goal_service.request_message_format` of an action.
    Goal,
    /// `feedback_topic.message_format` of an action.
    Feedback,
    /// `result_service.response_message_format` of an action.
    Result,
}

impl MessageSide {
    /// Every side, in the order of the variants.
    pub const ALL: [Self; 6] = [
        Self::Message,
        Self::Request,
        Self::Response,
        Self::Goal,
        Self::Feedback,
        Self::Result,
    ];

    /// The name of the side: `message`, `request`, `response`, `goal`,
    /// `feedback` or `result`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Request => "request",
            Self::Response => "response",
            Self::Goal => "goal",
            Self::Feedback => "feedback",
            Self::Result => "result",
        }
    }

    /// The side named `name` (see [`Self::as_str`]).
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|side| side.as_str() == name)
    }
}
