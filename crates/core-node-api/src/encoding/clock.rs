//! Cap'n Proto encoding utilities for clock-synchronization messages.
//!
//! See [`clock.capnp`](../../schemas/clock.capnp) for the wire-level NTP-style
//! 4-timestamp exchange. Encoders here MUST emit only [`ClockSource::Wall`];
//! the other variants are reserved.

use capnp::message::Builder;

use crate::clock_capnp;
use crate::{Payload, Result};

use super::{decode_message, encode_message};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockSource {
    Wall,
}

impl ClockSource {
    fn to_capnp(self) -> clock_capnp::ClockSource {
        match self {
            ClockSource::Wall => clock_capnp::ClockSource::Wall,
        }
    }

    fn from_capnp(value: clock_capnp::ClockSource) -> Self {
        match value {
            clock_capnp::ClockSource::Wall => ClockSource::Wall,
        }
    }
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
    pub clock_source: ClockSource,
}

impl ClockResponse {
    pub fn new(
        client_send_time: u64,
        server_recv_time: u64,
        server_send_time: u64,
        clock_source: ClockSource,
    ) -> Self {
        Self {
            client_send_time,
            server_recv_time,
            server_send_time,
            clock_source,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<clock_capnp::clock_response::Builder>();
            response.set_client_send_time(self.client_send_time);
            response.set_server_recv_time(self.server_recv_time);
            response.set_server_send_time(self.server_send_time);
            response.set_clock_source(self.clock_source.to_capnp());
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
            clock_source: ClockSource::from_capnp(response.get_clock_source()?),
        })
    }
}
