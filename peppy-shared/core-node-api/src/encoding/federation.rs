//! Encoding types for the daemon-to-daemon federated-launch messages.
//!
//! See `schemas/federation.capnp` for why the reservation exchange and the
//! relationship notifications have deliberately different guarantees: the
//! former is coordinator-driven and must be exact, the latter is best-effort
//! and idempotent.

use capnp::message::Builder;
use config::runtime::{
    BoundProducers, CoreNodeName, Name, ObservedPeer, ProducerRef, first_duplicate,
};
use std::collections::{BTreeMap, BTreeSet};

use crate::federation_capnp;
use crate::{Payload, Result};

use crate::encoding::{
    ObservationTarget, ObservationTargets, capnp_list_len, decode_message, encode_message,
    optional_text, read_core_node_name, read_name, read_name_list, read_text_list, required_text,
    required_text_list, write_text_list,
};

/// Reserves one participant for one launch, carrying the pins for every
/// deployment placed on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantReserveRequest {
    pub launch_id: String,
    /// The coordinator driving the launch. The participant watches this core
    /// node's presence for as long as it holds the reservation.
    pub coordinator_core_node: String,
    /// JSON5-encoded `DeploymentPins`, one per deployment placed on this
    /// participant: the root node's pin plus the pin of every transitive
    /// node dependency and every contract and pairing document in its
    /// closure. Opaque here: the pin model lives in peppy, whose serde
    /// decoding is the validation, and this crate has no business
    /// re-deriving it.
    pub deployment_pins_json5: Vec<String>,
}

impl ParticipantReserveRequest {
    pub fn new(launch_id: impl Into<String>, coordinator_core_node: impl Into<String>) -> Self {
        Self {
            launch_id: launch_id.into(),
            coordinator_core_node: coordinator_core_node.into(),
            deployment_pins_json5: Vec::new(),
        }
    }

