//! Encoding types for the NodeRun action (streaming version with feedback).

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use capnp::message::Builder;
use config::runtime::{NodeInstancePlan, ObservedPeer, ProducerRef};

use crate::node_capnp;
use crate::{NonEmptyPayload, Payload, Result};

use super::builder::FeedbackStream;
use crate::encoding::endpoints::{read_node_endpoints, write_node_endpoints};
use crate::encoding::{
    capnp_list_len, decode_message, encode_message, encode_message_non_empty, optional_text,
    read_text_list, required_text, write_text_list,
};
use crate::graph::InstanceEndpoint;

/// One peer reference carried by [`NodeRunGoal::requested_pairs`] /
/// [`NodeRunGoal::covered_pairs`]: the peer instance and, optionally, the
/// pinned complementary slot on it. `Display` renders the CLI/launcher
/// target grammar (`<peer_instance>` or `<peer_instance>/<peer_link_id>`);
/// instance ids and link_ids are `/`-free names, so the rendering is
/// unambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairTarget {
    /// The peer instance, addressed the way the whole mesh addresses
    /// instances. Its `core_node` is always populated, including for a
    /// same-daemon pair, so a daemon never has to infer "this must be local"
    /// from an absent field. Set by whoever plans the pair: the coordinator of
    /// a federated launch, or the receiving daemon's own name for a
    /// `node run --pair`.
    pub peer: ProducerRef,
    /// The complementary slot on the peer, when the request pins one.
    /// `None` is unpinned: exactly one available complementary slot must
    /// exist on the peer and the daemon resolves it.
    pub peer_link_id: Option<String>,
    /// The planner's verdict about a peer the receiving daemon cannot inspect,
    /// set only when [`Self::peer`] names a DIFFERENT machine.
    ///
    /// A daemon validates a pair against the two manifests it holds. For a peer
    /// on another machine it holds neither, so the rules cannot be re-derived
    /// there — and a federated launch's coordinator has already checked the
    /// whole plan against every participant's manifests. This carries that
    /// verdict; `None` means the peer is local and the local manifests decide.
    pub remote_peer: Option<RemotePeerPairing>,
}

/// What a daemon needs about a pair endpoint whose manifest it cannot read.
///
/// Mirrors the fields the pair registry records, so a cross-machine pair is
/// stored exactly like a same-daemon one once committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePeerPairing {
    pub pairing_name: String,
    pub pairing_tag: String,
    /// The role the peer's manifest declares for its side of the pair.
    pub peer_role: String,
    /// The cardinality the peer's manifest declares for its slot: only a
    /// scalar slot is taken by one pair.
    pub peer_cardinality: config::node::Cardinality,
    /// The copy the peer's instance belongs to; `None` outside a copy.
    pub peer_copy: Option<config::runtime::Name>,
}

impl PairTarget {
    pub fn new(peer_instance_id: impl Into<String>, peer_core_node: impl Into<String>) -> Self {
        Self {
            peer: ProducerRef::new(peer_core_node, peer_instance_id),
            peer_link_id: None,
            remote_peer: None,
        }
    }

    pub fn pinned(
        peer_instance_id: impl Into<String>,
        peer_link_id: impl Into<String>,
        peer_core_node: impl Into<String>,
    ) -> Self {
        Self {
            peer: ProducerRef::new(peer_core_node, peer_instance_id),
            peer_link_id: Some(peer_link_id.into()),
            remote_peer: None,
        }
    }

    /// Attaches the planner's verdict for a peer on another machine.
    ///
    /// Only a planner holding both manifests may call this: it is the
    /// receiving daemon's sole evidence about a slot it cannot read.
    pub fn with_remote_peer(mut self, remote_peer: RemotePeerPairing) -> Self {
        self.remote_peer = Some(remote_peer);
        self
    }
}

impl std::fmt::Display for PairTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.peer_link_id {
            Some(link) => write!(f, "{}/{}", self.peer.instance_id, link),
            None => f.write_str(&self.peer.instance_id),
        }
    }
}

/// One pairing an observer slot taps, carried by
/// [`NodeRunGoal::planned_observations`]: the source instance, the
/// source-side participant slot the source publishes the observed role under,
/// and, when the plan named the pair by its other end, that peer. Unlike a
/// [`PairTarget`] the source slot is always resolved (the planner fills it).
/// The three fields together are the member's identity within its slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObservationTarget {
    /// The source instance, addressed for the same self-describing-placement
    /// reason as [`PairTarget::peer`]. A remote source subscribes identically
    /// to a local one; what differs is that its lifecycle transitions arrive
    /// as notifications from its own daemon rather than from local lifecycle
    /// events.
    pub source: ProducerRef,
    pub source_link_id: String,
    /// The pair's other end, when the plan named the pair by it; `None`
    /// observes every pair of the source's slot.
    pub peer: Option<ObservedPeer>,
}

impl ObservationTarget {
    pub fn new(
        source_instance_id: impl Into<String>,
        source_link_id: impl Into<String>,
        source_core_node: impl Into<String>,
    ) -> Self {
        Self {
            source: ProducerRef::new(source_core_node, source_instance_id),
            source_link_id: source_link_id.into(),
            peer: None,
        }
    }
}

impl std::fmt::Display for ObservationTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}@{}",
            self.source.instance_id, self.source_link_id, self.source.core_node
        )?;
        match &self.peer {
            Some(peer) => write!(
                f,
                " to {}/{}@{}",
                peer.peer.instance_id, peer.peer_link_id, peer.peer.core_node
            ),
            None => Ok(()),
        }
    }
}

/// A member repeated inside one observer slot's target set, named by the
/// identity that repeated. Rejected rather than deduplicated: the same pairing
/// listed twice means the plan resolved two launcher entries onto one source
/// slot, which is a planning mistake, not a request for two subscriptions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateObservationTarget {
    pub observer_link_id: String,
    pub target: ObservationTarget,
}

impl std::fmt::Display for DuplicateObservationTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Duplicate observation target `{}` on observer slot `{}` — an observed pairing may \
             appear only once in a slot's member set",
            self.target, self.observer_link_id
        )
    }
}

impl std::error::Error for DuplicateObservationTarget {}

/// The ordered, duplicate-free member set of one observer slot, sized against
/// the slot's declared `cardinality` by the planner. Order is the plan's:
/// launcher array order, or `--link` occurrence order, preserved end to end so
/// the node's `sources()` matches what the deployment wrote. Mirrors
/// [`config::runtime::BoundProducers`] on the producer-binding side, and like
/// it rejects duplicates rather than removing or reordering them.
///
/// Never empty on the wire: a `zero_or_more` slot that observes nothing has no
/// entry in [`NodeRunGoal::planned_observations`] at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationTargets(Vec<ObservationTarget>);

impl ObservationTargets {
    /// Ordered construction from an already-collected member list, rejecting
    /// duplicates. The single construction gate: the goal decoder delegates
    /// here, and the launcher validator calls it when it materializes a slot's
    /// set, so every boundary rejects the same sets with the same error.
    pub fn new(
        observer_link_id: &str,
        targets: Vec<ObservationTarget>,
    ) -> std::result::Result<Self, Box<DuplicateObservationTarget>> {
        // The first duplicated member in plan order names the error.
        let mut seen = HashSet::with_capacity(targets.len());
        if let Some(duplicate) = targets.iter().find(|target| !seen.insert(*target)) {
            return Err(Box::new(DuplicateObservationTarget {
                observer_link_id: observer_link_id.to_string(),
                target: duplicate.clone(),
            }));
        }
        Ok(Self(targets))
    }

