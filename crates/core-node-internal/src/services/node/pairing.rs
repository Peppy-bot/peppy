//! The daemon's single authority for pairing operations: registry commits
//! (via [`NodeStack`]) plus live delivery of the resulting pin state to the
//! endpoint nodes over the `peer_update` service.
//!
//! Every establish/dissolve goes through the one [`PairingCoordinator`]
//! instance the daemon owns, whose `op_lock` serializes operations so two
//! concurrent `node_run`s can never interleave a reserve/deliver/revert
//! sequence. Its only callers are the node-run/launch establishment hooks and
//! the death/stop clear paths — there is no runtime pairing service; pairs
//! are only ever established at instance start.
//!
//! Delivery protocol per pair `(a, b)`: send `a`'s pin, then `b`'s. If `a`
//! fails, revert the registry commit (nothing was delivered). If `b` fails,
//! revert and send a best-effort Unpaired to `a`, the side that already
//! acked. Updates carry a strictly increasing sequence seeded from the
//! daemon's start time in unix-millis, so a node that outlives a daemon
//! restart rejects stale re-deliveries from the old daemon's queue.

use config::runtime::Name;
use core_node_api::encoding::PairTarget;
use daemon_config::launcher::{
    AlreadyPairedSlots, DeploymentInstance, LinkValue, PairingValidationItem, validate_pairings,
};
use node_stack::{NodeStack, Pairing, PairingNodeSnapshot, SlotAddr};
use peppylib::MessengerHandle;
use peppylib::encoding::peer_update::PeerUpdateRequest;
use peppylib::messaging::{PEER_UPDATE_SERVICE, PeerPin, ProducerRef};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

use super::common::SlotUpdateClient;

/// How long a single `peer_update` delivery may take before the operation is
/// treated as failed and reverted. The service is pre-setup (registered
/// before the node's ready signal), so a healthy endpoint answers promptly.
const PEER_UPDATE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct PairingCoordinator {
    updates: SlotUpdateClient,
    /// Serializes every pairing operation end-to-end (registry commit AND
    /// delivery), so reverts can trust that no other operation interleaved.
    op_lock: tokio::sync::Mutex<()>,
}

