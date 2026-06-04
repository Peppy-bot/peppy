use std::fmt;

/// Network protocol for the Zenoh transport endpoint. Needed by the client
/// session config (so it lives under the base `zenoh` feature, not `router`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum ZenohNetProtocol {
    #[default]
    Tcp,
    Udp,
    Quic,
    Ws,
}

impl fmt::Display for ZenohNetProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZenohNetProtocol::Tcp => write!(f, "tcp"),
            ZenohNetProtocol::Udp => write!(f, "udp"),
            ZenohNetProtocol::Quic => write!(f, "quic"),
            ZenohNetProtocol::Ws => write!(f, "ws"),
        }
    }
}

// The external zenohd daemon process supervision (facade, liveness probe,
// router config generation) only compiles when router management is enabled.
#[cfg(feature = "router")]
mod facade;
#[cfg(feature = "router")]
pub use facade::ZenohdFacade;

#[cfg(feature = "router")]
mod health;
#[cfg(feature = "router")]
pub use health::RouterHealthChecker;

#[cfg(feature = "router")]
mod router_config;
#[cfg(feature = "router")]
pub(crate) use router_config::router_config_path;