    /// Materializes a planner's per-slot member lists into the map a goal
    /// carries. Both planners (the launcher and the CLI preflight) group their
    /// validated plan this way, so the invariant they lean on — the launcher
    /// validator already rejected duplicates within a slot, making one here a
    /// planner bug rather than a user error — is asserted once, here.
    pub fn slots_from_plan(
        members: BTreeMap<String, Vec<ObservationTarget>>,
    ) -> BTreeMap<String, ObservationTargets> {
        members
            .into_iter()
            .map(|(link_id, targets)| {
                let targets = Self::new(&link_id, targets)
                    .expect("plan members are duplicate-free by validation");
                (link_id, targets)
            })
            .collect()
    }

    pub fn as_slice(&self) -> &[ObservationTarget] {
        &self.0
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ObservationTarget> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A one-member set, for `cardinality: "one"` slots and tests.
impl From<ObservationTarget> for ObservationTargets {
    fn from(target: ObservationTarget) -> Self {
        Self(vec![target])
    }
}

impl<'a> IntoIterator for &'a ObservationTargets {
    type Item = &'a ObservationTarget;
    type IntoIter = std::slice::Iter<'a, ObservationTarget>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Goal message for the NodeRun action.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRunGoal {
    /// What to start, NOT how it is wired into the mesh. See
    /// [`NodeInstancePlan`]: the receiving daemon owns the runtime identity of
    /// every node it spawns, so the messaging endpoint, `bound_core_node`, and
    /// the resolved framework values are added by the daemon, on every path
    /// including `peppy node run`. One assembly site, one invariant.
    pub instance_plan: NodeInstancePlan,
    /// SHA256 of the manifest the planner validated this instance against,
    /// which is the manifest the coordinator resolved for the whole launch.
    /// The spawning daemon refuses if the entity now in its stack hashes
    /// differently, so an entity replaced between the launch's add phase and
    /// this dispatch fails loudly rather than starting a node against a plan
    /// that was never checked for it.
    ///
    /// `None` on the in-process launch path, where planner and spawner are the
    /// same daemon reading the same entity under the same lock and there is no
    /// window to close.
    pub manifest_sha256: Option<String>,
    pub node_name: String,
    pub tag: String,
    pub env_vars: Vec<(String, String)>,
    pub timeout_secs: u64,
    /// Pairing requests from `--link <link_id>@<peer_instance>[/<peer_link_id>]`
    /// or a launch plan, keyed by the starting node's own slot link_id and
    /// holding every pair that slot takes, in the order the plan lists them:
    /// one for a scalar slot, one per peer for a multi slot.
    /// Commands to the daemon, not resolved config: the daemon validates and
    /// reserves each pair BEFORE spawning and delivers it live after the
    /// instance commits to Running.
    pub requested_pairs: BTreeMap<String, Vec<PairTarget>>,
    /// Pairing slots deliberately left unpaired, keyed by this instance's
    /// slot link_id; each value is the reason the deployment wrote down
    /// (`--vacant-link <link_id>=<why>`, or the launcher's
    /// `links: { <link_id>: { vacant: "<why>" } }`) and is never empty.
    /// Together with `requested_pairs` and `covered_pairs` these must cover
    /// every required pairing slot of the manifest, or the daemon rejects the
    /// run.
    pub vacant_pairs: BTreeMap<String, String>,
    /// Pairing slots of this instance that a LATER-starting instance of the
    /// same `stack launch` will claim through its own `requested_pairs`
    /// entry, keyed by this instance's slot link_id; each value names that
    /// future peer. A launch-mechanism marker, not user intent: the slot
    /// boots unpaired and needs no action, unlike a `vacant_pairs` entry
    /// which records a deliberate opt-out. Never set by the CLI.
    pub covered_pairs: BTreeMap<String, Vec<PairTarget>>,
    /// Observer requests from `--link <observer_link>@<source>[/<source_link>]`
    /// or a launch plan, keyed by the starting node's own observer-slot
    /// link_id and holding that slot's whole ordered member set. Commands to
    /// the daemon, not resolved config: the daemon registers each with its
    /// observation coordinator BEFORE the instance commits to Running, so
    /// every member's pin is delivered the moment both are up and re-delivered
    /// whenever a member restarts. A slot that observes nothing carries no
    /// entry.
    pub planned_observations: BTreeMap<String, ObservationTargets>,
    /// See [`crate::encoding::NodeAddGoal::launch_id`].
    pub launch_id: Option<String>,
    /// Core nodes that must hear about this instance's lifecycle but that the
    /// spawning daemon cannot work out for itself: the daemons whose observers
    /// tap it. A source is deliberately unaware of its observers, so only the
    /// planner can name them. Pairing recipients are NOT listed here; a pair
    /// names both endpoints, so the daemon derives those from its own registry.
    pub lifecycle_watchers: Vec<String>,
}

impl NodeRunGoal {
    pub fn new(
        instance_plan: NodeInstancePlan,
        node_name: impl Into<String>,
        tag: impl Into<String>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            instance_plan,
            manifest_sha256: None,
            node_name: node_name.into(),
            tag: tag.into(),
            env_vars: Vec::new(),
            timeout_secs,
            requested_pairs: BTreeMap::new(),
            vacant_pairs: BTreeMap::new(),
            covered_pairs: BTreeMap::new(),
            planned_observations: BTreeMap::new(),
            launch_id: None,
            lifecycle_watchers: Vec::new(),
        }
    }

    /// Pins the manifest the planner validated against. Set by a coordinator
    /// dispatching to a peer; left unset on the in-process launch path.
    pub fn with_manifest_sha256(mut self, manifest_sha256: impl Into<String>) -> Self {
        self.manifest_sha256 = Some(manifest_sha256.into());
        self
    }

    pub fn with_env_vars(mut self, env_vars: Vec<(String, String)>) -> Self {
        self.env_vars = env_vars;
        self
    }

    pub fn with_requested_pairs(
        mut self,
        requested_pairs: BTreeMap<String, Vec<PairTarget>>,
    ) -> Self {
        self.requested_pairs = requested_pairs;
        self
    }

    pub fn with_vacant_pairs(mut self, vacant_pairs: BTreeMap<String, String>) -> Self {
        self.vacant_pairs = vacant_pairs;
        self
    }

    pub fn with_covered_pairs(mut self, covered_pairs: BTreeMap<String, Vec<PairTarget>>) -> Self {
        self.covered_pairs = covered_pairs;
        self
    }

    pub fn with_planned_observations(
        mut self,
        planned_observations: BTreeMap<String, ObservationTargets>,
    ) -> Self {
        self.planned_observations = planned_observations;
        self
    }

    /// Marks this goal as one step of a federated launch's dispatch, so the
    /// receiving daemon accepts it while reserved for that launch.
    pub fn with_launch_id(mut self, launch_id: impl Into<String>) -> Self {
        self.launch_id = Some(launch_id.into());
        self
    }

    /// Names the daemons whose observers tap this instance, so the daemon that
    /// spawns it can report its lifecycle to them.
    pub fn with_lifecycle_watchers(mut self, lifecycle_watchers: Vec<String>) -> Self {
        self.lifecycle_watchers = lifecycle_watchers;
        self
    }

    /// Builds a goal for in-process execution that bypasses the action-loop
    /// gate (see `services::stack::launch::start_node_directly`). The
    /// `timeout_secs` field feeds the gate's busy-reporting and is unread on
    /// this path, so it is zero by construction.
    pub fn for_internal_execution(
        instance_plan: NodeInstancePlan,
        node_name: impl Into<String>,
        tag: impl Into<String>,
    ) -> Self {
        Self::new(instance_plan, node_name, tag, 0)
    }

    pub fn encode(&self) -> Result<Payload> {
        let instance_plan_json5 = serde_json5::to_string(&self.instance_plan)
            .map_err(|e| crate::Error::Encoding(format!("NodeRunGoal.instance_plan: {e}")))?;
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<node_capnp::node_run_goal::Builder>();
            goal.set_instance_plan_json5(&instance_plan_json5);
            goal.set_manifest_sha256(self.manifest_sha256.as_deref().unwrap_or(""));
            goal.set_launch_id(self.launch_id.as_deref().unwrap_or(""));

            let watcher_count = capnp_list_len(
                self.lifecycle_watchers.len(),
                "NodeRunGoal.lifecycle_watchers",
            )?;
            write_text_list(
                goal.reborrow().init_lifecycle_watchers(watcher_count),
                &self.lifecycle_watchers,
            );
            goal.set_node_name(&self.node_name);
            goal.set_tag(&self.tag);

            let env_var_count = capnp_list_len(self.env_vars.len(), "NodeRunGoal.env_vars")?;
            let mut env_vars = goal.reborrow().init_env_vars(env_var_count);
            for (idx, (key, value)) in self.env_vars.iter().enumerate() {
                let mut env_var = env_vars.reborrow().get(idx as u32);
                env_var.set_key(key);
                env_var.set_value(value);
            }

            goal.reborrow().set_timeout_secs(self.timeout_secs);

            let pair_count = capnp_list_len(
                total_pairs(&self.requested_pairs),
                "NodeRunGoal.requested_pairs",
            )?;
            fill_pair_requests(
                goal.reborrow().init_requested_pairs(pair_count),
                &self.requested_pairs,
            );

            let vacant_count = capnp_list_len(self.vacant_pairs.len(), "NodeRunGoal.vacant_pairs")?;
            fill_vacant_pairs(
                goal.reborrow().init_vacant_pairs(vacant_count),
                &self.vacant_pairs,
            );

            let covered_count = capnp_list_len(
                total_pairs(&self.covered_pairs),
                "NodeRunGoal.covered_pairs",
            )?;
            fill_pair_requests(
                goal.reborrow().init_covered_pairs(covered_count),
                &self.covered_pairs,
            );

            let observation_count = capnp_list_len(
                self.planned_observations.len(),
                "NodeRunGoal.planned_observations",
            )?;
            fill_observation_requests(
                goal.reborrow().init_planned_observations(observation_count),
                &self.planned_observations,
            );
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let goal = reader.get_root::<node_capnp::node_run_goal::Reader>()?;

        let env_vars_reader = goal.get_env_vars()?;
        let mut env_vars = Vec::with_capacity(env_vars_reader.len() as usize);
        for idx in 0..env_vars_reader.len() {
            let env_var = env_vars_reader.get(idx);
            env_vars.push((
                env_var.get_key()?.to_str()?.to_owned(),
                env_var.get_value()?.to_str()?.to_owned(),
            ));
        }

        // Cap'n Proto defaults an absent field to the empty string, so a goal
        // from a peppy that still ships an assembled runtime config decodes
        // here with no plan at all. Left unchecked that would spawn a node
        // under a defaulted identity. Refuse, and name the fix.
        let instance_plan_json5 = goal.get_instance_plan_json5()?.to_str()?;
        if instance_plan_json5.is_empty() {
            return Err(crate::Error::Decoding(
                "NodeRunGoal.instance_plan is empty: this goal came from a peppy that still \
                 assembles runtime configs caller-side. Upgrade the caller to the same version \
                 as this daemon."
                    .to_owned(),
            ));
        }
        let instance_plan: NodeInstancePlan = serde_json5::from_str(instance_plan_json5)
            .map_err(|e| crate::Error::Decoding(format!("NodeRunGoal.instance_plan: {e}")))?;

        Ok(Self {
            instance_plan,
            manifest_sha256: optional_text(goal.get_manifest_sha256()?.to_str()?),
            launch_id: optional_text(goal.get_launch_id()?.to_str()?),
            lifecycle_watchers: read_text_list(goal.get_lifecycle_watchers()?)?,
            node_name: goal.get_node_name()?.to_str()?.to_owned(),
            tag: goal.get_tag()?.to_str()?.to_owned(),
            env_vars,
            timeout_secs: goal.get_timeout_secs(),
            requested_pairs: read_pair_requests(goal.get_requested_pairs()?)?,
            vacant_pairs: read_vacant_pairs(goal.get_vacant_pairs()?)?,
            covered_pairs: read_pair_requests(goal.get_covered_pairs()?)?,
            planned_observations: read_observation_requests(goal.get_planned_observations()?)?,
        })
    }
}

/// How many `PairRequest` entries a slot map occupies on the wire: one per
/// pair, so a multi slot holding three pairs takes three.
fn total_pairs(pairs: &BTreeMap<String, Vec<PairTarget>>) -> usize {
    pairs.values().map(Vec::len).sum()
}

/// Writes a `link_id -> pairs` map into an initialized `List(PairRequest)`
/// builder ([`NodeRunGoal::requested_pairs`] and
/// [`NodeRunGoal::covered_pairs`] share the wire shape). One entry per pair,
/// so a multi slot's pairs are several entries sharing one `linkId`, in the
/// slot's order. An unpinned `peer_link_id` is encoded as the empty string,
/// and an absent `remote_peer` as an all-empty `remotePeer` struct.
fn fill_pair_requests(
    mut list: capnp::struct_list::Builder<'_, node_capnp::pair_request::Owned>,
    pairs: &BTreeMap<String, Vec<PairTarget>>,
) {
    let flattened = pairs
        .iter()
        .flat_map(|(link_id, targets)| targets.iter().map(move |target| (link_id, target)));
    for (idx, (link_id, target)) in flattened.enumerate() {
        let mut pair = list.reborrow().get(idx as u32);
        pair.set_link_id(link_id);
        pair.set_peer_link_id(target.peer_link_id.as_deref().unwrap_or(""));
        if let Some(remote) = &target.remote_peer {
            let mut builder = pair.reborrow().init_remote_peer();
            builder.set_pairing_name(&remote.pairing_name);
            builder.set_pairing_tag(&remote.pairing_tag);
            builder.set_peer_role(&remote.peer_role);
            builder.set_peer_cardinality(remote.peer_cardinality.as_str());
            builder.set_peer_copy(
                remote
                    .peer_copy
                    .as_ref()
                    .map(|copy| copy.as_str())
                    .unwrap_or(""),
            );
        }
        write_instance_address(pair.init_peer(), &target.peer);
    }
}

/// Inverse of [`fill_pair_requests`]: an empty `peerLinkId` decodes to
/// `None` (unpinned), and an empty `remotePeer.pairingName` to no remote
/// verdict. Entries sharing a `linkId` accumulate into that slot's pair
/// list, keeping wire order.
fn read_pair_requests(
    list: capnp::struct_list::Reader<'_, node_capnp::pair_request::Owned>,
) -> Result<BTreeMap<String, Vec<PairTarget>>> {
    let mut pairs: BTreeMap<String, Vec<PairTarget>> = BTreeMap::new();
    for idx in 0..list.len() {
        let pair = list.get(idx);
        let remote = pair.get_remote_peer()?;
        let pairing_name = remote.get_pairing_name()?.to_str()?;
        // A same-daemon peer carries no verdict, and Cap'n Proto defaults an
        // absent struct's text to "". Keying "set" on the pairing name means a
        // partially-filled verdict is refused below rather than silently
        // committing a pair under an empty pairing identity.
        let remote_peer = if pairing_name.is_empty() {
            None
        } else {
            Some(RemotePeerPairing {
                pairing_name: pairing_name.to_owned(),
                pairing_tag: required_text(
                    remote.get_pairing_tag()?.to_str()?,
                    "NodeRunGoal pair request remote_peer.pairing_tag",
                )?,
                peer_role: required_text(
                    remote.get_peer_role()?.to_str()?,
                    "NodeRunGoal pair request remote_peer.peer_role",
                )?,
                peer_cardinality: required_text(
                    remote.get_peer_cardinality()?.to_str()?,
                    "NodeRunGoal pair request remote_peer.peer_cardinality",
                )?
                .parse()
                .map_err(|spelling| {
                    crate::Error::Decoding(format!(
                        "NodeRunGoal pair request remote_peer.peer_cardinality `{spelling}` is none \
                         of one, zero_or_one, one_or_more, zero_or_more"
                    ))
                })?,
                peer_copy: match remote.get_peer_copy()?.to_str()? {
                    "" => None,
                    copy => Some(config::runtime::Name::new(copy).map_err(|e| {
                        crate::Error::Decoding(format!(
                            "NodeRunGoal pair request remote_peer.peer_copy is not a name: {e}"
                        ))
                    })?),
                },
            })
        };
        pairs
            .entry(pair.get_link_id()?.to_str()?.to_owned())
            .or_default()
            .push(PairTarget {
                peer: read_instance_address(pair.get_peer()?, "NodeRunGoal pair request")?,
                peer_link_id: optional_text(pair.get_peer_link_id()?.to_str()?),
                remote_peer,
            });
    }
    Ok(pairs)
}

/// Writes a `link_id -> reason` map into an initialized `List(VacantPair)`
/// builder ([`NodeRunGoal::vacant_pairs`]).
fn fill_vacant_pairs(
    mut list: capnp::struct_list::Builder<'_, node_capnp::vacant_pair::Owned>,
    vacant: &BTreeMap<String, String>,
) {
    for (idx, (link_id, reason)) in vacant.iter().enumerate() {
        let mut pair = list.reborrow().get(idx as u32);
        pair.set_link_id(link_id);
        pair.set_reason(reason);
    }
}

/// Inverse of [`fill_vacant_pairs`]. The reason is required: a vacancy is
/// rejected without one wherever it is written, so an empty one here means
/// the sender dropped it and the operator would be shown a blank explanation
/// for an unpaired slot.
fn read_vacant_pairs(
    list: capnp::struct_list::Reader<'_, node_capnp::vacant_pair::Owned>,
) -> Result<BTreeMap<String, String>> {
    let mut vacant = BTreeMap::new();
    for idx in 0..list.len() {
        let pair = list.get(idx);
        let link_id = pair.get_link_id()?.to_str()?.to_owned();
        let reason = required_text(
            pair.get_reason()?.to_str()?,
            &format!("NodeRunGoal.vacant_pairs[`{link_id}`].reason"),
        )?;
        vacant.insert(link_id, reason);
    }
    Ok(vacant)
}

/// Writes a [`ProducerRef`] into an initialized `InstanceAddress` builder.
fn write_instance_address(
    mut address: node_capnp::instance_address::Builder<'_>,
    producer: &ProducerRef,
) {
    address.set_core_node(&producer.core_node);
    address.set_instance_id(&producer.instance_id);
}

/// Inverse of [`write_instance_address`].
///
/// Both halves are required. Every wire message a peer acts on is
/// self-describing about placement, so an absent core node is a bug in the
/// sender rather than a shorthand for "local" — refusing here is what stops a
/// daemon from ever having to guess. `context` names the message, since the
/// address itself cannot say which one carried it.
fn read_instance_address(
    address: node_capnp::instance_address::Reader<'_>,
    context: &str,
) -> Result<ProducerRef> {
    let core_node = address.get_core_node()?.to_str()?;
    if core_node.is_empty() {
        return Err(crate::Error::Decoding(format!(
            "{context} carries an empty core_node: placement must be explicit, \
             even when the target is on the receiving daemon"
        )));
    }
    Ok(ProducerRef::new(
        core_node,
        required_text(
            address.get_instance_id()?.to_str()?,
            &format!("{context} instance_id"),
        )?,
    ))
}

/// Writes an `observer_link_id -> ObservationTargets` map into an initialized
/// `List(ObservationRequest)` builder ([`NodeRunGoal::planned_observations`]).
fn fill_observation_requests(
    mut list: capnp::struct_list::Builder<'_, node_capnp::observation_request::Owned>,
    observations: &BTreeMap<String, ObservationTargets>,
) {
    for (idx, (observer_link_id, targets)) in observations.iter().enumerate() {
        let mut observation = list.reborrow().get(idx as u32);
        observation.set_observer_link_id(observer_link_id);
        let mut members = observation.init_targets(targets.len() as u32);
        for (member_idx, target) in targets.iter().enumerate() {
            let mut member = members.reborrow().get(member_idx as u32);
            member.set_source_link_id(&target.source_link_id);
            write_instance_address(member.reborrow().init_source(), &target.source);
            if let Some(peer) = &target.peer {
                let mut wire_peer = member.init_peer();
                wire_peer.set_link_id(&peer.peer_link_id);
                write_instance_address(wire_peer.init_instance(), &peer.peer);
            }
        }
    }
}

/// Inverse of [`fill_observation_requests`].
///
/// Empty member sets and duplicates (of a member within a slot, or of a slot
/// within the goal) are refused for the same reason the writer never produces
/// them: each would silently drop a pairing the plan named.
fn read_observation_requests(
    list: capnp::struct_list::Reader<'_, node_capnp::observation_request::Owned>,
) -> Result<BTreeMap<String, ObservationTargets>> {
    let mut observations: BTreeMap<String, ObservationTargets> = BTreeMap::new();
    for idx in 0..list.len() {
        let observation = list.get(idx);
        let observer_link_id = required_text(
            observation.get_observer_link_id()?.to_str()?,
            "NodeRunGoal observation request observer_link_id",
        )?;
        let members = observation.get_targets()?;
        if members.is_empty() {
            return Err(crate::Error::Decoding(format!(
                "NodeRunGoal observation request for slot `{observer_link_id}` carries no \
                 targets: a slot that observes nothing is omitted from planned_observations \
                 rather than sent empty"
            )));
        }
        let targets = (0..members.len())
            .map(|member_idx| {
                let member = members.get(member_idx);
                let peer = if member.has_peer() {
                    let wire_peer = member.get_peer()?;
                    Some(ObservedPeer {
                        peer: read_instance_address(
                            wire_peer.get_instance()?,
                            "NodeRunGoal observation request peer",
                        )?,
                        peer_link_id: required_text(
                            wire_peer.get_link_id()?.to_str()?,
                            "NodeRunGoal observation request peer link_id",
                        )?,
                    })
                } else {
                    None
                };
                Ok(ObservationTarget {
                    source: read_instance_address(
                        member.get_source()?,
                        "NodeRunGoal observation request",
                    )?,
                    source_link_id: required_text(
                        member.get_source_link_id()?.to_str()?,
                        "NodeRunGoal observation request source_link_id",
                    )?,
                    peer,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let targets = ObservationTargets::new(&observer_link_id, targets)
            .map_err(|duplicate| crate::Error::Decoding(duplicate.to_string()))?;
        if observations
            .insert(observer_link_id.clone(), targets)
            .is_some()
        {
            return Err(crate::Error::Decoding(format!(
                "NodeRunGoal carries two observation requests for slot `{observer_link_id}`: a \
                 slot's whole member set travels in one request"
            )));
        }
    }
    Ok(observations)
}

/// Response to the NodeRun goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunGoalResponse {
    pub accepted: bool,
    pub log_path: PathBuf,
    pub rejection_reason: Option<String>,
}

impl NodeRunGoalResponse {
    pub fn accepted(log_path: impl Into<PathBuf>) -> Self {
        Self {
            accepted: true,
            log_path: log_path.into(),
            rejection_reason: None,
        }
    }

    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            log_path: PathBuf::new(),
            rejection_reason: Some(reason.into()),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_run_goal_response::Builder>();
            response.set_accepted(self.accepted);
            response.set_log_path(self.log_path.to_string_lossy().as_ref());
            if let Some(ref reason) = self.rejection_reason {
                response.set_rejection_reason(reason);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_run_goal_response::Reader>()?;
        Ok(Self {
            accepted: response.get_accepted(),
            log_path: PathBuf::from(response.get_log_path()?.to_str()?),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
        })
    }
}

/// Feedback message for the NodeRun action.
/// Represents a single line of output from the run_cmd process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunFeedback {
    pub stream: FeedbackStream,
    /// The line of output
    pub line: String,
}

impl NodeRunFeedback {
    pub fn from_stream(stream: FeedbackStream, line: impl Into<String>) -> Self {
        Self {
            stream,
            line: line.into(),
        }
    }

    pub fn stdout(line: impl Into<String>) -> Self {
        Self::from_stream(FeedbackStream::Stdout, line)
    }

    pub fn stderr(line: impl Into<String>) -> Self {
        Self::from_stream(FeedbackStream::Stderr, line)
    }

    pub fn warning(line: impl Into<String>) -> Self {
        Self::from_stream(FeedbackStream::Warning, line)
    }

    pub fn is_stdout(&self) -> bool {
        self.stream == FeedbackStream::Stdout
    }

    pub fn is_stderr(&self) -> bool {
        self.stream == FeedbackStream::Stderr
    }

    pub fn is_warning(&self) -> bool {
        self.stream == FeedbackStream::Warning
    }

    pub fn encode(&self) -> Result<NonEmptyPayload> {
        let mut builder = Builder::new_default();
        {
            let mut feedback = builder.init_root::<node_capnp::node_run_feedback::Builder>();
            feedback.set_stream(self.stream.to_capnp());
            feedback.set_line(&self.line);
        }
        encode_message_non_empty(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<node_capnp::node_run_feedback::Reader>()?;
        Ok(Self {
            stream: FeedbackStream::from_capnp(feedback.get_stream()?),
            line: feedback.get_line()?.to_str()?.to_owned(),
        })
    }
}

/// Result message for the NodeRun action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunResult {
    pub success: bool,
    pub error_message: Option<String>,
    /// Process ID of the started node (None if not available or failed).
    pub pid: Option<u32>,
    /// The endpoints the started instance serves, in label order, as the
    /// daemon hosting it expanded them; empty for a node that declares none
    /// and for a failed run.
    pub endpoints: Vec<InstanceEndpoint>,
}

impl NodeRunResult {
    pub fn new(success: bool, error_message: Option<String>, pid: Option<u32>) -> Self {
        Self {
            success,
            error_message,
            pid,
            endpoints: Vec::new(),
        }
    }

    pub fn success(pid: u32) -> Self {
        Self::new(true, None, Some(pid))
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, Some(error_message.into()), None)
    }

    /// The endpoints the started instance serves.
    pub fn with_endpoints(mut self, endpoints: Vec<InstanceEndpoint>) -> Self {
        self.endpoints = endpoints;
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<node_capnp::node_run_result::Builder>();
            result.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }
            result.set_pid(self.pid.unwrap_or(0));
            let endpoint_count = capnp_list_len(self.endpoints.len(), "NodeRunResult.endpoints")?;
            write_node_endpoints(result.init_endpoints(endpoint_count), &self.endpoints)?;
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<node_capnp::node_run_result::Reader>()?;
        let error_message = optional_text(result.get_error_message()?.to_str()?);
        let pid_value = result.get_pid();
        let pid = if pid_value == 0 {
            None
        } else {
            Some(pid_value)
        };
        Ok(Self {
            success: result.get_success(),
            error_message,
            pid,
            endpoints: read_node_endpoints(result.get_endpoints()?)?,
        })
    }
}

impl crate::encoding::Wire for NodeRunGoal {
    type Root = crate::node_capnp::node_run_goal::Owned;
}

impl crate::encoding::Wire for NodeRunGoalResponse {
    type Root = crate::node_capnp::node_run_goal_response::Owned;
}

impl crate::encoding::Wire for NodeRunFeedback {
    type Root = crate::node_capnp::node_run_feedback::Owned;
}

impl crate::encoding::Wire for NodeRunResult {
    type Root = crate::node_capnp::node_run_result::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- NodeRunGoal ---

