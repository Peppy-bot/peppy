//! Which clock every instance of a plan reads, and the rule that a
//! clock-dependent connection joins two instances reading the same one.
//!
//! A launch declares its domains once, at the document level, and each
//! instance either publishes one (because a declaration named it) or names
//! the one it reads. This module turns those declarations and bindings into
//! the concrete [`ClockBinding`] each instance is started with, and then holds
//! every connection of the plan to the timelines it just resolved.
//!
//! Resolution needs the whole flattened plan, because a domain declared in a
//! fragment can name a publisher another fragment deploys, and it needs the
//! placements, because a domain's identity includes the machine its publisher
//! runs on.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use config::runtime::{
    ClockBinding, ClockDomainId, ClockIncarnation, Name, ProducerRef, SlotBindings,
};

use super::observations::PlannedObservation;
use super::pairings::PlannedPairing;
use super::types::{ClockDeclaration, PeppyLauncher, Placements, WALL_CLOCK};
use crate::error::{ClockMismatch, ParsingError};

/// The lifetime a preview names.
///
/// A domain's lifetime is minted when a launch runs, so a check that starts
/// nothing (`peppy stack resolve`, the index check) has none to report. It
/// stands one in, which every rule here is indifferent to: they compare
/// domains within one plan, where a single value makes every comparison read
/// exactly as it will at launch.
const PREVIEW_INCARNATION: u64 = 1;

/// The lifetimes a launch minted for its domains, by domain name. A plan
/// resolved for preview passes an empty map.
pub type ClockIncarnations = BTreeMap<Name, ClockIncarnation>;

/// The lifetimes this process has minted, seeded on first use from unix-millis
/// so a restart resumes above every value the previous one reached.
static MINTED: AtomicU64 = AtomicU64::new(0);

/// A fresh lifetime for one domain.
///
/// Reusing a domain name mints a new one, so the ticks of an earlier lifetime
/// address a stream no consumer of the new one reads, and a delayed tick can
/// never reach a replacement.
///
/// Strictly increasing, which is what makes repeating a lifetime impossible
/// rather than unlikely: within a process the counter advances, and across a
/// restart the unix-millis seed already exceeds it, because minting a lifetime
/// every millisecond is far beyond what starting a publisher costs.
pub fn mint_incarnation() -> ClockIncarnation {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_millis() as u64)
        .unwrap_or_default();
    MINTED.fetch_max(seed, Ordering::Relaxed);
    let value = MINTED.fetch_add(1, Ordering::Relaxed) + 1;
    ClockIncarnation::try_from(value).expect("a counter seeded from unix-millis is in range")
}

/// The clock every instance of a plan reads.
#[derive(Debug, Clone, Default)]
pub struct ResolvedClocks {
    by_instance: BTreeMap<String, ClockBinding>,
}

impl ResolvedClocks {
    /// The clocks of instances already running, as a `stack list` reports
    /// them, so a `node run` preflight holds its new connections to the same
    /// rule a launch applies.
    pub fn of_running(entries: impl IntoIterator<Item = (String, ClockBinding)>) -> Self {
        Self {
            by_instance: entries.into_iter().collect(),
        }
    }

    /// The clock `instance_id` reads. An instance this plan does not place
    /// reads wall time, which is also what an omitted binding means.
    pub fn of(&self, instance_id: &str) -> &ClockBinding {
        const WALL: &ClockBinding = &ClockBinding::Wall;
        self.by_instance.get(instance_id).unwrap_or(WALL)
    }

    /// The binding to stamp on `instance_id`'s plan.
    pub fn binding_for(&self, instance_id: &str) -> ClockBinding {
        self.of(instance_id).clone()
    }

    /// Every instance this plan bound to a simulated domain, in id order.
    pub fn simulated(&self) -> impl Iterator<Item = (&str, &ClockDomainId)> {
        self.by_instance
            .iter()
            .filter_map(|(instance, binding)| Some((instance.as_str(), binding.domain()?)))
    }

