//! Cap'n Proto encoding utilities for clock-synchronization messages.
//!
//! See [`clock.capnp`](../../schemas/clock.capnp) for the wire-level NTP-style
//! 4-timestamp exchange.

use capnp::message::Builder;

use crate::clock_capnp;
use crate::{Payload, Result};

use super::{capnp_list_len, decode_message, encode_message, required_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockRequest {
    pub client_send_time: u64,
}

impl ClockRequest {
    pub fn new(client_send_time: u64) -> Self {
        Self { client_send_time }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<clock_capnp::clock_request::Builder>();
            request.set_client_send_time(self.client_send_time);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<clock_capnp::clock_request::Reader>()?;
        Ok(Self {
            client_send_time: request.get_client_send_time(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockResponse {
    pub client_send_time: u64,
    pub server_recv_time: u64,
    pub server_send_time: u64,
}

impl ClockResponse {
    pub fn new(client_send_time: u64, server_recv_time: u64, server_send_time: u64) -> Self {
        Self {
            client_send_time,
            server_recv_time,
            server_send_time,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<clock_capnp::clock_response::Builder>();
            response.set_client_send_time(self.client_send_time);
            response.set_server_recv_time(self.server_recv_time);
            response.set_server_send_time(self.server_send_time);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<clock_capnp::clock_response::Reader>()?;
        Ok(Self {
            client_send_time: response.get_client_send_time(),
            server_recv_time: response.get_server_recv_time(),
            server_send_time: response.get_server_send_time(),
        })
    }
}

/// Request to a node's `clock_offset` service. Empty on the wire — the node
/// performs the NTP exchange against the core node on receipt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockOffsetRequest;

impl ClockOffsetRequest {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        builder.init_root::<clock_capnp::clock_offset_request::Builder>();
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<clock_capnp::clock_offset_request::Reader>()?;
        Ok(Self)
    }
}

/// A node's measured clock offset relative to the core node, from an NTP-style
/// exchange. `offset_ns` is signed (`node_local + offset_ns ≈ core_node_time`);
/// `round_trip_delay_ns` is the measured RTT, used to bound the offset's
/// accuracy and self-diagnose low-confidence corrections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockOffsetResponse {
    pub offset_ns: i64,
    pub round_trip_delay_ns: u64,
    /// The clock domain the reporting instance reads, as `name@core_node`, or
    /// `None` for wall time. An offset means nothing without the timeline it
    /// was measured on.
    pub domain: Option<String>,
}

impl ClockOffsetResponse {
    /// An offset measured against the reporting instance's daemon, on wall
    /// time.
    pub fn new(offset_ns: i64, round_trip_delay_ns: u64) -> Self {
        Self {
            offset_ns,
            round_trip_delay_ns,
            domain: None,
        }
    }

    /// The answer from an instance reading a simulated domain: no offset,
    /// because every peer it may hold a clock-dependent connection to reads
    /// that same domain and stamps on the same timeline.
    pub fn in_domain(round_trip_delay_ns: u64, domain: impl Into<String>) -> Self {
        Self {
            offset_ns: 0,
            round_trip_delay_ns,
            domain: Some(domain.into()),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<clock_capnp::clock_offset_response::Builder>();
            response.set_offset_ns(self.offset_ns);
            response.set_round_trip_delay_ns(self.round_trip_delay_ns);
            response.set_domain(self.domain.as_deref().unwrap_or(""));
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<clock_capnp::clock_offset_response::Reader>()?;
        Ok(Self {
            offset_ns: response.get_offset_ns(),
            round_trip_delay_ns: response.get_round_trip_delay_ns(),
            domain: super::optional_text(response.get_domain()?.to_str()?),
        })
    }
}

/// One-way snapshot tick published on the `clock` topic. Use [`ClockResponse`]
/// (the request/response service) when you need to bound the staleness with an
/// NTP-style round-trip exchange.
///
/// A tick never carries `0`: every sim-time cache stores `0` as "no tick
/// observed yet", so a tick that read `0` would be indistinguishable from no
/// tick at all. The type enforces it at both boundaries, on construction and on
/// decode, which is what lets a cache trust any tick it is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockTick {
    time: u64,
}

impl ClockTick {
    /// The smallest time a tick carries, since `0` is the not-ready sentinel.
    pub const MIN_TIME_NS: u64 = 1;

    /// A tick at `time_ns` nanoseconds, clamped up to [`Self::MIN_TIME_NS`].
    pub fn new(time_ns: u64) -> Self {
        Self {
            time: time_ns.max(Self::MIN_TIME_NS),
        }
    }

    /// The instant this tick carries, in nanoseconds on the publishing core
    /// node's timeline: the Unix epoch in wall mode, and whatever origin the
    /// simulator counts from under simulated time. Never `0`.
    pub fn time(&self) -> u64 {
        self.time
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut tick = builder.init_root::<clock_capnp::clock_tick::Builder>();
            tick.set_time(self.time);
        }
        encode_message(&builder)
    }

    /// Decodes a tick, clamping a foreign publisher's literal `0` up to
    /// [`Self::MIN_TIME_NS`] so the invariant holds for bytes off the wire too.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let tick = reader.get_root::<clock_capnp::clock_tick::Reader>()?;
        Ok(Self::new(tick.get_time()))
    }
}

/// One clock domain a daemon hosts, as `peppy clock list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockDomainInfo {
    pub domain: config::runtime::ClockDomainId,
    /// The instance supplying the domain, on `domain.core_node`.
    pub publisher_instance_id: String,
    /// The launch that started the publisher, or `None` when a
    /// `peppy node run` did.
    pub launch: Option<crate::encoding::LaunchIdentity>,
    pub ready: bool,
    /// The last instant seen on the domain, or `None` for never.
    pub last_tick_ns: Option<u64>,
}

/// One instance on the answering daemon that reads a domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockConsumerInfo {
    pub instance_id: String,
    pub domain: config::runtime::ClockDomainId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockListRequest;

impl ClockListRequest {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        builder.init_root::<clock_capnp::clock_list_request::Builder>();
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<clock_capnp::clock_list_request::Reader>()?;
        Ok(Self)
    }
}

/// What one daemon knows about clock domains: the ones it hosts, and the
/// instances on it that read one. A federation's whole picture is these
/// answers from every live daemon, which is how `peppy clock list` builds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockListResponse {
    pub domains: Vec<ClockDomainInfo>,
    pub consumers: Vec<ClockConsumerInfo>,
}

