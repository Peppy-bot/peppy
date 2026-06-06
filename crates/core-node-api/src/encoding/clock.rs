//! Cap'n Proto encoding utilities for clock-synchronization messages.
//!
//! See [`clock.capnp`](../../schemas/clock.capnp) for the wire-level NTP-style
//! 4-timestamp exchange.

use std::time::{SystemTime, UNIX_EPOCH};

use capnp::message::Builder;

use crate::clock_capnp;
use crate::{Payload, Result};

use super::{decode_message, encode_message};

/// Wall-clock "now" in nanoseconds since the UNIX epoch — the canonical reader
/// on the publish/poll paths and in tests. Returns an error if the system
/// clock is set before the epoch; saturates to `u64::MAX` if the timestamp
/// would overflow `u64` (post-year-2554, unreachable in practice).
pub fn wall_now_ns() -> Result<u64> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(u64::try_from(nanos).unwrap_or(u64::MAX))
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockOffsetResponse {
    pub offset_ns: i64,
    pub round_trip_delay_ns: u64,
}

impl ClockOffsetResponse {
    pub fn new(offset_ns: i64, round_trip_delay_ns: u64) -> Self {
        Self {
            offset_ns,
            round_trip_delay_ns,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<clock_capnp::clock_offset_response::Builder>();
            response.set_offset_ns(self.offset_ns);
            response.set_round_trip_delay_ns(self.round_trip_delay_ns);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<clock_capnp::clock_offset_response::Reader>()?;
        Ok(Self {
            offset_ns: response.get_offset_ns(),
            round_trip_delay_ns: response.get_round_trip_delay_ns(),
        })
    }
}

/// One-way snapshot tick published on the `clock` topic. Use [`ClockResponse`]
/// (the request/response service) when you need to bound the staleness with an
/// NTP-style round-trip exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockTick {
    pub time: u64,
}

impl ClockTick {
    pub fn new(time: u64) -> Self {
        Self { time }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut tick = builder.init_root::<clock_capnp::clock_tick::Builder>();
            tick.set_time(self.time);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let tick = reader.get_root::<clock_capnp::clock_tick::Reader>()?;
        Ok(Self {
            time: tick.get_time(),
        })
    }
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
        }
    }
}
