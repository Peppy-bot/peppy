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
use node_stack::{NodeStack, Pairing, SlotAddr};
use peppylib::MessengerHandle;
use peppylib::encoding::peer_update::{PeerUpdateRequest, PeerUpdateResponse};
use peppylib::messaging::{
    PEER_UPDATE_SERVICE, PeerPin, ProducerRef, SenderTarget, ServiceMessenger, ServiceTarget,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, warn};

/// How long a single `peer_update` delivery may take before the operation is
/// treated as failed and reverted. The service is pre-setup (registered
/// before the node's ready signal), so a healthy endpoint answers promptly.
const PEER_UPDATE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct PairingCoordinator {
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    /// This daemon's core node name: both the identity `peer_update` calls
    /// are sent under and the `core_node` stamped into every delivered pin
    /// (stacks are daemon-scoped, so every pair endpoint lives here).
    core_node_name: String,
    /// The daemon's own instance id, used as the caller identity on
    /// `peer_update` service calls.
    caller_instance_id: String,
    /// Serializes every pairing operation end-to-end (registry commit AND
    /// delivery), so reverts can trust that no other operation interleaved.
    op_lock: tokio::sync::Mutex<()>,
    /// Monotonic `peer_update` sequence, seeded from unix-millis at daemon
    /// start so sequences stay strictly increasing across daemon restarts.
    seq: AtomicU64,
}