/// Rebuilds a domain identity from its three wire fields, refusing the
/// zero the incarnation never carries.
fn read_domain(
    name: &str,
    core_node: &str,
    incarnation: u64,
) -> Result<config::runtime::ClockDomainId> {
    let decoding = |reason: String| crate::Error::Decoding(reason);
    Ok(config::runtime::ClockDomainId::new(
        config::runtime::Name::new(name).map_err(|e| decoding(e.to_string()))?,
        config::runtime::CoreNodeName::new(core_node).map_err(|e| decoding(e.to_string()))?,
        config::runtime::ClockIncarnation::try_from(incarnation)
            .map_err(|e| decoding(e.to_string()))?,
    ))
}

/// The launch that started a publisher, from the pair of wire fields that
/// carry it. Both empty is the answer for a publisher `peppy node run`
/// started; one alone is a launch nothing can name, whichever field carried
/// it.
fn read_launch(
    launch_id: &str,
    coordinator_core_node: &str,
) -> Result<Option<crate::encoding::LaunchIdentity>> {
    if launch_id.is_empty() && coordinator_core_node.is_empty() {
        return Ok(None);
    }
    Ok(Some(crate::encoding::LaunchIdentity::new(
        required_text(launch_id, "domains.launch_id")?,
        required_text(coordinator_core_node, "domains.coordinator_core_node")?,
    )))
}