    fn plan(instance_id: &str) -> NodeInstancePlan {
        NodeInstancePlan::new(config::runtime::Name::new(instance_id).expect("valid name"))
    }

    /// A publisher binding, so a plan on the wire carries a whole clock
    /// identity: the domain's name, its machine and its incarnation.
    fn sim_clock() -> config::runtime::ClockBinding {
        config::runtime::ClockBinding::publisher(config::runtime::ClockDomainId::new(
            config::runtime::Name::new("robot").expect("valid name"),
            config::runtime::CoreNodeName::new("cn-sim").expect("valid core node name"),
            config::runtime::ClockIncarnation::try_from(7).expect("non-zero"),
        ))
    }

    #[test]
    fn node_run_goal_new_has_empty_env_vars() {
        let goal = NodeRunGoal::new(plan("inst_1"), "node", "tag", 30);
        assert_eq!(goal.instance_plan.instance_id.as_str(), "inst_1");
        assert_eq!(goal.node_name, "node");
        assert_eq!(goal.tag, "tag");
        assert!(goal.env_vars.is_empty());
        assert_eq!(goal.timeout_secs, 30);
        assert_eq!(goal.manifest_sha256, None);
    }

    #[test]
    fn node_run_goal_roundtrip_empty_env_vars() {
        let goal = NodeRunGoal::new(plan("inst_1"), "node", "tag", 30);
        let encoded = goal.encode().expect("encode");
        let decoded = NodeRunGoal::decode(&encoded).expect("decode");
        assert_eq!(decoded, goal);
        assert!(decoded.env_vars.is_empty());
    }

