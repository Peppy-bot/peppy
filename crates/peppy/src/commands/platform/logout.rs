//! `peppy platform logout`: revoke the session's refresh and access tokens at
//! the issuer and delete the local session and the context. An issuer that is unreachable or
//! refuses the revocation still results in the local session being cleared.
//! The machine's enrollment is untouched: the daemon keeps federating with its
//! certificate. API calls need a new sign-in, and so does the renewal of that
//! certificate.

use std::sync::Arc;

use secrecy::ExposeSecret;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::Result;
use auth::revoke::{self, TokenKind};
use auth::{discovery, http::HttpClient, profile, storage};

pub struct LogoutCommand {
    pub api_url: Option<String>,
    /// Test seam: override the peppy data dirs (the credentials file and
    /// `peppy_config.json5` both derive from it).
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for LogoutCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let super::PlatformSession {
            dirs,
            creds_path,
            http,
            ..
        } = super::PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;

        // Load-resilient: a malformed or version-mismatched file fails to parse
        // with `Error::Auth`; treat it as "already effectively logged out" rather
        // than wedging logout. A default has no session, so the early return
        // below would otherwise leave the bad file on disk; overwrite it with a
        // clean default here so logout actually heals it.
        let mut creds = match storage::load(&creds_path) {
            Ok(creds) => creds,
            Err(auth::AuthError::Auth(_)) => {
                let cleaned = storage::Credentials::default();
                storage::save(&creds_path, &cleaned)?;
                cleaned
            }
            Err(e) => return Err(e.into()),
        };
        let Some(pc) = creds.session.as_ref() else {
            println!("Not logged in ({}).", profile::build_env_name());
            return Ok(());
        };

        revoke_session(&http, pc);

        creds.session = None;
        storage::save(&creds_path, &creds)?;
        // The context names a workspace and a project of this account, so it
        // goes with the session.
        auth::context::remove(&dirs)?;
        println!("Logged out ({}).", profile::build_env_name());

        if let Ok(Some(enrollment)) = auth::enrollment::load(&dirs) {
            println!(
                "This machine stays enrolled in project {} as {}; run `peppy platform unenroll` \
                 to leave it.",
                enrollment.document.project_id, enrollment.document.peer_name
            );
            println!(
                "The daemon cannot renew the peer certificate with no session. The certificate \
                 expires on {}; run `peppy platform login` before then.",
                super::date_of(enrollment.certificate.not_after)
            );
        }
        Ok(())
    }
}

/// Revokes the refresh token, then the access token, at the issuer. Best
/// effort: every failure warns and lets logout clear the local session, so a
/// machine can always sign out even when the issuer is unreachable.
fn revoke_session(http: &HttpClient, pc: &storage::ProfileCreds) {
    let endpoints = match discovery::discover(http, &pc.issuer, profile::build_transport_policy()) {
        Ok(endpoints) => endpoints,
        Err(e) => {
            println!(
                "Warning: could not reach the issuer to revoke the tokens ({e}); clearing local \
                 credentials anyway."
            );
            return;
        }
    };
    let Some(revocation_endpoint) = endpoints.revocation_endpoint.as_deref() else {
        println!(
            "Warning: the issuer advertises no revocation endpoint; the tokens expire on their \
             own. Clearing local credentials anyway."
        );
        return;
    };
    for (kind, token) in [
        (TokenKind::RefreshToken, pc.refresh_token.expose_secret()),
        (TokenKind::AccessToken, pc.access_token.expose_secret()),
    ] {
        if let Err(e) = revoke::revoke_token(http, revocation_endpoint, &pc.client_id, token, kind)
        {
            println!("Warning: {e}; clearing local credentials anyway.");
        }
    }
}
