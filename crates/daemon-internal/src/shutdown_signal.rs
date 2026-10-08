//! The catchable shutdown signals of the serve daemon: SIGINT (ctrl+C) and
//! SIGTERM, the signal systemd stops a unit with.
//!
//! An OS signal reaches only the listeners that exist when it arrives. The
//! serve coordinator is the one observer of these signals, and it listens
//! before it starts the serve tasks, so it gets every signal of the run. The
//! serve tasks observe the shared teardown token that the coordinator cancels
//! when the run stops.

use tokio::signal::unix::{Signal, SignalKind, signal};

/// The listeners of the catchable shutdown signals. Each listener exists from
/// [`ShutdownSignal::listen`] on and keeps a signal that arrives until
/// [`ShutdownSignal::recv`] takes it.
pub(crate) struct ShutdownSignal {
    interrupt: Signal,
    terminate: Signal,
}

impl ShutdownSignal {
    /// Starts to listen. Call it inside a Tokio runtime. Returns `Err` only if
    /// installing the OS signal handler fails.
    pub(crate) fn listen() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Resolves on a shutdown signal that arrived after [`Self::listen`] and
    /// that no earlier call took. Cancel-safe: a signal that arrives while no
    /// `recv` future is polled stays for the next call.
    pub(crate) async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}