    pub fn with_deployment_pins(mut self, pins: Vec<String>) -> Self {
        self.deployment_pins_json5 = pins;
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<federation_capnp::participant_reserve_request::Builder>();
            request.set_launch_id(&self.launch_id);
            request.set_coordinator_core_node(&self.coordinator_core_node);
            let count = capnp_list_len(
                self.deployment_pins_json5.len(),
                "ParticipantReserveRequest.deployment_pins_json5",
            )?;
            write_text_list(
                request.reborrow().init_deployment_pins_json5(count),
                &self.deployment_pins_json5,
            );
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<federation_capnp::participant_reserve_request::Reader>()?;

        Ok(Self {
            launch_id: required_text(request.get_launch_id()?.to_str()?, "launch_id")?,
            coordinator_core_node: required_text(
                request.get_coordinator_core_node()?.to_str()?,
                "coordinator_core_node",
            )?,
            deployment_pins_json5: read_text_list(request.get_deployment_pins_json5()?)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantReserveResponse {
    pub accepted: bool,
    pub rejection_reason: Option<String>,
    /// The participant's peppy version, so a mixed-version federation is
    /// refused before any stack is touched. Same string the info service
    /// reports: one source of truth for "what version is that daemon".
    pub peppy_version: String,
    /// The participant's root entity instance id, folded into the
    /// coordinator's global instance-id uniqueness check.
    pub root_instance_id: String,
}

impl ParticipantReserveResponse {
    pub fn accepted(peppy_version: impl Into<String>, root_instance_id: impl Into<String>) -> Self {
        Self {
            accepted: true,
            rejection_reason: None,
            peppy_version: peppy_version.into(),
            root_instance_id: root_instance_id.into(),
        }
    }

    /// A refusal still reports the version, so a coordinator can tell "busy"
    /// apart from "too old" without a second round trip.
    pub fn rejected(reason: impl Into<String>, peppy_version: impl Into<String>) -> Self {
        Self {
            accepted: false,
            rejection_reason: Some(reason.into()),
            peppy_version: peppy_version.into(),
            root_instance_id: String::new(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<federation_capnp::participant_reserve_response::Builder>();
            response.set_accepted(self.accepted);
            response.set_rejection_reason(self.rejection_reason.as_deref().unwrap_or(""));
            response.set_peppy_version(&self.peppy_version);
            response.set_root_instance_id(&self.root_instance_id);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response =
            reader.get_root::<federation_capnp::participant_reserve_response::Reader>()?;

        Ok(Self {
            accepted: response.get_accepted(),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
            peppy_version: response.get_peppy_version()?.to_str()?.to_owned(),
            root_instance_id: response.get_root_instance_id()?.to_str()?.to_owned(),
        })
    }
}

/// Commits a reserved participant to replacing its stack slice, and hands it
/// the container bind sources that slice will need.
///
/// The destructive half of the exchange, deliberately split from the
/// reservation: reserving happens before the coordinator knows whether every
/// participant will accept, so folding a teardown into it would replace a
/// stack on one machine for a launch another is about to refuse.
///
/// [`Self::mount_sources`] rides along because this is the one moment the
/// participant's slice is empty; see the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantSliceBeginRequest {
    pub launch_id: String,
    pub mount_sources: Vec<String>,
    /// `true` adds to a slice of the same launch the participant already
    /// holds; `false` replaces whatever slice it holds.
    pub append: bool,
    /// Per local source instance, the machines whose observers it reports
    /// lifecycle events to; an empty set clears the instance's watchers.
    pub lifecycle_watchers: BTreeMap<Name, BTreeSet<CoreNodeName>>,
}

impl ParticipantSliceBeginRequest {
    pub fn new(launch_id: impl Into<String>, mount_sources: Vec<String>) -> Self {
        Self {
            launch_id: launch_id.into(),
            mount_sources,
            append: false,
            lifecycle_watchers: BTreeMap::new(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<federation_capnp::participant_slice_begin_request::Builder>();
            request.set_launch_id(&self.launch_id);
            request.set_append(self.append);
            let count = capnp_list_len(
                self.mount_sources.len(),
                "ParticipantSliceBeginRequest.mount_sources",
            )?;
            write_text_list(
                request.reborrow().init_mount_sources(count),
                &self.mount_sources,
            );
            let count = capnp_list_len(self.lifecycle_watchers.len(), "lifecycle_watchers")?;
            let mut entries = request.init_lifecycle_watchers(count);
            for (index, (instance, watchers)) in self.lifecycle_watchers.iter().enumerate() {
                let mut entry = entries.reborrow().get(index as u32);
                entry.set_instance_id(instance.as_str());
                write_text_list(
                    entry.init_core_nodes(capnp_list_len(watchers.len(), "core_nodes")?),
                    watchers.iter().map(|watcher| watcher.as_str()),
                );
            }
        }
        encode_message(&builder)
    }

    /// An empty launch id is refused: the exchange acts on exactly one launch,
    /// and a defaulted id names none. An empty mount source is refused one
    /// level down, for the same reason: it names no host path, and the
    /// participant would resolve it against its own working directory.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request =
            reader.get_root::<federation_capnp::participant_slice_begin_request::Reader>()?;
        let mut lifecycle_watchers = BTreeMap::new();
        for entry in request.get_lifecycle_watchers()? {
            let instance = read_name(
                entry.get_instance_id()?.to_str()?,
                "lifecycle_watchers.instance_id",
            )?;
            let watchers = entry
                .get_core_nodes()?
                .iter()
                .map(|watcher| {
                    read_core_node_name(watcher?.to_str()?, "lifecycle_watchers.core_nodes")
                })
                .collect::<Result<BTreeSet<_>>>()?;
            if lifecycle_watchers
                .insert(instance.clone(), watchers)
                .is_some()
            {
                return Err(crate::Error::Decoding(format!(
                    "`lifecycle_watchers` names `{instance}` twice"
                )));
            }
        }
        Ok(Self {
            launch_id: required_text(request.get_launch_id()?.to_str()?, "launch_id")?,
            mount_sources: required_text_list(
                read_text_list(request.get_mount_sources()?)?,
                "mount_sources",
            )?,
            append: request.get_append(),
            lifecycle_watchers,
        })
    }
}

/// The reply to `participant_slice_begin`: the verdict, plus the bind sources
/// the participant had to create to honour it.
///
/// [`Self::auto_created_mount_sources`] is what the coordinator turns back into
/// the warning an operator would have seen had the instance run on their own
/// machine; see the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantSliceBeginResponse {
    pub ok: bool,
    pub rejection_reason: Option<String>,
    pub auto_created_mount_sources: Vec<String>,
}

impl ParticipantSliceBeginResponse {
    pub fn ok(auto_created_mount_sources: Vec<String>) -> Self {
        Self {
            ok: true,
            rejection_reason: None,
            auto_created_mount_sources,
        }
    }

    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            rejection_reason: Some(reason.into()),
            auto_created_mount_sources: Vec::new(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<federation_capnp::participant_slice_begin_response::Builder>();
            response.set_ok(self.ok);
            response.set_rejection_reason(self.rejection_reason.as_deref().unwrap_or(""));
            let count = capnp_list_len(
                self.auto_created_mount_sources.len(),
                "ParticipantSliceBeginResponse.auto_created_mount_sources",
            )?;
            write_text_list(
                response.reborrow().init_auto_created_mount_sources(count),
                &self.auto_created_mount_sources,
            );
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response =
            reader.get_root::<federation_capnp::participant_slice_begin_response::Reader>()?;
        Ok(Self {
            ok: response.get_ok(),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
            auto_created_mount_sources: required_text_list(
                read_text_list(response.get_auto_created_mount_sources()?)?,
                "auto_created_mount_sources",
            )?,
        })
    }
}

/// The instances one removal names: at least one, each once, in the order
/// they are stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedInstances(Vec<Name>);

impl RemovedInstances {
    pub fn as_slice(&self) -> &[Name] {
        &self.0
    }
}

/// Why a list of instance ids is not a removal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemovedInstancesError {
    #[error("a removal names at least one instance")]
    Empty,
    #[error("a removal names `{0}` twice")]
    Repeated(Name),
}

/// The single construction gate: an empty or repeating list cannot exist,
/// decoded or built.
impl TryFrom<Vec<Name>> for RemovedInstances {
    type Error = RemovedInstancesError;

    fn try_from(ids: Vec<Name>) -> std::result::Result<Self, RemovedInstancesError> {
        if ids.is_empty() {
            return Err(RemovedInstancesError::Empty);
        }
        if let Some(repeated) = first_duplicate(&ids) {
            return Err(RemovedInstancesError::Repeated(repeated.clone()));
        }
        Ok(Self(ids))
    }
}

/// Idempotent instance cleanup, authorized by both the reservation and slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantInstancesRemoveRequest {
    pub launch_id: String,
    pub instance_ids: RemovedInstances,
}

impl ParticipantInstancesRemoveRequest {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        let mut request =
            builder.init_root::<federation_capnp::participant_instances_remove_request::Builder>();
        request.set_launch_id(&self.launch_id);
        let instance_ids = self.instance_ids.as_slice();
        let count = capnp_list_len(instance_ids.len(), "instance_ids")?;
        write_text_list(request.init_instance_ids(count), instance_ids);
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request =
            reader.get_root::<federation_capnp::participant_instances_remove_request::Reader>()?;
        Ok(Self {
            launch_id: required_text(request.get_launch_id()?.to_str()?, "launch_id")?,
            instance_ids: RemovedInstances::try_from(read_name_list(
                request.get_instance_ids()?,
                "instance_ids",
            )?)
            .map_err(|error| crate::Error::Decoding(format!("`instance_ids`: {error}")))?,
        })
    }
}

impl crate::encoding::Wire for ParticipantInstancesRemoveRequest {
    type Root = federation_capnp::participant_instances_remove_request::Owned;
}

/// Replaces set slots of instances a participant runs in a reserved launch's
/// slice, each with the whole member set it holds from now on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantSetsUpdateRequest {
    pub launch_id: String,
    pub sets: Vec<SlotSet>,
}