    #[test]
    fn node_run_goal_roundtrip_instance_plan_fields() {
        let goal = NodeRunGoal::new(
            NodeInstancePlan {
                clock: sim_clock(),
                slot_bindings: BTreeMap::from([(
                    "camera".to_owned(),
                    config::runtime::BoundProducers::try_from(vec![
                        config::runtime::ProducerRef::new("cn-robot-7", "wrist_cam_inst"),
                    ])
                    .expect("one producer is a valid set"),
                )]),
                ..plan("planner_inst")
            },
            "deliberative_planner",
            "v1",
            30,
        )
        .with_manifest_sha256("a".repeat(64));

        let encoded = goal.encode().expect("encode");
        let decoded = NodeRunGoal::decode(&encoded).expect("decode");
        assert_eq!(decoded, goal);
        assert_eq!(decoded.instance_plan.clock, sim_clock());
        assert_eq!(
            decoded.instance_plan.slot_bindings["camera"].as_slice()[0].core_node,
            "cn-robot-7"
        );
        assert_eq!(decoded.manifest_sha256, Some("a".repeat(64)));
    }

    /// A cross-daemon producer binding is carried the same way a local one is:
    /// the `ProducerRef` already names its core node, so nothing at the point
    /// of use records which machine the producer sits on.
    #[test]
    fn node_run_goal_carries_producers_on_other_core_nodes() {
        let goal = NodeRunGoal::new(
            NodeInstancePlan {
                slot_bindings: BTreeMap::from([(
                    "scene".to_owned(),
                    config::runtime::BoundProducers::try_from(vec![
                        config::runtime::ProducerRef::new("cn-robot-7", "wrist_cam_inst"),
                    ])
                    .expect("one producer is a valid set"),
                )]),
                ..plan("planner_inst")
            },
            "deliberative_planner",
            "v1",
            0,
        );
        let decoded = NodeRunGoal::decode(&goal.encode().expect("encode")).expect("decode");
        assert_eq!(decoded, goal);
    }

