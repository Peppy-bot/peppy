//! `peppy platform enroll`: join a project's cloud router. Picks the
//! workspace and project, mints this machine's key pair and signing request,
//! exchanges the request for a signed certificate, writes the enrollment
//! bundle, and pokes the daemon so it restarts under the new identity and
//! verifies the mutual-TLS link. The private key never leaves the machine.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::{
    FederationPokeAction, PlatformSession, confirm_restart, date_of, external_router_note,
    federation_is_managed, poke_federation_and_report, select,
};
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::csr::{self, PeerName};
use auth::enrollment::{self, EnrollmentBundle};
use auth::{client, storage};

pub struct EnrollCommand {
    pub api_url: Option<String>,
    pub workspace: Option<String>,
    pub project: Option<String>,
    /// The peer's name; defaults to this machine's core-node name.
    pub name: Option<String>,
    /// Replace an existing enrollment: enroll anew, then remove the old peer.
    pub replace: bool,
    /// Skip the daemon-restart confirmation prompt.
    pub yes: bool,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for EnrollCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let managed = federation_is_managed(session.daemon_state.as_ref(), &session.config);

        let existing = enrollment::load(&session.dirs).map_err(Error::AuthEngine)?;
        if let Some(existing) = &existing
            && !self.replace
        {
            return Err(Error::ExecutionFailed(format!(
                "this machine is already enrolled in project {} as {} (peer {}); pass --replace \
                 to enroll anew, or run `peppy platform unenroll` first",
                existing.document.project_id,
                existing.document.peer_name,
                existing.document.peer_id
            )));
        }

        let mut cred = session.credential()?;
        let name = peer_name(
            self.name.as_deref(),
            session
                .daemon_state
                .as_ref()
                .map(|s| s.core_node_name.as_str()),
            session.config.core_node_name.as_deref(),
        )?;
        let selection = select::resolve_project(
            &session.http,
            &session.api_url,
            &mut cred,
            self.workspace.as_deref(),
            self.project.as_deref(),
        )?;

        // Confirm before anything is minted, so an aborted enrollment leaves
        // no peer behind on the platform.
        if managed
            && !confirm_restart(
                ctx,
                self.yes,
                &FederationPokeAction::Enroll,
                session.daemon_state.as_ref(),
            )?
        {
            println!("Enrollment aborted.");
            return Ok(());
        }

        let identity = csr::generate_peer_identity(&name)?;
        let enrolled = client::enroll_peer(
            &session.http,
            &session.api_url,
            &mut cred,
            &selection.workspace.id,
            &selection.project.id,
            &identity.csr_pem,
        )?;
        let bundle = EnrollmentBundle::from_platform(
            &session.api_url,
            &selection.workspace.id,
            &selection.project.id,
            enrolled,
            identity.private_key_pem,
            storage::now_unix(),
        )?;
        enrollment::save(&session.dirs, &bundle)?;
        println!(
            "Enrolled {} in project {} ({}) of workspace {}.",
            bundle.document.peer_name,
            selection.project.name,
            selection.project.id,
            selection.workspace.name
        );
        println!(
            "The peer certificate expires on {}; re-run `peppy platform enroll --replace` before then.",
            date_of(bundle.document.certificate_expires_at)
        );

        // The new enrollment is on disk, so the old peer is now surplus on the
        // platform. Best effort: a failure here leaves a stale peer to remove
        // from the web app, never a machine without an enrollment.
        if let Some(old) = existing {
            match client::remove_peer(
                &session.http,
                &session.api_url,
                &mut cred,
                &old.document.workspace_id,
                &old.document.project_id,
                &old.document.peer_id,
            ) {
                Ok(_) => println!(
                    "Removed the previous peer {} ({}); the platform drops it at the router's next restart.",
                    old.document.peer_name, old.document.peer_id
                ),
                Err(e) => println!(
                    "Warning: could not remove the previous peer {} ({e}); remove it in the web app.",
                    old.document.peer_id
                ),
            }
        }

        if !managed {
            println!("{}", external_router_note(&session.dirs));
            return Ok(());
        }
        poke_federation_and_report(&session.dirs, FederationPokeAction::Enroll)
    }
}

/// The name this machine enrolls under: the flag, else the running daemon's
/// core-node name, else the configured one. The daemon derives a
/// machine-specific default that the CLI cannot see, so a machine whose daemon
/// has never run and whose config names nothing must be told the name.
fn peer_name(
    flag: Option<&str>,
    daemon_core_node_name: Option<&str>,
    configured_core_node_name: Option<&str>,
) -> Result<PeerName> {
    let raw = flag
        .or(daemon_core_node_name)
        .or(configured_core_node_name)
        .ok_or_else(|| {
            Error::ExecutionFailed(
                "no name for this machine: start the daemon once, set `core_node_name` in \
                 peppy_config.json5, or pass --name <name>"
                    .to_string(),
            )
        })?;
    PeerName::parse(raw).map_err(Error::AuthEngine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_peer_name_prefers_the_flag_then_the_daemon_then_the_config() {
        assert_eq!(
            peer_name(Some("flag"), Some("cn-daemon"), Some("cn-config"))
                .unwrap()
                .as_str(),
            "flag"
        );
        assert_eq!(
            peer_name(None, Some("cn-daemon"), Some("cn-config"))
                .unwrap()
                .as_str(),
            "cn-daemon"
        );
        assert_eq!(
            peer_name(None, None, Some("cn-config")).unwrap().as_str(),
            "cn-config"
        );
        let err = peer_name(None, None, None).unwrap_err();
        assert!(err.to_string().contains("--name"), "{err}");
        assert!(peer_name(Some(" bad "), None, None).is_err());
    }
}
