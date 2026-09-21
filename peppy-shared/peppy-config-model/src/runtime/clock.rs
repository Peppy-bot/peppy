//! Clock domains: the identity of one timeline and one instance's binding to
//! it.
//!
//! An instance reads exactly one clock for its whole lifetime. Wall time is
//! built in and needs no declaration. A simulated domain is supplied by one
//! publisher instance, and every other instance that reads it names it.
//!
//! A domain's identity is its name, the machine its publisher runs on, and an
//! incarnation minted for each lifetime of that name. Two domains are the same
//! timeline when all three agree, so compatibility is an equality check over
//! the whole identity.

use super::{CoreNodeName, Name, ProducerRef};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

/// One lifetime of a domain name on one machine.
///
/// Minted when a domain is declared, by the coordinator of a launch or by the
/// CLI starting a publisher. Reusing a name mints a new value, so the ticks of
/// an earlier lifetime address a stream no consumer of the new one reads, and
/// a delayed tick can never reach a replacement.
///
/// Zero is not a value: it is the "no tick observed" sentinel the clock cache
/// uses, and keeping the two apart in the type stops an absent incarnation
/// from reading as a real one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClockIncarnation(NonZeroU64);

impl ClockIncarnation {
    /// The largest value a lifetime may take.
    ///
    /// A runtime config reaches a spawned node as JSON5, whose numbers are
    /// IEEE doubles, so an integer above this comes back rounded. A lifetime
    /// that changed in transit addresses a different `link_id` from the one
    /// its publisher writes, so the node subscribes to a key nothing fills
    /// and its domain never reports ready.
    pub const MAX: u64 = 1 << 53;

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// A value that cannot serve as a lifetime, read off the wire or out of a
/// config where the field is a plain integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidIncarnation {
    #[error("a clock incarnation is never zero; zero is the wire's \"no tick observed\" sentinel")]
    Zero,
    #[error(
        "clock incarnation {0} is above {max}, so it cannot survive the JSON5 a runtime config \
         travels as: the value a node reads back would address a different clock stream from \
         the one its publisher writes",
        max = ClockIncarnation::MAX
    )]
    TooLarge(u64),
}

impl TryFrom<u64> for ClockIncarnation {
    type Error = InvalidIncarnation;

    /// Refuses both values a lifetime can never take: zero, the wire's
    /// not-observed sentinel, and anything a JSON5 number cannot carry
    /// intact. Rejecting here catches a rounded value wherever it enters,
    /// including one this process never minted.
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value > Self::MAX {
            return Err(InvalidIncarnation::TooLarge(value));
        }
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(InvalidIncarnation::Zero)
    }
}

/// Which timeline an instance reads, fully qualified.
///
/// `core_node` is the machine the publisher runs on, so a name is unique per
/// machine and two machines may each carry a `robot` without sharing a
/// timeline. Consumers address the domain by this whole identity wherever it
/// is placed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockDomainId {
    pub name: Name,
    pub core_node: CoreNodeName,
    pub incarnation: ClockIncarnation,
}

impl ClockDomainId {
    pub fn new(name: Name, core_node: CoreNodeName, incarnation: ClockIncarnation) -> Self {
        Self {
            name,
            core_node,
            incarnation,
        }
    }

    /// The wire segment this domain's ticks are published under, in the
    /// `link_id` slot of the `clock` topic. The name leads so a key dump reads
    /// as the domain an operator named; the incarnation follows so one
    /// lifetime's ticks never land on another's key.
    pub fn link_id(&self) -> String {
        format!("{}_{:016x}", self.name, self.incarnation.get())
    }
}

impl std::fmt::Display for ClockDomainId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.core_node)
    }
}

/// What an instance does with the domain it is bound to.
///
/// A publisher supplies the domain's time and reads back what it committed. A
/// consumer reads the ticks that publisher sends, and carries the publisher's
/// address so it subscribes to that one stream rather than to a name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ClockRole {
    Publisher,
    Consumer { publisher: ProducerRef },
}

/// The one clock an instance reads for its lifetime.
///
/// Wall is the default and the built-in: an instance whose deployment names no
/// domain reads its own machine's clock. `Sim` carries the whole identity of
/// the domain plus this instance's role in it, so a spawned node needs nothing
/// else to find its time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ClockBinding {
    #[default]
    Wall,
    Sim {
        domain: ClockDomainId,
        role: ClockRole,
    },
}

impl ClockBinding {
    /// This instance supplies `domain`.
    pub fn publisher(domain: ClockDomainId) -> Self {
        Self::Sim {
            domain,
            role: ClockRole::Publisher,
        }
    }

    /// This instance reads `domain`, supplied by `publisher`.
    pub fn consumer(domain: ClockDomainId, publisher: ProducerRef) -> Self {
        Self::Sim {
            domain,
            role: ClockRole::Consumer { publisher },
        }
    }

    /// Wall time, which is also what an omitted binding resolves to. Serialized
    /// forms skip the field on this value.
    pub fn is_wall(&self) -> bool {
        matches!(self, Self::Wall)
    }

    pub fn domain(&self) -> Option<&ClockDomainId> {
        match self {
            Self::Wall => None,
            Self::Sim { domain, .. } => Some(domain),
        }
    }

    pub fn is_publisher(&self) -> bool {
        matches!(
            self,
            Self::Sim {
                role: ClockRole::Publisher,
                ..
            }
        )
    }

    /// The instance whose ticks this binding reads, for a consumer.
    pub fn publisher_ref(&self) -> Option<&ProducerRef> {
        match self {
            Self::Sim {
                role: ClockRole::Consumer { publisher },
                ..
            } => Some(publisher),
            _ => None,
        }
    }

