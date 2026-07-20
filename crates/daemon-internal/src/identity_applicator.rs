//! Router-side application boundary for platform identities.
//!
//! Certificate enrollment and durable identity storage do not know how Zenoh
//! is configured or supervised. The daemon controller talks through this
//! interface, while the managed implementation owns config rendering, router
//! restart, retained-session reconnection, and real-link verification.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use pmi::{Messenger, MessengerBackend, RouterLinks, UpstreamLink};
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::router_process::RouterProcessRecorder;

/// Retained daemon sessions must observe a managed-router restart before an
/// identity operation can be considered applied.
const SESSION_RECONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) type ApplyFuture = Pin<Box<dyn Future<Output = Result<RouterApplyDisposition>> + Send>>;
pub(crate) type VerifyFuture =
    Pin<Box<dyn Future<Output = std::result::Result<VerifiedLink, String>> + Send>>;

/// Whether Peppy actually changed router state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouterApplyDisposition {
    Applied,
    /// A pinned or external router remains under operator control.
    OperatorManaged,
}

/// Proof that the router reported the configured outbound link established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VerifiedLink;

/// Narrow boundary between identity orchestration and router ownership.
pub(crate) trait IdentityApplicator: Send + Sync {
    fn apply(&self, upstream: Option<UpstreamLink>) -> ApplyFuture;

    fn apply_standalone(&self) -> ApplyFuture {
        self.apply(None)
    }

    /// Last-resort fail-closed action used when a managed router cannot be
    /// reconfigured during logout. Operator-managed routers are never stopped
    /// by Peppy and report [`RouterApplyDisposition::OperatorManaged`].
    fn stop(&self) -> ApplyFuture;

    fn verify(
        &self,
        host: String,
        port: u16,
        tls: pmi::TlsConfig,
        timeout: Duration,
    ) -> VerifyFuture;
}

/// Peppy-owned Zenoh router implementation.
pub(crate) struct ManagedIdentityApplicator {
    messenger: Arc<Mutex<Messenger>>,
    router_process_recorder: Option<RouterProcessRecorder>,
}

impl ManagedIdentityApplicator {
    pub(crate) fn new(
        messenger: Arc<Mutex<Messenger>>,
        router_process_recorder: Option<RouterProcessRecorder>,
    ) -> Self {
        Self {
            messenger,
            router_process_recorder,
        }
    }
}

impl IdentityApplicator for ManagedIdentityApplicator {
    fn apply(&self, upstream: Option<UpstreamLink>) -> ApplyFuture {
        let messenger = Arc::clone(&self.messenger);
        let router_process_recorder = self.router_process_recorder.clone();
        Box::pin(async move {
            let mut messenger = messenger.lock().await;
            let rewrote = messenger
                .refederate(RouterLinks {
                    upstream,
                    tls: None,
                })
                .map_err(Error::PeppyMessagingInterface)?;
            if !rewrote {
                return Ok(RouterApplyDisposition::OperatorManaged);
            }
            messenger
                .restart_router_and_wait_for_session(SESSION_RECONNECT_TIMEOUT)
                .await
                .map_err(Error::PeppyMessagingInterface)?;
            if let Some(recorder) = router_process_recorder
                && let Err(error) = recorder.capture_current()
            {
                let _ = messenger.stop_router().await;
                return Err(error);
            }
            Ok(RouterApplyDisposition::Applied)
        })
    }

    fn verify(
        &self,
        host: String,
        port: u16,
        _tls: pmi::TlsConfig,
        timeout: Duration,
    ) -> VerifyFuture {
        let messenger = Arc::clone(&self.messenger);
        Box::pin(async move {
            let probe = {
                let messenger = messenger.lock().await;
                messenger.router_links_probe()
            }
            .ok_or_else(|| {
                format!(
                    "managed zenohd exposes no configured link to {host}:{port}; federation cannot be verified"
                )
            })?;

            if probe.wait_established(timeout).await {
                Ok(VerifiedLink)
            } else {
                Err(format!(
                    "managed zenohd did not establish its configured link to {host}:{port} within {timeout:?}"
                ))
            }
        })
    }

    fn stop(&self) -> ApplyFuture {
        let messenger = Arc::clone(&self.messenger);
        Box::pin(async move {
            messenger
                .lock()
                .await
                .stop_router()
                .await
                .map_err(Error::PeppyMessagingInterface)?;
            Ok(RouterApplyDisposition::Applied)
        })
    }
}

/// External-router implementation. Peppy may enroll and rotate its local
/// identity, but it never claims to have installed or verified it in the
/// operator's router.
pub(crate) struct OperatorManagedIdentityApplicator;

impl IdentityApplicator for OperatorManagedIdentityApplicator {
    fn apply(&self, _upstream: Option<UpstreamLink>) -> ApplyFuture {
        Box::pin(async { Ok(RouterApplyDisposition::OperatorManaged) })
    }

    fn verify(
        &self,
        _host: String,
        _port: u16,
        _tls: pmi::TlsConfig,
        _timeout: Duration,
    ) -> VerifyFuture {
        Box::pin(async {
            Err(
                "the router is operator-managed; Peppy cannot verify its installed identity"
                    .to_string(),
            )
        })
    }

    fn stop(&self) -> ApplyFuture {
        Box::pin(async { Ok(RouterApplyDisposition::OperatorManaged) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn operator_managed_applicator_never_claims_application_or_verification() {
        let applicator = OperatorManagedIdentityApplicator;
        assert_eq!(
            applicator.apply_standalone().await.unwrap(),
            RouterApplyDisposition::OperatorManaged
        );
        assert!(
            applicator
                .verify(
                    "router.example".into(),
                    7447,
                    pmi::TlsConfig::default(),
                    Duration::from_secs(1),
                )
                .await
                .is_err()
        );
        assert_eq!(
            applicator.stop().await.unwrap(),
            RouterApplyDisposition::OperatorManaged
        );
    }
}
