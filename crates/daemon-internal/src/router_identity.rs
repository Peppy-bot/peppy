//! The daemon's persisted zenoh router identity, scoped to its workspace.
//!
//! The managed router's config pins a [`RouterId`] (see [`pmi::RouterId`]),
//! which is what lets the platform attribute a transport session in the shared
//! router's session list to a particular site. That only works if the id
//! survives a restart, so it is persisted here rather than minted per boot.
//!
//! # Why the identity is scoped to the workspace, not the machine
//!
//! The record on disk is a `(namespace, router_id)` pair, and a resolved
//! namespace that differs from the stored one mints a fresh id. That falls out
//! of how namespaces already work: a namespace is resolved once per daemon
//! generation and fixed for that generation's whole lifetime, and changing it
//! restarts the daemon into a new generation. A router identity that outlived
//! that boundary would be the only thing on the machine that did.
//!
//! It also removes a class of wrong answer. A machine that logs into a
//! different account without logging out first leaves a registry row behind in
//! its old workspace. If the id persisted across the switch, that abandoned row
//! would keep matching the shared router's live session list and read as a
//! healthy uplink behind a dead daemon, rather than as a machine that left.
//! With a workspace-scoped id the old row's id is simply no longer connected,
//! which is exactly what happened.

use std::path::Path;

use config::namespace::Namespace;
use pmi::RouterId;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::error::{Error, Result};

/// The on-disk record: which workspace this router identity belongs to, and
/// what it is.
///
/// The namespace is stored rather than inferred so a workspace change is
/// detectable from the file alone. Both fields are required: a record that
/// cannot say which workspace it is for cannot be trusted for either answer,
/// and is re-minted (see [`resolve`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredIdentity {
    namespace: String,
    router_id: String,
}

/// This generation's router identity: the stored one when it belongs to
/// `namespace`, otherwise a freshly minted one, persisted before returning.
///
/// A record that is absent, unreadable, unparseable, or holds an id zenoh would
/// not accept is treated the same as one for a different workspace: warn and
/// re-mint. Losing an identity costs one re-registration on the next federation
/// pull, whereas refusing to start would take the daemon down over a file it
/// can simply rewrite.
///
/// A *write* failure is a hard error, and deliberately not symmetric with the
/// read side. Continuing with an id that was never persisted would mint a new
/// one on every boot, which is precisely the anonymity this module exists to
/// remove, and it would do so silently: the daemon would look healthy while its
/// registry row pointed at an identity that no longer connects.
pub(crate) fn resolve(path: &Path, namespace: &Namespace) -> Result<RouterId> {
    if let Some(existing) = stored_for(path, namespace) {
        return Ok(existing);
    }

    let minted = RouterId::generate();
    let record = StoredIdentity {
        namespace: namespace.as_str().to_string(),
        router_id: minted.to_string(),
    };
    let document = serde_json::to_string_pretty(&record).map_err(|e| {
        Error::ExecutionFailed(format!("could not serialize the router identity: {e}"))
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::ExecutionFailed(format!(
                "could not create {} to persist the daemon's router identity: {e}",
                parent.display()
            ))
        })?;
    }
    std::fs::write(path, document).map_err(|e| {
        Error::ExecutionFailed(format!(
            "could not persist the daemon's router identity to {}: {e}. Without a persisted \
             identity the router would take a new one on every restart, so the platform could \
             not tell this site apart from a new machine.",
            path.display()
        ))
    })?;
    info!(
        namespace = %namespace,
        router_id = %minted,
        "minted a new zenoh router identity for this workspace"
    );
    Ok(minted)
}