    /// Whether two instances may hold a clock-dependent connection: they read
    /// the same timeline. Every alias of wall time is one timeline; two
    /// simulated domains are one only when their identities agree.
    pub fn is_compatible_with(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Wall, Self::Wall) => true,
            (Self::Sim { domain: a, .. }, Self::Sim { domain: b, .. }) => a == b,
            _ => false,
        }
    }

    /// How a refusal names this clock to an operator.
    pub fn label(&self) -> String {
        match self {
            Self::Wall => "wall".to_owned(),
            Self::Sim { domain, .. } => domain.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incarnation(value: u64) -> ClockIncarnation {
        ClockIncarnation::try_from(value).expect("non-zero")
    }

    fn domain(name: &str, core_node: &str, value: u64) -> ClockDomainId {
        ClockDomainId::new(
            Name::new(name).expect("valid name"),
            CoreNodeName::new(core_node).expect("valid core node name"),
            incarnation(value),
        )
    }

    /// A runtime config crosses to a spawned node as JSON5, whose numbers are
    /// doubles. Every value the type admits has to survive that trip, or the
    /// node reads a lifetime its daemon never chose.
    #[test]
    fn every_admitted_lifetime_survives_a_json5_round_trip() {
        for value in [
            1,
            2,
            1_000_000,
            ClockIncarnation::MAX - 1,
            ClockIncarnation::MAX,
        ] {
            let held = ClockIncarnation::try_from(value).expect("inside the range");
            let text = serde_json5::to_string(&held).expect("serializes");
            let back: ClockIncarnation = serde_json5::from_str(&text).expect("parses");
            assert_eq!(back, held, "lifetime changed in transit: {text}");
        }
    }

    /// The value that broke a three-machine launch: a draw from the whole u64
    /// range, which JSON5 rounds to a neighbour no publisher writes.
    #[test]
    fn a_lifetime_too_large_for_a_config_is_refused() {
        let rounded = 7_714_581_040_992_002_510u64;
        assert_eq!(
            ClockIncarnation::try_from(rounded),
            Err(InvalidIncarnation::TooLarge(rounded))
        );
        assert!(ClockIncarnation::try_from(ClockIncarnation::MAX + 1).is_err());
        assert!(ClockIncarnation::try_from(u64::MAX).is_err());
    }

    #[test]
    fn a_zero_incarnation_is_refused() {
        assert_eq!(ClockIncarnation::try_from(0), Err(InvalidIncarnation::Zero));
        assert_eq!(incarnation(1).get(), 1);
    }

    #[test]
    fn a_domain_reads_as_name_at_machine_and_keys_by_incarnation() {
        let robot = domain("robot", "cn-sim", 0x1234);
        assert_eq!(robot.to_string(), "robot@cn-sim");
        assert_eq!(robot.link_id(), "robot_0000000000001234");
    }

    /// The link_id lands in a zenoh key segment, which forbids `/` and `@`
    /// and reserves `_` on its own. The rendering must never produce one.
    #[test]
    fn a_link_id_is_a_usable_wire_segment() {
        let link_id = domain("robot", "cn-sim", ClockIncarnation::MAX).link_id();
        assert!(!link_id.contains('/'));
        assert!(!link_id.contains('@'));
        assert!(!link_id.is_empty());
        assert_ne!(link_id, "_");
        assert_ne!(link_id, "*");
    }

    /// A name is one timeline per machine and per lifetime: the same name on
    /// two machines, and the same name across two lifetimes, are two
    /// timelines.
    #[test]
    fn a_name_is_one_timeline_per_machine_and_per_lifetime() {
        let here = ClockBinding::publisher(domain("robot", "cn-a", 7));
        assert!(here.is_compatible_with(&ClockBinding::publisher(domain("robot", "cn-a", 7))));
        assert!(!here.is_compatible_with(&ClockBinding::publisher(domain("robot", "cn-b", 7))));
        assert!(!here.is_compatible_with(&ClockBinding::publisher(domain("robot", "cn-a", 8))));
    }

    #[test]
    fn wall_is_one_timeline_and_never_a_simulated_one() {
        let sim = ClockBinding::publisher(domain("robot", "cn-a", 1));
        assert!(ClockBinding::Wall.is_compatible_with(&ClockBinding::Wall));
        assert!(!ClockBinding::Wall.is_compatible_with(&sim));
        assert!(!sim.is_compatible_with(&ClockBinding::Wall));
    }

    #[test]
    fn a_publisher_and_its_consumer_share_a_timeline() {
        let robot = domain("robot", "cn-a", 1);
        let publisher = ClockBinding::publisher(robot.clone());
        let consumer = ClockBinding::consumer(robot, ProducerRef::new("cn-a", "sim_inst"));
        assert!(publisher.is_compatible_with(&consumer));
        assert!(publisher.is_publisher());
        assert!(!consumer.is_publisher());
        assert_eq!(
            consumer.publisher_ref(),
            Some(&ProducerRef::new("cn-a", "sim_inst"))
        );
        assert_eq!(consumer.label(), "robot@cn-a");
    }

    #[test]
    fn every_shape_round_trips_and_wall_is_the_default() {
        assert!(ClockBinding::default().is_wall());
        for binding in [
            ClockBinding::Wall,
            ClockBinding::publisher(domain("robot", "cn-a", 9)),
            ClockBinding::consumer(
                domain("robot", "cn-a", 9),
                ProducerRef::new("cn-a", "sim_inst"),
            ),
        ] {
            let text = serde_json5::to_string(&binding).expect("serializes");
            let back: ClockBinding = serde_json5::from_str(&text).expect("parses");
            assert_eq!(back, binding, "round trip of {text}");
        }
    }
}