    #[test]
    fn node_run_goal_roundtrip_pairs() {
        let goal = NodeRunGoal::new(plan("inst_1"), "node", "tag", 30)
            .with_requested_pairs(
                [
                    ("arm".to_owned(), vec![PairTarget::new("arm_1", "cn-local")]),
                    (
                        "gripper".to_owned(),
                        vec![PairTarget::pinned("grip_1", "controller", "cn-local")],
                    ),
                    // The cross-daemon case: same shape, different core node.
                    (
                        "deliberation".to_owned(),
                        vec![PairTarget::pinned(
                            "planner_inst",
                            "deliberator",
                            "cn-atlas-h100",
                        )],
                    ),
                ]
                .into_iter()
                .collect(),
            )
            .with_vacant_pairs(
                [(
                    "spare".to_owned(),
                    "bench rig: no second controller on this bench".to_owned(),
                )]
                .into_iter()
                .collect(),
            )
            .with_covered_pairs(
                [(
                    "left".to_owned(),
                    vec![PairTarget::pinned("cmd_1", "left_arm", "cn-local")],
                )]
                .into_iter()
                .collect(),
            )
            .with_planned_observations(
                [(
                    "observed_arm".to_owned(),
                    ObservationTarget::new("arm_1", "controller", "cn-local").into(),
                )]
                .into_iter()
                .collect(),
            );
        let encoded = goal.encode().expect("encode");
        let decoded = NodeRunGoal::decode(&encoded).expect("decode");
        assert_eq!(decoded, goal);
        // An unpinned target's empty peerLinkId decodes back to None.
        assert_eq!(decoded.requested_pairs["arm"][0].peer_link_id, None);
        assert_eq!(
            decoded.requested_pairs["gripper"][0]
                .peer_link_id
                .as_deref(),
            Some("controller")
        );
        assert_eq!(
            decoded.requested_pairs["deliberation"][0].peer.core_node,
            "cn-atlas-h100"
        );
        assert_eq!(
            decoded.vacant_pairs["spare"],
            "bench rig: no second controller on this bench"
        );
        assert_eq!(
            decoded.covered_pairs["left"],
            vec![PairTarget::pinned("cmd_1", "left_arm", "cn-local")]
        );
        assert_eq!(
            decoded.planned_observations["observed_arm"].as_slice(),
            [ObservationTarget::new("arm_1", "controller", "cn-local")]
        );
    }

