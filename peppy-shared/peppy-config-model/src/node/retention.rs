//! What a topic keeps for subscribers that join later.
//!
//! A topic's producer declares the policy with the `retention` key:
//! `{ latest: N }` keeps the newest N messages. A topic with no `retention`
//! key, or with `{ latest: 0 }`, is live only.

use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, Visitor},
    ser::{SerializeMap, Serializer},
};
use std::{fmt, num::NonZeroUsize};

/// The most messages a topic can retain.
pub const MAX_RETENTION_DEPTH: usize = 1024;

const LATEST_KEY: &str = "latest";

/// How many messages a retaining topic keeps, from 1 to
/// [`MAX_RETENTION_DEPTH`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionDepth(NonZeroUsize);

impl RetentionDepth {
    /// The newest message alone.
    pub const ONE: Self = Self(NonZeroUsize::MIN);

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

/// A retention depth above [`MAX_RETENTION_DEPTH`].
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("`latest` keeps from 0 to {MAX_RETENTION_DEPTH} messages, got {depth}")]
pub struct RetentionDepthOutOfRange {
    depth: u64,
}

/// What a topic keeps for subscribers that join later.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TopicRetention {
    /// A subscriber receives the messages published after it attaches.
    #[default]
    LiveOnly,
    /// The publishing instance keeps its newest `depth` messages, and a new
    /// subscriber receives them too.
    Latest { depth: RetentionDepth },
}

impl TopicRetention {
    /// The policy that keeps the newest `depth` messages. A depth of 0 is
    /// [`TopicRetention::LiveOnly`].
    pub const fn latest(depth: usize) -> Result<Self, RetentionDepthOutOfRange> {
        match NonZeroUsize::new(depth) {
            None => Ok(Self::LiveOnly),
            Some(kept) if depth <= MAX_RETENTION_DEPTH => Ok(Self::Latest {
                depth: RetentionDepth(kept),
            }),
            Some(_) => Err(RetentionDepthOutOfRange {
                depth: depth as u64,
            }),
        }
    }

    pub const fn is_live_only(&self) -> bool {
        matches!(self, Self::LiveOnly)
    }

    /// How many messages the topic keeps: 0 when it is live only.
    const fn depth(&self) -> usize {
        match self {
            Self::LiveOnly => 0,
            Self::Latest { depth } => depth.get(),
        }
    }
}

impl fmt::Display for TopicRetention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LiveOnly => f.write_str("live only"),
            Self::Latest { depth } => write!(f, "latest {}", depth.get()),
        }
    }
}

/// One written form per value, `{ latest: N }`, so equal policies serialize to
/// equal bytes.
impl Serialize for TopicRetention {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(LATEST_KEY, &self.depth())?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for TopicRetention {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(TopicRetentionVisitor)
    }
}

struct TopicRetentionVisitor;

impl<'de> Visitor<'de> for TopicRetentionVisitor {
    type Value = TopicRetention;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{ {LATEST_KEY}: <0 to {MAX_RETENTION_DEPTH}> }}")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let Some(key) = map.next_key::<String>()? else {
            return Err(de::Error::invalid_length(0, &self));
        };
        if key != LATEST_KEY {
            return Err(de::Error::unknown_field(&key, &[LATEST_KEY]));
        }
        let WrittenDepth(depth) = map.next_value()?;
        match map.next_key::<String>()? {
            None => {}
            Some(second) if second == LATEST_KEY => {
                return Err(de::Error::duplicate_field(LATEST_KEY));
            }
            Some(other) => return Err(de::Error::unknown_field(&other, &[LATEST_KEY])),
        }
        match usize::try_from(depth) {
            Ok(depth) => TopicRetention::latest(depth).map_err(de::Error::custom),
            Err(_) => Err(de::Error::custom(RetentionDepthOutOfRange { depth })),
        }
    }
}

/// A depth as written: a whole number from 0. `serde_json5` casts a negative
/// or fractional number to an unsigned one, so the depth is read through this
/// visitor.
struct WrittenDepth(u64);

impl<'de> Deserialize<'de> for WrittenDepth {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(WrittenDepthVisitor)
    }
}

struct WrittenDepthVisitor;