impl ClockListResponse {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<clock_capnp::clock_list_response::Builder>();
            {
                let count = capnp_list_len(self.domains.len(), "domains")?;
                let mut domains = response.reborrow().init_domains(count);
                for (index, info) in self.domains.iter().enumerate() {
                    let mut entry = domains.reborrow().get(index as u32);
                    entry.set_name(info.domain.name.as_str());
                    entry.set_core_node(info.domain.core_node.as_str());
                    entry.set_incarnation(info.domain.incarnation.get());
                    entry.set_publisher_instance_id(&info.publisher_instance_id);
                    let (launch_id, coordinator) =
                        info.launch.as_ref().map_or(("", ""), |launch| {
                            (
                                launch.launch_id.as_str(),
                                launch.coordinator_core_node.as_str(),
                            )
                        });
                    entry.set_launch_id(launch_id);
                    entry.set_coordinator_core_node(coordinator);
                    entry.set_ready(info.ready);
                    entry.set_last_tick_ns(info.last_tick_ns.unwrap_or(0));
                }
            }
            let count = capnp_list_len(self.consumers.len(), "consumers")?;
            let mut consumers = response.init_consumers(count);
            for (index, info) in self.consumers.iter().enumerate() {
                let mut entry = consumers.reborrow().get(index as u32);
                entry.set_instance_id(&info.instance_id);
                entry.set_domain_name(info.domain.name.as_str());
                entry.set_domain_core_node(info.domain.core_node.as_str());
                entry.set_incarnation(info.domain.incarnation.get());
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<clock_capnp::clock_list_response::Reader>()?;
        let mut domains = Vec::new();
        for entry in response.get_domains()?.iter() {
            domains.push(ClockDomainInfo {
                domain: read_domain(
                    entry.get_name()?.to_str()?,
                    entry.get_core_node()?.to_str()?,
                    entry.get_incarnation(),
                )?,
                publisher_instance_id: entry.get_publisher_instance_id()?.to_str()?.to_owned(),
                launch: read_launch(
                    entry.get_launch_id()?.to_str()?,
                    entry.get_coordinator_core_node()?.to_str()?,
                )?,
                ready: entry.get_ready(),
                last_tick_ns: Some(entry.get_last_tick_ns()).filter(|tick| *tick != 0),
            });
        }
        let mut consumers = Vec::new();
        for entry in response.get_consumers()?.iter() {
            consumers.push(ClockConsumerInfo {
                instance_id: entry.get_instance_id()?.to_str()?.to_owned(),
                domain: read_domain(
                    entry.get_domain_name()?.to_str()?,
                    entry.get_domain_core_node()?.to_str()?,
                    entry.get_incarnation(),
                )?,
            });
        }
        Ok(Self { domains, consumers })
    }
}

impl crate::encoding::Wire for ClockListRequest {
    type Root = clock_capnp::clock_list_request::Owned;
}

impl crate::encoding::Wire for ClockListResponse {
    type Root = clock_capnp::clock_list_response::Owned;
}

impl crate::encoding::Wire for ClockRequest {
    type Root = crate::clock_capnp::clock_request::Owned;
}

impl crate::encoding::Wire for ClockResponse {
    type Root = crate::clock_capnp::clock_response::Owned;
}

impl crate::encoding::Wire for ClockOffsetRequest {
    type Root = crate::clock_capnp::clock_offset_request::Owned;
}

impl crate::encoding::Wire for ClockOffsetResponse {
    type Root = crate::clock_capnp::clock_offset_response::Owned;
}

impl crate::encoding::Wire for ClockTick {
    type Root = crate::clock_capnp::clock_tick::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_offset_request_roundtrips() {
        let req = ClockOffsetRequest::new();
        let bytes = req.encode().expect("encode");
        assert_eq!(ClockOffsetRequest::decode(&bytes).expect("decode"), req);
    }

    #[test]
    fn clock_offset_response_roundtrips_positive_and_negative() {
        for offset in [0i64, 1_234_567, -987_654] {
            let resp = ClockOffsetResponse::new(offset, 42_000);
            let bytes = resp.encode().expect("encode");
            let decoded = ClockOffsetResponse::decode(&bytes).expect("decode");
            assert_eq!(decoded, resp);
            assert_eq!(decoded.domain, None);
        }
    }

    /// An instance reading a simulated domain reports the domain and no
    /// offset: its peers stamp on that same timeline.
    #[test]
    fn a_simulated_offset_carries_its_domain_and_no_correction() {
        let resp = ClockOffsetResponse::in_domain(42_000, "robot@cn-sim");
        let decoded = ClockOffsetResponse::decode(&resp.encode().expect("encode")).expect("decode");
        assert_eq!(decoded, resp);
        assert_eq!(decoded.offset_ns, 0);
        assert_eq!(decoded.domain.as_deref(), Some("robot@cn-sim"));
    }

