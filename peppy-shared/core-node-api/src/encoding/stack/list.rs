use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{
    capnp_list_len, decode_message, encode_message, read_core_node_name, read_name, read_name_list,
    required_text, write_text_list,
};
use config::runtime::{CopyName, CoreNodeName, Name};
use std::collections::BTreeSet;

use super::words::{AxisSelection, read_axis_selection, read_copy_name, write_axis_selection};

/// One copy on the stack: a named instance of an option of an axis that runs
/// as copies, the copy's machine, which runs its base, the instances it
/// minted, each filled own axis with the machine of the part it selects when
/// that part runs elsewhere, and the members it added to stack instances'
/// set slots.
///
/// A listed copy is its own recipe: `peppy stack join OPTION:NAME@CORE_NODE
/// --with SELECTIONS` adds the same copy on the same machines.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CopyInfo {
    pub name: CopyName,
    pub core_node: CoreNodeName,
    pub instance_ids: Vec<Name>,
    pub selections: Vec<AxisSelection>,
    pub option: String,
    pub set_members: Vec<SetMember>,
}

impl CopyInfo {
    /// Every machine the copy runs on: the copy's machine and the machine of
    /// each part that runs elsewhere.
    pub fn core_nodes(&self) -> BTreeSet<&CoreNodeName> {
        std::iter::once(&self.core_node)
            .chain(
                self.selections
                    .iter()
                    .filter_map(|selection| selection.core_node.as_ref()),
            )
            .collect()
    }
}

/// One member a copy added to a stack instance's set slot: the stack instance,
/// its slot, and the instance that joined the set (`instance` or
/// `instance/link_id`), by the id the stack runs it under.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetMember {
    pub instance_id: Name,
    pub link_id: String,
    pub target: String,
}

/// The change of one copy that holds a daemon's stack: the join of a copy,
/// which `copies` lists only once the join ends, or the removal of a copy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CopyChange {
    pub action: CopyAction,
    pub name: CopyName,
    pub option: String,
}

/// What a [`CopyChange`] does to its copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyAction {
    Join,
    Remove,
}

impl CopyAction {
    /// The action as `robot_membership` names its member: `join` or `remove`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Join => "join",
            Self::Remove => "remove",
        }
    }
}

