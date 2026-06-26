//! Keeps the daemon's local zenohd router federated to the caller's *per-user
//! cloud router* for the daemon's lifetime.
//!
//! The initial federation — if the user was already logged in when `serve`
//! started — is set up by the builder, which passes the upstream connect
//! endpoint straight into the local router's config. This task owns the *ongoing*
//! concerns the builder cannot:
//!
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
use tokio::sync::Mutex;
use tracing::{info, warn};

/// How often the manager re-resolves the caller's cloud-router config. Frequent
/// enough to (de)federate within ~a poll of a login/logout; the config-pull
/// keepalive is itself cache-gated, so a steady state mostly hits the local
/// cache and does no network I/O.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Background task (a [`ServeAsyncCommand`]) that sustains and updates the local
/// router's federation to the per-user cloud router. See the module docs.
pub(crate) struct RouterFederation {
    messenger: Arc<Mutex<Messenger>>,
    api_url: String,
    /// The upstream the builder federated to at startup (its baseline), so the
    /// first poll only acts on a *change* rather than re-applying the same link.
    initial_endpoint: Option<String>,
}

impl RouterFederation {
    pub(crate) fn new(
        messenger: Arc<Mutex<Messenger>>,
        api_url: String,
        initial_endpoint: Option<String>,
    ) -> Self {
        Self {
            messenger,
            api_url,
            initial_endpoint,
        }
    }
}

impl ServeAsyncCommand for RouterFederation {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let RouterFederation {
            messenger,
            api_url,
            initial_endpoint,
        } = *self;
        let future = Box::pin(async move {
            // Race the maintenance loop against shutdown so the daemon can exit
            // promptly (the loop is otherwise infinite).
            tokio::select! {
                _ = manage_federation(&messenger, &api_url, initial_endpoint) => {}
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

/// Polls the resolved upstream and applies any change to the local router. Loops
/// for the daemon's lifetime (the caller races it against the shutdown signal).
async fn manage_federation(
    messenger: &Arc<Mutex<Messenger>>,
    api_url: &str,
    mut applied: Option<String>,
) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        // `resolve_federation_target` is blocking (HTTP + file I/O); keep it off
        // the async worker. It also performs the cloud router's keepalive re-pull
        // when its cached config has gone stale.
        let url = api_url.to_string();
        let target = match tokio::task::spawn_blocking(move || {
            crate::auth::router::resolve_federation_target(&url)
        })
        .await
        {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "router federation: resolve task panicked; will retry");
                continue;
            }
        };

        let desired = target.as_ref().map(|(ep, _)| ep.clone());
        if desired == applied {
            // Steady state (including the cache-gated keepalive re-pull): the
            // upstream is unchanged, so there is nothing to re-render or restart.
            continue;
        }

        match refederate_and_restart(messenger, &target).await {
            Ok(()) => {
                match &desired {
                    Some(ep) => {
                        info!(upstream = %ep, "router federation: (re)federated local router to cloud router")
                    }
                    None => {
                        info!(
                            "router federation: de-federated local router (logged out / no upstream)"
                        )
                    }
                }
                applied = desired;
            }
            Err(e) => {
                // Leave `applied` unchanged so the next poll retries the apply.
                warn!(error = %e, "router federation: failed to apply upstream change; will retry");
            }
        }
    }
}

/// Re-renders the local router's config with the (possibly empty) upstream and
/// restarts zenohd so it takes effect. Holds the messenger lock across the whole
/// stop/start so it cannot interleave with the watchdog's own restart.
async fn refederate_and_restart(
    messenger: &Arc<Mutex<Messenger>>,
    target: &Option<(String, pmi::TlsConfig)>,
) -> Result<()> {
    let (connect_endpoints, tls) = match target {
        Some((ep, tls)) => (vec![ep.clone()], Some(tls.clone())),
        None => (Vec::new(), None),
    };
    let mut messenger = messenger.lock().await;
    messenger
        .refederate(connect_endpoints, tls)
        .map_err(Error::PeppyMessagingInterface)?;
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
    Ok(())
}
