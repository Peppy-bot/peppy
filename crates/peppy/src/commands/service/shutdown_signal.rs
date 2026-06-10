//! Shared catchable-shutdown signal for the serve daemon.
//!
//! `tokio::signal::ctrl_c()` only catches SIGINT, but the daemon is meant to run
//! under systemd, which stops a unit with SIGTERM. [`shutdown_signal`] resolves
//! on the first of either, so the serve loop, the core-node runner, and the
//! messaging router all run the same teardown whether the operator presses
//! ctrl+C or runs `systemctl stop`.

/// Resolves on the first catchable shutdown signal: SIGINT (ctrl+C) on every
/// platform, plus SIGTERM (systemd stop) on unix. Returns `Err` only if
/// installing the OS signal handler fails.
pub async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}