    /// The reason is what a vacancy IS on the wire, so a goal that carries an
    /// empty one is refused at the boundary rather than shown to an operator
    /// as an unexplained unpaired slot.
    #[test]
    fn node_run_goal_rejects_a_vacant_pair_without_a_reason() {
        let goal = NodeRunGoal::new(plan("arm_1"), "robot_arm", "v1", 0).with_vacant_pairs(
            [("controller".to_owned(), String::new())]
                .into_iter()
                .collect(),
        );
        let encoded = goal.encode().expect("encode");
        let err = NodeRunGoal::decode(&encoded).expect_err("an empty reason must be refused");
        assert!(
            err.to_string().contains("controller"),
            "the error should name the slot: {err}"
        );
    }

    /// A multi-member observer slot travels as one request, and the order the
    /// plan wrote survives the wire: the node's `sources()` is that order, so
    /// a deployment can associate member N with its own Nth command slot. A
    /// member pinned to one pair carries that pair's other end.
    #[test]
    fn node_run_goal_roundtrip_multi_member_observation_preserves_order() {
        let pinned = ObservationTarget {
            peer: Some(ObservedPeer {
                peer: ProducerRef::new("cn-local", "backbone_1"),
                peer_link_id: "left_arm_link".to_string(),
            }),
            ..ObservationTarget::new("follower_1", "joint", "cn-atlas-h100")
        };
        let targets = ObservationTargets::new(
            "observed_joints",
            vec![
                ObservationTarget::new("follower_2", "joint", "cn-local"),
                pinned.clone(),
            ],
        )
        .expect("distinct members");
        let goal = NodeRunGoal::new(plan("commander_1"), "commander", "v1", 0)
            .with_planned_observations(
                [("observed_joints".to_owned(), targets.clone())]
                    .into_iter()
                    .collect(),
            );
        let decoded = NodeRunGoal::decode(&goal.encode().expect("encode")).expect("decode");
        assert_eq!(decoded, goal);
        assert_eq!(
            decoded.planned_observations["observed_joints"].as_slice(),
            [
                ObservationTarget::new("follower_2", "joint", "cn-local"),
                pinned,
            ],
            "plan order is not sorted away on the wire, and a pinned member keeps its peer"
        );
    }

    /// The same pairing may not be observed twice through one slot: two
    /// launcher entries resolving onto one source slot is a planning mistake,
    /// and the construction gate is the same one the launcher validator uses.
    #[test]
    fn observation_targets_reject_a_repeated_member() {
        let duplicate = ObservationTargets::new(
            "observed_joints",
            vec![
                ObservationTarget::new("follower_1", "joint", "cn-local"),
                ObservationTarget::new("follower_1", "joint", "cn-local"),
            ],
        )
        .expect_err("a repeated member must be rejected");
        assert_eq!(
            duplicate.target,
            ObservationTarget::new("follower_1", "joint", "cn-local")
        );
        let message = duplicate.to_string();
        assert!(
            message.contains("observed_joints") && message.contains("follower_1/joint@cn-local"),
            "the message names the slot and the repeated member: {message}"
        );

        // The same source instance under a DIFFERENT source slot is a
        // different pairing, and a legitimate second member.
        ObservationTargets::new(
            "observed_joints",
            vec![
                ObservationTarget::new("follower_1", "joint", "cn-local"),
                ObservationTarget::new("follower_1", "gripper", "cn-local"),
            ],
        )
        .expect("distinct source slots are distinct members");
    }

    /// A multi slot's pairs travel as one `PairRequest` per pair, all sharing
    /// that slot's link_id, and decode back into the slot's list in wire
    /// order. This is what lets one instance hold several pairs on one slot.
    #[test]
    fn node_run_goal_roundtrip_carries_every_pair_of_a_multi_slot() {
        let goal = NodeRunGoal::new(plan("engine_1"), "sim_engine", "v1", 0).with_requested_pairs(
            [(
                "limbs".to_owned(),
                vec![
                    PairTarget::pinned("ctrl_1", "arm", "cn-local"),
                    PairTarget::pinned("ctrl_2", "arm", "cn-local"),
                    PairTarget::pinned("ctrl_3", "arm", "cn-atlas-h100"),
                ],
            )]
            .into_iter()
            .collect(),
        );
        let decoded = NodeRunGoal::decode(&goal.encode().expect("encode")).expect("decode");
        assert_eq!(decoded, goal);
        assert_eq!(
            decoded.requested_pairs["limbs"]
                .iter()
                .map(|pair| pair.peer.instance_id.as_str())
                .collect::<Vec<_>>(),
            ["ctrl_1", "ctrl_2", "ctrl_3"],
            "every pair of the slot survives, in the order the plan wrote them"
        );
    }