impl Visitor<'_> for WrittenDepthVisitor {
    type Value = WrittenDepth;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a whole number of messages")
    }

    fn visit_u64<E: de::Error>(self, depth: u64) -> Result<Self::Value, E> {
        Ok(WrittenDepth(depth))
    }

    fn visit_i64<E: de::Error>(self, depth: i64) -> Result<Self::Value, E> {
        u64::try_from(depth)
            .map(WrittenDepth)
            .map_err(|_| E::invalid_value(de::Unexpected::Signed(depth), &self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(written: &str) -> Result<TopicRetention, String> {
        serde_json5::from_str(written).map_err(|error| error.to_string())
    }

    fn latest(depth: usize) -> TopicRetention {
        TopicRetention::latest(depth).expect("a depth in range")
    }

    #[test]
    fn each_written_form_parses_to_its_policy() {
        assert_eq!(parse("{ latest: 1 }"), Ok(latest(1)));
        assert_eq!(parse("{ latest: 5 }"), Ok(latest(5)));
        assert_eq!(parse("{ latest: 0 }"), Ok(TopicRetention::LiveOnly));
        assert_eq!(
            parse(&format!("{{ latest: {MAX_RETENTION_DEPTH} }}")),
            Ok(latest(MAX_RETENTION_DEPTH))
        );
    }

    /// A generated module states its topic's retention as a constant.
    #[test]
    fn a_policy_is_built_in_a_const_context() {
        const RETAINS_THREE: TopicRetention = match TopicRetention::latest(3) {
            Ok(retention) => retention,
            Err(_) => panic!("3 is in range"),
        };
        assert_eq!(
            RETAINS_THREE,
            TopicRetention::Latest {
                depth: RetentionDepth(NonZeroUsize::new(3).unwrap())
            }
        );
    }

    #[test]
    fn a_depth_of_zero_and_the_default_are_live_only() {
        assert_eq!(TopicRetention::latest(0), Ok(TopicRetention::LiveOnly));
        assert_eq!(TopicRetention::default(), TopicRetention::LiveOnly);
    }

    #[test]
    fn a_depth_above_the_maximum_is_refused_with_the_range() {
        let depth = MAX_RETENTION_DEPTH + 1;
        let refusal =
            format!("`latest` keeps from 0 to {MAX_RETENTION_DEPTH} messages, got {depth}");
        assert_eq!(
            TopicRetention::latest(depth)
                .expect_err("above the maximum")
                .to_string(),
            refusal
        );
        let error = parse(&format!("{{ latest: {depth} }}")).expect_err("above the maximum");
        assert!(error.contains(&refusal), "{error}");
    }

    #[test]
    fn a_depth_that_is_not_a_whole_number_from_zero_is_refused() {
        for written in ["{ latest: -1 }", "{ latest: 2.5 }", "{ latest: \"3\" }"] {
            let error = parse(written).expect_err("not a depth");
            assert!(
                error.contains("expected a whole number of messages"),
                "{written}: {error}"
            );
        }
    }

    #[test]
    fn an_unknown_form_is_refused_with_the_accepted_forms() {
        for written in ["\"forever\"", "{}", "3"] {
            let error = parse(written).expect_err("an unknown form");
            assert!(
                error.contains("{ latest: <0 to 1024> }"),
                "{written}: {error}"
            );
        }
        let error = parse("{ newest: 2 }").expect_err("an unknown key");
        assert!(
            error.contains("unknown field `newest`, expected `latest`"),
            "{error}"
        );
        let error = parse("{ latest: 2, expires: 3 }").expect_err("a second key");
        assert!(
            error.contains("unknown field `expires`, expected `latest`"),
            "{error}"
        );
        let error = parse("{ latest: 2, latest: 3 }").expect_err("a repeated key");
        assert!(error.contains("duplicate field `latest`"), "{error}");
    }

    #[test]
    fn a_policy_serializes_to_one_form_and_parses_back() {
        for (policy, written) in [
            (TopicRetention::LiveOnly, "{\"latest\":0}"),
            (latest(1), "{\"latest\":1}"),
            (latest(5), "{\"latest\":5}"),
        ] {
            assert_eq!(serde_json5::to_string(&policy).unwrap(), written);
            assert_eq!(parse(written), Ok(policy));
        }
    }

    /// A manifest is also carried as JSON, whose numbers arrive unsigned.
    #[test]
    fn a_policy_round_trips_through_json() {
        for policy in [TopicRetention::LiveOnly, latest(1), latest(5)] {
            let written = serde_json::to_value(policy).unwrap();
            assert_eq!(
                serde_json::from_value::<TopicRetention>(written).unwrap(),
                policy
            );
        }
    }

    #[test]
    fn a_policy_displays_in_plain_words() {
        assert_eq!(TopicRetention::LiveOnly.to_string(), "live only");
        assert_eq!(latest(3).to_string(), "latest 3");
    }
}
