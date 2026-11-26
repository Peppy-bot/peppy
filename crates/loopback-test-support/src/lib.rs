//! A loopback port that refuses each connection, for the tests that check
//! what a client does when nothing listens at its endpoint.

use std::net::{Ipv4Addr, SocketAddr};
use tokio::net::TcpSocket;

/// A loopback port that refuses each connection while the value lives.
///
/// A socket binds the port and never listens, so the kernel refuses each
/// connection to the port. The socket does not set `SO_REUSEADDR`, so no
/// other socket can bind the port on the loopback address while the value
/// lives: a bind that names the port fails with `EADDRINUSE`, and the kernel
/// gives the port to no automatic bind (port 0) and to no connection as its
/// source port. A port that a test binds and then frees has no such guard: a
/// test that runs in parallel can listen on it, and a connection to it then
/// gets an answer.
///
/// Keep the value in a named binding (`let closed = ClosedPort::hold();`)
/// until the last connection of the test. A temporary value, as in
/// `ClosedPort::hold().address()`, frees the port at the end of its
/// statement.
#[derive(Debug)]
pub struct ClosedPort {
    socket: TcpSocket,
}

impl ClosedPort {
    /// Binds a free loopback port, and holds it until the value drops.
    #[must_use = "the port is free again when the value drops"]
    pub fn hold() -> Self {
        let socket = TcpSocket::new_v4().expect("a TCP socket opens");
        socket
            .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .expect("a free loopback port binds");
        Self { socket }
    }

    /// `127.0.0.1:<port>`, the address of the held port.
    pub fn address(&self) -> SocketAddr {
        self.socket
            .local_addr()
            .expect("the held port has an address")
    }
}

/// Each test runs no tokio runtime, as the tests that hold a port in a plain
/// `#[test]` do.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn a_closed_port_is_on_the_loopback_address() {
        let closed = ClosedPort::hold();

        assert_eq!(closed.address().ip(), Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn a_closed_port_refuses_a_connection() {
        let closed = ClosedPort::hold();

        let error = TcpStream::connect(closed.address()).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::ConnectionRefused, "{error}");
    }

    /// `TcpListener::bind` of std and of tokio sets `SO_REUSEADDR` on Unix.
    /// The bind fails also with this option.
    #[test]
    fn no_listener_can_bind_a_closed_port() {
        let closed = ClosedPort::hold();

        let error = TcpListener::bind(closed.address()).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::AddrInUse, "{error}");
    }

    /// Linux sees a conflict between the wildcard address and the loopback
    /// address on one port, so a listener on all the addresses of the host
    /// cannot take the port either.
    #[cfg(target_os = "linux")]
    #[test]
    fn no_listener_can_bind_a_closed_port_on_the_wildcard_address() {
        let closed = ClosedPort::hold();

        let error =
            TcpListener::bind((Ipv4Addr::UNSPECIFIED, closed.address().port())).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::AddrInUse, "{error}");
    }
}
