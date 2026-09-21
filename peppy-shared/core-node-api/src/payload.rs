//! Wire-level `Payload` type — a cheap `bytes::Bytes` wrapper used across
//! the peppy messaging stack. Lives in `core-node-api` so that the capnp
//! `encode()` helpers can return it directly without a `Vec<u8>` boundary
//! hop, and so that `peppylib` and other crates can share the same type
//! without depending on each other.
//!
//! The construction surface is deliberately narrow: `from_static` for byte
//! literals, `From<Bytes>` / `From<Vec<u8>>` for owned buffers,
//! `AsRef<[u8]>` / `Deref<Target = [u8]>` for read-only access. Anything
//! beyond that goes through `Bytes` explicitly.

use bytes::Bytes;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Payload(Bytes);

impl Payload {
    /// Create a new `Payload` from a static slice.
    pub fn from_static(bytes: &'static [u8]) -> Self {
        Self(Bytes::from_static(bytes))
    }

    /// Create an empty `Payload`.
    pub fn new() -> Self {
        Self(Bytes::new())
    }

    /// Convert into the inner `Bytes`.
    pub fn into_inner(self) -> Bytes {
        self.0
    }
}

impl Default for Payload {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Bytes> for Payload {
    fn from(bytes: Bytes) -> Self {
        Self(bytes)
    }
}

impl From<Vec<u8>> for Payload {
    fn from(vec: Vec<u8>) -> Self {
        Self(Bytes::from(vec))
    }
}

impl AsRef<[u8]> for Payload {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl std::ops::Deref for Payload {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

// `assert_eq!(payload, &expected_payload)` — compares a `Payload` returned by
// value against a borrowed `Payload` held by the test.
impl PartialEq<&Payload> for Payload {
    fn eq(&self, other: &&Payload) -> bool {
        self.0 == other.0
    }
}

impl PartialEq<Payload> for &Payload {
    fn eq(&self, other: &Payload) -> bool {
        self.0 == other.0
    }
}

/// Error returned when constructing a [`NonEmptyPayload`] from an empty
/// payload. The empty payload is reserved as the end-of-stream sentinel
/// for `peppylib::messaging::ActionFeedbackPublisher::publish_end`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error(
    "payload is empty (empty payloads are reserved for the action-feedback end-of-stream sentinel)"
)]
pub struct EmptyPayloadError;

/// A [`Payload`] guaranteed at the type level to be non-empty. Required by
/// `peppylib::messaging::ActionFeedbackPublisher::publish` so an empty
/// payload, which is reserved as the end-of-stream sentinel for
/// `publish_end`, cannot be sent through the regular publish path by
/// mistake.
///
/// Lives next to [`Payload`] so that capnp `encode()` helpers can return a
/// non-emptiness-typed payload directly without `peppylib` and
/// `core-node-api` having to depend on each other.
#[derive(Clone)]
pub struct NonEmptyPayload(Payload);

impl NonEmptyPayload {
    /// Construct a `NonEmptyPayload`, returning [`EmptyPayloadError`] if
    /// `payload` is empty.
    pub fn try_new(payload: Payload) -> core::result::Result<Self, EmptyPayloadError> {
        if payload.is_empty() {
            Err(EmptyPayloadError)
        } else {
            Ok(Self(payload))
        }
    }

    /// Consume the wrapper and return the inner [`Payload`].
    pub fn into_inner(self) -> Payload {
        self.0
    }
}

impl TryFrom<Payload> for NonEmptyPayload {
    type Error = EmptyPayloadError;

    fn try_from(payload: Payload) -> core::result::Result<Self, Self::Error> {
        Self::try_new(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_default_are_empty() {
        assert!(Payload::new().is_empty());
        assert!(Payload::default().is_empty());
        assert_eq!(Payload::new(), Payload::default());
    }

    #[test]
    fn from_static_preserves_bytes() {
        let payload = Payload::from_static(b"abc");
        assert_eq!(payload.as_ref(), b"abc");
        // Deref gives slice access too.
        assert_eq!(&*payload, b"abc");
        assert_eq!(payload.len(), 3);
    }

    #[test]
    fn from_vec_and_from_bytes_agree_and_round_trip() {
        let from_vec = Payload::from(vec![1u8, 2, 3]);
        let from_bytes = Payload::from(Bytes::from_static(&[1u8, 2, 3]));
        assert_eq!(from_vec, from_bytes);
        // into_inner returns the underlying bytes unchanged.
        assert_eq!(from_vec.into_inner(), Bytes::from_static(&[1, 2, 3]));
    }

    // The `&`s are the whole point here — this exercises the two by-reference
    // `PartialEq` impls, so clippy's "drop the reference" suggestion is wrong.
    #[allow(clippy::op_ref)]
    #[test]
    fn partial_eq_by_reference_both_directions() {
        let owned = Payload::from(vec![9u8, 8, 7]);
        let borrowed = Payload::from(vec![9u8, 8, 7]);
        // `Payload == &Payload` and `&Payload == Payload` (the two helper impls).
        assert!(owned == &borrowed);
        assert!(&borrowed == owned);
    }

    #[test]
    fn non_empty_payload_rejects_empty() {
        // `NonEmptyPayload` is intentionally not `Debug`, so match rather than
        // `expect_err` to inspect the error.
        match NonEmptyPayload::try_new(Payload::new()) {
            Ok(_) => panic!("empty payload must be rejected"),
            Err(err) => assert!(err.to_string().contains("empty"), "got: {err}"),
        }
        // The `TryFrom` impl mirrors `try_new`.
        assert!(NonEmptyPayload::try_from(Payload::new()).is_err());
    }

    #[test]
    fn non_empty_payload_accepts_non_empty_and_round_trips() {
        let payload = Payload::from_static(b"\x00"); // a single NUL byte is non-empty
        let wrapped = NonEmptyPayload::try_new(payload.clone()).expect("non-empty must pass");
        assert_eq!(wrapped.into_inner(), payload);
        // `TryFrom` agrees.
        let via_try_from = NonEmptyPayload::try_from(payload.clone()).expect("try_from non-empty");
        assert_eq!(via_try_from.into_inner(), payload);
    }
}
