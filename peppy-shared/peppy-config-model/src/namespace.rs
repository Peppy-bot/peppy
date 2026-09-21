//! Workspace namespace: the zenoh session-level namespace prefix used to route
//! one workspace's robot traffic separately from another's across the platform
//! hub.
//!
//! The value is **always present**. Logged out it resolves to the constant
//! [`LOCAL_NAMESPACE`] (`"local"`); logged in it resolves to the platform
//! workspace id (a stable UUID). A logged-out daemon never dials the platform
//! router, so `local` never reaches it, while two logged-out robots on the same
//! LAN share `local` and interoperate. The value is parsed into a zenoh non-wild
//! key expression up front, so an invalid id can never reach a live session:
//! zenoh's egress prepends `<ns>/` to every declared key and ingress strips it,
//! so two sessions interoperate iff their namespaces are equal.
//!
//! A namespace is routing context only. It does not enforce workspace
//! membership and is not a key-expression authorization boundary.

use std::fmt;

use serde::{Deserialize, Serialize};
use zenoh_keyexpr::OwnedNonWildKeyExpr;

/// Namespace used whenever there is no workspace id (logged out). A logged-out
/// daemon never federates, so `local` never reaches the platform router; two
/// logged-out robots on the same LAN share it and interoperate.
pub const LOCAL_NAMESPACE: &str = "local";

/// A validated zenoh session namespace prefix for one workspace.
///
/// Constructed only through [`Namespace::parse`] or the infallible
/// [`Namespace::local`]; the wrapped string is guaranteed to be a valid
/// non-wild key expression, so it is always safe to render into a zenoh session
/// config's `namespace` field. It serializes as a plain string and fails loudly
/// when deserializing an invalid value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Namespace(String);

/// A candidate namespace that is not a valid non-wild key expression (empty,
/// contains a wildcard such as `*`/`**`/`$*`, ...) and so cannot be used as a
/// zenoh session namespace.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid namespace {value:?}: {message}")]
pub struct InvalidNamespace {
    value: String,
    message: String,
}

impl InvalidNamespace {
    /// The rejected value, for diagnostics.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl Namespace {
    /// The local namespace used when logged out. Infallible: [`LOCAL_NAMESPACE`]
    /// is a constant, valid non-wild key expression.
    pub fn local() -> Self {
        Self::parse(LOCAL_NAMESPACE)
            .expect("LOCAL_NAMESPACE must be a valid non-wild key expression")
    }

    /// Parse and validate a candidate namespace (a workspace id or the local
    /// constant). Rejects anything zenoh would not accept as a non-wild key
    /// expression so the value can never poison a live session.
    ///
    /// A namespace must be a *single* chunk: zenoh prepends `<ns>/` on egress and
    /// strips it chunk-by-chunk on ingress, so a multi-chunk value like `a/b`
    /// would collide with namespace `a` + key `b/...`. A UUID and the `local`
    /// constant contain no `/`, so this never rejects a legitimate id.
    pub fn parse(s: &str) -> Result<Self, InvalidNamespace> {
        if s.contains('/') {
            return Err(InvalidNamespace {
                value: s.to_owned(),
                message: "namespace must be a single chunk (must not contain '/')".to_owned(),
            });
        }
        match OwnedNonWildKeyExpr::try_from(s.to_owned()) {
            Ok(_) => Ok(Self(s.to_owned())),
            Err(err) => Err(InvalidNamespace {
                value: s.to_owned(),
                message: err.to_string(),
            }),
        }
    }

    /// Whether this is the logged-out [`LOCAL_NAMESPACE`]. This is the
    /// federation gate, fail-closed by construction: an absent workspace id
    /// resolves to `local` and an invalid one fails [`Namespace::parse`], so
    /// only a present, valid, non-local namespace ever federates.
    pub fn is_local(&self) -> bool {
        self.0 == LOCAL_NAMESPACE
    }

    /// The validated namespace string, ready to render into a session config.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Namespace {
    type Error = InvalidNamespace;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Namespace> for String {
    fn from(namespace: Namespace) -> Self {
        namespace.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn parse_accepts_uuid_and_local() {
        assert!(Namespace::parse(UUID).is_ok(), "hyphenated UUID must parse");
        assert!(Namespace::parse(LOCAL_NAMESPACE).is_ok());
    }

    #[test]
    fn parse_rejects_multi_chunk() {
        for bad in ["a/b", "a/b/c"] {
            assert!(
                Namespace::parse(bad).is_err(),
                "{bad:?} must be rejected: a namespace is a single chunk \
                 (multi-chunk collides once zenoh prepends the namespace)"
            );
        }
    }

    #[test]
    fn parse_rejects_empty_and_wildcards() {
        for bad in ["", "a*", "$*", "**"] {
            assert!(
                Namespace::parse(bad).is_err(),
                "{bad:?} must be rejected as a namespace"
            );
        }
    }

    #[test]
    fn local_round_trips() {
        assert_eq!(Namespace::local().as_str(), LOCAL_NAMESPACE);
        assert_eq!(
            Namespace::local(),
            Namespace::parse(LOCAL_NAMESPACE).unwrap()
        );
    }

    #[test]
    fn is_local_is_the_fail_closed_federation_gate() {
        assert!(Namespace::local().is_local(), "logged out never federates");
        assert!(
            !Namespace::parse(UUID).unwrap().is_local(),
            "a valid workspace id federates"
        );
    }

    #[test]
    fn serde_round_trips_as_a_plain_string() {
        let namespace = Namespace::parse(UUID).unwrap();
        let serialized = serde_json::to_string(&namespace).unwrap();
        assert_eq!(serialized, format!("{UUID:?}"));
        let deserialized: Namespace = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, namespace);
    }

    #[test]
    fn deserialize_fails_loud_on_an_invalid_namespace() {
        for bad in ["\"**\"", "\"a/b\"", "\"\""] {
            assert!(
                serde_json::from_str::<Namespace>(bad).is_err(),
                "{bad} must fail namespace deserialization"
            );
        }
    }
}
