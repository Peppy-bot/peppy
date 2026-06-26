//! Federates the daemon's local zenohd router to the caller's *per-user cloud
//! router* and keeps it federated for the daemon's lifetime.
//!
//! The local router is always started *standalone* by the builder; resolving the
//! cloud-router endpoint needs a backend round-trip, which is deliberately kept
//! off the synchronous startup path (a slow/unreachable backend must never stall
//! daemon startup). This task owns the whole federation lifecycle instead:
//!
//! * **Initial federation.** Once the router is up (it waits on `messaging_ready`
//!   so it cannot race [`MessagingRouter`](super::messaging_router)'s own
//!   `start_router`), the first poll resolves the upstream and federates the
//!   local router to it — so a logged-in user is federated shortly after startup,
//!   without having blocked it.
//! * **Keepalive.** The cloud router is idle-reaped by the backend unless its
//!   config is re-pulled within the idle window. Re-resolving here re-pulls when
//!   the cached config goes stale, which refreshes the server-side `last_seen_at`
//!   (and re-provisions a router that was already reaped).
//! * **Live (re)federation.** When the resolved upstream changes — the user logs
//!   in, logs out, or the endpoint moves — the local router's zenohd config is
//!   re-rendered and the router restarted, so the change takes effect without a
//!   full daemon restart.

use super::serve::{ServeAsyncCommand, ServeAsyncHandle};
use crate::error::{Error, Result};
use pmi::{Messenger, MessengerBackend};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, watch};
use tracing::{info, warn};

/// How often the manager re-resolves the caller's cloud-router config. Frequent
/// enough to (de)federate within ~a poll of a login/logout; the config-pull
/// keepalive is itself cache-gated, so a steady state mostly hits the local
/// cache and does no network I/O.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Background task (a [`ServeAsyncCommand`]) that federates the local router to
/// the per-user cloud router and keeps it federated. See the module docs.
pub(crate) struct RouterFederation {
    messenger: Arc<Mutex<Messenger>>,
    api_url: String,
    /// Goes `true` once the router process is up (MessagingRouter ran
    /// `start_router` + `start_session`). The task waits on this before touching
    /// the router so its initial federation cannot race the router's own startup.
    messaging_ready: watch::Receiver<bool>,
}

impl RouterFederation {
    pub(crate) fn new(
        messenger: Arc<Mutex<Messenger>>,
        api_url: String,
        messaging_ready: watch::Receiver<bool>,
    ) -> Self {
        Self {
            messenger,
            api_url,
            messaging_ready,
        }
    }
}

impl ServeAsyncCommand for RouterFederation {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let RouterFederation {
            messenger,
            api_url,
            messaging_ready,
        } = *self;
        let future = Box::pin(async move {
            // Race the maintenance loop against shutdown so the daemon can exit
            // promptly (the loop is otherwise infinite).
            tokio::select! {
                _ = manage_federation(&messenger, &api_url, messaging_ready) => {}
                res = super::shutdown_signal::shutdown_signal() => {
                    res.map_err(|e| Error::ExecutionFailed(
                        format!("router federation: failed to listen for shutdown: {e}")
                    ))?;
                }
            }
            Ok(())
        });
        // No readiness gate: federation is best-effort background maintenance and
        // must not hold up `serve` reporting ready.
        ServeAsyncHandle::new(future, None)
    }
}