    /// The coordinator's verdict about a peer this daemon cannot inspect is
    /// what makes a cross-machine pair committable at all: the receiver holds
    /// no manifest for the far side, so the pairing identity and the peer's
    /// role have to arrive with the request.
    #[test]
    fn a_remote_peer_verdict_survives_the_wire() {
        let goal = NodeRunGoal::new(plan("reflex_inst"), "reactive_policy", "v1", 0)
            .with_requested_pairs(
                [(
                    "deliberation".to_owned(),
                    vec![
                        PairTarget::pinned("planner_inst", "deliberation", "cn-atlas-h100")
                            .with_remote_peer(RemotePeerPairing {
                                pairing_name: "deliberation_link".to_owned(),
                                pairing_tag: "v1".to_owned(),
                                peer_role: "planner".to_owned(),
                                peer_cardinality: config::node::Cardinality::ZeroOrMore,
                                peer_copy: Some(config::runtime::Name::new("atlas").unwrap()),
                            }),
                    ],
                )]
                .into_iter()
                .collect(),
            );
        let decoded = NodeRunGoal::decode(&goal.encode().expect("encode")).expect("decode");
        assert_eq!(decoded, goal);
        let remote = decoded.requested_pairs["deliberation"][0]
            .remote_peer
            .as_ref()
            .expect("a peer on another machine carries the planner's verdict");
        assert_eq!(remote.peer_role, "planner");
        assert_eq!(remote.pairing_name, "deliberation_link");
        assert_eq!(
            remote.peer_cardinality,
            config::node::Cardinality::ZeroOrMore
        );
        assert_eq!(
            remote.peer_copy.as_ref().map(|copy| copy.as_str()),
            Some("atlas")
        );
    }

    /// A same-daemon peer carries no verdict: the local manifests are the
    /// authority, and a second opinion would be one too many.
    #[test]
    fn a_same_daemon_peer_carries_no_remote_verdict() {
        let goal = NodeRunGoal::new(plan("inst_1"), "node", "tag", 0).with_requested_pairs(
            [("arm".to_owned(), vec![PairTarget::new("arm_1", "cn-local")])]
                .into_iter()
                .collect(),
        );
        let decoded = NodeRunGoal::decode(&goal.encode().expect("encode")).expect("decode");
        assert_eq!(decoded.requested_pairs["arm"][0].remote_peer, None);
    }

    /// Placement is never inferred from an absent field, so a pair or
    /// observation whose core node did not survive encoding is refused rather
    /// than quietly read as "local".
    #[test]
    fn node_run_goal_decode_rejects_pair_without_a_core_node() {
        let goal = NodeRunGoal::new(plan("inst_1"), "node", "tag", 0).with_requested_pairs(
            [("arm".to_owned(), vec![PairTarget::new("arm_1", "")])]
                .into_iter()
                .collect(),
        );
        let encoded = goal.encode().expect("encode");
        let error = NodeRunGoal::decode(&encoded).expect_err("empty core node must fail");
        assert!(
            error.to_string().contains("empty core_node"),
            "got: {error}"
        );
    }

    #[test]
    fn node_run_goal_decode_rejects_observation_without_a_core_node() {
        let goal = NodeRunGoal::new(plan("inst_1"), "node", "tag", 0).with_planned_observations(
            [(
                "observed".to_owned(),
                ObservationTarget::new("arm_1", "controller", "").into(),
            )]
            .into_iter()
            .collect(),
        );
        let encoded = goal.encode().expect("encode");
        let error = NodeRunGoal::decode(&encoded).expect_err("empty core node must fail");
        assert!(
            error.to_string().contains("empty core_node"),
            "got: {error}"
        );
    }

    /// The break with pre-federation callers is enforced here, not by the
    /// codec: Cap'n Proto defaults an absent field to the empty string, so a
    /// goal that still ships an assembled runtime config would otherwise
    /// decode into a defaulted plan and spawn a node under the wrong identity.
    #[test]
    fn node_run_goal_decode_rejects_a_goal_carrying_no_instance_plan() {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<node_capnp::node_run_goal::Builder>();
            goal.set_node_name("node");
            goal.set_tag("tag");
        }
        let encoded = crate::encoding::encode_message(&builder).expect("encode");
        let error = NodeRunGoal::decode(&encoded).expect_err("a goal with no plan must fail");
        assert!(
            error.to_string().contains("instance_plan is empty"),
            "got: {error}"
        );
        assert!(
            error.to_string().contains("assembles runtime configs"),
            "the message must name the version gap, got: {error}"
        );
    }

    /// Encodes a goal whose observation requests are hand-built, so the
    /// decoder's shape rules can be exercised against messages the writer
    /// never produces.
    fn goal_with_raw_observations(
        fill: impl FnOnce(capnp::struct_list::Builder<'_, node_capnp::observation_request::Owned>),
        request_count: u32,
    ) -> Payload {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<node_capnp::node_run_goal::Builder>();
            goal.set_instance_plan_json5(
                serde_json5::to_string(&plan("inst_1")).expect("serialize plan"),
            );
            goal.set_node_name("node");
            goal.set_tag("tag");
            fill(goal.init_planned_observations(request_count));
        }
        crate::encoding::encode_message(&builder).expect("encode")
    }

    /// An empty member list is never written (a slot that observes nothing is
    /// omitted), so decoding one means the sender disagrees about that rule.
    #[test]
    fn node_run_goal_decode_rejects_an_empty_observation_target_list() {
        let encoded = goal_with_raw_observations(
            |mut list| {
                let mut request = list.reborrow().get(0);
                request.set_observer_link_id("observed_grippers");
                request.init_targets(0);
            },
            1,
        );
        let error = NodeRunGoal::decode(&encoded).expect_err("an empty member list must fail");
        assert!(
            error.to_string().contains("observed_grippers")
                && error.to_string().contains("carries no targets"),
            "got: {error}"
        );
    }

    /// A slot's whole member set travels in one request, so two requests for
    /// one slot would mean one of them silently wins the map insert.
    #[test]
    fn node_run_goal_decode_rejects_two_requests_for_one_slot() {
        let encoded = goal_with_raw_observations(
            |mut list| {
                for (idx, instance) in ["follower_1", "follower_2"].into_iter().enumerate() {
                    let mut request = list.reborrow().get(idx as u32);
                    request.set_observer_link_id("observed_joints");
                    let mut member = request.init_targets(1).get(0);
                    member.set_source_link_id("joint");
                    let mut source = member.init_source();
                    source.set_core_node("cn-local");
                    source.set_instance_id(instance);
                }
            },
            2,
        );
        let error = NodeRunGoal::decode(&encoded).expect_err("a repeated slot must fail");
        assert!(
            error.to_string().contains("two observation requests"),
            "got: {error}"
        );
    }

    /// The duplicate gate holds on the wire too, not only where the launcher
    /// builds the set.
    #[test]
    fn node_run_goal_decode_rejects_a_repeated_member_within_a_slot() {
        let encoded = goal_with_raw_observations(
            |mut list| {
                let mut request = list.reborrow().get(0);
                request.set_observer_link_id("observed_joints");
                let mut members = request.init_targets(2);
                for idx in 0..2 {
                    let mut member = members.reborrow().get(idx);
                    member.set_source_link_id("joint");
                    let mut source = member.init_source();
                    source.set_core_node("cn-local");
                    source.set_instance_id("follower_1");
                }
            },
            1,
        );
        let error = NodeRunGoal::decode(&encoded).expect_err("a repeated member must fail");
        assert!(
            error.to_string().contains("Duplicate observation target")
                && error.to_string().contains("observed_joints"),
            "got: {error}"
        );
    }

