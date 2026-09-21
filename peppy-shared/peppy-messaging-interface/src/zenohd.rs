use crate::error::{Error, Result};
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

/// Network protocol for the Zenoh transport endpoint. Needed by the client
/// session config (so it lives under the base `zenoh` feature, not `router`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
// Udp/Quic/Ws are constructed only by the router config parser in the
// `router`-gated `facade` module, so a `zenoh`-without-`router` build sees them
// as never-constructed. Scope the allow to that combo so the default (router)
// build still warns if a variant genuinely goes dead.
#[cfg_attr(not(feature = "router"), allow(dead_code))]
pub enum ZenohNetProtocol {
    #[default]
    Tcp,
    Udp,
    Quic,
    Ws,
    /// TLS-over-TCP. The transport is byte-streamed over TCP, so everything that
    /// special-cases `Tcp` for liveness/readiness (a `TcpStream::connect` probe)
    /// must treat `Tls` identically. The cert/key/CA material is carried out of
    /// band in [`crate::TlsConfig`] and rendered into `transport.link.tls`.
    Tls,
}

impl fmt::Display for ZenohNetProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZenohNetProtocol::Tcp => write!(f, "tcp"),
            ZenohNetProtocol::Udp => write!(f, "udp"),
            ZenohNetProtocol::Quic => write!(f, "quic"),
            ZenohNetProtocol::Ws => write!(f, "ws"),
            ZenohNetProtocol::Tls => write!(f, "tls"),
        }
    }
}

/// A parsed Zenoh transport endpoint in `<protocol>/<host>:<port>` form.
///
/// The host is retained exactly as written (including brackets around an IPv6
/// literal), so formatting the value reconstructs a locator that can be passed
/// directly to Zenoh. Parsing is deliberately transport-level only; callers
/// that intend to *dial* an endpoint should additionally use
/// [`validate_external_tcp`](Self::validate_external_tcp), which rejects listen
/// wildcards and non-TCP transports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZenohEndpoint {
    protocol: ZenohNetProtocol,
    host: String,
    port: u16,
}

impl ZenohEndpoint {
    pub(crate) fn new(protocol: ZenohNetProtocol, host: String, port: u16) -> Self {
        Self {
            protocol,
            host,
            port,
        }
    }

    pub fn protocol(&self) -> ZenohNetProtocol {
        self.protocol
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Validates the endpoint used to adopt an already-running router.
    /// External router adoption currently uses a TCP socket probe, so only a
    /// concrete, dialable `tcp/` endpoint is accepted.
    pub fn validate_external_tcp(&self) -> Result<()> {
        if self.protocol != ZenohNetProtocol::Tcp {
            return Err(Error::ConfigurationError(format!(
                "external zenoh router endpoint must use `tcp/`, got `{self}`"
            )));
        }
        if self.port == 0 {
            return Err(Error::ConfigurationError(
                "external zenoh router endpoint must use a non-zero port".to_string(),
            ));
        }

        let unbracketed_host = self
            .host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(&self.host);
        let unspecified_ip = unbracketed_host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_unspecified());
        if unspecified_ip || self.host == "*" {
            return Err(Error::ConfigurationError(format!(
                "external zenoh router endpoint `{self}` uses a listen wildcard; configure a dialable host such as `127.0.0.1` instead"
            )));
        }
        Ok(())
    }
}

impl fmt::Display for ZenohEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}:{}", self.protocol, self.host, self.port)
    }
}

impl FromStr for ZenohEndpoint {
    type Err = Error;