/// One instance's slot and the set it holds from now on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotSet {
    pub instance_id: Name,
    pub link_id: String,
    pub members: SlotMembers,
}

/// A set slot's members, in plan order: a producer-binding slot's producers or
/// an observer slot's observed pairings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotMembers {
    Producers(BoundProducers),
    Observed(ObservationTargets),
}

impl ParticipantSetsUpdateRequest {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<federation_capnp::participant_sets_update_request::Builder>();
            request.set_launch_id(&self.launch_id);
            let mut sets = request.init_sets(capnp_list_len(self.sets.len(), "sets")?);
            for (index, set) in self.sets.iter().enumerate() {
                let mut wire = sets.reborrow().get(index as u32);
                wire.set_instance_id(set.instance_id.as_str());
                wire.set_link_id(&set.link_id);
                let members = wire.init_members();
                match &set.members {
                    SlotMembers::Producers(producers) => {
                        let mut list =
                            members.init_producers(capnp_list_len(producers.len(), "producers")?);
                        for (member_index, producer) in producers.iter().enumerate() {
                            write_instance_address(
                                list.reborrow().get(member_index as u32),
                                producer,
                            );
                        }
                    }
                    SlotMembers::Observed(targets) => {
                        let mut list =
                            members.init_observed(capnp_list_len(targets.len(), "observed")?);
                        for (member_index, target) in targets.iter().enumerate() {
                            write_observation_member(
                                list.reborrow().get(member_index as u32),
                                target,
                            );
                        }
                    }
                }
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request =
            reader.get_root::<federation_capnp::participant_sets_update_request::Reader>()?;
        let sets = request
            .get_sets()?
            .iter()
            .map(|wire| {
                use federation_capnp::slot_set::members::Which;
                let link_id = required_text(wire.get_link_id()?.to_str()?, "sets.link_id")?;
                let members = match wire.get_members().which()? {
                    Which::Producers(list) => {
                        let producers = list?
                            .iter()
                            .map(|address| read_instance_address(address, "sets.producers"))
                            .collect::<Result<Vec<_>>>()?;
                        SlotMembers::Producers(BoundProducers::try_from(producers).map_err(
                            |error| crate::Error::Decoding(format!("slot `{link_id}`: {error}")),
                        )?)
                    }
                    Which::Unset(()) => {
                        return Err(crate::Error::Decoding(format!(
                            "slot `{link_id}`: the set names neither producers nor observed \
                             pairings"
                        )));
                    }
                    Which::Observed(list) => {
                        let targets = list?
                            .iter()
                            .map(read_observation_member)
                            .collect::<Result<Vec<_>>>()?;
                        SlotMembers::Observed(
                            ObservationTargets::new(&link_id, targets).map_err(|duplicate| {
                                crate::Error::Decoding(duplicate.to_string())
                            })?,
                        )
                    }
                };
                Ok(SlotSet {
                    instance_id: read_name(wire.get_instance_id()?.to_str()?, "sets.instance_id")?,
                    link_id,
                    members,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            launch_id: required_text(request.get_launch_id()?.to_str()?, "launch_id")?,
            sets,
        })
    }
}

impl crate::encoding::Wire for ParticipantSetsUpdateRequest {
    type Root = federation_capnp::participant_sets_update_request::Owned;
}

/// The reply to every federation exchange whose answer is "did you do it, and
/// if not, why": `pair_commit`, `participant_release`,
/// `participant_instances_remove` and `participant_sets_update`. One codec for
/// all of them, because they differ only in which verb the bool reports and
/// that verb is already the service name.
///
/// [`Self::rejection_reason`] is load-bearing on refusal — see the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationVerdict {
    pub ok: bool,
    pub rejection_reason: Option<String>,
}

impl FederationVerdict {
    pub fn ok() -> Self {
        Self {
            ok: true,
            rejection_reason: None,
        }
    }

    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            rejection_reason: Some(reason.into()),
        }
    }

    /// The verdict as a result: a refusal carries the participant's reason.
    pub fn into_result(self) -> std::result::Result<(), String> {
        if self.ok {
            return Ok(());
        }
        Err(self
            .rejection_reason
            .unwrap_or_else(|| "no reason given".to_owned()))
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut verdict = builder.init_root::<federation_capnp::federation_verdict::Builder>();
            verdict.set_ok(self.ok);
            verdict.set_rejection_reason(self.rejection_reason.as_deref().unwrap_or(""));
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let verdict = reader.get_root::<federation_capnp::federation_verdict::Reader>()?;
        Ok(Self {
            ok: verdict.get_ok(),
            rejection_reason: optional_text(verdict.get_rejection_reason()?.to_str()?),
        })
    }
}

