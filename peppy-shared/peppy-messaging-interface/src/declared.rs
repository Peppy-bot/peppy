//! The publish keys a session declared, and the publisher of each retaining
//! one.
//!
//! A retaining topic has one publisher per publish key expression for the
//! life of its session. That publisher owns the messages a subscriber that
//! joins later receives, and every declaration of the key shares it. A
//! retaining key takes one retention and one QoS for the life of the session.

use crate::error::{Error, Result};
use crate::types::PublisherQoS;
use config::node::{RetentionDepth, TopicRetention};
use std::collections::HashMap;

/// The publish keys one session declared. `H` is the adapter's handle to a
/// retaining topic's publisher.
pub(crate) struct DeclaredTopics<H> {
    topics: HashMap<String, Declared<H>>,
}

/// What a session declared on one publish key.
enum Declared<H> {
    LiveOnly,
    Retaining(RetainingTopic<H>),
}

struct RetainingTopic<H> {
    depth: RetentionDepth,
    qos: PublisherQoS,
    handle: H,
}

impl<H> Default for DeclaredTopics<H> {
    fn default() -> Self {
        Self {
            topics: HashMap::new(),
        }
    }
}

impl<H: Clone> DeclaredTopics<H> {
    /// The retaining publisher a declaration of `keyexpr` publishes through,
    /// declared by `declare` on first use, or `None` for a live-only
    /// declaration. A declaration that asks for another retention than the
    /// key's first one, or for another QoS on a retaining key, is refused.
    pub(crate) fn declare(
        &mut self,
        keyexpr: &str,
        retention: TopicRetention,
        qos: PublisherQoS,
        declare: impl FnOnce(RetentionDepth) -> Result<H>,
    ) -> Result<Option<H>> {
        match (self.topics.get(keyexpr), retention) {
            (None, TopicRetention::LiveOnly) => {
                self.topics.insert(keyexpr.to_string(), Declared::LiveOnly);
                Ok(None)
            }
            (None, TopicRetention::Latest { depth }) => {
                let handle = declare(depth)?;
                self.topics.insert(
                    keyexpr.to_string(),
                    Declared::Retaining(RetainingTopic {
                        depth,
                        qos,
                        handle: handle.clone(),
                    }),
                );
                Ok(Some(handle))
            }
            (Some(Declared::LiveOnly), TopicRetention::LiveOnly) => Ok(None),
            (Some(Declared::Retaining(topic)), TopicRetention::Latest { depth })
                if topic.depth == depth && topic.qos == qos =>
            {
                Ok(Some(topic.handle.clone()))
            }
            (Some(declared), _) => Err(declared.mismatch(keyexpr, retention, qos)),
        }
    }

    /// Refuses a live-only publish on a key this session retains.
    pub(crate) fn refuse_live_only(&self, keyexpr: &str, qos: PublisherQoS) -> Result<()> {
        match self.topics.get(keyexpr) {
            Some(declared @ Declared::Retaining(_)) => {
                Err(declared.mismatch(keyexpr, TopicRetention::LiveOnly, qos))
            }
            _ => Ok(()),
        }
    }

    /// Every retaining topic and its publisher.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &H)> {
        self.topics
            .iter()
            .filter_map(|(keyexpr, declared)| match declared {
                Declared::Retaining(topic) => Some((keyexpr.as_str(), &topic.handle)),
                Declared::LiveOnly => None,
            })
    }

    /// Forgets every key, dropping each retaining publisher.
    pub(crate) fn clear(&mut self) {
        self.topics.clear();
    }
}

impl<H> Declared<H> {
    fn mismatch(&self, keyexpr: &str, retention: TopicRetention, qos: PublisherQoS) -> Error {
        let (declared_retention, declared_qos) = match self {
            Self::LiveOnly => (TopicRetention::LiveOnly, None),
            Self::Retaining(topic) => (
                TopicRetention::Latest { depth: topic.depth },
                Some(topic.qos),
            ),
        };
        Error::RetainingTopicMismatch {
            topic: keyexpr.to_string(),
            declared_retention,
            declared_qos,
            requested_retention: retention,
            requested_qos: qos,
        }
    }
}