/// Waits for the router to come up, then polls the resolved upstream and applies
/// any change to the local router. Loops for the daemon's lifetime (the caller
/// races it against the shutdown signal).
async fn manage_federation(
    messenger: &Arc<Mutex<Messenger>>,
    api_url: &str,
    mut messaging_ready: watch::Receiver<bool>,
) {
    // Don't touch the router until it is actually up, or the initial federation
    // could race MessagingRouter's `start_router`/`start_session`. If the sender
    // drops first (the router task never started or already exited) there is
    // nothing to federate, so stop.
    if !wait_until_ready(&mut messaging_ready).await {
        return;
    }

    // The router was started standalone, so nothing is federated yet; the first
    // poll runs immediately (no leading sleep) so a logged-in user is federated
    // shortly after startup rather than after a full poll interval.
    let mut applied: Option<String> = None;
    loop {
        poll_and_apply(messenger, api_url, &mut applied).await;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Waits until `messaging_ready` is `true`. Returns `false` if the sender is
/// dropped before that (the router task never started / already exited), in which
/// case there is nothing to federate.
async fn wait_until_ready(ready: &mut watch::Receiver<bool>) -> bool {
    while !*ready.borrow() {
        if ready.changed().await.is_err() {
            return false;
        }
    }
    true
}

/// One poll: resolve the desired upstream and, if it changed, (re)federate the
/// local router. Updates `*applied` to the upstream now in effect.
async fn poll_and_apply(
    messenger: &Arc<Mutex<Messenger>>,
    api_url: &str,
    applied: &mut Option<String>,
) {
    // `resolve_federation_target` is blocking (HTTP + file I/O); keep it off the
    // async worker. It also performs the cloud router's keepalive re-pull when its
    // cached config has gone stale.
    let url = api_url.to_string();
    let resolved = match tokio::task::spawn_blocking(move || {
        crate::auth::router::resolve_federation_target(&url)
    })
    .await
    {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "router federation: resolve task panicked; will retry");
            return;
        }
    };

    let desired = resolved.as_ref().map(|(ep, _)| ep.clone());
    if desired == *applied {
        // Steady state (including the cache-gated keepalive re-pull): the upstream
        // is unchanged, so there is nothing to re-render or restart.
        return;
    }

    match refederate_and_restart(messenger, &resolved).await {
        Ok(true) => {
            match &desired {
                Some(ep) => {
                    info!(upstream = %ep, "router federation: (re)federated local router to cloud router")
                }
                None => {
                    info!("router federation: de-federated local router (logged out / no upstream)")
                }
            }
            *applied = desired;
        }
        Ok(false) => {
            // An operator-pinned `ZENOH_CONFIG` owns the router config, so the
            // desired change cannot be applied here. Advance `applied` so this is
            // noted once per change (login/logout) rather than every poll, but warn
            // so the operator knows federation is not being auto-managed.
            warn!(
                "router federation: ZENOH_CONFIG pins the router config; the desired \
                 federation change was not applied (the operator owns this router's config)"
            );
            *applied = desired;
        }
        Err(e) => {
            // Leave `applied` unchanged so the next poll retries the apply.
            warn!(error = %e, "router federation: failed to apply upstream change; will retry");
        }
    }
}

/// Re-renders the local router's config with the (possibly empty) upstream and,
/// if the config actually changed, restarts zenohd so it takes effect. Holds the
/// messenger lock across the whole stop/start so it cannot interleave with the
/// watchdog's own restart.
///
/// Returns whether zenohd was restarted: `false` when [`Messenger::refederate`]
/// was a no-op (an operator-pinned `ZENOH_CONFIG`), so a pointless bounce is
/// skipped; `true` when the config was rewritten and the router bounced.
async fn refederate_and_restart(
    messenger: &Arc<Mutex<Messenger>>,
    target: &Option<(String, pmi::TlsConfig)>,
) -> Result<bool> {
    let (connect_endpoints, tls) = match target {
        Some((ep, tls)) => (vec![ep.clone()], Some(tls.clone())),
        None => (Vec::new(), None),
    };
    let mut messenger = messenger.lock().await;
    let rewrote = messenger
        .refederate(connect_endpoints, tls)
        .map_err(Error::PeppyMessagingInterface)?;
    if !rewrote {
        // The config was not rewritten (operator-pinned), so bouncing zenohd would
        // change nothing. Skip the restart and report it.
        return Ok(false);
    }
    // Apply the new config by bouncing zenohd. The daemon's reconnecting session
    // and the nodes re-establish automatically (same path as a watchdog restart).
    messenger
        .stop_router()
        .await
        .map_err(Error::PeppyMessagingInterface)?;
    messenger
        .start_router()
        .await
        .map_err(Error::PeppyMessagingInterface)?;
    Ok(true)
}