/// Writes a [`ProducerRef`] into an initialized `InstanceAddress` builder.
///
/// The federation twin of `node::run`'s helper of the same name: the two
/// schemas declare the struct separately because each `.capnp` is compiled on
/// its own, but both decode to one [`ProducerRef`].
fn write_instance_address(
    mut address: federation_capnp::instance_address::Builder<'_>,
    producer: &ProducerRef,
) {
    address.set_core_node(&producer.core_node);
    address.set_instance_id(&producer.instance_id);
}

/// Writes one observed pairing into an initialized `ObservationMember` builder.
///
/// The federation twin of `node::run`'s member codec: both schemas declare the
/// struct separately because each `.capnp` is compiled on its own, and both
/// decode to one [`ObservationTarget`].
fn write_observation_member(
    mut member: federation_capnp::observation_member::Builder<'_>,
    target: &ObservationTarget,
) {
    member.set_source_link_id(&target.source_link_id);
    write_instance_address(member.reborrow().init_source(), &target.source);
    if let Some(peer) = &target.peer {
        let mut wire_peer = member.init_peer();
        wire_peer.set_link_id(&peer.peer_link_id);
        write_instance_address(wire_peer.init_instance(), &peer.peer);
    }
}

/// Inverse of [`write_observation_member`]. A member with no peer observes
/// every pair of its source's slot.
fn read_observation_member(
    member: federation_capnp::observation_member::Reader<'_>,
) -> Result<ObservationTarget> {
    let peer = if member.has_peer() {
        let wire_peer = member.get_peer()?;
        Some(ObservedPeer {
            peer: read_instance_address(wire_peer.get_instance()?, "sets.observed.peer")?,
            peer_link_id: required_text(
                wire_peer.get_link_id()?.to_str()?,
                "sets.observed.peer.link_id",
            )?,
        })
    } else {
        None
    };
    Ok(ObservationTarget {
        source: read_instance_address(member.get_source()?, "sets.observed.source")?,
        source_link_id: required_text(
            member.get_source_link_id()?.to_str()?,
            "sets.observed.source_link_id",
        )?,
        peer,
    })
}

/// Inverse of [`write_instance_address`]. Both halves are required: an address
/// missing either one names no instance in particular.
fn read_instance_address(
    address: federation_capnp::instance_address::Reader<'_>,
    field: &str,
) -> Result<ProducerRef> {
    Ok(ProducerRef::new(
        required_text(
            address.get_core_node()?.to_str()?,
            &format!("{field}.core_node"),
        )?,
        required_text(
            address.get_instance_id()?.to_str()?,
            &format!("{field}.instance_id"),
        )?,
    ))
}

/// Asks a peer daemon to record its half of a cross-daemon pair and deliver the
/// pin to its own endpoint.
///
/// The field names are relative to the RECEIVER: `local_*` is the endpoint on
/// the daemon being asked, `peer_*` is the one on the daemon asking. Both
/// addresses carry their core node, so the receiver checks that `local` really
/// names it rather than assuming so; see the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairCommitRequest {
    pub pairing_name: String,
    pub pairing_tag: String,
    pub local: ProducerRef,
    pub local_link_id: String,
    pub local_role: String,
    pub peer: ProducerRef,
    pub peer_link_id: String,
    pub peer_role: String,
    /// The declared cardinality of the peer's slot: only a scalar slot is
    /// taken by one pair.
    pub peer_cardinality: config::node::Cardinality,
    /// The copy the peer's instance belongs to; `None` outside a copy.
    pub peer_copy: Option<config::runtime::Name>,
}

impl PairCommitRequest {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<federation_capnp::pair_commit_request::Builder>();
            request.set_pairing_name(&self.pairing_name);
            request.set_pairing_tag(&self.pairing_tag);
            request.set_local_link_id(&self.local_link_id);
            request.set_local_role(&self.local_role);
            request.set_peer_link_id(&self.peer_link_id);
            request.set_peer_role(&self.peer_role);
            request.set_peer_cardinality(self.peer_cardinality.as_str());
            request.set_peer_copy(
                self.peer_copy
                    .as_ref()
                    .map(|copy| copy.as_str())
                    .unwrap_or(""),
            );
            write_instance_address(request.reborrow().init_local(), &self.local);
            write_instance_address(request.init_peer(), &self.peer);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<federation_capnp::pair_commit_request::Reader>()?;
        Ok(Self {
            pairing_name: required_text(request.get_pairing_name()?.to_str()?, "pairing_name")?,
            pairing_tag: required_text(request.get_pairing_tag()?.to_str()?, "pairing_tag")?,
            local: read_instance_address(request.get_local()?, "local")?,
            local_link_id: required_text(request.get_local_link_id()?.to_str()?, "local_link_id")?,
            local_role: required_text(request.get_local_role()?.to_str()?, "local_role")?,
            peer: read_instance_address(request.get_peer()?, "peer")?,
            peer_link_id: required_text(request.get_peer_link_id()?.to_str()?, "peer_link_id")?,
            peer_role: required_text(request.get_peer_role()?.to_str()?, "peer_role")?,
            peer_cardinality: required_text(
                request.get_peer_cardinality()?.to_str()?,
                "peer_cardinality",
            )?
            .parse()
            .map_err(|spelling| {
                crate::Error::Decoding(format!(
                    "peer_cardinality `{spelling}` is none of one, zero_or_one, one_or_more, zero_or_more"
                ))
            })?,
            peer_copy: match request.get_peer_copy()?.to_str()? {
                "" => None,
                copy => Some(config::runtime::Name::new(copy).map_err(|e| {
                    crate::Error::Decoding(format!("peer_copy is not a name: {e}"))
                })?),
            },
        })
    }
}