    pub fn insert(&mut self, instance_id: impl Into<String>, binding: ClockBinding) {
        self.by_instance.insert(instance_id.into(), binding);
    }

    /// Forgets `instance_id`'s clock, for an instance leaving the stack. It
    /// reads wall time from here on, which is what an instance this plan does
    /// not place reads anyway.
    pub fn remove(&mut self, instance_id: &str) {
        self.by_instance.remove(instance_id);
    }
}

/// Resolves the clock of every instance in `launcher`, which must be a
/// flattened plan.
///
/// Every refusal names the instance and the domain it is about, and none of
/// them depends on a node manifest, so a preview reports them with an empty
/// manifest cache.
pub fn resolve_clocks(
    launcher: &PeppyLauncher,
    placements: &Placements,
    incarnations: &ClockIncarnations,
) -> Result<ResolvedClocks, Vec<ParsingError>> {
    let mut errors = Vec::new();
    let declared = &launcher.framework.clocks;
    let instances: BTreeSet<&str> = launcher
        .deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
        .map(|instance| instance.instance_id.as_str())
        .collect();

    // `wall` is the name of the time every machine already keeps, so a
    // declaration could only ever redefine it.
    for domain in declared.keys() {
        if domain.as_str() == WALL_CLOCK {
            errors.push(ParsingError::ClockDomainReserved {
                domains: crate::format_quoted_list(
                    declared.keys().map(Name::as_str).collect::<Vec<_>>(),
                ),
            });
        }
    }

    // One instance supplies at most one domain: it reads one clock, and
    // supplying two would be two.
    let mut publishes: BTreeMap<&str, Vec<&Name>> = BTreeMap::new();
    for (domain, declaration) in declared {
        let Some(publisher) = declaration.publisher() else {
            continue;
        };
        if !instances.contains(publisher.as_str()) {
            errors.push(if instances.is_empty() {
                ParsingError::ClockPublisherWithoutDeployments {
                    domain: domain.to_string(),
                    publisher: publisher.to_string(),
                }
            } else {
                ParsingError::ClockPublisherUnknown {
                    domain: domain.to_string(),
                    publisher: publisher.to_string(),
                    instances: crate::format_quoted_list(instances.iter().copied()),
                }
            });
            continue;
        }
        publishes
            .entry(publisher.as_str())
            .or_default()
            .push(domain);
    }
    for (publisher, domains) in &publishes {
        if domains.len() > 1 {
            errors.push(ParsingError::ClockPublisherOfSeveral {
                instance: (*publisher).to_owned(),
                domains: crate::format_quoted_list(domains.iter().copied()),
            });
        }
    }

    let domain_id = |domain: &Name, publisher: &str| -> ClockDomainId {
        ClockDomainId::new(
            domain.clone(),
            placements.core_node_of(publisher).clone(),
            incarnations.get(domain).copied().unwrap_or_else(|| {
                ClockIncarnation::try_from(PREVIEW_INCARNATION).expect("non-zero")
            }),
        )
    };

    let mut resolved = ResolvedClocks::default();
    for instance in launcher
        .deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
    {
        let id = instance.instance_id.as_str();
        let supplies = publishes
            .get(id)
            .and_then(|domains| domains.first().copied());

        if let Some(domain) = supplies {
            // The declaration already assigns this instance its domain, so a
            // binding beside it is a second place to say the same thing, or a
            // different one.
            if let Some(bound) = &instance.framework.clock {
                errors.push(ParsingError::ClockPublisherBound {
                    instance: id.to_owned(),
                    domain: domain.to_string(),
                    bound: bound.to_string(),
                });
                continue;
            }
            resolved.insert(id, ClockBinding::publisher(domain_id(domain, id)));
            continue;
        }

        let Some(bound) = &instance.framework.clock else {
            resolved.insert(id, ClockBinding::Wall);
            continue;
        };
        if bound.as_str() == WALL_CLOCK {
            resolved.insert(id, ClockBinding::Wall);
            continue;
        }
        match declared.get(bound) {
            None => errors.push(ParsingError::ClockDomainUnknown {
                instance: id.to_owned(),
                clock: bound.to_string(),
                declared: {
                    // `wall` leads the list: it is always nameable, so the
                    // message reads the same whether or not a domain exists.
                    crate::format_quoted_list(
                        std::iter::once(WALL_CLOCK)
                            .chain(declared.keys().map(Name::as_str))
                            .collect::<Vec<_>>(),
                    )
                },
            }),
            // A descriptive name for wall time is an alias, not a timeline of
            // its own, so every instance naming one reads the same clock.
            Some(ClockDeclaration::Wall) => resolved.insert(id, ClockBinding::Wall),
            Some(ClockDeclaration::Sim { publisher }) => resolved.insert(
                id,
                ClockBinding::consumer(
                    domain_id(bound, publisher.as_str()),
                    ProducerRef::new(placements.of(publisher.as_str()), publisher.as_str()),
                ),
            ),
        }
    }

    if errors.is_empty() {
        Ok(resolved)
    } else {
        Err(errors)
    }
}

