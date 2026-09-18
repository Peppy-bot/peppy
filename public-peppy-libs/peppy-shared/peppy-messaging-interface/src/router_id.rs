//! The stable transport identity a peppy-rendered zenohd router runs under.
//!
//! Zenoh mints a fresh random `zid` on every start unless the config pins one,
//! so an unpinned router is anonymous across restarts: nothing that observes a
//! transport session can say *which* router opened it, and nothing that reads a
//! router's logs can correlate two runs of the same process. Pinning the `id`
//! is what makes both possible, so every router this crate renders a config for
//! is given one.
//!
//! [`RouterId`] is the parsed form. Its constructor is the only way to build
//! one, so an unchecked string cannot reach a rendered config, and the rules it
//! enforces are exactly zenoh's own: a router config carrying a `RouterId` is
//! accepted by zenoh, and the value zenoh prints back (in its admin space, in
//! another router's session list, in its logs) is byte-identical to the one
//! that went in. That round trip is the whole point, since the observer side
//! compares strings.

use std::fmt;

use crate::error::{Error, Result};

/// Maximum characters in a router id.
///
/// Zenoh parses the config's `id` as a 128-bit value (`u128::from_str_radix`
/// with radix 16 inside `uhlc::ID`), so 32 hexadecimal digits is the ceiling
/// and a longer string overflows rather than truncating.
const MAX_CHARS: usize = 32;

/// A zenoh router identity (`zid`), in the exact lexical form zenoh accepts in
/// a config's `id` field *and* prints back everywhere it reports one.
///
/// The accepted form is an allow-list, and it is zenoh 1.10's own (`uhlc::ID`'s
/// `FromStr`, reached through `zenoh_protocol::core::ZenohIdProto`): 1 to
/// [`MAX_CHARS`] lowercase hexadecimal digits with no leading `0`. Each rule
/// exists because zenoh either rejects the value or renders it differently:
///
/// * **Lowercase only.** Zenoh rejects uppercase hexadecimal outright, with a
///   message telling the caller to use lowercase.
/// * **No leading `0`.** Zenoh rejects it, because its own `Display` is
///   `{:x}` over the parsed integer and so never produces one. Accepting
///   `0abc` here would mean the value read back off the wire (`abc`) did not
///   match the value configured, breaking the string comparison this type
///   exists to make sound. It also excludes `0` itself, which zenoh reserves.
/// * **At most 32 digits.** See [`MAX_CHARS`].
///
/// So `RouterId::parse(s).map(|id| id.to_string()) == Ok(s)` for every accepted
/// `s`: the type is a fixed point, not merely a validated string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouterId(String);

impl RouterId {
    /// Parses a router id, rejecting anything zenoh would reject or render
    /// back differently (see the type docs).
    pub fn parse(raw: &str) -> Result<Self> {
        let invalid = |reason: &str| {
            Error::ConfigurationError(format!("invalid zenoh router id {raw:?}: {reason}"))
        };
        if raw.is_empty() {
            return Err(invalid("must not be empty"));
        }
        if raw.len() > MAX_CHARS {
            return Err(invalid(&format!(
                "must be at most {MAX_CHARS} hexadecimal digits (zenoh parses it as a 128-bit value)"
            )));
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Err(invalid(
                "must contain only lowercase hexadecimal digits (zenoh rejects uppercase)",
            ));
        }
        if raw.starts_with('0') {
            return Err(invalid(
                "must not start with '0' (zenoh rejects leading zeros and never prints one)",
            ));
        }
        Ok(Self(raw.to_string()))
    }

    /// Mints a fresh random router id.
    ///
    /// 128 bits from the OS entropy source, which is the same width zenoh's own
    /// unpinned default uses, so pinning costs no collision resistance. The
    /// `max(1)` excludes zero, the one value that would not round-trip (zenoh
    /// rejects `0`); it moves a single 1-in-2^128 outcome onto `1` rather than
    /// leaving a value this type promises never to produce.
    pub fn generate() -> Self {
        let bits = rand::random::<u128>().max(1);
        // `{:x}` over a non-zero `u128` is lowercase, leading-zero-free, and at
        // most 32 digits: exactly what `parse` accepts, which the
        // `generated_ids_round_trip` test pins rather than assumes.
        Self(format!("{bits:x}"))
    }

    /// The stored form, for rendering into a config or a request body.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RouterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_accepts_the_forms_zenoh_renders() {
        for accepted in ["1", "abc123", "7f3a9c1e", &"f".repeat(MAX_CHARS)] {
            let id = RouterId::parse(accepted)
                .unwrap_or_else(|e| panic!("{accepted:?} should parse: {e}"));
            assert_eq!(
                id.to_string(),
                accepted,
                "a parsed id must render back byte-identically"
            );
        }
    }

    #[test]
    fn parsing_rejects_what_zenoh_rejects_or_renders_differently() {
        for rejected in [
            "",
            // Uppercase: zenoh rejects it outright.
            "ABC123",
            "7F3a",
            // Leading zero: zenoh rejects it, and its own Display never emits one,
            // so accepting it would break the round trip this type guarantees.
            "0abc",
            "0",
            // Not hexadecimal at all.
            "cn-a1b2c3",
            "g123",
            "7f3a9c1e ",
            // Over the 128-bit ceiling.
            &"f".repeat(MAX_CHARS + 1),
        ] {
            assert!(
                RouterId::parse(rejected).is_err(),
                "{rejected:?} must be rejected"
            );
        }
    }

    /// The generator must only ever emit ids its own parser accepts, or a daemon
    /// could mint an identity that fails to render into its router's config.
    #[test]
    fn generated_ids_round_trip() {
        for _ in 0..256 {
            let generated = RouterId::generate();
            let reparsed = RouterId::parse(generated.as_str())
                .unwrap_or_else(|e| panic!("generated {generated} should parse: {e}"));
            assert_eq!(reparsed, generated);
        }
    }

    /// Two mints must differ, or the whole scheme collapses into the anonymity
    /// it exists to remove.
    #[test]
    fn generated_ids_are_distinct() {
        let ids: std::collections::HashSet<String> =
            (0..64).map(|_| RouterId::generate().to_string()).collect();
        assert_eq!(ids.len(), 64);
    }
}