/// Test support shared by this crate's unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    use config::node::TopicRetention;

    /// The policy that keeps the newest `depth` messages.
    pub(crate) fn latest(depth: usize) -> TopicRetention {
        TopicRetention::latest(depth).expect("a depth in range")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::latest;

    const KEYEXPR: &str = "*/core/*/instance/topic/node/robot/v1/_/mode";

    fn declared(
        topics: &mut DeclaredTopics<u32>,
        retention: TopicRetention,
        qos: PublisherQoS,
    ) -> Result<Option<u32>> {
        topics.declare(KEYEXPR, retention, qos, |_| Ok(7))
    }

    fn declared_on(
        topics: &mut DeclaredTopics<u32>,
        keyexpr: &str,
        retention: TopicRetention,
    ) -> Result<Option<u32>> {
        topics.declare(keyexpr, retention, PublisherQoS::Standard, |_| Ok(7))
    }

    #[test]
    fn a_key_is_declared_once_and_shared_after() {
        let mut topics = DeclaredTopics::default();
        let mut declarations = 0;
        for _ in 0..3 {
            let handle = topics
                .declare(KEYEXPR, latest(1), PublisherQoS::Standard, |depth| {
                    assert_eq!(depth, RetentionDepth::ONE);
                    declarations += 1;
                    Ok(7u32)
                })
                .expect("the same depth and QoS share the publisher");
            assert_eq!(handle, Some(7));
        }
        assert_eq!(declarations, 1);
    }

    #[test]
    fn a_second_depth_or_qos_is_refused() {
        let mut topics = DeclaredTopics::default();
        declared(&mut topics, latest(1), PublisherQoS::Standard).expect("the first declaration");

        for (retention, qos) in [
            (latest(3), PublisherQoS::Standard),
            (latest(1), PublisherQoS::Important),
            (TopicRetention::LiveOnly, PublisherQoS::Standard),
        ] {
            let error = declared(&mut topics, retention, qos).expect_err("another policy or QoS");
            assert!(
                matches!(
                    error,
                    Error::RetainingTopicMismatch { ref topic, requested_retention, requested_qos, .. }
                        if topic == KEYEXPR && requested_retention == retention && requested_qos == qos
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn a_mismatch_says_what_to_declare() {
        let mut topics = DeclaredTopics::default();
        declared(&mut topics, latest(1), PublisherQoS::Standard).expect("the first declaration");

        let error = declared(
            &mut topics,
            TopicRetention::LiveOnly,
            PublisherQoS::Important,
        )
        .expect_err("another policy and QoS");
        assert_eq!(
            error.to_string(),
            format!(
                "topic key `{KEYEXPR}` is declared with retention `latest 1` and QoS Standard, and \
                 this publisher asks for retention `live only` and QoS Important; declare every \
                 publisher of the topic with the same retention and QoS"
            )
        );

        let live_only_key = "*/core/*/instance/topic/node/robot/v1/_/pose";
        declared_on(&mut topics, live_only_key, TopicRetention::LiveOnly)
            .expect("a live-only declaration");
        let error = topics
            .declare(live_only_key, latest(1), PublisherQoS::Important, |_| Ok(7))
            .expect_err("the key is live only");
        assert_eq!(
            error.to_string(),
            format!(
                "topic key `{live_only_key}` is declared with retention `live only`, and this \
                 publisher asks for retention `latest 1` and QoS Important; declare every \
                 publisher of the topic with the same retention"
            )
        );
    }

    #[test]
    fn a_live_only_declaration_takes_no_publisher() {
        let mut topics = DeclaredTopics::default();
        assert_eq!(
            declared(
                &mut topics,
                TopicRetention::LiveOnly,
                PublisherQoS::Standard
            )
            .expect("no retaining topic on this key"),
            None
        );
        assert_eq!(topics.iter().count(), 0);
    }

    #[test]
    fn a_failed_declaration_leaves_the_key_free() {
        let mut topics = DeclaredTopics::<u32>::default();
        topics
            .declare(KEYEXPR, latest(1), PublisherQoS::Standard, |_| {
                Err(Error::ShutdownError)
            })
            .expect_err("the declaration failed");
        assert_eq!(
            declared(&mut topics, latest(3), PublisherQoS::Important)
                .expect("the key is still free"),
            Some(7)
        );
    }

    #[test]
    fn a_live_only_publish_is_refused_on_a_retaining_key_only() {
        let mut topics = DeclaredTopics::default();
        topics
            .refuse_live_only(KEYEXPR, PublisherQoS::Standard)
            .expect("no retaining topic on this key yet");
        declared(&mut topics, latest(1), PublisherQoS::Standard).expect("the first declaration");

        let error = topics
            .refuse_live_only(KEYEXPR, PublisherQoS::Standard)
            .expect_err("the key retains");
        assert!(
            matches!(
                error,
                Error::RetainingTopicMismatch {
                    requested_retention: TopicRetention::LiveOnly,
                    ..
                }
            ),
            "{error}"
        );

        let live_only_key = "*/core/*/instance/topic/node/robot/v1/_/pose";
        declared_on(&mut topics, live_only_key, TopicRetention::LiveOnly)
            .expect("a live-only declaration");
        topics
            .refuse_live_only(live_only_key, PublisherQoS::Important)
            .expect("a one-shot publish on a live-only key");
    }

    #[test]
    fn clearing_frees_every_key() {
        let mut topics = DeclaredTopics::default();
        declared(&mut topics, latest(1), PublisherQoS::Standard).expect("the first declaration");
        topics.clear();
        assert_eq!(topics.iter().count(), 0);
        declared(&mut topics, latest(3), PublisherQoS::Important).expect("the key is free again");
    }
}