/// Holds every clock-dependent connection of the plan to one timeline.
///
/// Producer bindings, participant pairings and observations are all
/// clock-dependent: each carries timestamps one side stamps and the other
/// interprets. Two aliases of wall time are one timeline; two simulated
/// domains never are, however close their instants happen to run.
pub fn validate_clock_connections(
    clocks: &ResolvedClocks,
    slot_bindings: &BTreeMap<String, SlotBindings>,
    pairings: &[PlannedPairing],
    observations: &[PlannedObservation],
) -> Vec<ParsingError> {
    let mut errors = Vec::new();
    let mut refuse = |a: &str, b: &str, via: String| {
        let (a_clock, b_clock) = (clocks.of(a), clocks.of(b));
        if a_clock.is_compatible_with(b_clock) {
            return;
        }
        errors.push(ParsingError::ClockMismatch(Box::new(ClockMismatch {
            a: a.to_owned(),
            a_clock: a_clock.label(),
            b: b.to_owned(),
            b_clock: b_clock.label(),
            via,
        })));
    };

    for (consumer, slots) in slot_bindings {
        for (link_id, producers) in slots {
            for producer in producers.as_slice() {
                refuse(
                    consumer,
                    &producer.instance_id,
                    format!("binding `{link_id}`"),
                );
            }
        }
    }
    for pairing in pairings {
        refuse(
            &pairing.a.instance_id,
            &pairing.b.instance_id,
            format!("pairing `{}`", pairing.a.link_id),
        );
    }
    for observation in observations {
        refuse(
            &observation.observer_instance_id,
            &observation.source.instance_id,
            format!("observation `{}`", observation.observer_link_id),
        );
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_minted_lifetime_is_distinct_and_rising() {
        let minted: Vec<u64> = (0..1_000).map(|_| mint_incarnation().get()).collect();
        let mut sorted = minted.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), minted.len(), "a lifetime is never reused");
        assert!(
            minted.windows(2).all(|pair| pair[0] < pair[1]),
            "each lifetime exceeds the one before it"
        );
    }

    #[test]
    fn concurrent_minting_never_hands_out_one_lifetime_twice() {
        let minted: BTreeSet<u64> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        (0..500)
                            .map(|_| mint_incarnation().get())
                            .collect::<Vec<u64>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().expect("a minting thread never panics"))
                .collect::<Vec<u64>>()
        })
        .into_iter()
        .collect();
        assert_eq!(minted.len(), 8 * 500, "every thread got its own lifetimes");
    }

    #[test]
    fn a_lifetime_starts_past_the_wall_clock_so_a_restart_never_repeats_one() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the test host is past the epoch")
            .as_millis() as u64;
        assert!(
            mint_incarnation().get() > before,
            "a fresh process resumes above every value the last one reached"
        );
    }
}
