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
