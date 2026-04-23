//! Wire-level `Payload` type — a cheap `bytes::Bytes` wrapper used across
//! the peppy messaging stack. Lives in `core-node-api` so that the capnp
//! `encode()` helpers can return it directly without a `Vec<u8>` boundary
//! hop, and so that `peppylib` and other crates can share the same type
//! without depending on each other.

use bytes::Bytes;

/// A wrapper around `bytes::Bytes` to avoid exposing the `bytes` crate in the public API.
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

impl From<&'static [u8]> for Payload {
    fn from(slice: &'static [u8]) -> Self {
        Self(Bytes::from_static(slice))
    }
}

impl From<String> for Payload {
    fn from(s: String) -> Self {
        Self(Bytes::from(s))
    }
}

impl From<&'static str> for Payload {
    fn from(s: &'static str) -> Self {
        Self(Bytes::from_static(s.as_bytes()))
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

impl PartialEq<Bytes> for Payload {
    fn eq(&self, other: &Bytes) -> bool {
        self.0 == other
    }
}

impl PartialEq<Payload> for Bytes {
    fn eq(&self, other: &Payload) -> bool {
        self == &other.0
    }
}

impl PartialEq<&[u8]> for Payload {
    fn eq(&self, other: &&[u8]) -> bool {
        self.0.as_ref() == *other
    }
}

impl PartialEq<Payload> for &[u8] {
    fn eq(&self, other: &Payload) -> bool {
        *self == other.0.as_ref()
    }
}

impl PartialEq<Vec<u8>> for Payload {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self.0.as_ref() == other.as_slice()
    }
}

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
