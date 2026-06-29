//! `peppy auth whoami` (alias `status`): resolve the cached credential, call
//! `GET /me`, and print the identity, backend, and token validity. `--json`
//! emits a machine-readable object (never including raw tokens).

use std::path::Path;
use std::sync::Arc;

use config::consts::PeppyDirs;

use crate::auth::client::Principal;
use crate::auth::{client, http::HttpClient, profile, resolver, storage};
use crate::commands::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

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
        let dirs = self.peppy_dirs.unwrap_or_default();
        let config = config::peppy_config::load_or_create(&dirs).map_err(Error::PeppyConfig)?;
        let api_url = profile::resolve_api_url(self.api_url.as_deref(), &config.resource_servers)?;
        let creds_path = storage::credentials_path(&dirs);
        let http = HttpClient::new();
        let pat = resolver::pat_from_env();

        match resolver::resolve(&creds_path, &http, pat) {
            Ok(mut cred) => {
                let principal = client::get_me(&http, &api_url, &mut cred)?;
                let expires_at = session_expiry(&creds_path);
                let env_name = profile::build_env_name();
                if self.json {
                    print_json(env_name, &api_url, &principal, expires_at);
                } else {
                    print_human(env_name, &api_url, &principal, expires_at);
                }
                Ok(())
            }
            Err(Error::NotAuthenticated) => {
                if self.json {
                    let doc = serde_json::json!({
                        "authenticated": false,
                        "profile": profile::build_env_name(),
                        "api_url": api_url,
                    });
                    println!("{doc}");
                } else {
                    println!(
                        "Not authenticated ({}). Run `peppy auth login`.",
                        profile::build_env_name()
                    );
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// Reads the cached session's access-token expiry (unix seconds), if any. A PAT
/// has no stored expiry, so this returns `None`.
fn session_expiry(creds_path: &Path) -> Option<i64> {
    storage::load(creds_path)
        .ok()?
        .session
        .map(|pc| pc.expires_at)
}

fn token_is_valid(expires_at: Option<i64>) -> bool {
    // No stored expiry (PAT) but `/me` succeeded → treat as valid.
    // This is a display heuristic for `whoami` output; the authoritative
    // expiry check (with a 30s skew) lives in `ProfileCreds::is_expired` and
    // is used by the resolver to decide when to refresh. A token may read as
    // "valid" here for up to 30s after the resolver would already consider it
    // expired.
    match expires_at {
        None => true,
        Some(exp) => storage::now_unix() < exp,
    }
}

fn print_human(env_name: &str, api_url: &str, p: &Principal, expires_at: Option<i64>) {
    println!("Logged in as {} ({env_name})", p.display_name());
    println!("  subject : {}", p.sub);
    if let Some(kind) = &p.kind {
        println!("  type    : {kind}");
    }
    if let Some(role) = &p.role {
        println!("  role    : {role}");
    }
    if let Some(email) = &p.email {
        println!("  email   : {email}");
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
            "id": p.id,
            "sub": p.sub,
            "kind": p.kind,
            "username": p.username,
            "email": p.email,
            "role": p.role,
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
    fn token_validity_handles_pat_and_expiry() {
        assert!(token_is_valid(None), "PAT (no expiry) reads as valid");
        assert!(token_is_valid(Some(storage::now_unix() + 60)));
        assert!(!token_is_valid(Some(storage::now_unix() - 60)));
    }
}