impl PairingCoordinator {
    pub fn new(
        node_stack: Arc<NodeStack>,
        messenger: MessengerHandle,
        core_node_name: impl Into<String>,
        caller_instance_id: impl Into<String>,
    ) -> Self {
        Self {
            updates: SlotUpdateClient::new(
                node_stack,
                messenger,
                core_node_name,
                caller_instance_id,
            ),
            op_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Commits a pair to the registry WITHOUT delivering it (the
    /// reserve-before-spawn step of `node_run`: the owning instance is still
    /// `Starting`, and pins are only delivered once it commits to Running).
    /// All of `pair_slots`' validation applies: existence, liveness,
    /// same-pairing, complementary roles, sha pins, exclusivity.
    pub async fn reserve(&self, a: &SlotAddr, b: &SlotAddr) -> std::result::Result<(), String> {
        let _guard = self.op_lock.lock().await;
        self.updates
            .node_stack()
            .pair_slots(a, b)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Delivers the current pin state of every pair involving `instance_id`
    /// to both endpoints — the post-commit-to-Running step of `node_run`.
    /// Endpoints that are not live are skipped (they will receive their pins
    /// from their own `node_run` flow; `peer_update` is absolute-state and
    /// idempotent, so double delivery converges).
    ///
    /// `planned` is the set of pairs this run reserved. A concurrent
    /// stop/unpair can dissolve a reserved pair between reservation and this
    /// call; delivering whatever remains would let the run report "paired"
    /// for a pair that no longer exists, so every planned pair is re-checked
    /// against the registry (under the same `op_lock` as the delivery) and a
    /// missing one fails the call.
    ///
    /// On a delivery failure the failed pair is reverted (registry cleared +
    /// best-effort Unpaired to whichever side already acked) and an error
    /// naming the pair is returned; pairs already delivered in this call are
    /// left standing (the caller decides whether to dissolve everything).
    pub async fn deliver_pairs_for_instance(
        &self,
        instance_id: &str,
        planned: &[PlannedPair],
    ) -> std::result::Result<(), String> {
        let _guard = self.op_lock.lock().await;
        let pairs: Vec<Pairing> = self
            .updates
            .node_stack()
            .pairs()
            .into_iter()
            .filter(|p| p.involves_instance(instance_id))
            .collect();
        if let Some(missing) = find_missing_planned_pair(&pairs, planned) {
            return Err(format!(
                "pair `{} ⇌ {}` was dissolved before delivery completed \
                 (its peer was stopped or unpaired concurrently)",
                missing.own, missing.peer
            ));
        }
        for pairing in pairs {
            self.deliver_pair(&pairing).await?;
        }
        Ok(())
    }

    /// Dissolves every pair involving `instance_id` and best-effort notifies
    /// each live survivor that its slot is now Unpaired. Called from the
    /// stop paths, the process-exit watcher, and the `node_run` unwind
    /// branches (death auto-clears; re-pairing is explicit).
    pub async fn dissolve_for_instance(&self, instance_id: &str) {
        let _guard = self.op_lock.lock().await;
        for pairing in self
            .updates
            .node_stack()
            .dissolve_pairs_for_instance(instance_id)
        {
            debug!(
                "Dissolved pair `{}` ({}:{}) — instance '{}' is gone",
                pairing_label(&pairing),
                pairing.pairing_name,
                pairing.pairing_tag,
                instance_id
            );
            for endpoint in [&pairing.a, &pairing.b] {
                if endpoint.slot.instance_id == instance_id {
                    continue;
                }
                self.notify_unpaired_best_effort(&endpoint.slot).await;
            }
        }
    }

    /// Sends both endpoints of `pairing` their pins; reverts on failure per
    /// the module-level protocol.
    async fn deliver_pair(&self, pairing: &Pairing) -> std::result::Result<(), String> {
        let sides = [(&pairing.a, &pairing.b), (&pairing.b, &pairing.a)];
        for (idx, (endpoint, peer)) in sides.into_iter().enumerate() {
            if !self
                .updates
                .node_stack()
                .instance_is_live_for_pairing(&endpoint.slot.instance_id)
            {
                continue;
            }
            let pin = PeerPin {
                producer: ProducerRef::new(self.updates.core_node_name(), &peer.slot.instance_id),
                peer_link_id: peer.slot.link_id.clone(),
            };
            if let Err(reason) = self.send_peer_update(&endpoint.slot, Some(pin)).await {
                // Revert the commit; if the OTHER side already acked its pin,
                // best-effort roll it back to Unpaired.
                self.updates.node_stack().clear_pair(&endpoint.slot);
                if idx == 1 {
                    self.notify_unpaired_best_effort(&peer.slot).await;
                }
                return Err(format!(
                    "failed to deliver pair `{}` to instance '{}': {reason}",
                    pairing_label(pairing),
                    endpoint.slot.instance_id,
                ));
            }
        }
        Ok(())
    }

    /// Best-effort absolute Unpaired delivery; failures are logged, never
    /// propagated (the target may be mid-death, and the boot default plus
    /// lazy registry pruning make the unpaired state eventually consistent).
    async fn notify_unpaired_best_effort(&self, slot: &SlotAddr) {
        if !self
            .updates
            .node_stack()
            .instance_is_live_for_pairing(&slot.instance_id)
        {
            return;
        }
        if let Err(reason) = self.send_peer_update(slot, None).await {
            warn!(
                "Best-effort unpair notification to '{}' slot `{}` failed: {reason}",
                slot.instance_id, slot.link_id
            );
        }
    }

    /// One `peer_update` service call carrying the absolute pin state of
    /// `slot` on its owning instance. `Ok` on an accepted or stale reply
    /// (stale means a newer absolute state already landed, which is exactly
    /// the invariant we want).
    async fn send_peer_update(
        &self,
        slot: &SlotAddr,
        pin: Option<PeerPin>,
    ) -> std::result::Result<(), String> {
        let request = PeerUpdateRequest {
            link_id: slot.link_id.clone(),
            sequence: self.updates.next_sequence(),
            pin,
        };
        let payload = request.encode().map_err(|e| e.to_string())?;

        self.updates
            .send(
                &slot.instance_id,
                PEER_UPDATE_SERVICE,
                payload,
                PEER_UPDATE_TIMEOUT,
                "peer_update rejected",
            )
            .await
    }
}

/// `a_inst:a_link ⇌ b_inst:b_link`, the human-readable pair label used in
/// logs and error messages (matches `peppy stack list`).
fn pairing_label(pairing: &Pairing) -> String {
    format!("{} ⇌ {}", pairing.a.slot, pairing.b.slot)
}

/// The first planned pair with no matching registry pair (either endpoint
/// order), or `None` when every planned pair is present.
fn find_missing_planned_pair<'a>(
    pairs: &[Pairing],
    planned: &'a [PlannedPair],
) -> Option<&'a PlannedPair> {
    planned.iter().find(|plan| {
        !pairs.iter().any(|pair| {
            pair.peer_of(&plan.own)
                .is_some_and(|peer| peer.slot == plan.peer)
        })
    })
}

/// One resolved `--link` request: this instance's slot and the concrete peer
/// slot it will be paired with (ready for [`PairingCoordinator::reserve`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPair {
    pub own: SlotAddr,
    pub peer: SlotAddr,
}

/// The new instance's side of a plan-phase pairing check: its identity plus
/// the `node_run` goal's pairing arguments.
pub struct PairingRequest<'a> {
    pub node_name: &'a str,
    pub node_tag: &'a str,
    pub instance_id: &'a str,
    pub pairing_deps: &'a [config::node::PairingDependency],
    /// `link_id -> peer target` from `--link` / a launch plan.
    pub requested: &'a std::collections::BTreeMap<String, PairTarget>,
    /// Slots deliberately starting unpaired (`--defer-link` / the
    /// launcher's `defer_links:`).
    pub deferred: &'a [String],
    /// Launch-mechanism markers: slots a later-starting instance of the
    /// same launch will claim ([`NodeRunGoal::covered_pairs`] — see its doc
    /// for the covered-vs-deferred distinction).
    ///
    /// [`NodeRunGoal::covered_pairs`]: core_node_api::encoding::NodeRunGoal::covered_pairs
    pub covered: &'a std::collections::BTreeMap<String, PairTarget>,
}