impl PairingCoordinator {
    pub fn new(
        node_stack: Arc<NodeStack>,
        messenger: MessengerHandle,
        core_node_name: impl Into<String>,
        caller_instance_id: impl Into<String>,
    ) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(1);
        Self {
            node_stack,
            messenger,
            core_node_name: core_node_name.into(),
            caller_instance_id: caller_instance_id.into(),
            op_lock: tokio::sync::Mutex::new(()),
            seq: AtomicU64::new(seed),
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Commits a pair to the registry WITHOUT delivering it (the
    /// reserve-before-spawn step of `node_run`: the owning instance is still
    /// `Starting`, and pins are only delivered once it commits to Running).
    /// All of `pair_slots`' validation applies: existence, liveness,
    /// same-pairing, complementary roles, sha pins, exclusivity.
    pub async fn reserve(
        &self,
        a: &SlotAddr,
        b: &SlotAddr,
    ) -> std::result::Result<Pairing, String> {
        let _guard = self.op_lock.lock().await;
        self.node_stack.pair_slots(a, b).map_err(|e| e.to_string())
    }

    /// Delivers the current pin state of every pair involving `instance_id`
    /// to both endpoints — the post-commit-to-Running step of `node_run`.
    /// Endpoints that are not live are skipped (they will receive their pins
    /// from their own `node_run` flow; `peer_update` is absolute-state and
    /// idempotent, so double delivery converges).
    ///
    /// On a delivery failure the failed pair is reverted (registry cleared +
    /// best-effort Unpaired to whichever side already acked) and an error
    /// naming the pair is returned; pairs already delivered in this call are
    /// left standing (the caller decides whether to dissolve everything).
    pub async fn deliver_pairs_for_instance(
        &self,
        instance_id: &str,
    ) -> std::result::Result<(), String> {
        let _guard = self.op_lock.lock().await;
        let pairs: Vec<Pairing> = self
            .node_stack
            .pairs()
            .into_iter()
            .filter(|p| p.involves_instance(instance_id))
            .collect();
        for pairing in pairs {
            self.deliver_pair(&pairing).await?;
        }
        Ok(())
    }

    /// Dissolves every pair involving `instance_id` and best-effort notifies
    /// each live survivor that its slot is now Unpaired. Called from the
    /// stop paths, the process-exit watcher, and the `node_run` unwind
    /// branches (death auto-clears; re-pairing is explicit). Returns the
    /// dissolved pairs for logging.
    pub async fn dissolve_for_instance(&self, instance_id: &str) -> Vec<Pairing> {
        let _guard = self.op_lock.lock().await;
        let dissolved = self.node_stack.dissolve_pairs_for_instance(instance_id);
        for pairing in &dissolved {
            debug!(
                "Dissolved pair `{}` ({}:{}) — instance '{}' is gone",
                pairing_label(pairing),
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
        dissolved
    }

    /// Sends both endpoints of `pairing` their pins; reverts on failure per
    /// the module-level protocol.
    async fn deliver_pair(&self, pairing: &Pairing) -> std::result::Result<(), String> {
        let sides = [(&pairing.a, &pairing.b), (&pairing.b, &pairing.a)];
        for (idx, (endpoint, peer)) in sides.into_iter().enumerate() {
            if !self
                .node_stack
                .instance_is_live_for_pairing(&endpoint.slot.instance_id)
            {
                continue;
            }
            let pin = PeerPin {
                producer: ProducerRef::new(&self.core_node_name, &peer.slot.instance_id),
                peer_link_id: peer.slot.link_id.clone(),
            };
            if let Err(reason) = self.send_peer_update(&endpoint.slot, Some(pin)).await {
                // Revert the commit; if the OTHER side already acked its pin,
                // best-effort roll it back to Unpaired.
                self.node_stack.clear_pair(&endpoint.slot);
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
            .node_stack
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
        let instance_name = Name::new(slot.instance_id.as_str())
            .map_err(|e| format!("invalid instance id: {e}"))?;
        let (node_name, node_tag) = self
            .node_stack
            .find_entity_label_for_instance_id_any_state(&instance_name)
            .ok_or_else(|| "instance is no longer tracked".to_string())?;

        let request = PeerUpdateRequest {
            link_id: slot.link_id.clone(),
            sequence: self.next_seq(),
            pin,
        };
        let payload = request.encode().map_err(|e| e.to_string())?;

        let reply = ServiceMessenger::poll(
            &self.messenger,
            &self.core_node_name,
            &self.caller_instance_id,
            SenderTarget::node(&node_name, &node_tag).map_err(|e| e.to_string())?,
            PEER_UPDATE_SERVICE,
            ServiceTarget::Producer(&ProducerRef::new(&self.core_node_name, &slot.instance_id)),
            payload,
            PEER_UPDATE_TIMEOUT,
        )
        .await
        .map_err(|e| e.to_string())?;

        let response =
            PeerUpdateResponse::decode(&reply.payload_bytes()).map_err(|e| e.to_string())?;
        if response.accepted || response.stale_sequence {
            Ok(())
        } else {
            Err(if response.message.is_empty() {
                "peer_update rejected".to_string()
            } else {
                response.message
            })
        }
    }
}

/// `a_inst:a_link ⇌ b_inst:b_link`, the human-readable pair label used in
/// logs and error messages (matches `peppy stack list`).
pub fn pairing_label(pairing: &Pairing) -> String {
    format!("{} ⇌ {}", pairing.a.slot, pairing.b.slot)
}

/// One resolved `--pair` request: this instance's slot and the concrete peer
/// slot it will be paired with (ready for [`PairingCoordinator::reserve`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPair {
    pub own: SlotAddr,
    pub peer: SlotAddr,
}

/// The daemon-side re-check of a `node_run` goal's pairing arguments — the
/// trust-boundary twin of the CLI preflight and the launcher validator. Runs
/// BEFORE the instance is spawned so every violation fails loudly with
/// nothing to unwind:
///
/// - every requested/deferred link_id must be a declared pairing slot;
/// - a slot cannot be both requested and deferred;
/// - every required (non-`optional`) slot must be requested or deferred;
/// - each requested target `peer_instance[/peer_link]` must resolve to
///   exactly one unpaired complementary slot on a live instance (ambiguity
///   names the candidates and the `/<peer_link>` disambiguator);
/// - two requested slots cannot claim the same peer slot.
///
/// `available` is the stack's current candidate pool
/// ([`NodeStack::unpaired_pairing_slots`]).
pub fn plan_requested_pairs(
    available: &[(SlotAddr, config::node::PairingDependency)],
    instance_id: &str,
    pairing_deps: &[config::node::PairingDependency],
    requested: &std::collections::BTreeMap<String, String>,
    deferred: &[String],
) -> std::result::Result<Vec<PlannedPair>, String> {
    let dep_by_link: std::collections::BTreeMap<&str, &config::node::PairingDependency> =
        pairing_deps
            .iter()
            .map(|d| (d.link_id.as_str(), d))
            .collect();

    let declared = || {
        if dep_by_link.is_empty() {
            "the node declares no pairing slots".to_string()
        } else {
            format!(
                "declared pairing slots: [{}]",
                dep_by_link.keys().copied().collect::<Vec<_>>().join(", ")
            )
        }
    };

    for link_id in requested
        .keys()
        .map(String::as_str)
        .chain(deferred.iter().map(String::as_str))
    {
        if !dep_by_link.contains_key(link_id) {
            return Err(format!(
                "pairing slot `{link_id}` is not declared by this node ({})",
                declared()
            ));
        }
    }
    if let Some(link_id) = requested.keys().find(|l| deferred.contains(l)) {
        return Err(format!(
            "pairing slot `{link_id}` is both paired and deferred; pick one of \
             `--pair {link_id}@...` or `--defer-pair {link_id}`"
        ));
    }

    let uncovered: Vec<&str> = pairing_deps
        .iter()
        .filter(|d| {
            !d.optional
                && !requested.contains_key(d.link_id.as_str())
                && !deferred.iter().any(|l| l == &d.link_id)
        })
        .map(|d| d.link_id.as_str())
        .collect();
    if !uncovered.is_empty() {
        return Err(format!(
            "required pairing slot(s) not covered: [{}]. Pass `--pair <link_id>@<peer_instance>` \
             to pair each at start, or `--defer-pair <link_id>` to explicitly start unpaired",
            uncovered.join(", ")
        ));
    }

    // Claims within this plan are tracked so two slots cannot resolve to the
    // same peer slot.
    let mut planned: Vec<PlannedPair> = Vec::new();

    for (link_id, target) in requested {
        let own_dep = dep_by_link[link_id.as_str()];
        let (peer_instance, peer_link) = daemon_config::launcher::split_pair_target(target);
        if peer_instance == instance_id {
            return Err(format!(
                "pairing slot `{link_id}` targets its own instance '{instance_id}'; \
                 a pair joins two distinct instances"
            ));
        }

        let candidates: Vec<&SlotAddr> = available
            .iter()
            .filter(|(slot, dep)| {
                slot.instance_id == peer_instance
                    && dep.name == own_dep.name
                    && dep.tag == own_dep.tag
                    && dep.role != own_dep.role
                    && peer_link.is_none_or(|l| slot.link_id == l)
                    && !planned.iter().any(|p| &p.peer == slot)
            })
            .map(|(slot, _)| slot)
            .collect();

        let peer_slot = match candidates.as_slice() {
            [one] => (*one).clone(),
            [] => {
                let target_desc = match peer_link {
                    Some(l) => format!("slot `{l}` of instance '{peer_instance}'"),
                    None => format!("instance '{peer_instance}'"),
                };
                return Err(format!(
                    "pairing slot `{link_id}`: no unpaired complementary slot found on \
                     {target_desc} for pairing `{}:{}` (this side's role: `{}`). The peer \
                     instance may not be running, may not declare a complementary slot, or \
                     its slot may already be paired",
                    own_dep.name.as_str(),
                    own_dep.tag,
                    own_dep.role,
                ));
            }
            many => {
                let listed: Vec<String> = many
                    .iter()
                    .map(|slot| format!("{target}/{}", slot.link_id))
                    .collect();
                return Err(format!(
                    "pairing slot `{link_id}`: target '{peer_instance}' has multiple \
                     complementary slots; disambiguate with one of: [{}]",
                    listed.join(", ")
                ));
            }
        };

        planned.push(PlannedPair {
            own: SlotAddr::new(instance_id, link_id.as_str()),
            peer: peer_slot,
        });
    }

    Ok(planned)
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

    /// One unpaired complementary slot on the running arm instance.
    fn arm_pool() -> Vec<(SlotAddr, PairingDependency)> {
        vec![(
            SlotAddr::new("arm_1", "controller"),
            dep("arm", "controller", true),
        )]
    }

    fn requested(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn resolves_a_single_complementary_slot() {
        let deps = [dep("controller", "arm", false)];
        let plan = plan_requested_pairs(
            &arm_pool(),
            "ctrl_1",
            &deps,
            &requested(&[("arm", "arm_1")]),
            &[],
        )
        .expect("unambiguous target resolves");
        assert_eq!(
            plan,
            vec![PlannedPair {
                own: SlotAddr::new("ctrl_1", "arm"),
                peer: SlotAddr::new("arm_1", "controller"),
            }]
        );
    }

    #[test]
    fn explicit_peer_link_pin_resolves_and_wrong_pin_fails() {
        let deps = [dep("controller", "arm", false)];
        let plan = plan_requested_pairs(
            &arm_pool(),
            "ctrl_1",
            &deps,
            &requested(&[("arm", "arm_1/controller")]),
            &[],
        )
        .expect("pinned target resolves");
        assert_eq!(plan[0].peer, SlotAddr::new("arm_1", "controller"));

        let err = plan_requested_pairs(
            &arm_pool(),
            "ctrl_1",
            &deps,
            &requested(&[("arm", "arm_1/nope")]),
            &[],
        )
        .expect_err("wrong peer_link rejected");
        assert!(err.contains("no unpaired complementary slot"), "{err}");
    }

    #[test]
    fn uncovered_required_slot_names_the_exact_flags() {
        let deps = [dep("controller", "arm", false)];
        let err = plan_requested_pairs(&arm_pool(), "ctrl_1", &deps, &BTreeMap::new(), &[])
            .expect_err("required slot must be covered");
        assert!(
            err.contains("arm") && err.contains("--pair") && err.contains("--defer-pair"),
            "error should name the slot and both flags: {err}"
        );

        // Deferring it satisfies coverage; optional slots never need covering.
        let deps_opt = [dep("controller", "arm", true)];
        assert!(
            plan_requested_pairs(&arm_pool(), "ctrl_1", &deps_opt, &BTreeMap::new(), &[])
                .expect("optional slot boots unpaired freely")
                .is_empty()
        );
        assert!(
            plan_requested_pairs(
                &arm_pool(),
                "ctrl_1",
                &deps,
                &BTreeMap::new(),
                &["arm".to_string()],
            )
            .expect("deferred required slot passes coverage")
            .is_empty()
        );
    }

    #[test]
    fn unknown_slot_and_request_defer_overlap_are_rejected() {
        let deps = [dep("controller", "arm", true)];
        let err = plan_requested_pairs(
            &arm_pool(),
            "ctrl_1",
            &deps,
            &requested(&[("ghost", "arm_1")]),
            &[],
        )
        .expect_err("unknown link_id rejected");
        assert!(err.contains("ghost") && err.contains("arm"), "{err}");

        let err = plan_requested_pairs(
            &arm_pool(),
            "ctrl_1",
            &deps,
            &requested(&[("arm", "arm_1")]),
            &["arm".to_string()],
        )
        .expect_err("request+defer overlap rejected");
        assert!(err.contains("both paired and deferred"), "{err}");
    }

    #[test]
    fn ambiguous_target_lists_candidates_with_disambiguator() {
        // The peer instance exposes TWO complementary slots of the same
        // pairing (the two-arm-commander shape, seen from the other side).
        let pool = vec![
            (
                SlotAddr::new("cmd_1", "left_arm"),
                dep("controller", "left_arm", true),
            ),
            (
                SlotAddr::new("cmd_1", "right_arm"),
                dep("controller", "right_arm", true),
            ),
        ];
        let deps = [dep("arm", "controller", false)];
        let err = plan_requested_pairs(
            &pool,
            "arm_1",
            &deps,
            &requested(&[("controller", "cmd_1")]),
            &[],
        )
        .expect_err("two candidates is ambiguous");
        assert!(
            err.contains("cmd_1/left_arm") && err.contains("cmd_1/right_arm"),
            "candidates should be listed as ready-to-paste targets: {err}"
        );
    }

    #[test]
    fn same_role_and_self_targets_are_rejected() {
        // Same role on both sides: not complementary, no candidates.
        let pool = vec![(
            SlotAddr::new("other_1", "arm"),
            dep("controller", "arm", true),
        )];
        let deps = [dep("controller", "arm", false)];
        let err = plan_requested_pairs(
            &pool,
            "ctrl_1",
            &deps,
            &requested(&[("arm", "other_1")]),
            &[],
        )
        .expect_err("same-role slots must not match");
        assert!(err.contains("no unpaired complementary slot"), "{err}");

        let err = plan_requested_pairs(
            &arm_pool(),
            "ctrl_1",
            &deps,
            &requested(&[("arm", "ctrl_1")]),
            &[],
        )
        .expect_err("self-pairing rejected");
        assert!(err.contains("own instance"), "{err}");
    }

    #[test]
    fn two_slots_cannot_claim_the_same_peer_slot() {
        // One available peer slot, two own slots both targeting it: the
        // second resolution must fail instead of double-claiming.
        let deps = [
            dep("controller", "left", false),
            dep("controller", "right", false),
        ];
        let err = plan_requested_pairs(
            &arm_pool(),
            "cmd_1",
            &deps,
            &requested(&[("left", "arm_1"), ("right", "arm_1")]),
            &[],
        )
        .expect_err("double-claim rejected");
        assert!(err.contains("no unpaired complementary slot"), "{err}");
    }
}
