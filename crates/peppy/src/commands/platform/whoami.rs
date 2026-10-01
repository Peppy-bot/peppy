//! `peppy platform whoami`: resolve the cached session, call `GET /me`, and
//! print the identity, backend, and token validity. `--json` emits a
//! machine-readable object (never including raw tokens).

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::PlatformSession;
use crate::context::AppContext;
use crate::error::Result;
use auth::client::Principal;
use auth::{profile, storage};

pub struct WhoamiCommand {
    pub api_url: Option<String>,
    /// Emit machine-readable JSON instead of human text.
    pub json: bool,
    /// Test seam: override the peppy data dirs (the credentials file and
    /// `peppy_config.json5` both derive from it).
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for WhoamiCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let env_name = profile::build_env_name();

        match session.api() {
            Ok(mut api) => {
                let principal = api.get_me()?;
                // Read after the resolution of the credential, which refreshes
                // and persists a token that is about to expire.
                let expires_at = session.cached_session().map(|pc| pc.expires_at);
                if self.json {
                    print_json(env_name, &session.api_url, &principal, expires_at);
                } else {
                    print_human(env_name, &session.api_url, &principal, expires_at);
                }
                Ok(())
            }
            Err(crate::error::Error::AuthEngine(auth::AuthError::NotAuthenticated)) => {
                if self.json {
                    let doc = serde_json::json!({
                        "authenticated": false,
                        "profile": env_name,
                        "api_url": session.api_url,
                    });
                    println!("{doc}");
                } else {
                    println!("Not authenticated ({env_name}). Run `peppy platform login`.");
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

fn token_is_valid(expires_at: Option<i64>) -> bool {
    // A display heuristic for `whoami` output; the authoritative expiry check
    // (with a 30s skew) lives in `ProfileCreds::is_expired` and is used by the
    // resolver to decide when to refresh. A token may read as "valid" here for
    // up to 30s after the resolver would already consider it expired.
    expires_at.is_none_or(|exp| storage::now_unix() < exp)
}

fn print_human(env_name: &str, api_url: &str, p: &Principal, expires_at: Option<i64>) {
    println!("Logged in as {} ({env_name})", p.display_name());
    println!("  subject : {}", p.sub);
    if let Some(kind) = &p.kind {
        println!("  type    : {kind}");
    }
    if let Some(email) = &p.email {
        println!("  email   : {email}");
    }
    if let Some(region) = &p.region {
        println!("  region  : {region}");
    }
    println!("  backend : {api_url}");
    let token = if token_is_valid(expires_at) {
        "valid"
    } else {
        "expired"
    };
    println!("  token   : {token}");
}

fn print_json(env_name: &str, api_url: &str, p: &Principal, expires_at: Option<i64>) {
    let doc = serde_json::json!({
        "authenticated": true,
        "profile": env_name,
        "api_url": api_url,
        "principal": {
            "sub": p.sub,
            "kind": p.kind,
            "username": p.username,
            "email": p.email,
            "region": p.region,
        },
        "token": {
            "valid": token_is_valid(expires_at),
            "expires_at": expires_at,
        },
    });
    println!("{doc}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validity_follows_the_expiry() {
        assert!(token_is_valid(None));
        assert!(token_is_valid(Some(storage::now_unix() + 60)));
        assert!(!token_is_valid(Some(storage::now_unix() - 60)));
    }
}