/// Releases a reservation. Idempotent: releasing one that is not held
/// succeeds, because a coordinator unwinding a failed preflight cannot always
/// know which participants actually acked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantReleaseRequest {
    pub launch_id: String,
}

impl ParticipantReleaseRequest {
    pub fn new(launch_id: impl Into<String>) -> Self {
        Self {
            launch_id: launch_id.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            builder
                .init_root::<federation_capnp::launch_scoped_request::Builder>()
                .set_launch_id(&self.launch_id);
        }
        encode_message(&builder)
    }

    /// An empty launch id is refused: the exchange acts on exactly one launch,
    /// and a defaulted id names none.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<federation_capnp::launch_scoped_request::Reader>()?;
        Ok(Self {
            launch_id: required_text(request.get_launch_id()?.to_str()?, "launch_id")?,
        })
    }
}

/// What happened to an instance, as reported by the daemon that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationshipEvent {
    /// Reached Running under a fresh incarnation. Observing daemons advance
    /// their incarnation counter for this source and redeliver its pin.
    ReachedRunning,
    /// Stopped or died. A daemon holding a pair with it dissolves that pair.
    Stopped,
}

/// Best-effort, idempotent notification from the daemon that owns an instance
/// to a daemon holding a relationship with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipNotification {
    /// The instance whose lifecycle moved, and the daemon that owns it. Two
    /// daemons can host same-named instances, so the pair is the identity.
    pub instance: ProducerRef,
    pub event: RelationshipEvent,
}

impl RelationshipNotification {
    pub fn new(
        instance_id: impl Into<String>,
        core_node: impl Into<String>,
        event: RelationshipEvent,
    ) -> Self {
        Self {
            instance: ProducerRef::new(core_node, instance_id),
            event,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut notification =
                builder.init_root::<federation_capnp::relationship_notification::Builder>();
            let mut event = notification.reborrow().init_event();
            match self.event {
                RelationshipEvent::ReachedRunning => event.set_reached_running(()),
                RelationshipEvent::Stopped => event.set_stopped(()),
            }
            write_instance_address(notification.init_instance(), &self.instance);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use federation_capnp::relationship_notification::event::Which;

        let reader = decode_message(data)?;
        let notification =
            reader.get_root::<federation_capnp::relationship_notification::Reader>()?;

        let event = match notification.get_event().which()? {
            Which::ReachedRunning(()) => RelationshipEvent::ReachedRunning,
            Which::Stopped(()) => RelationshipEvent::Stopped,
        };

        Ok(Self {
            instance: read_instance_address(notification.get_instance()?, "instance")?,
            event,
        })
    }
}

/// Carries no fields: the notification is best-effort and the receiver simply
/// converges on what it is told, so a well-formed reply is itself the ack —
/// the same contract [`HealthRequest`](crate::encoding::HealthRequest) uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RelationshipNotificationAck;

impl RelationshipNotificationAck {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            builder.init_root::<federation_capnp::relationship_notification_ack::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        // Validate the framing decodes as the ack; the struct is empty, so
        // there is nothing else to read back.
        reader.get_root::<federation_capnp::relationship_notification_ack::Reader>()?;
        Ok(Self)
    }
}

impl crate::encoding::Wire for ParticipantReserveRequest {
    type Root = crate::federation_capnp::participant_reserve_request::Owned;
}

impl crate::encoding::Wire for ParticipantReserveResponse {
    type Root = crate::federation_capnp::participant_reserve_response::Owned;
}

impl crate::encoding::Wire for ParticipantSliceBeginRequest {
    type Root = crate::federation_capnp::participant_slice_begin_request::Owned;
}

impl crate::encoding::Wire for ParticipantSliceBeginResponse {
    type Root = crate::federation_capnp::participant_slice_begin_response::Owned;
}

impl crate::encoding::Wire for ParticipantReleaseRequest {
    type Root = crate::federation_capnp::launch_scoped_request::Owned;
}

impl crate::encoding::Wire for PairCommitRequest {
    type Root = crate::federation_capnp::pair_commit_request::Owned;
}

impl crate::encoding::Wire for FederationVerdict {
    type Root = crate::federation_capnp::federation_verdict::Owned;
}

impl crate::encoding::Wire for RelationshipNotification {
    type Root = crate::federation_capnp::relationship_notification::Owned;
}

