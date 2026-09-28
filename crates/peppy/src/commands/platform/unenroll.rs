//! `peppy platform unenroll`: leave the project's cloud router. Removes the
//! peer on the platform (unless `--local-only`), deletes the local enrollment
//! bundle, and pokes the daemon so it restarts standalone under `local`.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::{
    FederationPokeAction, PlatformSession, confirm_restart, external_router_note,
    federation_is_managed, poke_federation_and_report,
};
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::client::{self, PeerRemoval};
use auth::enrollment;

pub struct UnenrollCommand {
    pub api_url: Option<String>,
    /// Only delete the local enrollment; leave the platform's peer alone.
    pub local_only: bool,
    /// Skip the daemon-restart confirmation prompt.
    pub yes: bool,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for UnenrollCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let managed = federation_is_managed(session.daemon_state.as_ref(), &session.config);

        let Some(existing) = enrollment::load(&session.dirs).map_err(Error::AuthEngine)? else {
            println!("This machine is not enrolled.");
            return Ok(());
        };
        let document = &existing.document;

        if managed
            && !confirm_restart(
                ctx,
                self.yes,
                &FederationPokeAction::Unenroll,
                session.daemon_state.as_ref(),
            )?
        {
            println!("Unenrollment aborted.");
            return Ok(());
        }

        if !self.local_only {
            let mut cred = session.credential().map_err(|e| {
                match e {
                Error::AuthEngine(auth::AuthError::NotAuthenticated) => Error::Auth(
                    "not signed in; run `peppy platform login` to remove the peer on the platform, \
                     or pass --local-only to only delete the local enrollment"
                        .to_string(),
                ),
                other => other,
            }
            })?;
            match client::remove_peer(
                &session.http,
                &session.api_url,
                &mut cred,
                &document.workspace_id,
                &document.project_id,
                &document.peer_id,
            )? {
                PeerRemoval::Staged(_) => println!(
                    "Removed peer {} ({}) from project {}; the platform drops it at the router's \
                     next restart.",
                    document.peer_name, document.peer_id, document.project_id
                ),
                PeerRemoval::AlreadyRemoved => println!(
                    "Peer {} ({}) was already removed from project {}.",
                    document.peer_name, document.peer_id, document.project_id
                ),
            }
        }

        enrollment::remove(&session.dirs)?;
        println!("Deleted the local enrollment.");

        if !managed {
            println!("{}", external_router_note(&session.dirs));
            return Ok(());
        }
        poke_federation_and_report(&session.dirs, FederationPokeAction::Unenroll)
    }
}