    /// Pins tenet 9 at the type level: a daemon owns the runtime identity of
    /// every node it spawns, so nothing a requester sends may name a messaging
    /// endpoint or a daemon. This holds for `peppy node run` exactly as it does
    /// for a federated dispatch, which is what makes the invariant checkable in
    /// one place instead of two.
    #[test]
    fn an_instance_plan_names_no_endpoint_and_no_daemon() {
        let serialized = serde_json5::to_string(&plan("inst_1")).expect("serialize");

        for forbidden in [
            "messaging_host",
            "messaging_port",
            "bound_core_node",
            "discovery",
            "lifecycle",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "an instance plan must not carry `{forbidden}`, got: {serialized}"
            );
        }
    }

    /// `Display` renders the CLI/launcher target grammar, the format fed to
    /// the shared pairing validator. Placement is deliberately absent from it:
    /// a target names an instance, and where that instance runs is declared
    /// once on the instance, never repeated at the point of use.
    #[test]
    fn pair_target_display_matches_target_grammar() {
        assert_eq!(PairTarget::new("arm_1", "cn-local").to_string(), "arm_1");
        assert_eq!(
            PairTarget::pinned("cmd_1", "left_arm", "cn-atlas").to_string(),
            "cmd_1/left_arm"
        );
    }

    #[test]
    fn node_run_goal_roundtrip_populated_env_vars() {
        let goal = NodeRunGoal::new(plan("inst_1"), "node", "tag", 42).with_env_vars(vec![
            ("KEY1".to_owned(), "VAL1".to_owned()),
            ("KEY2".to_owned(), "VAL2".to_owned()),
        ]);
        let encoded = goal.encode().expect("encode");
        let decoded = NodeRunGoal::decode(&encoded).expect("decode");
        assert_eq!(decoded, goal);
        assert_eq!(
            decoded.env_vars,
            vec![
                ("KEY1".to_owned(), "VAL1".to_owned()),
                ("KEY2".to_owned(), "VAL2".to_owned()),
            ]
        );
    }

    #[test]
    fn node_run_goal_for_internal_execution_has_zero_timeout() {
        let goal = NodeRunGoal::for_internal_execution(plan("inst_1"), "node", "tag");
        assert_eq!(goal.timeout_secs, 0);
        assert!(goal.env_vars.is_empty());
        // The in-process path has no preflight/dispatch window to close, so it
        // pins no manifest.
        assert_eq!(goal.manifest_sha256, None);
        let encoded = goal.encode().expect("encode");
        let decoded = NodeRunGoal::decode(&encoded).expect("decode");
        assert_eq!(decoded, goal);
    }

    /// A dispatched start names the launch that reserved the machine; a
    /// user-typed one names nothing. The receiving daemon tells them apart by
    /// exactly this field, so it has to survive the wire.
    #[test]
    fn node_run_goal_round_trips_the_launch_it_belongs_to() {
        let dispatched = NodeRunGoal::new(plan("inst_1"), "node", "tag", 30)
            .with_launch_id("launch-abc123")
            .with_lifecycle_watchers(vec!["cn-atlas".to_owned()]);
        let decoded = NodeRunGoal::decode(&dispatched.encode().expect("encode")).expect("decode");
        assert_eq!(decoded.launch_id.as_deref(), Some("launch-abc123"));
        assert_eq!(decoded.lifecycle_watchers, ["cn-atlas"]);
        assert_eq!(decoded, dispatched);

        let typed = NodeRunGoal::new(plan("inst_1"), "node", "tag", 30);
        let decoded = NodeRunGoal::decode(&typed.encode().expect("encode")).expect("decode");
        assert_eq!(
            decoded.launch_id, None,
            "a goal nobody dispatched must not claim a launch"
        );
        assert!(decoded.lifecycle_watchers.is_empty());
    }

    #[test]
    fn node_run_goal_decode_rejects_malformed_bytes() {
        assert!(NodeRunGoal::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }

    // --- NodeRunGoalResponse ---

    #[test]
    fn node_run_goal_response_accepted_roundtrip() {
        let response = NodeRunGoalResponse::accepted("/var/log/run.log");
        assert!(response.accepted);
        assert_eq!(response.log_path, PathBuf::from("/var/log/run.log"));
        assert_eq!(response.rejection_reason, None);
        let encoded = response.encode().expect("encode");
        let decoded = NodeRunGoalResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn node_run_goal_response_rejected_roundtrip() {
        let response = NodeRunGoalResponse::rejected("busy");
        assert!(!response.accepted);
        assert_eq!(response.log_path, PathBuf::new());
        assert_eq!(response.rejection_reason, Some("busy".to_owned()));
        let encoded = response.encode().expect("encode");
        let decoded = NodeRunGoalResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn node_run_goal_response_decode_rejects_malformed_bytes() {
        assert!(NodeRunGoalResponse::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }

    // --- NodeRunFeedback ---

    #[test]
    fn node_run_feedback_from_stream_roundtrip() {
        let feedback = NodeRunFeedback::from_stream(FeedbackStream::Stdout, "line");
        assert_eq!(feedback.stream, FeedbackStream::Stdout);
        assert_eq!(feedback.line, "line");
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeRunFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
    }

    #[test]
    fn node_run_feedback_stdout_predicates() {
        let feedback = NodeRunFeedback::stdout("out");
        assert!(feedback.is_stdout());
        assert!(!feedback.is_stderr());
        assert!(!feedback.is_warning());
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeRunFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
        assert!(decoded.is_stdout());
    }

    #[test]
    fn node_run_feedback_stderr_predicates() {
        let feedback = NodeRunFeedback::stderr("err");
        assert!(!feedback.is_stdout());
        assert!(feedback.is_stderr());
        assert!(!feedback.is_warning());
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeRunFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
        assert!(decoded.is_stderr());
    }

    #[test]
    fn node_run_feedback_warning_predicates() {
        let feedback = NodeRunFeedback::warning("warn");
        assert!(!feedback.is_stdout());
        assert!(!feedback.is_stderr());
        assert!(feedback.is_warning());
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeRunFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
        assert!(decoded.is_warning());
    }

    #[test]
    fn node_run_feedback_decode_rejects_malformed_bytes() {
        assert!(NodeRunFeedback::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }

    // --- NodeRunResult ---

    #[test]
    fn node_run_result_new_roundtrip() {
        let result = NodeRunResult::new(true, Some("warn".to_owned()), Some(7));
        assert!(result.success);
        assert_eq!(result.error_message, Some("warn".to_owned()));
        assert_eq!(result.pid, Some(7));
        let encoded = result.encode().expect("encode");
        let decoded = NodeRunResult::decode(&encoded).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn node_run_result_success_roundtrip() {
        let result = NodeRunResult::success(1234);
        assert!(result.success);
        assert_eq!(result.error_message, None);
        assert_eq!(result.pid, Some(1234));
        let encoded = result.encode().expect("encode");
        let decoded = NodeRunResult::decode(&encoded).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn node_run_result_failure_roundtrip() {
        let result = NodeRunResult::failure("boom");
        assert!(!result.success);
        assert_eq!(result.error_message, Some("boom".to_owned()));
        assert_eq!(result.pid, None);
        let encoded = result.encode().expect("encode");
        let decoded = NodeRunResult::decode(&encoded).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn node_run_result_decode_rejects_malformed_bytes() {
        assert!(NodeRunResult::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }

    /// The endpoints a started instance serves ride the result, with their
    /// kind and every URL; a node that declares none carries the empty set.
    #[test]
    fn node_run_result_roundtrips_endpoints() {
        let endpoints = crate::encoding::endpoints::sample_endpoints();
        let result = NodeRunResult::success(42).with_endpoints(endpoints.clone());
        let decoded = NodeRunResult::decode(&result.encode().expect("encode")).expect("decode");
        assert_eq!(decoded.endpoints, endpoints);
        assert_eq!(decoded, result);

        let plain = NodeRunResult::success(42);
        let decoded = NodeRunResult::decode(&plain.encode().expect("encode")).expect("decode");
        assert!(decoded.endpoints.is_empty());
    }
}