impl crate::encoding::Wire for RelationshipNotificationAck {
    type Root = crate::federation_capnp::relationship_notification_ack::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_update_round_trips_producer_and_observer_sets_in_plan_order() {
        let pairing = |instance: &str| ObservationTarget {
            source: ProducerRef::new("cn-robot", instance),
            source_link_id: "controller".into(),
            peer: None,
        };
        let pinned = ObservationTarget {
            peer: Some(ObservedPeer {
                peer: ProducerRef::new("cn-cloud", "alpha_leader_inst"),
                peer_link_id: "followers".into(),
            }),
            ..pairing("hub_inst")
        };
        let request = ParticipantSetsUpdateRequest {
            launch_id: "launch-abc123".into(),
            sets: vec![
                SlotSet {
                    instance_id: Name::new("planner_inst").unwrap(),
                    link_id: "robots".into(),
                    members: SlotMembers::Producers(
                        BoundProducers::try_from(vec![
                            ProducerRef::new("cn-robot", "bravo_arm_inst"),
                            ProducerRef::new("cn-cloud", "alpha_arm_inst"),
                        ])
                        .unwrap(),
                    ),
                },
                SlotSet {
                    instance_id: Name::new("monitor_inst").unwrap(),
                    link_id: "fleet".into(),
                    members: SlotMembers::Observed(
                        ObservationTargets::new(
                            "fleet",
                            vec![pairing("bravo_arm_inst"), pairing("alpha_arm_inst"), pinned],
                        )
                        .unwrap(),
                    ),
                },
                SlotSet {
                    instance_id: Name::new("monitor_inst").unwrap(),
                    link_id: "spare".into(),
                    members: SlotMembers::Observed(
                        ObservationTargets::new("spare", Vec::new()).unwrap(),
                    ),
                },
                // What removing the last copy delivers to a `zero_or_more`
                // consumer slot.
                SlotSet {
                    instance_id: Name::new("planner_inst").unwrap(),
                    link_id: "cameras".into(),
                    members: SlotMembers::Producers(BoundProducers::default()),
                },
            ],
        };
        assert_eq!(
            ParticipantSetsUpdateRequest::decode(&request.encode().unwrap()).unwrap(),
            request
        );
    }

    #[test]
    fn sets_update_refuses_a_repeated_producer_and_an_unplaced_member() {
        for (core_node, repeated) in [("cn-robot", true), ("", false)] {
            let mut builder = Builder::new_default();
            {
                let mut request = builder
                    .init_root::<federation_capnp::participant_sets_update_request::Builder>();
                request.set_launch_id("launch-abc123");
                let mut set = request.init_sets(1).get(0);
                set.set_instance_id("planner_inst");
                set.set_link_id("robots");
                let count = if repeated { 2 } else { 1 };
                let mut list = set.init_members().init_producers(count);
                for index in 0..count {
                    let mut address = list.reborrow().get(index);
                    address.set_core_node(core_node);
                    address.set_instance_id("alpha_arm_inst");
                }
            }
            assert!(
                ParticipantSetsUpdateRequest::decode(&encode_message(&builder).unwrap()).is_err(),
                "core_node `{core_node}`, repeated {repeated}"
            );
        }
    }