    fn from_str(endpoint: &str) -> Result<Self> {
        if endpoint.trim() != endpoint || endpoint.is_empty() {
            return Err(Error::ConfigurationError(format!(
                "invalid Zenoh endpoint `{endpoint}`: expected `<protocol>/<host>:<port>` without surrounding whitespace"
            )));
        }

        let (protocol, host_port) = endpoint.split_once('/').ok_or_else(|| {
            Error::ConfigurationError(format!(
                "invalid Zenoh endpoint `{endpoint}`: expected `<protocol>/<host>:<port>`"
            ))
        })?;
        let protocol = match protocol {
            "tcp" => ZenohNetProtocol::Tcp,
            "udp" => ZenohNetProtocol::Udp,
            "quic" => ZenohNetProtocol::Quic,
            "ws" => ZenohNetProtocol::Ws,
            "tls" => ZenohNetProtocol::Tls,
            other => {
                return Err(Error::ConfigurationError(format!(
                    "unknown Zenoh endpoint protocol `{other}` in `{endpoint}`"
                )));
            }
        };

        let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
            let (host, port) = bracketed.split_once("]:").ok_or_else(|| {
                Error::ConfigurationError(format!(
                    "invalid Zenoh endpoint `{endpoint}`: bracketed hosts must be followed by `:<port>`"
                ))
            })?;
            host.parse::<std::net::Ipv6Addr>().map_err(|_| {
                Error::ConfigurationError(format!(
                    "invalid Zenoh endpoint `{endpoint}`: brackets are only valid around an IPv6 address"
                ))
            })?;
            (format!("[{host}]"), port)
        } else {
            let (host, port) = host_port.rsplit_once(':').ok_or_else(|| {
                Error::ConfigurationError(format!(
                    "invalid Zenoh endpoint `{endpoint}`: expected `<host>:<port>`"
                ))
            })?;
            if host.contains(':') {
                return Err(Error::ConfigurationError(format!(
                    "invalid Zenoh endpoint `{endpoint}`: IPv6 hosts must be enclosed in brackets"
                )));
            }
            (host.to_string(), port)
        };

        if host.is_empty()
            || host == "[]"
            || host.contains('/')
            || host.chars().any(char::is_whitespace)
        {
            return Err(Error::ConfigurationError(format!(
                "invalid Zenoh endpoint `{endpoint}`: host must not be empty"
            )));
        }
        let port = port.parse::<u16>().map_err(|_| {
            Error::ConfigurationError(format!(
                "invalid Zenoh endpoint `{endpoint}`: port must be an integer from 0 through 65535"
            ))
        })?;

        Ok(Self::new(protocol, host, port))
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn hostname_and_port_roundtrip_without_becoming_a_binary_path() {
        let endpoint: ZenohEndpoint = "tcp/zenoh-router.internal:17448"
            .parse()
            .expect("parse TCP endpoint");

        assert_eq!(endpoint.protocol(), ZenohNetProtocol::Tcp);
        assert_eq!(endpoint.host(), "zenoh-router.internal");
        assert_eq!(endpoint.port(), 17448);
        assert_eq!(endpoint.to_string(), "tcp/zenoh-router.internal:17448");
        endpoint
            .validate_external_tcp()
            .expect("hostname is a dialable external endpoint");
    }

    #[test]
    fn bracketed_ipv6_roundtrips() {
        let endpoint: ZenohEndpoint = "tcp/[::1]:7448".parse().expect("parse IPv6 endpoint");

        assert_eq!(endpoint.host(), "[::1]");
        assert_eq!(endpoint.port(), 7448);
        assert_eq!(endpoint.to_string(), "tcp/[::1]:7448");
        endpoint
            .validate_external_tcp()
            .expect("loopback IPv6 is dialable");
    }

    #[test]
    fn external_tcp_validation_rejects_wildcards_non_tcp_and_port_zero() {
        for endpoint in [
            "tcp/0.0.0.0:7448",
            "tcp/[::]:7448",
            "tls/router.internal:7448",
            "tcp/router.internal:0",
        ] {
            let endpoint: ZenohEndpoint = endpoint.parse().expect("parse transport endpoint");
            assert!(
                endpoint.validate_external_tcp().is_err(),
                "{endpoint} must not be accepted as an external TCP dial endpoint"
            );
        }
    }
}

// The external zenohd daemon process supervision (facade, liveness probe,
// router config generation) only compiles when router management is enabled.
#[cfg(feature = "router")]
mod facade;
#[cfg(feature = "router")]
pub use facade::{ZENOHD_PATH_VAR, ZenohdFacade};

#[cfg(feature = "router")]
mod health;
#[cfg(feature = "router")]
pub use health::RouterHealthChecker;

#[cfg(feature = "router")]
mod links;
#[cfg(feature = "router")]
pub use links::RouterLinksProbe;

#[cfg(feature = "router")]
mod router_config;
#[cfg(feature = "router")]
pub(crate) use router_config::{config_override, render_router_config_to_path, router_config_path};