/// `members` grouped by the slot they were added to, each slot once in the
/// order the copy first added to it, with its targets in the order they were
/// added. The order `stack list` prints and a join or removal delivers by.
pub fn slots_in_first_added_order(members: &[SetMember]) -> Vec<((&Name, &str), Vec<&str>)> {
    let mut by_slot: Vec<((&Name, &str), Vec<&str>)> = Vec::new();
    for member in members {
        let slot = (&member.instance_id, member.link_id.as_str());
        match by_slot.iter_mut().find(|(held, _)| *held == slot) {
            Some((_, targets)) => targets.push(member.target.as_str()),
            None => by_slot.push((slot, vec![member.target.as_str()])),
        }
    }
    by_slot
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackListRequest;

impl StackListRequest {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        builder.init_root::<node_capnp::stack_list_request::Builder>();
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::stack_list_request::Reader>()?;
        let size = request.total_size()?;
        if size.word_count != 0 || size.cap_count != 0 {
            return Err(crate::Error::Decoding(format!(
                "StackListRequest must be an empty struct, got {} words and {} capabilities",
                size.word_count, size.cap_count
            )));
        }
        Ok(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackListResponse {
    pub graph_json: String,
    /// Presence identity of the serving daemon: its core-node name and
    /// daemon-generation instance id, matching its core-node presence token.
    pub core_node: String,
    pub instance_id: String,
    /// Hostname of the machine the serving daemon runs on.
    pub host_name: String,
    /// The launch this daemon's slice belongs to: its id and the coordinator
    /// that drove it. `None` when the stack was not started by a federated
    /// launch.
    ///
    /// Carrying it here is what makes a slice self-describing, and therefore
    /// what lets a coordinator REDISCOVER its participants instead of
    /// remembering them. `stack list` already fans out to every live core
    /// node, so rediscovery is that same fan-out filtered by launch id: a
    /// restarted coordinator finds its own launch again, and `stack reset`
    /// works from any machine in the federation.
    pub launch: Option<LaunchIdentity>,
    /// The reservation currently held over this machine: the launch it guards
    /// and the coordinator driving it. `None` when no launch holds the
    /// machine.
    ///
    /// Distinct from `launch`, which describes the stack the LAST launch left
    /// behind; the reservation describes the launch holding the machine RIGHT
    /// NOW. Carrying it here is what makes a held machine visible to
    /// `stack list` and discoverable by `stack reset --federated`, including
    /// one whose launch died before populating a slice.
    pub reservation: Option<LaunchIdentity>,
    /// The copies on this daemon's stack, whoever added them, each with the
    /// option it copies, its machine, its selections and its instances.
    pub copies: Vec<CopyInfo>,
    /// The cooperative shutdown grace this daemon is configured with, which
    /// a destructive request's budget is derived from. An absent field reads
    /// as 0, which a daemon configured to wait for no cleanup also reports.
    pub shutdown_grace_secs: u64,
    /// The join or the removal of a copy that holds this daemon's stack now.
    /// `None` while no change runs, and while any other change runs.
    pub copy_change: Option<CopyChange>,
    /// The copies the launch of this stack started, each as the launch
    /// started it, in name order. A join or a removal leaves them as they
    /// are; a reset, the next launch and the end of the daemon drop them.
    pub launch_copies: Vec<CopyInfo>,
}

/// Which launch a slice belongs to, and who drove it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaunchIdentity {
    pub launch_id: String,
    pub coordinator_core_node: String,
}

impl LaunchIdentity {
    pub fn new(launch_id: impl Into<String>, coordinator_core_node: impl Into<String>) -> Self {
        Self {
            launch_id: launch_id.into(),
            coordinator_core_node: coordinator_core_node.into(),
        }
    }
}

impl StackListResponse {
    pub fn new(
        graph_json: impl Into<String>,
        core_node: impl Into<String>,
        instance_id: impl Into<String>,
        host_name: impl Into<String>,
    ) -> Self {
        Self {
            graph_json: graph_json.into(),
            copies: Vec::new(),
            launch_copies: Vec::new(),
            shutdown_grace_secs: config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS,
            core_node: core_node.into(),
            instance_id: instance_id.into(),
            host_name: host_name.into(),
            launch: None,
            reservation: None,
            copy_change: None,
        }
    }

    /// Attaches the launch this slice belongs to. A daemon whose stack came
    /// from a federated launch always sets this.
    pub fn with_launch(mut self, launch: LaunchIdentity) -> Self {
        self.launch = Some(launch);
        self
    }

    /// Attaches the reservation currently held over this machine. A daemon
    /// reserved by a federated launch always sets this.
    pub fn with_reservation(mut self, reservation: LaunchIdentity) -> Self {
        self.reservation = Some(reservation);
        self
    }

    /// Attaches the join or the removal of a copy that holds the stack now.
    pub fn with_copy_change(mut self, change: CopyChange) -> Self {
        self.copy_change = Some(change);
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::stack_list_response::Builder>();
            response.set_graph_json(&self.graph_json);
            response.set_shutdown_grace_secs(self.shutdown_grace_secs);
            response.set_core_node(&self.core_node);
            response.set_instance_id(&self.instance_id);
            response.set_host_name(&self.host_name);
            write_copies(
                response
                    .reborrow()
                    .init_copies(capnp_list_len(self.copies.len(), "copies")?),
                &self.copies,
            )?;
            write_copies(
                response
                    .reborrow()
                    .init_launch_copies(capnp_list_len(self.launch_copies.len(), "launch_copies")?),
                &self.launch_copies,
            )?;
            let mut launch = response.reborrow().init_launch();
            match &self.launch {
                Some(identity) => write_identity(launch.init_identity(), identity),
                None => launch.set_standalone(()),
            }
            let mut reservation = response.reborrow().init_reservation();
            match &self.reservation {
                Some(identity) => write_identity(reservation.init_identity(), identity),
                None => reservation.set_none(()),
            }
            let mut copy_change = response.reborrow().init_copy_change();
            match &self.copy_change {
                Some(change) => {
                    let mut wire = match change.action {
                        CopyAction::Join => copy_change.init_join(),
                        CopyAction::Remove => copy_change.init_remove(),
                    };
                    wire.set_name(change.name.as_str());
                    wire.set_option(&change.option);
                }
                None => copy_change.set_none(()),
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::stack_list_response::Reader>()?;

        // The unions make "part of a launch or not" and "held by a launch or
        // not" wire facts, so there is no half-formed identity left to police
        // here.
        let launch = {
            use node_capnp::stack_list_response::launch::Which;
            match response.get_launch().which()? {
                Which::Standalone(()) => None,
                Which::Identity(identity) => Some(decode_identity(identity?, "launch")?),
            }
        };
        let reservation = {
            use node_capnp::stack_list_response::reservation::Which;
            match response.get_reservation().which()? {
                Which::None(()) => None,
                Which::Identity(identity) => Some(decode_identity(identity?, "reservation")?),
            }
        };
        let copy_change = {
            use node_capnp::stack_list_response::copy_change::Which;
            match response.get_copy_change().which()? {
                Which::None(()) => None,
                Which::Join(copy) => Some(decode_copy_change(CopyAction::Join, copy?)?),
                Which::Remove(copy) => Some(decode_copy_change(CopyAction::Remove, copy?)?),
            }
        };

        Ok(Self {
            graph_json: response.get_graph_json()?.to_str()?.to_owned(),
            shutdown_grace_secs: response.get_shutdown_grace_secs(),
            copies: decode_copies(response.get_copies()?, "copies")?,
            launch_copies: decode_copies(response.get_launch_copies()?, "launch_copies")?,
            core_node: response.get_core_node()?.to_str()?.to_owned(),
            instance_id: response.get_instance_id()?.to_str()?.to_owned(),
            host_name: response.get_host_name()?.to_str()?.to_owned(),
            launch,
            reservation,
            copy_change,
        })
    }
}

/// Writes `copies` into `list`, a wire list sized to hold them.
fn write_copies(
    mut list: capnp::struct_list::Builder<'_, node_capnp::copy_info::Owned>,
    copies: &[CopyInfo],
) -> Result<()> {
    for (index, copy) in copies.iter().enumerate() {
        let mut wire = list.reborrow().get(index as u32);
        wire.set_name(copy.name.as_str());
        wire.set_option(&copy.option);
        wire.set_core_node(copy.core_node.as_str());
        write_text_list(
            wire.reborrow()
                .init_instance_ids(capnp_list_len(copy.instance_ids.len(), "instance_ids")?),
            &copy.instance_ids,
        );
        let mut selections = wire
            .reborrow()
            .init_selections(capnp_list_len(copy.selections.len(), "selections")?);
        for (selection_index, selection) in copy.selections.iter().enumerate() {
            write_axis_selection(selections.reborrow().get(selection_index as u32), selection);
        }
        let mut set_members =
            wire.init_set_members(capnp_list_len(copy.set_members.len(), "set_members")?);
        for (entry_index, set_member) in copy.set_members.iter().enumerate() {
            let mut entry = set_members.reborrow().get(entry_index as u32);
            entry.set_instance_id(set_member.instance_id.as_str());
            entry.set_link_id(&set_member.link_id);
            entry.set_target(&set_member.target);
        }
    }
    Ok(())
}

/// Decodes the wire list `field` of copies, refusing a blank or invalid
/// field of any copy, named under `field`.
fn decode_copies(
    list: capnp::struct_list::Reader<'_, node_capnp::copy_info::Owned>,
    field: &str,
) -> Result<Vec<CopyInfo>> {
    list.iter().map(|copy| decode_copy(copy, field)).collect()
}

/// Decodes one wire copy of the list `field`, refusing a blank or invalid
/// field of the copy or of any of its set members.
fn decode_copy(copy: node_capnp::copy_info::Reader<'_>, field: &str) -> Result<CopyInfo> {
    Ok(CopyInfo {
        name: read_copy_name(copy.get_name()?.to_str()?, &format!("{field}.name"))?,
        core_node: read_core_node_name(
            copy.get_core_node()?.to_str()?,
            &format!("{field}.core_node"),
        )?,
        instance_ids: read_name_list(copy.get_instance_ids()?, &format!("{field}.instance_ids"))?,
        selections: copy
            .get_selections()?
            .iter()
            .enumerate()
            .map(|(index, selection)| {
                read_axis_selection(selection, &format!("{field}.selections[{index}]"))
            })
            .collect::<Result<_>>()?,
        option: required_text(copy.get_option()?.to_str()?, &format!("{field}.option"))?,
        set_members: copy
            .get_set_members()?
            .iter()
            .map(|entry| decode_set_member(entry, field))
            .collect::<Result<_>>()?,
    })
}

/// Decodes one member a copy of the list `field` added to a set slot; every
/// field is required.
fn decode_set_member(entry: node_capnp::set_member::Reader<'_>, field: &str) -> Result<SetMember> {
    Ok(SetMember {
        instance_id: read_name(
            entry.get_instance_id()?.to_str()?,
            &format!("{field}.set_members.instance_id"),
        )?,
        link_id: required_text(
            entry.get_link_id()?.to_str()?,
            &format!("{field}.set_members.link_id"),
        )?,
        target: required_text(
            entry.get_target()?.to_str()?,
            &format!("{field}.set_members.target"),
        )?,
    })
}

/// Writes one launch identity, for either union that carries one.
fn write_identity(mut wire: node_capnp::launch_identity::Builder<'_>, identity: &LaunchIdentity) {
    wire.set_launch_id(&identity.launch_id);
    wire.set_coordinator_core_node(&identity.coordinator_core_node);
}

/// Decodes the copy of a wire copy change, refusing a name that is not a
/// name and a blank option.
fn decode_copy_change(
    action: CopyAction,
    copy: node_capnp::changed_copy::Reader<'_>,
) -> Result<CopyChange> {
    Ok(CopyChange {
        action,
        name: read_copy_name(copy.get_name()?.to_str()?, "copy_change.name")?,
        option: required_text(copy.get_option()?.to_str()?, "copy_change.option")?,
    })
}

/// Decodes one wire launch identity, refusing a blank half: a launch nobody
/// can name is no better than no launch at all, whichever field carried it.
fn decode_identity(
    identity: node_capnp::launch_identity::Reader<'_>,
    field: &str,
) -> Result<LaunchIdentity> {
    Ok(LaunchIdentity::new(
        required_text(
            identity.get_launch_id()?.to_str()?,
            &format!("{field}.launch_id"),
        )?,
        required_text(
            identity.get_coordinator_core_node()?.to_str()?,
            &format!("{field}.coordinator_core_node"),
        )?,
    ))
}

impl crate::encoding::Wire for StackListRequest {
    type Root = crate::node_capnp::stack_list_request::Owned;
}

impl crate::encoding::Wire for StackListResponse {
    type Root = crate::node_capnp::stack_list_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::decoding_error;

    /// Interleaved members group under their slot, slots in the order each was
    /// first added to and targets in the order they were added.
    #[test]
    fn set_members_group_by_slot_in_first_added_order() {
        let member = |instance: &str, link_id: &str, target: &str| SetMember {
            instance_id: Name::new(instance).unwrap(),
            link_id: link_id.into(),
            target: target.into(),
        };
        let members = [
            member("panel_inst", "cameras", "alpha_wrist_left"),
            member("monitor_inst", "robots", "alpha_arm_inst"),
            member("panel_inst", "cameras", "alpha_chest"),
        ];
        let grouped: Vec<(&str, &str, Vec<&str>)> = slots_in_first_added_order(&members)
            .into_iter()
            .map(|((instance, link_id), targets)| (instance.as_str(), link_id, targets))
            .collect();
        assert_eq!(
            grouped,
            [
                (
                    "panel_inst",
                    "cameras",
                    vec!["alpha_wrist_left", "alpha_chest"]
                ),
                ("monitor_inst", "robots", vec!["alpha_arm_inst"]),
            ]
        );
    }

    #[test]
    fn request_round_trips() {
        let request = StackListRequest::new();
        let payload = request.encode().expect("encode");
        let decoded = StackListRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn request_decode_rejects_a_struct_that_carries_data() {
        // A request is an empty struct, so a root struct with one data word
        // is refused whatever the word holds. The schema has no field to
        // set, so both framed messages are built by hand.
        for flag in [false, true] {
            let mut payload = Vec::new();
            payload.extend_from_slice(&0_u32.to_le_bytes()); // one segment
            payload.extend_from_slice(&2_u32.to_le_bytes()); // two words
            payload.extend_from_slice(&(1_u64 << 32).to_le_bytes()); // struct: data=1, ptrs=0
            payload.extend_from_slice(&u64::from(flag).to_le_bytes());

            let error = StackListRequest::decode(&payload)
                .expect_err("a request that carries data must not decode");
            assert!(
                error.to_string().contains("must be an empty struct"),
                "got: {error}"
            );
        }
    }

    #[test]
    fn request_decode_rejects_malformed() {
        assert!(StackListRequest::decode(b"not capnp").is_err());
    }

    #[test]
    fn response_round_trips_graph_json_and_daemon_identity() {
        let response = StackListResponse::new(
            r#"{"nodes":["a","b"],"edges":[["a","b"]]}"#,
            "core_a",
            "generation_1",
            "robo-a",
        );
        let payload = response.encode().expect("encode");
        let decoded = StackListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert_eq!(
            decoded.graph_json,
            r#"{"nodes":["a","b"],"edges":[["a","b"]]}"#
        );
        assert_eq!(decoded.core_node, "core_a");
        assert_eq!(decoded.instance_id, "generation_1");
        assert_eq!(decoded.host_name, "robo-a");
        assert_eq!(decoded.launch, None, "a local stack belongs to no launch");
    }

    #[test]
    fn response_preserves_copies_and_requires_their_identity() {
        let mut response = StackListResponse::new("{}", "cn-atlas", "generation_1", "atlas");
        response.copies.push(CopyInfo {
            name: CopyName::new("alpha").unwrap(),
            option: "openarm_v2".into(),
            core_node: CoreNodeName::new("jetson-1").unwrap(),
            instance_ids: vec![
                Name::new("alpha_backbone_inst").unwrap(),
                Name::new("alpha_commander_inst").unwrap(),
                Name::new("alpha_brain_inst").unwrap(),
            ],
            selections: vec![
                "commander=xr_commander".parse().unwrap(),
                "brain=ai_brain_vla@cn-gpu-box".parse().unwrap(),
            ],
            set_members: vec![SetMember {
                instance_id: Name::new("monitor_inst").unwrap(),
                link_id: "robots".into(),
                target: "alpha_backbone_inst".into(),
            }],
        });
        assert_eq!(
            StackListResponse::decode(&response.encode().unwrap()).unwrap(),
            response
        );
        let machines: Vec<&str> = response.copies[0]
            .core_nodes()
            .into_iter()
            .map(CoreNodeName::as_str)
            .collect();
        assert_eq!(machines, ["cn-gpu-box", "jetson-1"]);
        // A copy that arrives with a blank name, option, or machine, a copy
        // named `self`, or an instance id that is not a name, is refused at
        // the wire boundary.
        for (name, option, core_node, instance_id) in [
            ("", "openarm_v2", "jetson-1", "alpha_arm_inst"),
            ("self", "openarm_v2", "jetson-1", "alpha_arm_inst"),
            ("alpha", "", "jetson-1", "alpha_arm_inst"),
            ("alpha", "openarm_v2", "", "alpha_arm_inst"),
            ("alpha", "openarm_v2", "jetson-1", "bad/id"),
        ] {
            let mut builder = Builder::new_default();
            let mut wire = builder.init_root::<node_capnp::stack_list_response::Builder>();
            let mut member = wire.reborrow().init_copies(1).get(0);
            member.set_name(name);
            member.set_option(option);
            member.set_core_node(core_node);
            member.init_instance_ids(1).set(0, instance_id);
            let payload = encode_message(&builder).unwrap();
            assert!(
                StackListResponse::decode(&payload).is_err(),
                "{name} {option} {core_node} {instance_id}"
            );
        }
    }

    /// The launch copies travel beside the copies, as given, and a launch
    /// copy with a blank field is refused at the wire boundary under
    /// `launch_copies`.
    #[test]
    fn response_round_trips_the_launch_copies_apart_from_the_copies() {
        let copy = |name: &str, machine: &str, selections: &[&str]| CopyInfo {
            name: CopyName::new(name).unwrap(),
            option: "openarm_v2".into(),
            core_node: CoreNodeName::new(machine).unwrap(),
            instance_ids: vec![Name::new(format!("{name}_arm_inst")).unwrap()],
            selections: selections
                .iter()
                .map(|selection| selection.parse().unwrap())
                .collect(),
            set_members: Vec::new(),
        };
        let mut response = StackListResponse::new("{}", "cn-atlas", "generation_1", "atlas");
        response.copies = vec![copy(
            "alpha",
            "jetson-1",
            &["brain=ai_brain_vla@cn-gpu-box"],
        )];
        response.launch_copies = vec![
            copy("bravo", "jetson-2", &[]),
            copy("alpha", "jetson-1", &["brain=ai_brain_vla@cn-gpu-box"]),
        ];
        let decoded = StackListResponse::decode(&response.encode().unwrap()).unwrap();
        assert_eq!(decoded, response);

        let mut builder = Builder::new_default();
        let mut wire = builder.init_root::<node_capnp::stack_list_response::Builder>();
        let mut member = wire.reborrow().init_launch_copies(1).get(0);
        member.set_name("");
        member.set_option("openarm_v2");
        member.set_core_node("jetson-1");
        let error = StackListResponse::decode(&encode_message(&builder).unwrap())
            .expect_err("a blank launch copy name must fail");
        assert!(
            error.to_string().contains("launch_copies.name"),
            "got: {error}"
        );
    }

    /// A set member with a blank or invalid field, or a selection whose axis
    /// is not a name or whose machine is `self`, is refused at the wire
    /// boundary with the field that carried it.
    #[test]
    fn response_decode_refuses_a_bad_set_member_field_or_selection() {
        for ((axis, machine), (instance_id, link_id, target), expected) in [
            (
                ("commander", None),
                ("bad/id", "robots", "alpha_arm_inst"),
                "copies.set_members.instance_id",
            ),
            (
                ("commander", None),
                ("monitor_inst", "", "alpha_arm_inst"),
                "copies.set_members.link_id",
            ),
            (
                ("commander", None),
                ("monitor_inst", "robots", ""),
                "copies.set_members.target",
            ),
            (
                ("", None),
                ("monitor_inst", "robots", "alpha_arm_inst"),
                "copies.selections[0].axis",
            ),
            (
                ("commander", Some("self")),
                ("monitor_inst", "robots", "alpha_arm_inst"),
                "copies.selections[0].core_node.named",
            ),
        ] {
            let mut builder = Builder::new_default();
            let wire = builder.init_root::<node_capnp::stack_list_response::Builder>();
            let mut copy = wire.init_copies(1).get(0);
            copy.set_name("alpha");
            copy.set_option("openarm_v2");
            copy.set_core_node("jetson-1");
            copy.reborrow()
                .init_instance_ids(1)
                .set(0, "alpha_arm_inst");
            let mut selection = copy.reborrow().init_selections(1).get(0);
            selection.set_axis(axis);
            selection.set_option("xr_commander");
            match machine {
                Some(machine) => selection.init_core_node().set_named(machine),
                None => selection.init_core_node().set_with_copy(()),
            }
            let mut member = copy.init_set_members(1).get(0);
            member.set_instance_id(instance_id);
            member.set_link_id(link_id);
            member.set_target(target);

            let refusal = decoding_error(StackListResponse::decode(
                &encode_message(&builder).unwrap(),
            ));
            assert!(refusal.contains(expected), "{refusal}");
        }
    }

    /// A slice is self-describing: it names the launch it belongs to and the
    /// coordinator that drove it, which is what lets a restarted coordinator
    /// rediscover its participants rather than lose them.
    #[test]
    fn response_round_trips_launch_identity() {
        let response = StackListResponse::new("{}", "cn-atlas-h100", "generation_1", "atlas")
            .with_launch(LaunchIdentity::new("launch-abc123", "cn-robot-7"));
        let payload = response.encode().expect("encode");
        let decoded = StackListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        let launch = decoded.launch.expect("launch identity must survive");
        assert_eq!(launch.launch_id, "launch-abc123");
        assert_eq!(launch.coordinator_core_node, "cn-robot-7");
    }

    /// A held machine is self-describing the same way a slice is: the
    /// reservation names the launch holding it and the coordinator driving
    /// it, which is what lets `stack list` show a wedged machine and
    /// `stack reset --federated` discover one whose launch never populated a
    /// slice.
    #[test]
    fn response_round_trips_reservation_identity() {
        let response = StackListResponse::new("{}", "cn-atlas-h100", "generation_1", "atlas")
            .with_reservation(LaunchIdentity::new("launch-abc123", "cn-robot-7"));
        let payload = response.encode().expect("encode");
        let decoded = StackListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        let reservation = decoded.reservation.expect("reservation must survive");
        assert_eq!(reservation.launch_id, "launch-abc123");
        assert_eq!(reservation.coordinator_core_node, "cn-robot-7");
        assert_eq!(decoded.launch, None, "slice and reservation travel apart");
    }

    /// The unions carry "part of a launch or not" and "held by a launch or
    /// not" in their discriminants, so the two halves cannot be sent apart.
    /// What remains decodable-but-wrong is an identity arm with a blank half,
    /// and that is still refused: a launch nobody can name is no better than
    /// no launch at all, whichever field carried it.
    #[test]
    fn response_decode_rejects_a_blank_half_of_the_identity() {
        for (launch_id, coordinator, expected) in [
            ("launch-abc123", "", "coordinator_core_node"),
            ("", "cn-robot-7", "launch_id"),
        ] {
            let launch = StackListResponse::new("{}", "cn-atlas", "gen_1", "atlas")
                .with_launch(LaunchIdentity::new(launch_id, coordinator));
            let reservation = StackListResponse::new("{}", "cn-atlas", "gen_1", "atlas")
                .with_reservation(LaunchIdentity::new(launch_id, coordinator));
            for response in [launch, reservation] {
                let payload = response.encode().expect("encode");
                let error =
                    StackListResponse::decode(payload.as_ref()).expect_err("blank half must fail");
                assert!(error.to_string().contains(expected), "got: {error}");
            }
        }
    }

    /// The join or the removal that holds the stack travels with its action,
    /// and a stack that no copy change holds reports none.
    #[test]
    fn response_round_trips_the_copy_change() {
        let idle = StackListResponse::new("{}", "cn-atlas", "gen_1", "atlas");
        let decoded = StackListResponse::decode(&idle.encode().unwrap()).unwrap();
        assert_eq!(decoded.copy_change, None);
        for action in [CopyAction::Join, CopyAction::Remove] {
            let response = StackListResponse::new("{}", "cn-atlas", "gen_1", "atlas")
                .with_copy_change(CopyChange {
                    action,
                    name: CopyName::new("bravo").unwrap(),
                    option: "so101_sim".into(),
                });
            let decoded = StackListResponse::decode(&response.encode().unwrap()).unwrap();
            assert_eq!(decoded, response);
        }
    }

    /// A copy change that names no copy, or no option, is refused at the wire
    /// boundary.
    #[test]
    fn response_decode_rejects_a_copy_change_without_its_copy() {
        for (name, option, expected) in [
            ("", "so101_sim", "copy_change.name"),
            ("bad/name", "so101_sim", "copy_change.name"),
            ("bravo", "", "copy_change.option"),
        ] {
            let mut builder = Builder::new_default();
            let wire = builder.init_root::<node_capnp::stack_list_response::Builder>();
            let mut copy = wire.init_copy_change().init_join();
            copy.set_name(name);
            copy.set_option(option);
            let payload = encode_message(&builder).unwrap();
            let error = StackListResponse::decode(&payload).expect_err("a blank copy must fail");
            assert!(error.to_string().contains(expected), "got: {error}");
        }
    }

    /// A standalone slice must decode as one even though `LaunchIdentity` is a
    /// pointer field: the discriminant, not an empty string, is what says so.
    #[test]
    fn standalone_is_the_discriminant_not_an_empty_identity() {
        let payload = StackListResponse::new("{}", "cn-solo", "gen_1", "solo")
            .encode()
            .expect("encode");
        let decoded = StackListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.launch, None);
    }

    #[test]
    fn response_round_trips_empty_fields() {
        let response = StackListResponse::new("", "", "", "");
        let payload = response.encode().expect("encode");
        let decoded = StackListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.graph_json.is_empty());
        assert!(decoded.core_node.is_empty());
        assert!(decoded.instance_id.is_empty());
        assert!(decoded.host_name.is_empty());
    }

    #[test]
    fn response_decode_rejects_malformed() {
        assert!(StackListResponse::decode(b"not capnp").is_err());
    }

    #[test]
    fn round_trip_preserves_the_daemons_shutdown_grace() {
        let mut original = StackListResponse::new("{}", "robot", "root", "host");
        original.shutdown_grace_secs = 3600;
        let decoded = StackListResponse::decode(&original.encode().unwrap()).unwrap();
        assert_eq!(decoded, original);
    }

    /// The configured grace travels verbatim, 0 included: a daemon that waits
    /// for no cleanup is a daemon the config model admits.
    #[test]
    fn a_zero_shutdown_grace_decodes_as_zero() {
        let mut original = StackListResponse::new("{}", "robot", "root", "host");
        original.shutdown_grace_secs = 0;
        let decoded = StackListResponse::decode(&original.encode().unwrap()).unwrap();
        assert_eq!(decoded.shutdown_grace_secs, 0);
        assert_eq!(decoded, original);
    }
}