/// The stored identity when it exists, parses, and belongs to `namespace`.
///
/// Every rejection is logged with its reason, so a re-mint is never silent: it
/// changes how this site appears in the platform's roster until the next
/// federation pull re-registers it.
fn stored_for(path: &Path, namespace: &Namespace) -> Option<RouterId> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        // First boot under this data root is the overwhelmingly common case, so
        // it must not warn.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            warn!(
                path = %path.display(), %error,
                "could not read the stored zenoh router identity; minting a new one"
            );
            return None;
        }
    };

    let stored: StoredIdentity = match serde_json5::from_str(&raw) {
        Ok(stored) => stored,
        Err(error) => {
            warn!(
                path = %path.display(), %error,
                "the stored zenoh router identity is unreadable; minting a new one"
            );
            return None;
        }
    };

    if stored.namespace != namespace.as_str() {
        info!(
            stored_namespace = %stored.namespace,
            namespace = %namespace,
            "the stored zenoh router identity belongs to another workspace; minting a new one"
        );
        return None;
    }

    match RouterId::parse(&stored.router_id) {
        Ok(router_id) => Some(router_id),
        Err(error) => {
            warn!(
                path = %path.display(), %error,
                "the stored zenoh router identity is not a valid zenoh id; minting a new one"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(raw: &str) -> Namespace {
        Namespace::parse(raw).expect("a valid namespace literal")
    }

    const WORKSPACE_A: &str = "550e8400-e29b-41d4-a716-446655440000";
    const WORKSPACE_B: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

    /// The whole point: a restart in the same workspace keeps the identity, so
    /// the platform sees one continuous router rather than a new machine per
    /// boot.
    #[test]
    fn the_identity_survives_a_restart_in_the_same_workspace() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("router_identity.json5");

        let first = resolve(&path, &workspace(WORKSPACE_A)).expect("first resolve");
        let second = resolve(&path, &workspace(WORKSPACE_A)).expect("second resolve");

        assert_eq!(
            first, second,
            "a restart under the same workspace must reuse the persisted identity"
        );
    }

    /// And the other half of the rule: a workspace change re-mints, so an
    /// abandoned registry row in the old workspace stops matching the shared
    /// router's session list instead of reporting a healthy uplink forever.
    #[test]
    fn the_identity_is_reminted_when_the_workspace_changes_and_only_then() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("router_identity.json5");

        let in_a = resolve(&path, &workspace(WORKSPACE_A)).expect("resolve in workspace A");
        let in_b = resolve(&path, &workspace(WORKSPACE_B)).expect("resolve in workspace B");
        assert_ne!(in_a, in_b, "a workspace change must mint a new identity");

        // The new one is now the persisted one, and is itself stable.
        let again_in_b =
            resolve(&path, &workspace(WORKSPACE_B)).expect("re-resolve in workspace B");
        assert_eq!(in_b, again_in_b);

        // Returning to A does not resurrect A's old identity: only one record is
        // kept, so the machine reads as a new arrival there, which it is.
        let back_in_a =
            resolve(&path, &workspace(WORKSPACE_A)).expect("resolve back in workspace A");
        assert_ne!(back_in_a, in_a);
    }

    #[test]
    fn a_logged_out_daemon_gets_a_local_scoped_identity() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("router_identity.json5");

        let local = resolve(&path, &Namespace::local()).expect("resolve while logged out");
        let again = resolve(&path, &Namespace::local()).expect("re-resolve while logged out");
        assert_eq!(local, again);

        // Logging in is a workspace change like any other.
        let logged_in = resolve(&path, &workspace(WORKSPACE_A)).expect("resolve after login");
        assert_ne!(local, logged_in);
    }

    #[test]
    fn an_unreadable_record_is_replaced_rather_than_failing_the_daemon() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("router_identity.json5");

        for corrupt in [
            "not json at all",
            // Parses as JSON but is not a record.
            "{}",
            // A well-formed record holding an id zenoh would reject (uppercase).
            &format!(r#"{{ "namespace": "{WORKSPACE_A}", "router_id": "ABCDEF" }}"#),
        ] {
            std::fs::write(&path, corrupt).expect("write a corrupt record");

            let resolved = resolve(&path, &workspace(WORKSPACE_A))
                .unwrap_or_else(|e| panic!("a corrupt record must be replaced, not fatal: {e}"));
            // And the replacement is persisted, so the next boot is stable again.
            assert_eq!(
                resolved,
                resolve(&path, &workspace(WORKSPACE_A)).expect("re-resolve"),
                "the replacement must be written back"
            );
        }
    }

    #[test]
    fn the_record_is_created_under_a_data_root_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("fresh").join("root").join("id.json5");

        let minted = resolve(&path, &workspace(WORKSPACE_A)).expect("first boot creates the root");
        assert_eq!(
            minted,
            resolve(&path, &workspace(WORKSPACE_A)).expect("re-resolve")
        );
    }
}