    fn robot_domain() -> config::runtime::ClockDomainId {
        config::runtime::ClockDomainId::new(
            config::runtime::Name::new("robot").unwrap(),
            config::runtime::CoreNodeName::new("cn-sim").unwrap(),
            config::runtime::ClockIncarnation::try_from(0x1234).unwrap(),
        )
    }

    /// A listing round-trips, and a domain whose publisher a `node run`
    /// started carries no launch.
    #[test]
    fn clock_listings_round_trip() {
        let domain = robot_domain();
        let response = ClockListResponse {
            domains: vec![ClockDomainInfo {
                domain: domain.clone(),
                publisher_instance_id: "sim_inst".to_owned(),
                launch: Some(crate::encoding::LaunchIdentity::new("launch-1", "cn-sim")),
                ready: true,
                last_tick_ns: Some(42),
            }],
            consumers: vec![ClockConsumerInfo {
                instance_id: "arm_inst".to_owned(),
                domain: domain.clone(),
            }],
        };
        let decoded = ClockListResponse::decode(&response.encode().unwrap()).unwrap();
        assert_eq!(decoded, response);
        assert_eq!(decoded.domains[0].domain.to_string(), "robot@cn-sim");

        let started_by_hand = ClockListResponse {
            domains: vec![ClockDomainInfo {
                domain,
                publisher_instance_id: "sim_inst".to_owned(),
                launch: None,
                ready: false,
                last_tick_ns: None,
            }],
            consumers: Vec::new(),
        };
        let decoded = ClockListResponse::decode(&started_by_hand.encode().unwrap()).unwrap();
        assert_eq!(decoded, started_by_hand);
        assert!(decoded.domains[0].launch.is_none());

        assert_eq!(
            ClockListRequest::decode(&ClockListRequest::new().encode().unwrap()).unwrap(),
            ClockListRequest
        );
    }

    /// A launch and the coordinator that drove it travel as a pair. One alone
    /// names a launch nothing can find, and decoding refuses it by field.
    #[test]
    fn a_half_empty_launch_is_refused_by_field() {
        for (launch_id, coordinator, expected) in [
            ("launch-1", "", "domains.coordinator_core_node"),
            ("", "cn-sim", "domains.launch_id"),
        ] {
            let response = ClockListResponse {
                domains: vec![ClockDomainInfo {
                    domain: robot_domain(),
                    publisher_instance_id: "sim_inst".to_owned(),
                    launch: Some(crate::encoding::LaunchIdentity::new(launch_id, coordinator)),
                    ready: true,
                    last_tick_ns: None,
                }],
                consumers: Vec::new(),
            };
            let error = ClockListResponse::decode(&response.encode().unwrap())
                .expect_err("a half-empty launch must not decode");
            assert!(error.to_string().contains(expected), "got: {error}");
        }
    }

    #[test]
    fn clock_request_roundtrips() {
        let req = ClockRequest::new(1_234_567_890);
        let bytes = req.encode().expect("encode");
        assert_eq!(ClockRequest::decode(&bytes).expect("decode"), req);
    }

    #[test]
    fn clock_response_roundtrips() {
        let resp = ClockResponse::new(111, 222, 333);
        let bytes = resp.encode().expect("encode");
        assert_eq!(ClockResponse::decode(&bytes).expect("decode"), resp);
    }

    #[test]
    fn clock_tick_roundtrips() {
        let tick = ClockTick::new(9_999_999);
        let bytes = tick.encode().expect("encode");
        assert_eq!(ClockTick::decode(&bytes).expect("decode"), tick);
        assert_eq!(tick.time(), 9_999_999);
    }

    /// `0` is every sim-time cache's not-ready sentinel, so a tick cannot carry
    /// it: construction clamps, and so does decoding bytes a publisher outside
    /// this crate built with a literal zero.
    #[test]
    fn clock_tick_never_carries_the_not_ready_sentinel() {
        assert_eq!(ClockTick::new(0).time(), ClockTick::MIN_TIME_NS);

        let mut builder = Builder::new_default();
        builder
            .init_root::<clock_capnp::clock_tick::Builder>()
            .set_time(0);
        let raw_zero = encode_message(&builder).expect("encode");
        assert_eq!(
            ClockTick::decode(&raw_zero).expect("decode").time(),
            ClockTick::MIN_TIME_NS
        );
    }
}
