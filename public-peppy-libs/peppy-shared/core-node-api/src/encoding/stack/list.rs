use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{
    capnp_list_len, decode_message, encode_message, read_core_node_name, read_name, read_name_list,
    read_text_list, required_text, required_text_list, write_text_list,
};
use config::runtime::{CoreNodeName, Name};

/// One copy on the stack: a named instance of an option of a `zero_or_more`
/// axis, where it runs, the instances it minted, and the `axis=option`
/// words its own axes were filled with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CopyInfo {
    pub name: Name,
    pub core_node: CoreNodeName,
    pub instance_ids: Vec<Name>,
    pub selections: Vec<String>,
    pub option: String,
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
    /// The copies the launch recorded on this daemon, each with the option it
    /// copies, its host, its selections and its instances.
    pub copies: Vec<CopyInfo>,
    /// The cooperative shutdown grace this daemon is configured with, which
    /// a destructive request's budget is derived from. An absent field reads
    /// as 0, which a daemon configured to wait for no cleanup also reports.
    pub shutdown_grace_secs: u64,
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
            shutdown_grace_secs: config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS,
            core_node: core_node.into(),
            instance_id: instance_id.into(),
            host_name: host_name.into(),
            launch: None,
            reservation: None,
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

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::stack_list_response::Builder>();
            response.set_graph_json(&self.graph_json);
            response.set_shutdown_grace_secs(self.shutdown_grace_secs);
            response.set_core_node(&self.core_node);
            response.set_instance_id(&self.instance_id);
            response.set_host_name(&self.host_name);
            let mut copies = response
                .reborrow()
                .init_copies(capnp_list_len(self.copies.len(), "copies")?);
            for (index, member) in self.copies.iter().enumerate() {
                let mut wire = copies.reborrow().get(index as u32);
                wire.set_name(member.name.as_str());
                wire.set_option(&member.option);
                wire.set_core_node(member.core_node.as_str());
                write_text_list(
                    wire.reborrow().init_instance_ids(capnp_list_len(
                        member.instance_ids.len(),
                        "instance_ids",
                    )?),
                    &member.instance_ids,
                );
                write_text_list(
                    wire.init_selections(capnp_list_len(member.selections.len(), "selections")?),
                    &member.selections,
                );
            }
            let mut launch = response.reborrow().init_launch();
            match &self.launch {
                Some(identity) => {
                    let mut wire = launch.init_identity();
                    wire.set_launch_id(&identity.launch_id);
                    wire.set_coordinator_core_node(&identity.coordinator_core_node);
                }
                None => launch.set_standalone(()),
            }
            let mut reservation = response.reborrow().init_reservation();
            match &self.reservation {
                Some(identity) => {
                    let mut wire = reservation.init_identity();
                    wire.set_launch_id(&identity.launch_id);
                    wire.set_coordinator_core_node(&identity.coordinator_core_node);
                }
                None => reservation.set_none(()),
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

        Ok(Self {
            graph_json: response.get_graph_json()?.to_str()?.to_owned(),
            shutdown_grace_secs: response.get_shutdown_grace_secs(),
            copies: response
                .get_copies()?
                .iter()
                .map(|member| {
                    Ok(CopyInfo {
                        name: read_name(member.get_name()?.to_str()?, "copies.name")?,
                        core_node: read_core_node_name(
                            member.get_core_node()?.to_str()?,
                            "copies.core_node",
                        )?,
                        instance_ids: read_name_list(
                            member.get_instance_ids()?,
                            "copies.instance_ids",
                        )?,
                        selections: required_text_list(
                            read_text_list(member.get_selections()?)?,
                            "copies.selections",
                        )?,
                        option: required_text(member.get_option()?.to_str()?, "copies.option")?,
                    })
                })
                .collect::<Result<_>>()?,
            core_node: response.get_core_node()?.to_str()?.to_owned(),
            instance_id: response.get_instance_id()?.to_str()?.to_owned(),
            host_name: response.get_host_name()?.to_str()?.to_owned(),
            launch,
            reservation,
        })
    }
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

    #[test]
    fn request_round_trips() {
        let request = StackListRequest::new();
        let payload = request.encode().expect("encode");
        let decoded = StackListRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn request_decode_rejects_legacy_nonempty_struct() {
        // The old schema encoded a removed boolean in a one-data-word root
        // struct. Build both possible framed messages directly so this
        // regression fixture needs no legacy generated API.
        for legacy_flag in [false, true] {
            let mut payload = Vec::new();
            payload.extend_from_slice(&0_u32.to_le_bytes()); // one segment
            payload.extend_from_slice(&2_u32.to_le_bytes()); // two words
            payload.extend_from_slice(&(1_u64 << 32).to_le_bytes()); // struct: data=1, ptrs=0
            payload.extend_from_slice(&u64::from(legacy_flag).to_le_bytes());

            let error = StackListRequest::decode(&payload)
                .expect_err("legacy request shape must not decode as the new empty request");
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
            name: Name::new("alpha").unwrap(),
            option: "openarm_v2".into(),
            core_node: CoreNodeName::new("jetson-1").unwrap(),
            instance_ids: vec![
                Name::new("alpha_backbone_inst").unwrap(),
                Name::new("alpha_commander_inst").unwrap(),
            ],
            selections: vec!["commander=xr_commander".into()],
        });
        assert_eq!(
            StackListResponse::decode(&response.encode().unwrap()).unwrap(),
            response
        );
        // A copy that arrives with a blank name, option, or host, or an
        // instance id that is not a name, is refused at the wire boundary.
        for (name, option, core_node, instance_id) in [
            ("", "openarm_v2", "jetson-1", "alpha_arm_inst"),
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
    /// not" in their discriminants, so the two halves can no longer be sent
    /// apart. What remains decodable-but-wrong is an identity arm with a blank
    /// half, and that is still refused: a launch nobody can name is no better
    /// than no launch at all, whichever field carried it.
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
