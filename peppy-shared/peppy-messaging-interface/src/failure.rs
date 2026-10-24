//! The failure of a refused or failed goal, or of the error reply of a
//! service: a failure kind and a message, and their wire form.
//!
//! Wire layout: `[kind_len: u8][kind: ASCII][message: UTF-8]`. The message
//! runs to the end of the bytes, so a failure always ends the envelope that
//! carries it.

use bytes::{BufMut, Bytes, BytesMut};
use config::failure_kind::{FailureKindName, InvalidFailureKindName};
use std::fmt;

/// Why a goal was refused or failed, or why a service gave an error reply:
/// the kind, from the closed set that peppy and the contract of the member
/// declare, and a message for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    kind: FailureKindName,
    message: String,
}

impl Failure {
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

    /// The wire form of the failure.
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.encoded_len());
        self.encode_into(&mut buf);
        buf.freeze()
    }

    /// Appends the wire form of the failure to `buf`, for an envelope that
    /// ends with it.
    pub fn encode_into(&self, buf: &mut BytesMut) {
        let kind = self.kind.as_str();
        // `FailureKindName` holds at most 64 bytes, so the length fits a u8.
        buf.put_u8(kind.len() as u8);
        buf.extend_from_slice(kind.as_bytes());
        buf.extend_from_slice(self.message.as_bytes());
    }

    /// The length of the wire form of the failure.
    pub fn encoded_len(&self) -> usize {
        1 + self.kind.as_str().len() + self.message.len()
    }

    /// Reads a failure from its wire form, which takes all of `wire`.
    pub fn decode(wire: &[u8]) -> Result<Self, FailureDecodeError> {
        let kind_len = *wire.first().ok_or(FailureDecodeError::Empty)? as usize;
        let message_start = 1 + kind_len;
        let kind = wire
            .get(1..message_start)
            .ok_or(FailureDecodeError::Truncated { kind_len })?;
        let kind = std::str::from_utf8(kind).map_err(|_| FailureDecodeError::KindNotUtf8)?;
        let kind = FailureKindName::parse(kind).map_err(FailureDecodeError::InvalidKind)?;
        let message = std::str::from_utf8(&wire[message_start..])
            .map_err(|_| FailureDecodeError::MessageNotUtf8)?;
        Ok(Self {
            kind,
            message: message.to_owned(),
        })
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            return write!(f, "{}", self.kind);
        }
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// Why bytes are not the wire form of a [`Failure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureDecodeError {
    Empty,
    Truncated { kind_len: usize },
    KindNotUtf8,
    InvalidKind(InvalidFailureKindName),
    MessageNotUtf8,
}

impl fmt::Display for FailureDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the failure is empty"),
            Self::Truncated { kind_len } => {
                write!(
                    f,
                    "the failure is too short for its kind of {kind_len} bytes"
                )
            }
            Self::KindNotUtf8 => write!(f, "the failure kind is not UTF-8"),
            Self::InvalidKind(err) => write!(f, "{err}"),
            Self::MessageNotUtf8 => write!(f, "the failure message is not UTF-8"),
        }
    }
}

impl std::error::Error for FailureDecodeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use config::failure_kind::PeppyFailureKind;

    fn kind(name: &str) -> FailureKindName {
        FailureKindName::parse(name).unwrap()
    }

    #[test]
    fn a_failure_round_trips_through_its_wire_form() {
        let failure = Failure::new(kind("unreachable"), "no plan reaches the target");
        let wire = failure.encode();
        assert_eq!(wire.len(), failure.encoded_len());
        assert_eq!(Failure::decode(&wire), Ok(failure));
    }

    #[test]
    fn a_failure_with_no_message_round_trips() {
        let failure = Failure::new(PeppyFailureKind::Rejected, "");
        assert_eq!(Failure::decode(&failure.encode()), Ok(failure));
    }

    #[test]
    fn the_wire_form_is_the_kind_length_the_kind_and_the_message() {
        let wire = Failure::new(PeppyFailureKind::Timeout, "late").encode();
        assert_eq!(wire.as_ref(), b"\x07timeoutlate");
    }

    #[test]
    fn decode_refuses_bytes_that_are_not_a_failure() {
        assert_eq!(Failure::decode(b""), Err(FailureDecodeError::Empty));
        assert_eq!(
            Failure::decode(b"\x09timeout"),
            Err(FailureDecodeError::Truncated { kind_len: 9 })
        );
        assert!(matches!(
            Failure::decode(b"\x07Timeout"),
            Err(FailureDecodeError::InvalidKind(_))
        ));
        assert_eq!(
            Failure::decode(b"\x07timeout\xff"),
            Err(FailureDecodeError::MessageNotUtf8)
        );
    }

    #[test]
    fn display_names_the_kind_then_the_message() {
        assert_eq!(
            Failure::new(kind("not_found"), "no red cup").to_string(),
            "not_found: no red cup"
        );
        assert_eq!(
            Failure::new(PeppyFailureKind::Rejected, "").to_string(),
            "rejected"
        );
    }
}