    /// A set naming neither producers nor observed pairings is a sender that
    /// never filled the union in, and is refused at decode.
    #[test]
    fn sets_update_refuses_a_set_of_neither_kind() {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<federation_capnp::participant_sets_update_request::Builder>();
            request.set_launch_id("launch-abc123");
            let mut set = request.init_sets(1).get(0);
            set.set_instance_id("planner_inst");
            set.set_link_id("robots");
        }
        let error = ParticipantSetsUpdateRequest::decode(&encode_message(&builder).unwrap())
            .expect_err("a set of neither kind is refused");
        assert!(
            error.to_string().contains("neither producers nor observed"),
            "{error}"
        );
    }

    #[test]
    fn reserve_request_round_trips() {
        let request = ParticipantReserveRequest::new("launch-abc123", "cn-robot-7")
            .with_deployment_pins(vec![
                r#"{root:{kind:"node",name:"deliberative_planner",tag:"v1"},closure:[]}"#
                    .to_owned(),
                r#"{root:{kind:"node",name:"episode_recorder",tag:"v1"},closure:[]}"#.to_owned(),
            ]);
        let payload = request.encode().expect("encode");
        assert_eq!(
            ParticipantReserveRequest::decode(payload.as_ref()).expect("decode"),
            request
        );
    }

    #[test]
    fn reserve_request_round_trips_with_no_deployments() {
        let request = ParticipantReserveRequest::new("launch-abc123", "cn-robot-7");
        let payload = request.encode().expect("encode");
        let decoded = ParticipantReserveRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
        assert!(decoded.deployment_pins_json5.is_empty());
    }

    #[test]
    fn reserve_request_decode_rejects_missing_identity() {
        for (launch_id, coordinator, expected) in [
            ("", "cn-robot-7", "launch_id"),
            ("launch-abc123", "", "coordinator_core_node"),
        ] {
            let request = ParticipantReserveRequest::new(launch_id, coordinator);
            let payload = request.encode().expect("encode");
            let error = ParticipantReserveRequest::decode(payload.as_ref())
                .expect_err("missing identity must fail");
            assert!(error.to_string().contains(expected), "got: {error}");
        }
    }

    #[test]
    fn reserve_response_round_trips_acceptance() {
        let response =
            ParticipantReserveResponse::accepted("v0.20.0-3-g8c7cbaa7", "core_node_gen_1");
        let payload = response.encode().expect("encode");
        let decoded = ParticipantReserveResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.accepted);
        assert_eq!(decoded.root_instance_id, "core_node_gen_1");
    }

    /// A refusal still reports the version so "busy" and "too old" are
    /// distinguishable without a second round trip.
    #[test]
    fn reserve_response_round_trips_refusal_with_version() {
        let response = ParticipantReserveResponse::rejected(
            "already reserved for launch `launch-other`",
            "v0.19.0",
        );
        let payload = response.encode().expect("encode");
        let decoded = ParticipantReserveResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(!decoded.accepted);
        assert_eq!(decoded.peppy_version, "v0.19.0");
        assert!(decoded.root_instance_id.is_empty());
    }

    #[test]
    fn release_round_trips() {
        let request = ParticipantReleaseRequest::new("launch-abc123");
        let payload = request.encode().expect("encode");
        assert_eq!(
            ParticipantReleaseRequest::decode(payload.as_ref()).expect("decode"),
            request
        );
    }

    #[test]
    fn slice_begin_round_trips_with_mount_sources() {
        let request = ParticipantSliceBeginRequest::new(
            "launch-abc123",
            vec![
                "/tmp/video_reconstruction".to_owned(),
                "/data/episodes".to_owned(),
            ],
        );
        let payload = request.encode().expect("encode");
        let decoded = ParticipantSliceBeginRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
        assert_eq!(decoded.mount_sources.len(), 2);
        assert_eq!(decoded.mount_sources[1], "/data/episodes");
    }

    /// A slice with no container node binds nothing, which is not the same
    /// shape of message as one that failed to say what it binds.
    #[test]
    fn slice_begin_round_trips_with_no_mount_sources() {
        let request = ParticipantSliceBeginRequest::new("launch-abc123", Vec::new());
        let payload = request.encode().expect("encode");
        let decoded = ParticipantSliceBeginRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
        assert!(decoded.mount_sources.is_empty());
    }

    /// A defaulted entry names no host path. Accepting one would have the
    /// participant create a directory for the empty string.
    #[test]
    fn slice_begin_decode_rejects_an_empty_mount_source() {
        let request = ParticipantSliceBeginRequest::new(
            "launch-abc123",
            vec![String::new(), "/ok".to_owned()],
        );
        let payload = request.encode().expect("encode");
        let error = ParticipantSliceBeginRequest::decode(payload.as_ref())
            .expect_err("an empty mount source must fail");
        assert!(
            error.to_string().contains("mount_sources[0]"),
            "got: {error}"
        );
    }

    #[test]
    fn slice_begin_response_round_trips_both_outcomes() {
        for response in [
            ParticipantSliceBeginResponse::ok(vec!["/tmp/video_reconstruction".to_owned()]),
            ParticipantSliceBeginResponse::ok(Vec::new()),
            ParticipantSliceBeginResponse::refused("reserved for launch `launch-other`"),
        ] {
            let payload = response.encode().expect("encode");
            assert_eq!(
                ParticipantSliceBeginResponse::decode(payload.as_ref()).expect("decode"),
                response
            );
        }
    }

    #[test]
    fn verdict_round_trips_both_outcomes() {
        for verdict in [
            FederationVerdict::ok(),
            FederationVerdict::refused("reserved for launch `launch-other`"),
        ] {
            let payload = verdict.encode().expect("encode");
            assert_eq!(
                FederationVerdict::decode(payload.as_ref()).expect("decode"),
                verdict
            );
        }
    }

    /// The destructive step must never act on a defaulted launch id: that is
    /// how a machine would get its stack replaced on behalf of nobody.
    #[test]
    fn slice_begin_decode_rejects_empty_launch_id() {
        let payload = ParticipantSliceBeginRequest::new("", Vec::new())
            .encode()
            .expect("encode");
        let error = ParticipantSliceBeginRequest::decode(payload.as_ref())
            .expect_err("empty launch id must fail");
        assert!(error.to_string().contains("launch_id"), "got: {error}");
    }

    #[test]
    fn slice_begin_round_trips_append_mode() {
        for append in [false, true] {
            let request = ParticipantSliceBeginRequest {
                append,
                lifecycle_watchers: BTreeMap::from([
                    (
                        Name::new("arm_inst").unwrap(),
                        BTreeSet::from([
                            CoreNodeName::new("robot").unwrap(),
                            CoreNodeName::new("cloud").unwrap(),
                        ]),
                    ),
                    (Name::new("camera_inst").unwrap(), BTreeSet::new()),
                ]),
                ..ParticipantSliceBeginRequest::new("launch-abc123", Vec::new())
            };
            assert_eq!(
                ParticipantSliceBeginRequest::decode(&request.encode().unwrap()).unwrap(),
                request
            );
        }
    }

    #[test]
    fn slice_begin_rejects_invalid_watcher_identities() {
        for (instances, host) in [
            (vec![""], "robot"),
            (vec!["arm/inst"], "robot"),
            (vec!["arm_inst", "arm_inst"], "robot"),
            (vec!["arm_inst"], "self"),
            (vec!["arm_inst"], "robot/cloud"),
        ] {
            let mut message = Builder::new_default();
            let mut request =
                message.init_root::<federation_capnp::participant_slice_begin_request::Builder>();
            request.set_launch_id("launch-abc123");
            let mut watchers = request.init_lifecycle_watchers(instances.len() as u32);
            for (index, instance) in instances.iter().enumerate() {
                let mut entry = watchers.reborrow().get(index as u32);
                entry.set_instance_id(instance);
                entry.init_core_nodes(1).set(0, host);
            }
            assert!(
                ParticipantSliceBeginRequest::decode(&encode_message(&message).unwrap()).is_err(),
                "accepted {instances:?} on {host}"
            );
        }
    }

    #[test]
    fn instance_removal_round_trips() {
        let request = ParticipantInstancesRemoveRequest {
            launch_id: "launch-abc123".into(),
            instance_ids: RemovedInstances::try_from(
                ["alpha_backbone_inst", "alpha_commander_inst"]
                    .into_iter()
                    .map(|id| Name::new(id).unwrap())
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
        };
        assert_eq!(
            ParticipantInstancesRemoveRequest::decode(&request.encode().unwrap()).unwrap(),
            request
        );
    }

    #[test]
    fn instance_removal_rejects_missing_identity_and_invalid_targets() {
        for (launch, names) in [
            ("", vec!["alpha_backbone_inst"]),
            ("launch-abc123", vec![]),
            ("launch-abc123", vec!["alpha_inst", "alpha_inst"]),
            ("launch-abc123", vec!["bad/name"]),
        ] {
            let mut message = Builder::new_default();
            let mut request = message
                .init_root::<federation_capnp::participant_instances_remove_request::Builder>(
            );
            request.set_launch_id(launch);
            let mut ids = request.init_instance_ids(names.len() as u32);
            for (index, name) in names.iter().enumerate() {
                ids.set(index as u32, name);
            }
            assert!(
                ParticipantInstancesRemoveRequest::decode(&encode_message(&message).unwrap())
                    .is_err(),
                "accepted launch {launch:?}, instances {names:?}"
            );
        }
    }

    fn pair_commit() -> PairCommitRequest {
        PairCommitRequest {
            pairing_name: "task_delegation".to_owned(),
            pairing_tag: "v1".to_owned(),
            local: ProducerRef::new("cn-robot-7", "reflex_inst"),
            local_link_id: "delegation".to_owned(),
            local_role: "executor".to_owned(),
            peer: ProducerRef::new("cn-atlas", "planner_inst"),
            peer_link_id: "delegation".to_owned(),
            peer_role: "planner".to_owned(),
            peer_cardinality: config::node::Cardinality::ZeroOrMore,
            peer_copy: Some(config::runtime::Name::new("bravo").unwrap()),
        }
    }

    /// Both endpoints name their core node, so a request is readable without
    /// knowing which machine opened it.
    #[test]
    fn pair_commit_round_trips() {
        let request = pair_commit();
        let payload = request.encode().expect("encode");
        let decoded = PairCommitRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
        assert_eq!(decoded.local.core_node, "cn-robot-7");
        assert_eq!(decoded.peer.core_node, "cn-atlas");
    }

    /// Every field addresses a specific slot on a specific machine. A defaulted
    /// one would pair the wrong thing silently, so each is refused at decode
    /// rather than filled in.
    #[test]
    fn pair_commit_decode_rejects_any_empty_address_field() {
        type Blank = fn(&mut PairCommitRequest);
        let fields: [(&str, Blank); 10] = [
            ("pairing_name", |r| r.pairing_name.clear()),
            ("pairing_tag", |r| r.pairing_tag.clear()),
            ("local.core_node", |r| r.local.core_node.clear()),
            ("local.instance_id", |r| r.local.instance_id.clear()),
            ("local_link_id", |r| r.local_link_id.clear()),
            ("local_role", |r| r.local_role.clear()),
            ("peer.core_node", |r| r.peer.core_node.clear()),
            ("peer.instance_id", |r| r.peer.instance_id.clear()),
            ("peer_link_id", |r| r.peer_link_id.clear()),
            ("peer_role", |r| r.peer_role.clear()),
        ];
        for (field, blank) in fields {
            let mut request = pair_commit();
            blank(&mut request);
            let payload = request.encode().expect("encode");
            let error = PairCommitRequest::decode(payload.as_ref())
                .err()
                .unwrap_or_else(|| panic!("a blank `{field}` must be refused"));
            assert!(error.to_string().contains(field), "got: {error}");
        }
    }

    #[test]
    fn release_request_decode_rejects_empty_launch_id() {
        let payload = ParticipantReleaseRequest::new("").encode().expect("encode");
        let error = ParticipantReleaseRequest::decode(payload.as_ref())
            .expect_err("empty launch id must fail");
        assert!(error.to_string().contains("launch_id"), "got: {error}");
    }

    #[test]
    fn relationship_notification_round_trips_every_event() {
        for event in [
            RelationshipEvent::ReachedRunning,
            RelationshipEvent::Stopped,
        ] {
            let notification = RelationshipNotification::new("reflex_inst", "cn-robot-7", event);
            let payload = notification.encode().expect("encode");
            let decoded = RelationshipNotification::decode(payload.as_ref()).expect("decode");
            assert_eq!(decoded, notification);
            assert_eq!(decoded.event, event);
        }
    }

    /// A notification names the instance AND the daemon that owns it: two
    /// daemons can host same-named instances, so the pair is the identity.
    #[test]
    fn relationship_notification_decode_rejects_a_partial_address() {
        for (instance_id, core_node, expected) in [
            ("", "cn-robot-7", "instance_id"),
            ("reflex_inst", "", "core_node"),
        ] {
            let notification =
                RelationshipNotification::new(instance_id, core_node, RelationshipEvent::Stopped);
            let payload = notification.encode().expect("encode");
            let error = RelationshipNotification::decode(payload.as_ref())
                .expect_err("partial address must fail");
            assert!(error.to_string().contains(expected), "got: {error}");
        }
    }

    #[test]
    fn relationship_ack_round_trips() {
        let ack = RelationshipNotificationAck::new();
        let payload = ack.encode().expect("encode");
        assert_eq!(
            RelationshipNotificationAck::decode(payload.as_ref()).expect("decode"),
            ack
        );
    }

    #[test]
    fn decode_rejects_malformed_bytes() {
        assert!(ParticipantReserveRequest::decode(b"not capnp").is_err());
        assert!(ParticipantReserveResponse::decode(b"not capnp").is_err());
        assert!(ParticipantReleaseRequest::decode(b"not capnp").is_err());
        assert!(FederationVerdict::decode(b"not capnp").is_err());
        assert!(RelationshipNotification::decode(b"not capnp").is_err());
        assert!(RelationshipNotificationAck::decode(b"not capnp").is_err());
    }
}