/// The daemon-side re-check of a `node_run` goal's pairing arguments — the
/// trust-boundary twin of the CLI preflight and the launcher validator,
/// sharing their [`validate_pairings`] core so the plan-phase rule set
/// (declared-slot keys, request/defer overlap, required-slot coverage,
/// complementary-target resolution, exclusivity) exists exactly once. Runs
/// BEFORE the instance is spawned so every violation fails loudly with
/// nothing to unwind. Two daemon-specific wrinkles on top of the shared
/// core:
///
/// - a request targeting the new instance itself is rejected up front: the
///   instance is not in the stack yet, so the shared resolver would report
///   a misleading "unknown instance" instead of the real problem;
/// - covered slots (the earlier endpoints of launch-planned pairs) satisfy
///   the coverage rule like defers, but optional covered slots are dropped
///   before the validator: optional slots pass coverage on their own, and
///   the validator rightly rejects optional entries in `defer_links` as
///   a user error. A covered key naming an unknown slot is kept so the
///   validator reports it.
///
/// `snapshot` and `live_pairs` are the stack's live instances and claimed
/// slots ([`NodeStack::pairing_node_snapshots`] / [`NodeStack::live_pairs`]);
/// the new instance is synthesized from `request` and must not appear in
/// the snapshot. [`NodeStack::pair_slots`] remains the authoritative
/// re-validation at the reserve commit point; this plan-phase check exists
/// to fail before anything is spawned.
pub fn plan_requested_pairs(
    snapshot: &[PairingNodeSnapshot],
    live_pairs: &[Pairing],
    request: &PairingRequest<'_>,
) -> std::result::Result<Vec<PlannedPair>, String> {
    let &PairingRequest {
        node_name,
        node_tag,
        instance_id,
        pairing_deps,
        requested,
        deferred,
        covered,
    } = request;
    for (link_id, target) in requested {
        if target.peer_instance_id == instance_id {
            return Err(format!(
                "pairing slot `{link_id}` targets its own instance '{instance_id}'; \
                 a pair joins two distinct instances"
            ));
        }
    }

    // Every requested / covered / deferred key must name one of this node's
    // participant pairing slots. `validate_pairings` intentionally SKIPS keys
    // that are not participant slots (the unified `links` map lets producer and
    // observer keys share the namespace), so this boundary — where the goal has
    // already classified pairs — is where a stray key is caught. This restores
    // the old dead-key rejection that the by-validator classification dropped.
    let participant_slots: std::collections::BTreeSet<&str> = pairing_deps
        .iter()
        .filter(|dependency| dependency.is_participant())
        .map(|dependency| dependency.link_id())
        .collect();
    for link_id in requested
        .keys()
        .chain(covered.keys())
        .chain(deferred.iter())
    {
        if !participant_slots.contains(link_id.as_str()) {
            return Err(format!(
                "pairing slot `{link_id}` on instance '{instance_id}' matches no declared \
                 participant pairing slot; declared: [{}]",
                participant_slots
                    .iter()
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    let defer_like: Vec<String> = deferred
        .iter()
        .chain(covered.keys().filter(|link| {
            // Only a required (non-optional) participant slot needs a
            // defer-like entry; observer slots are not participants.
            !pairing_deps.iter().any(|d| {
                matches!(
                    d,
                    config::node::PairingDependency::Participant(p)
                        if p.link_id == **link && p.optional
                )
            })
        }))
        .cloned()
        .collect();
    let own_instances = vec![DeploymentInstance {
        // Rendered into the validator's launcher target grammar
        // (`peer[/peer_link]`) as a scalar link value; lossless, since
        // instance ids and link_ids are `/`-free names.
        links: requested
            .iter()
            .map(|(link_id, target)| (link_id.clone(), LinkValue::Scalar(target.to_string())))
            .collect(),
        defer_links: defer_like,
        ..DeploymentInstance::empty(
            Name::new(instance_id)
                .map_err(|e| format!("invalid instance id `{instance_id}`: {e}"))?,
        )
    }];
    let peer_instances: Vec<Vec<DeploymentInstance>> = snapshot
        .iter()
        .map(|node| {
            node.instance_ids
                .iter()
                .map(|id| {
                    Name::new(id.as_str())
                        .map(DeploymentInstance::empty)
                        .map_err(|e| format!("invalid instance id `{id}` in stack: {e}"))
                })
                .collect()
        })
        .collect::<std::result::Result<_, String>>()?;

    let mut items: Vec<PairingValidationItem<'_>> = snapshot
        .iter()
        .zip(&peer_instances)
        .map(|(node, instances)| PairingValidationItem {
            node_name: &node.node_name,
            node_tag: &node.node_tag,
            instances,
            pairing_deps: &node.pairing_deps,
            preexisting: true,
        })
        .collect();
    items.push(PairingValidationItem {
        node_name,
        node_tag,
        instances: &own_instances,
        pairing_deps,
        preexisting: false,
    });

    let already_paired: AlreadyPairedSlots = live_pairs
        .iter()
        .flat_map(|p| {
            [
                (
                    (p.a.slot.instance_id.clone(), p.a.slot.link_id.clone()),
                    p.b.slot.to_string(),
                ),
                (
                    (p.b.slot.instance_id.clone(), p.b.slot.link_id.clone()),
                    p.a.slot.to_string(),
                ),
            ]
        })
        .collect();

    let validated = validate_pairings(&items, &already_paired);
    if !validated.errors.is_empty() {
        let errors: Vec<String> = validated.errors.iter().map(|e| e.to_string()).collect();
        return Err(daemon_config::format_bulleted(&errors));
    }

    Ok(validated
        .planned
        .into_iter()
        .map(|pair| {
            // `a` is the declaring side, and the only declaring (non-
            // preexisting) item here is the new instance.
            debug_assert_eq!(pair.a.instance_id, instance_id);
            PlannedPair {
                own: SlotAddr::new(pair.a.instance_id, pair.a.link_id),
                peer: SlotAddr::new(pair.b.instance_id, pair.b.link_id),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::PairingDependency;
    use std::collections::BTreeMap;

    fn dep(role: &str, link_id: &str, optional: bool) -> PairingDependency {
        serde_json5::from_str(&format!(
            r#"{{ name: "arm_link", tag: "v1", role: "{role}", link_id: "{link_id}",
                 optional: {optional} }}"#
        ))
        .expect("valid pairing dependency")
    }

    fn snapshot_node(
        name: &str,
        instance_ids: &[&str],
        deps: &[PairingDependency],
    ) -> PairingNodeSnapshot {
        PairingNodeSnapshot {
            node_name: name.to_string(),
            node_tag: "v1".to_string(),
            instance_ids: instance_ids.iter().map(|s| s.to_string()).collect(),
            pairing_deps: deps.to_vec(),
        }
    }

    /// One running arm instance with one unpaired complementary slot.
    fn arm_snapshot() -> Vec<PairingNodeSnapshot> {
        vec![snapshot_node(
            "robot_arm",
            &["arm_1"],
            &[dep("arm", "controller", true)],
        )]
    }

    fn requested(entries: &[(&str, PairTarget)]) -> BTreeMap<String, PairTarget> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// `plan_requested_pairs` with no live pairs, no covered slots, and a
    /// fixed new-node label.
    fn plan(
        snapshot: &[PairingNodeSnapshot],
        instance_id: &str,
        deps: &[PairingDependency],
        req: &BTreeMap<String, PairTarget>,
        deferred: &[String],
    ) -> std::result::Result<Vec<PlannedPair>, String> {
        plan_requested_pairs(
            snapshot,
            &[],
            &PairingRequest {
                node_name: "new_node",
                node_tag: "v1",
                instance_id,
                pairing_deps: deps,
                requested: req,
                deferred,
                covered: &BTreeMap::new(),
            },
        )
    }

    #[test]
    fn resolves_a_single_complementary_slot() {
        let deps = [dep("controller", "arm", false)];
        let planned = plan(
            &arm_snapshot(),
            "ctrl_1",
            &deps,
            &requested(&[("arm", PairTarget::new("arm_1"))]),
            &[],
        )
        .expect("unambiguous target resolves");
        assert_eq!(
            planned,
            vec![PlannedPair {
                own: SlotAddr::new("ctrl_1", "arm"),
                peer: SlotAddr::new("arm_1", "controller"),
            }]
        );
    }

    #[test]
    fn explicit_peer_link_pin_resolves_and_wrong_pin_fails() {
        let deps = [dep("controller", "arm", false)];
        let planned = plan(
            &arm_snapshot(),
            "ctrl_1",
            &deps,
            &requested(&[("arm", PairTarget::pinned("arm_1", "controller"))]),
            &[],
        )
        .expect("pinned target resolves");
        assert_eq!(planned[0].peer, SlotAddr::new("arm_1", "controller"));

        let err = plan(
            &arm_snapshot(),
            "ctrl_1",
            &deps,
            &requested(&[("arm", PairTarget::pinned("arm_1", "nope"))]),
            &[],
        )
        .expect_err("wrong peer_link rejected");
        assert!(err.contains("complementary"), "{err}");
    }

    #[test]
    fn uncovered_required_slot_names_the_exact_flags() {
        let deps = [dep("controller", "arm", false)];
        let err = plan(&arm_snapshot(), "ctrl_1", &deps, &BTreeMap::new(), &[])
            .expect_err("required slot must be covered");
        assert!(
            err.contains("arm") && err.contains("--link") && err.contains("--defer-link"),
            "error should name the slot and both flags: {err}"
        );

        // Deferring it satisfies coverage; optional slots never need covering.
        let deps_opt = [dep("controller", "arm", true)];
        assert!(
            plan(&arm_snapshot(), "ctrl_1", &deps_opt, &BTreeMap::new(), &[])
                .expect("optional slot boots unpaired freely")
                .is_empty()
        );
        assert!(
            plan(
                &arm_snapshot(),
                "ctrl_1",
                &deps,
                &BTreeMap::new(),
                &["arm".to_string()],
            )
            .expect("deferred required slot passes coverage")
            .is_empty()
        );
    }

    /// `plan_requested_pairs` with covered slots (the earlier endpoints of
    /// launch-planned pairs) and nothing else.
    fn plan_covered(
        deps: &[PairingDependency],
        covered: &BTreeMap<String, PairTarget>,
    ) -> std::result::Result<Vec<PlannedPair>, String> {
        plan_requested_pairs(
            &arm_snapshot(),
            &[],
            &PairingRequest {
                node_name: "new_node",
                node_tag: "v1",
                instance_id: "ctrl_1",
                pairing_deps: deps,
                requested: &BTreeMap::new(),
                deferred: &[],
                covered,
            },
        )
    }

    /// A covered slot — one a later-starting launch peer will claim —
    /// satisfies the coverage rule whether the slot is required or
    /// optional, while a covered key naming an unknown slot fails loudly.
    #[test]
    fn covered_slots_satisfy_coverage() {
        let covered = requested(&[("arm", PairTarget::pinned("cmd_9", "left"))]);
        for optional in [false, true] {
            let deps = [dep("controller", "arm", optional)];
            assert!(
                plan_covered(&deps, &covered)
                    .expect("a covered slot passes the plan-phase re-check")
                    .is_empty()
            );
        }

        let deps = [dep("controller", "arm", false)];
        let covered = requested(&[
            ("arm", PairTarget::pinned("cmd_9", "left")),
            ("ghost", PairTarget::pinned("cmd_9", "right")),
        ]);
        let err = plan_covered(&deps, &covered)
            .expect_err("a covered key naming an unknown slot is rejected");
        assert!(err.contains("ghost"), "{err}");
    }

    /// A user `--defer-link` on an OPTIONAL slot is now flagged by the
    /// daemon too: launch-mechanism markers ride `covered_pairs`, so the
    /// defer list carries only user intent and gets the same strict rule
    /// as the CLI preflight and the launcher validator.
    #[test]
    fn user_defer_of_optional_slot_is_rejected() {
        let deps_opt = [dep("controller", "arm", true)];
        let err = plan(
            &arm_snapshot(),
            "ctrl_1",
            &deps_opt,
            &BTreeMap::new(),
            &["arm".to_string()],
        )
        .expect_err("optional-slot defer is a user error on every surface");
        assert!(err.contains("optional"), "{err}");
    }

    #[test]
    fn unknown_slot_and_request_defer_overlap_are_rejected() {
        let deps = [dep("controller", "arm", true)];
        let err = plan(
            &arm_snapshot(),
            "ctrl_1",
            &deps,
            &requested(&[("ghost", PairTarget::new("arm_1"))]),
            &[],
        )
        .expect_err("unknown link_id rejected");
        assert!(err.contains("ghost") && err.contains("arm"), "{err}");

        let err = plan(
            &arm_snapshot(),
            "ctrl_1",
            &deps,
            &requested(&[("arm", PairTarget::new("arm_1"))]),
            &["arm".to_string()],
        )
        .expect_err("request+defer overlap rejected");
        assert!(err.contains("also paired"), "{err}");
    }

    #[test]
    fn ambiguous_target_lists_candidates_with_disambiguator() {
        // The peer instance exposes TWO complementary slots of the same
        // pairing (the two-arm-commander shape, seen from the other side).
        let snapshot = vec![snapshot_node(
            "arm_commander",
            &["cmd_1"],
            &[
                dep("controller", "left_arm", true),
                dep("controller", "right_arm", true),
            ],
        )];
        let deps = [dep("arm", "controller", false)];
        let err = plan(
            &snapshot,
            "arm_1",
            &deps,
            &requested(&[("controller", PairTarget::new("cmd_1"))]),
            &[],
        )
        .expect_err("two candidates is ambiguous");
        assert!(
            err.contains("left_arm")
                && err.contains("right_arm")
                && err.contains("cmd_1/<peer_link_id>"),
            "candidates and the disambiguator syntax should be listed: {err}"
        );
    }

    #[test]
    fn same_role_and_self_targets_are_rejected() {
        // Same role on both sides: not complementary, no candidates.
        let snapshot = vec![snapshot_node(
            "other_node",
            &["other_1"],
            &[dep("controller", "arm", true)],
        )];
        let deps = [dep("controller", "arm", false)];
        let err = plan(
            &snapshot,
            "ctrl_1",
            &deps,
            &requested(&[("arm", PairTarget::new("other_1"))]),
            &[],
        )
        .expect_err("same-role slots must not match");
        assert!(err.contains("complementary"), "{err}");

        let err = plan(
            &arm_snapshot(),
            "ctrl_1",
            &deps,
            &requested(&[("arm", PairTarget::new("ctrl_1"))]),
            &[],
        )
        .expect_err("self-pairing rejected");
        assert!(err.contains("own instance"), "{err}");
    }

    /// A slot claimed in the live registry is rejected naming the existing
    /// peer (the `already_paired` wiring from [`NodeStack::live_pairs`]).
    #[test]
    fn already_paired_peer_slot_is_rejected_with_peer_named() {
        use node_stack::PairEndpoint;

        let live = vec![Pairing {
            pairing_name: "arm_link".to_string(),
            pairing_tag: "v1".to_string(),
            a: PairEndpoint {
                slot: SlotAddr::new("arm_1", "controller"),
                role: "arm".to_string(),
            },
            b: PairEndpoint {
                slot: SlotAddr::new("ctrl_0", "arm"),
                role: "controller".to_string(),
            },
        }];
        let deps = [dep("controller", "arm", false)];
        let err = plan_requested_pairs(
            &arm_snapshot(),
            &live,
            &PairingRequest {
                node_name: "new_node",
                node_tag: "v1",
                instance_id: "ctrl_1",
                pairing_deps: &deps,
                requested: &requested(&[("arm", PairTarget::new("arm_1"))]),
                deferred: &[],
                covered: &BTreeMap::new(),
            },
        )
        .expect_err("a live-paired slot is exclusive");
        assert!(
            err.contains("already paired") && err.contains("ctrl_0"),
            "the existing peer should be named: {err}"
        );
    }

    #[test]
    fn missing_planned_pair_is_detected_in_either_endpoint_order() {
        use node_stack::PairEndpoint;

        let registry_pair = |a: SlotAddr, b: SlotAddr| Pairing {
            pairing_name: "arm_link".to_string(),
            pairing_tag: "v1".to_string(),
            a: PairEndpoint {
                slot: a,
                role: "controller".to_string(),
            },
            b: PairEndpoint {
                slot: b,
                role: "arm".to_string(),
            },
        };
        let planned = vec![PlannedPair {
            own: SlotAddr::new("ctrl_1", "arm"),
            peer: SlotAddr::new("arm_1", "controller"),
        }];

        // Present as reserved: no missing pair, whichever side is `a`.
        let same_order = [registry_pair(
            SlotAddr::new("ctrl_1", "arm"),
            SlotAddr::new("arm_1", "controller"),
        )];
        assert!(find_missing_planned_pair(&same_order, &planned).is_none());
        let swapped = [registry_pair(
            SlotAddr::new("arm_1", "controller"),
            SlotAddr::new("ctrl_1", "arm"),
        )];
        assert!(find_missing_planned_pair(&swapped, &planned).is_none());

        // Dissolved (registry empty) or replaced by an unrelated pair: the
        // planned pair is reported missing.
        assert_eq!(
            find_missing_planned_pair(&[], &planned),
            Some(&planned[0]),
            "an empty registry must flag the planned pair"
        );
        let unrelated = [registry_pair(
            SlotAddr::new("ctrl_1", "arm"),
            SlotAddr::new("arm_2", "controller"),
        )];
        assert_eq!(
            find_missing_planned_pair(&unrelated, &planned),
            Some(&planned[0]),
            "a pair with a different peer must not satisfy the plan"
        );
    }

    #[test]
    fn two_slots_cannot_claim_the_same_peer_slot() {
        // One available peer slot, two own slots both targeting it: the
        // second resolution must fail instead of double-claiming.
        let deps = [
            dep("controller", "left", false),
            dep("controller", "right", false),
        ];
        let err = plan(
            &arm_snapshot(),
            "cmd_1",
            &deps,
            &requested(&[
                ("left", PairTarget::new("arm_1")),
                ("right", PairTarget::new("arm_1")),
            ]),
            &[],
        )
        .expect_err("double-claim rejected");
        assert!(
            err.contains("conflicting pairing declarations") || err.contains("already paired"),
            "{err}"
        );
    }
}
